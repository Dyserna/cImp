#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod advisor;
mod attach;
mod audio;
mod checks;
mod compact_hook;
mod content;
mod context_hook;
mod error;
mod graph;
mod ipc;
mod logging;
mod notifications;
mod oob;
mod offload;
mod postedit_hook;
mod preview;
mod process_guard;
mod processing;
mod pty;
mod read_hook;
mod settings;
mod shell;
mod state;
mod statusline;
mod stt;
mod sysmon;
mod theming;
mod tabs;
mod tts;
mod usage;
mod workbench;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, RwLock};

use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tokio::sync::{broadcast, mpsc, Mutex as TokioMutex};
use tracing::{info, warn};

use crate::audio::{spawn_amplitude_streamer, AudioOutput};
use crate::ipc::commands::{
    acknowledge_error, advisor_dismiss, ai_tool_tab_defaults, close_settings_window,
    compose_attach_image,
    compose_content_changed,
    compose_templates, compose_templates_global_get, compose_templates_global_set,
    compose_templates_project_get,
    consume_settings_deep_link, content_clear, content_open_folder, get_claude_usage,
    get_system_stats, graph_cycles, graph_dead_exports, graph_fact_add, graph_fact_update, graph_facts,
    graph_history, graph_impact, graph_language_census,
    graph_context_preview, graph_memory, graph_memory_clear, graph_note_set_pinned, graph_rebuild,
    graph_rebuild_embeddings, graph_set_language_enabled, graph_set_watch_paused,
    graph_status, graph_test_embedder, graph_usage, graph_usage_advice, list_tabs,
    list_voices, offload_backend_restart, offload_backend_start, offload_backend_stop,
    offload_derive_opencode_provider,
    offload_server_log, offload_server_metrics, offload_server_restart, offload_server_start,
    offload_server_stop,
    offload_service_status,
    offload_reload_mcp,
    offload_status,
    offload_statuses, offload_test, open_settings_window, open_settings_window_to_tab,
    pty_get_scrollback,
    pty_rebind_channel, pty_resize, pty_restart, pty_start, pty_write, request_tab_restart,
    restart_shell_tab, set_active_tab, set_window_square_corners, settings_get, settings_update,
    stt_cancel, stt_list_input_devices, stt_list_models, stt_start_recording, stt_stop_recording,
    tab_activate, tts_speak, tts_set_paused, tts_speak_selection, tts_stop, tts_test,
    workbench_checkpoint_diff, workbench_checkpoint_now, workbench_checkpoints, workbench_diff_file,
    workbench_diff_summary, workbench_restore, workbench_revert_hunk, workbench_send_hunk,
    workbench_status, workbench_worktree_check_status, workbench_worktree_create,
    workbench_worktree_diff, workbench_worktree_discard, workbench_worktree_merge,
    workbench_worktree_run_checks, workbench_worktrees,
};
use crate::ipc::layout::{
    delete_layout_preset, rename_layout_preset, save_layout, save_layout_preset,
};
use crate::ipc::note::{read_note, write_note};
use crate::ipc::tab_lifecycle::{
    close_tab, create_ai_tab, create_ai_tab_in_worktree, create_preview_tab, create_shell_tab,
    default_shell_spec, get_shell_tab_config, open_note_tab, open_tool_tab,
    reconfigure_shell_tab, rename_tab, set_enabled_ai_tabs,
};
use crate::ipc::{AppState, LaunchContext};
use crate::settings::{
    LayoutNodePersisted, LayoutPersisted, LogLevel, LogRetention, Settings, SettingsHandle,
    TabConfig,
};
use crate::state::{spawn_state_manager, StateEvent, StateSignal, TabId, TabKind, TabMeta};
use crate::stt::SttHandle;
use crate::preview::{
    preview_capture, preview_close, preview_hide, preview_navigate, preview_open,
    preview_reload, preview_set_rect, preview_show, preview_update_config,
};
use crate::tabs::{TabRegistry, TabRegistryHandle};
use crate::tts::{spawn_tts_worker, ActiveTab, AiTtsSuppressed, SpeakSession, TtsRequest};

fn main() {
    // Status-line subcommand: Claude Code invokes `cimp --statusline`,
    // pipes the session JSON to our stdin, and reads the rendered context
    // bar from our stdout. Handle it before any Tauri/audio/settings init
    // so it's instant and never spins up the GUI. Works under the release
    // `windows` subsystem too — inherited stdio pipes stay usable; only
    // console allocation is suppressed.
    if std::env::args().skip(1).any(|a| a == "--statusline") {
        statusline::run();
        return;
    }

    // V10 context-injection hook: Claude Code invokes `cimp --context-hook` as a
    // UserPromptSubmit hook, pipes the hook JSON to our stdin, and reads the
    // `additionalContext` to prepend from our stdout. Handled here before any
    // Tauri init so it stays GUI-free and fast; on any error it prints nothing.
    if std::env::args().skip(1).any(|a| a == "--context-hook") {
        context_hook::run();
        return;
    }

    // V11 Phase D context-compaction hook: Claude Code invokes
    // `cimp --precompact-hook` as a `PreCompact` hook. It POSTs to the app's
    // loopback `/context/compaction` (which clears dedup + marks the session
    // post-compaction, and returns a working-set/notes block to survive the
    // summary). GUI-free and fast like the other shims; prints nothing on error.
    if std::env::args().skip(1).any(|a| a == "--precompact-hook") {
        compact_hook::run();
        return;
    }

    // V11 Phase E read advisor: Claude Code invokes `cimp --read-hook` as a
    // `PreToolUse` (matcher `Read`) hook. It asks the app's loopback
    // `/context/should_read` whether the file was already read unchanged this
    // session; on a "remind" it denies the Read with the file's outline/body as
    // the reason. GUI-free like the other shims; prints nothing (⇒ read proceeds)
    // on any error, so it never wrongly blocks a read.
    if std::env::args().skip(1).any(|a| a == "--read-hook") {
        read_hook::run();
        return;
    }

    // V12 Phase F: Claude Code invokes `cimp --postedit-hook` as a
    // `PostToolUse` (matcher `Edit|Write|MultiEdit`) hook right after an edit
    // completes. It POSTs to the app's loopback `/context/post_edit` (which
    // debounces, runs the project's configured checks, and diffs against the
    // session's baseline) and prints only NEW/worsened diagnostics as
    // additional context. GUI-free like the other shims; prints nothing on
    // any error, so it never blocks or perturbs an edit.
    if std::env::args().skip(1).any(|a| a == "--postedit-hook") {
        postedit_hook::run();
        return;
    }

    // V8-01 offload MCP server: a host agent invokes `cimp --offload-mcp`
    // (Claude via `--mcp-config`; OpenCode via the injected `mcp` block) and
    // speaks newline-delimited JSON-RPC over stdio. Handle it before any
    // Tauri/audio/settings init so it stays GUI-free and fast to spawn per
    // session — same contract as `--statusline`. It connects to the app-owned
    // llama-server over HTTP and never loads its own model.
    //
    // V19: an optional `--consumer <name>` (default `claude`, or `opencode`)
    // selects which per-consumer MCP-server set the app proxies to this child.
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.iter().any(|a| a == "--offload-mcp") {
            let consumer = args
                .iter()
                .position(|a| a == "--consumer")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str)
                .unwrap_or("claude");
            offload::mcp::run(consumer);
            return;
        }
    }

    let launch_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let extra_args: Vec<String> = std::env::args().skip(1).collect();
    // V14 Phase B: one id per app run, scoping the compose overlay's
    // image-attachment temp dir (`attach::attach_dir`). See
    // `LaunchContext::launch_id`'s doc comment.
    let launch_id = uuid::Uuid::new_v4().to_string();

    // Window title reflects the project the user launched from. If the
    // launch cwd is anywhere inside a git working copy, the title uses
    // the repo-root folder name; otherwise it falls back to the launch
    // dir's own folder name. Format is "<project> - cImp". Applied to
    // the main window in the Tauri setup hook below.
    let window_title = format!("{} - cImp", project_label_for(&launch_cwd));

    // Tracing comes up before settings load so the load path's own logs
    // hit the file. The default-level guard is replaced once settings
    // load completes (`logging::set_level` below); RUST_LOG, when set,
    // wins over both.
    let _log_guard = logging::init(LogLevel::default());
    info!(
        cwd = %launch_cwd.display(),
        args = ?extra_args,
        logs_dir = %logging::logs_dir().display(),
        "cimp starting"
    );

    // Probe the platform default shell once and cache it for every Shell
    // tab launch path. The cache is an `Arc` so the registry, settings
    // window, and the new-shell-tab dialog (M2) can all share it without
    // re-running detection.
    let default_shell = Arc::new(shell::detect::default_shell());

    // Settings load runs migration (v1 / v1.1 → v1.2) and an integrity
    // check that ensures the three reserved-id tab entries exist. The
    // resolved default shell is needed to fill in Shell-1's command on
    // fresh installs and during the v1.1 → v1.2 transform that consumes
    // the legacy `_shell_1_tmp` interim key.
    let settings_handle = settings::init(&default_shell, &launch_cwd);

    // Apply the user's saved log level to the live filter. RUST_LOG, when
    // set, was already locked in by `logging::init` and remains in effect
    // until the user picks a new level here — at which point we reload to
    // the chosen level and the env override is overridden. The cleanup
    // pass deletes old rolled files per the user's retention setting.
    // Content capture is disabled by default — `set_enabled` mirrors the
    // saved flag, and the cleanup pass also runs against the content
    // subdirectory.
    {
        let snap = settings_handle.current();
        logging::set_level(snap.logging.level);
        logging::run_cleanup(snap.logging.retention);
        content::set_enabled(snap.logging.content_capture.enabled);
        content::run_cleanup(snap.logging.content_capture.retention);
    }

    // V14 Phase B: sweep compose-attach directories orphaned by a previous
    // run that crashed or was killed before its own exit-time prune (below,
    // in the `CloseRequested` handler) ran. Fixed 3-day age cap — not a
    // user setting, this is opportunistic disk hygiene, not a feature.
    attach::prune(3);

    // TTS / audio pipeline. Failures are non-fatal — the app launches with
    // TTS silent and a warning logged. Init is deferred to the Tauri `setup`
    // hook because spawn_tts_worker requires the Tauri/tokio runtime.
    let (tts_tx, tts_rx) = mpsc::channel::<TtsRequest>(64);
    let tts_rx_slot = Arc::new(Mutex::new(Some(tts_rx)));

    // State-machine input channel. Shared by every tab and every signal kind,
    // so it must be large enough that a burst of control edges (output
    // start/stop, permission resolve, subprocess exit) never overflows and
    // drops a state transition — a dropped edge desyncs the avatar/permission
    // state machine. Sized generously for that reason.
    let (state_tx, state_rx) = mpsc::channel::<StateSignal>(512);
    let state_rx_slot = Arc::new(Mutex::new(Some(state_rx)));

    // In-process broadcast of every StateEvent the manager emits to the
    // frontend. Subscribed by the notification manager (V2-04) so it can
    // queue announcements off the same edges the avatar reacts to. Capacity
    // 64 matches the input channel; lag here means a notification missed an
    // edge, which the next event recovers naturally.
    let (state_event_tx, _) = broadcast::channel::<StateEvent>(64);

    // Launch-seed tab list comes from settings now. The integrity check
    // guarantees the AI builtins (claude / claude-local) are present;
    // shell-default-1 is seeded only on a fresh install (it's a closable
    // shell, so once the user closes it, it stays closed). User-created
    // Shell tabs that have been persisted across launches are appended in
    // their stored order. Each entry's name reflects the user's last-seen
    // edit (rename, configure dialog, settings window).
    let tab_metas: Vec<TabMeta> = build_tab_metas_from_settings(&settings_handle.current());
    let seed_tabs: Vec<TabId> = tab_metas.iter().map(|m| m.id.clone()).collect();

    // Per-tab unsent-input length counters. Shared (Arc<RwLock<...>>) so
    // the state manager can grow/shrink the map at runtime while the IPC
    // layer reads counter Arcs by tab id.
    let input_lengths = crate::tabs::registry::make_input_lengths(&seed_tabs);

    let audio_slot: Arc<RwLock<Option<Arc<AudioOutput>>>> = Arc::new(RwLock::new(None));

    // Detection patterns. Lives next to settings.json on disk; auto-seeded
    // with a sensible default permission pattern + a disabled question
    // template on first launch. Hot-reload is intentionally not wired —
    // patterns rarely change and a relaunch is fine.
    let patterns = Arc::new(processing::patterns_file::load_or_seed());

    // Active-tab cell shared with the TTS worker (filters background-tab
    // synthesis requests) and the audio thread (tags TtsPlaybackStarted/
    // Stopped signals with the speaking tab). Synchronous so both consumers
    // can read it without runtime gymnastics.
    //
    // Resolution order:
    //   1. The layout's focused-pane active tab (V4-04) — this is the v1.3
    //      source of truth. After the v1.2 → v1.3 migration session.active_tab_id
    //      is dropped from settings, so on its own that field is None for
    //      migrated users; without consulting the layout here we'd start
    //      Claude-active while the frontend hydrates the layout to the
    //      user's actual last tab, and the two would ping-pong on launch.
    //   2. session.active_tab_id (legacy / fresh-install path).
    //   3. First tab in order (post-integrity that's always Claude).
    let snap = settings_handle.current();
    let layout_active_id: Option<String> = snap
        .layout
        .as_ref()
        .and_then(layout_focused_active_tab_id);
    let session_active_id: Option<&str> = snap.session.active_tab_id.as_deref();
    let resolved_id: Option<String> = layout_active_id.or_else(|| session_active_id.map(String::from));
    let initial_active = resolved_id
        .as_deref()
        .and_then(|id| {
            tab_metas
                .iter()
                .find(|m| m.id.as_str() == id)
                .map(|m| m.id.clone())
        })
        .unwrap_or_else(|| {
            tab_metas
                .first()
                .map(|m| m.id.clone())
                .unwrap_or(TabId::Claude)
        });
    drop(snap);
    let tts_active: ActiveTab = Arc::new(RwLock::new(initial_active.clone()));
    // Shared selection-read session id (0 = none). Shared between the
    // `tts_speak_selection`/`tts_stop` commands and the TTS worker.
    let speak_session: SpeakSession = Arc::new(AtomicU64::new(0));
    // Shared "suppress AI-tag TTS" flag. Set by Esc (`tts_stop`), cleared by
    // the state manager on the next `ClaudeOutputStarted`.
    let ai_tts_suppressed: AiTtsSuppressed = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // V6-01 STT handle. The capture + transcription threads are spawned in
    // the Tauri `setup` hook (they need an AppHandle to emit events); the
    // runtime half is stashed here until then, mirroring the tts_rx slot.
    let (stt_handle, stt_runtime) = SttHandle::new();
    let stt_runtime_slot = Arc::new(Mutex::new(Some(stt_runtime)));

    // Tab registry — one PtyManager per tab, lazy-spawn at frontend mount.
    let registry = TabRegistry::new(
        tab_metas.clone(),
        initial_active.clone(),
        tts_active.clone(),
        state_tx.clone(),
        patterns.clone(),
    );
    let tabs_handle: TabRegistryHandle = Arc::new(TokioMutex::new(registry));

    // Clone the TTS sender once for the notification manager — AppState
    // gets the original. Both producers race to put work on the same mpsc;
    // the worker filters/synthesizes serially.
    let tts_tx_for_notifications = tts_tx.clone();

    let state = AppState {
        stt: stt_handle,
        tabs: tabs_handle.clone(),
        launch: LaunchContext {
            cwd: launch_cwd,
            extra_args,
            launch_id,
        },
        tts_segments: tts_tx,
        speak_session: speak_session.clone(),
        ai_tts_suppressed: ai_tts_suppressed.clone(),
        user_typed_tts: Arc::new(Mutex::new(HashSet::new())),
        user_input_buf: Arc::new(Mutex::new(HashMap::new())),
        state_signals: state_tx.clone(),
        input_lengths: input_lengths.clone(),
        settings: settings_handle.clone(),
        audio: audio_slot.clone(),
        pending_settings_deep_link: Arc::new(Mutex::new(None)),
        sysmon: Arc::new(crate::sysmon::SystemStatsState::new()),
        lifecycle_serializer: Arc::new(TokioMutex::new(())),
    };

    let tts_rx_for_setup = tts_rx_slot.clone();
    let state_rx_for_setup = state_rx_slot.clone();
    let state_events_for_setup = state_event_tx.clone();
    let state_events_for_notifications = state_event_tx.clone();
    let audio_state_tx = state_tx.clone();
    let input_lengths_for_setup = input_lengths.clone();
    let settings_for_setup = settings_handle.clone();
    let settings_for_notifications = settings_handle.clone();
    let tts_active_for_setup = tts_active.clone();
    let tts_active_for_notifications = tts_active.clone();
    let speak_session_for_setup = speak_session.clone();
    let ai_tts_suppressed_for_tts = ai_tts_suppressed.clone();
    let ai_tts_suppressed_for_state = ai_tts_suppressed.clone();
    let initial_active_for_state = initial_active.clone();
    let initial_active_for_tts = initial_active.clone();
    let initial_active_for_notifications = initial_active.clone();
    let stt_runtime_for_setup = stt_runtime_slot.clone();
    let settings_for_stt = settings_handle.clone();
    let settings_for_offload = settings_handle.clone();
    let settings_for_graph = settings_handle.clone();
    let settings_for_workbench = settings_handle.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        // V14 Phase F: "open in the system browser" for Preview-tab links that
        // fall outside the localhost/RFC-1918 navigation policy (or a denied
        // `window.open`) — see `preview::open_external`.
        .plugin(tauri_plugin_opener::init())
        // Windows 11 Snap Layouts for the custom TuiTitleBar maximize button
        // (id "snap-max-btn"). No-op on Linux/macOS and pre-Win11, so it's
        // registered unconditionally. The frontend attaches/detaches the
        // overlay per-window as the title bar mounts (see TuiTitleBar.svelte).
        .plugin(
            tauri_plugin_snap_layout::init()
                .button_id("snap-max-btn")
                .build(),
        )
        .manage(state)
        // V14 Phase F: one child webview per open Preview tab, keyed by tab
        // id — needs no AppHandle to construct (unlike WorkbenchService/
        // GraphService below), so it's managed right away rather than from
        // inside `.setup()`.
        .manage(preview::PreviewRegistry::default())
        .setup(move |app| {
            // Recover a poisoned guard rather than `.ok()` skipping it: silently
            // not spawning the state manager would leave the whole avatar /
            // permission state machine dead with no diagnostic.
            if let Some(rx) = state_rx_for_setup
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                spawn_state_manager(
                    app.handle().clone(),
                    rx,
                    state_events_for_setup.clone(),
                    input_lengths_for_setup.clone(),
                    tab_metas.clone(),
                    initial_active_for_state,
                    ai_tts_suppressed_for_state.clone(),
                );
            }
            if let Some(rx) = tts_rx_for_setup
                .lock()
                .ok()
                .and_then(|mut g| g.take())
            {
                init_tts_pipeline(
                    app.handle().clone(),
                    rx,
                    audio_state_tx.clone(),
                    settings_for_setup.clone(),
                    audio_slot.clone(),
                    tts_active_for_setup.clone(),
                    initial_active_for_tts,
                    speak_session_for_setup.clone(),
                    ai_tts_suppressed_for_tts.clone(),
                );
                // Notification manager piggybacks on the audio output we
                // just built. If audio init failed above, audio_slot is
                // None and we skip — without audio there's nothing to
                // play and nothing to wait on.
                if let Some(audio) = audio_slot
                    .read()
                    .ok()
                    .and_then(|g| g.as_ref().cloned())
                {
                    notifications::spawn_notification_manager(
                        state_events_for_notifications.subscribe(),
                        audio,
                        tts_tx_for_notifications.clone(),
                        settings_for_notifications.clone(),
                        tts_active_for_notifications.clone(),
                        initial_active_for_notifications,
                    );
                }
            }
            // V6-01 STT: spawn the capture + transcription threads. The
            // engine is constructed lazily on the first recording, so a
            // missing model never blocks launch — it surfaces as an `error`
            // state on the first record attempt instead.
            if let Some(rt) = stt_runtime_for_setup
                .lock()
                .ok()
                .and_then(|mut g| g.take())
            {
                stt::spawn(app.handle().clone(), settings_for_stt.clone(), rt);
            }

            // V8-01 offload: construct the supervisor (needs the
            // AppHandle for `offload-state` events) and manage it as its
            // own state. With `enabled` + `autostart`, kick off a
            // non-blocking start; otherwise it stays Stopped/Disabled and
            // the user starts it from Settings (or it's lazy on first
            // offload). Fail-soft: a bad command surfaces as an Error
            // status, never blocks launch.
            {
                let supervisor = crate::offload::OffloadSupervisor::new(
                    app.handle().clone(),
                    settings_for_offload.clone(),
                );
                app.manage(supervisor.clone());

                // V8-03: the app-side offload service — owns the warm pool,
                // the global concurrency gate, the router, and the MCP host.
                // Managed unconditionally so the IPC + loopback can reach it;
                // the heavy machinery (warm host, loopback endpoint, health
                // watch) only spins up when offload is enabled.
                let service = crate::offload::OffloadService::new(
                    app.handle().clone(),
                    settings_for_offload.clone(),
                    supervisor.clone(),
                );
                app.manage(service.clone());

                // The offload runtime (autostart, warm host, loopback discovery
                // endpoint, health watch, metrics poller) is started by a
                // single idempotent helper. `started` guards against a double
                // start — both the launch path and the runtime-enable watcher
                // below call it, but the loopback binds a port and the pollers
                // spawn tasks, so it must run at most once.
                let offload_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                // Start the runtime (loopback + warm host) when offload is enabled
                // OR a server is exposed to Claude Code — Claude reaches MCP tools
                // over the loopback independent of offload.
                if settings_for_offload.current().offload.mcp_host_needed() {
                    if !offload_started.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        start_offload_runtime(
                            app.handle().clone(),
                            service.clone(),
                            supervisor.clone(),
                        );
                    }
                }
                // V8: a user who launches with offload disabled and enables it
                // later in Settings must still get the loopback discovery
                // endpoint (without it, MCP children can't connect back). Same
                // for adding a Claude-Code-exposed MCP server while offload is
                // off. Watch for either transition and start once.
                // (Disabling at runtime leaves the runtime up but harmless —
                // `OffloadService::run` is gated on `enabled` and refuses; a
                // full teardown happens on the next relaunch.)
                {
                    let svc = service.clone();
                    let sup = supervisor.clone();
                    let app_handle = app.handle().clone();
                    let watch = settings_for_offload.clone();
                    let started = offload_started.clone();
                    tauri::async_runtime::spawn(async move {
                        let mut rx = watch.subscribe();
                        loop {
                            match rx.recv().await {
                                Ok(s) => {
                                    if s.offload.mcp_host_needed()
                                        && !started.swap(true, std::sync::atomic::Ordering::SeqCst)
                                    {
                                        info!("offload: MCP host needed at runtime — starting offload runtime");
                                        start_offload_runtime(
                                            app_handle.clone(),
                                            svc.clone(),
                                            sup.clone(),
                                        );
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    });
                }
            }

            // V13 Phase A: the Workbench service (fs-batch broadcast today;
            // checkpoint scheduling and worktree bookkeeping in later phases).
            // Managed unconditionally, before the graph service below, since
            // `GraphService::reindex_paths` looks it up via `AppHandle::state`
            // on every watcher batch — construct it first so that lookup never
            // races an empty state table during startup.
            {
                let workbench_service = crate::workbench::WorkbenchService::new(
                    app.handle().clone(),
                    settings_for_workbench.clone(),
                );
                // V13 Phase D D3: reconcile git's worktree bookkeeping once at
                // startup (a worktree directory the user deleted out-of-band
                // since the last run). Best-effort/fire-and-forget — see
                // `worktree_prune_at_startup`'s doc comment; never blocks launch.
                if let Ok(root) = std::env::current_dir() {
                    let svc = workbench_service.clone();
                    tauri::async_runtime::spawn(async move {
                        svc.worktree_prune_at_startup(&root).await;
                    });
                }
                app.manage(workbench_service);
            }

            // V9-01 code knowledge graph: the app-owned graph service that
            // builds `<root>/<db_subdir>/graph.db` so the `graph_*` MCP tools
            // have data to read. Managed unconditionally (the IPC reaches it
            // either way); a full build only runs when the feature is enabled.
            // Like the supervisor, it's fail-soft — a build error surfaces as
            // an `error` status, never blocks launch.
            {
                let graph_service = crate::graph::GraphService::new(
                    app.handle().clone(),
                    settings_for_graph.clone(),
                );
                app.manage(graph_service.clone());

                // Build the launch project's graph in the background on startup
                // so a session opened immediately after launch finds an index.
                // Runtime enable (false→true) also kicks one build via the
                // settings watcher below.
                if settings_for_graph.current().graph.enabled {
                    if let Ok(root) = std::env::current_dir() {
                        graph_service.spawn_rebuild(root.clone());
                        // Phase D: keep the index live as files change.
                        graph_service.start_watch(root);
                    }
                }
                {
                    let svc = graph_service.clone();
                    let watch = settings_for_graph.clone();
                    tauri::async_runtime::spawn(async move {
                        let mut rx = watch.subscribe();
                        let mut was_enabled = watch.current().graph.enabled;
                        loop {
                            match rx.recv().await {
                                Ok(s) => {
                                    let now = s.graph.enabled;
                                    if now && !was_enabled {
                                        if let Ok(root) = std::env::current_dir() {
                                            info!("graph: enabled at runtime — building index");
                                            svc.spawn_rebuild(root.clone());
                                            svc.start_watch(root);
                                        }
                                    }
                                    was_enabled = now;
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    });
                }
            }

            spawn_settings_broadcast(app.handle().clone(), settings_for_setup.clone());

            // Apply the project-derived window title. The hardcoded
            // "cImp" from tauri.conf.json is what the OS sees before
            // this fires; this overwrite happens during setup so the
            // user only briefly sees the bare default.
            if let Some(win) = app.get_webview_window("main") {
                if let Err(e) = win.set_title(&window_title) {
                    warn!(error = %e, "set_title for main window failed");
                }
            }

            // V1.4-04 D.6: orphan-prune the scrollback dir so files
            // for tabs deleted between sessions don't accumulate. We
            // ask the registry for its sanitized known IDs (matches
            // exactly what `pty::scrollback::scrollback_file_for`
            // writes).
            {
                let app_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let state = app_handle.state::<AppState>();
                    let registry = state.tabs.lock().await;
                    let known = registry.known_scrollback_ids();
                    drop(registry);
                    crate::pty::scrollback::prune_orphans(&known);
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pty_start,
            pty_restart,
            pty_rebind_channel,
            pty_get_scrollback,
            pty_write,
            pty_resize,
            tts_test,
            tts_speak,
            tts_speak_selection,
            tts_stop,
            tts_set_paused,
            stt_start_recording,
            stt_stop_recording,
            stt_cancel,
            stt_list_models,
            stt_list_input_devices,
            get_claude_usage,
            get_system_stats,
            settings_get,
            settings_update,
            ai_tool_tab_defaults,
            list_voices,
            list_tabs,
            open_settings_window,
            open_settings_window_to_tab,
            consume_settings_deep_link,
            close_settings_window,
            request_tab_restart,
            restart_shell_tab,
            compose_content_changed,
            compose_templates,
            compose_templates_global_get,
            compose_templates_global_set,
            compose_templates_project_get,
            compose_attach_image,
            acknowledge_error,
            tab_activate,
            set_active_tab,
            create_shell_tab,
            create_ai_tab,
            create_ai_tab_in_worktree,
            create_preview_tab,
            preview_open,
            preview_navigate,
            preview_reload,
            preview_set_rect,
            preview_hide,
            preview_show,
            preview_close,
            preview_capture,
            preview_update_config,
            close_tab,
            rename_tab,
            reconfigure_shell_tab,
            default_shell_spec,
            get_shell_tab_config,
            set_enabled_ai_tabs,
            open_tool_tab,
            open_note_tab,
            read_note,
            write_note,
            set_window_square_corners,
            save_layout,
            save_layout_preset,
            delete_layout_preset,
            rename_layout_preset,
            content_open_folder,
            content_clear,
            offload_status,
            offload_statuses,
            offload_server_start,
            offload_server_stop,
            offload_server_restart,
            offload_backend_start,
            offload_backend_stop,
            offload_backend_restart,
            offload_test,
            offload_derive_opencode_provider,
            offload_service_status,
            offload_reload_mcp,
            offload_server_log,
            offload_server_metrics,
            graph_status,
            graph_rebuild,
            graph_rebuild_embeddings,
            graph_set_watch_paused,
            graph_language_census,
            graph_set_language_enabled,
            graph_test_embedder,
            graph_history,
            graph_dead_exports,
            graph_cycles,
            graph_impact,
            graph_memory,
            graph_memory_clear,
            graph_note_set_pinned,
            graph_facts,
            graph_fact_update,
            graph_fact_add,
            graph_context_preview,
            graph_usage,
            graph_usage_advice,
            advisor_dismiss,
            workbench_status,
            workbench_diff_summary,
            workbench_diff_file,
            workbench_revert_hunk,
            workbench_send_hunk,
            workbench_checkpoints,
            workbench_checkpoint_diff,
            workbench_checkpoint_now,
            workbench_restore,
            workbench_worktrees,
            workbench_worktree_create,
            workbench_worktree_diff,
            workbench_worktree_merge,
            workbench_worktree_discard,
            workbench_worktree_run_checks,
            workbench_worktree_check_status,
            theming::themes_list,
            theming::palettes_list,
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let label = window.label().to_string();
                if label != "main" {
                    return;
                }
                api.prevent_close();
                let window = window.clone();
                let app = window.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppState>();
                    // Flush any settings edit still inside the 500ms debounce
                    // window before we tear down — otherwise an edit made and
                    // immediately followed by quitting (common after toggling
                    // one option and closing) never reaches disk, since the
                    // debounced saver is still mid-sleep.
                    state.settings.flush();
                    // V1.4-04 D.4: persist each tab's scrollback ring
                    // before shutting down. Best-effort — a failed
                    // persist for one tab doesn't block the others or
                    // the shutdown. Hard kills (SIGKILL, taskkill,
                    // power loss) bypass this entirely; that's the
                    // documented contract.
                    let persist_enabled = state.settings.current().terminal.scrollback.persist;
                    let registry = state.tabs.lock().await;
                    if persist_enabled {
                        let tab_ids: Vec<TabId> = registry.tab_order_snapshot();
                        for tab in tab_ids {
                            match registry.scrollback_snapshot(tab.clone()).await {
                                Ok(bytes) if !bytes.is_empty() => {
                                    if let Err(e) =
                                        crate::pty::scrollback::persist_to_disk(&tab, &bytes)
                                    {
                                        tracing::warn!(?tab, error = %e, "scrollback persist failed");
                                    }
                                }
                                Ok(_) => {} // empty ring; skip
                                Err(e) => {
                                    tracing::debug!(?tab, error = %e, "no live PTY to snapshot");
                                }
                            }
                        }
                    }
                    registry.shutdown_all().await;
                    drop(registry);
                    // V8-01/V8-02: kill every local offload `llama-server`
                    // child on graceful exit so none outlive the app. (Each
                    // child is also `kill_on_drop`; this is the clean path.)
                    app.state::<std::sync::Arc<crate::offload::OffloadSupervisor>>()
                        .stop_all()
                        .await;
                    // V8-03: remove the loopback discovery file and reap the
                    // warm MCP-host server children.
                    if let Some(lb) =
                        app.try_state::<std::sync::Arc<crate::offload::loopback::Loopback>>()
                    {
                        lb.stop();
                    }
                    app.state::<std::sync::Arc<crate::offload::OffloadService>>()
                        .shutdown()
                        .await;
                    // V9-01: drop the warm graph index handles (SQLite
                    // connections close on drop).
                    app.state::<std::sync::Arc<crate::graph::GraphService>>()
                        .shutdown();
                    // V14 Phase B: best-effort compose-attach cleanup. Same
                    // 3-day age cap as the startup sweep — a clean exit
                    // doesn't guarantee THIS run's own directory is empty
                    // (an attached image may still be sitting in a draft the
                    // user never submitted), so this only ever catches
                    // directories already past the age cap, same as startup.
                    attach::prune(3);
                    // V14 code-review fix (webview leak): best-effort drain of
                    // every still-open Preview child webview. Each one is
                    // otherwise destroyed only by its own tab's close (or the
                    // frontend's `onDestroy`), so a Preview tab left open at
                    // quit time would leave its child webview attached through
                    // the rest of this teardown; catch it here too.
                    preview::close_all(&app.state::<preview::PreviewRegistry>());
                    // Closing the main window also closes the settings window
                    // if it's open — otherwise it would keep the process alive
                    // with no main window. Destroy it before the main window.
                    if let Some(settings) =
                        app.get_webview_window(crate::ipc::windows::SETTINGS_LABEL)
                    {
                        let _ = settings.destroy();
                    }
                    let _ = window.destroy();
                });
            }
        })
        .run(tauri::generate_context!())
        .expect("failed to launch tauri app");
}

/// Start the offload runtime: autostart opted-in Local backends, warm the MCP
/// host, spawn the health watch + metrics poller, and bring up the loopback
/// discovery endpoint. Call at most once (guarded by the caller) — the loopback
/// binds a port and the pollers spawn long-lived tasks.
fn start_offload_runtime(
    app_handle: tauri::AppHandle,
    service: std::sync::Arc<crate::offload::OffloadService>,
    supervisor: std::sync::Arc<crate::offload::OffloadSupervisor>,
) {
    // V8-02: autostart every Local backend that opted in.
    tauri::async_runtime::spawn(async move {
        supervisor.autostart_all().await;
    });
    // V8-03: warm the MCP host, start the loopback endpoint (writes the
    // discovery file), and watch backend health so `/events` →
    // `tools/list_changed` tracks up/down.
    tauri::async_runtime::spawn(async move {
        service.warm_host().await;
        service.spawn_health_watch();
        service.spawn_metrics_poller();
        match crate::offload::loopback::Loopback::start(service.clone(), app_handle.clone()).await {
            Ok(lb) => {
                app_handle.manage(lb);
            }
            Err(e) => {
                warn!(error = %e, "offload: loopback endpoint failed to start")
            }
        }
    });
}

/// Resolve the project label used in the OS window title. Walks up
/// from `cwd` looking for a `.git` entry (directory or file — submodules
/// and worktrees use a `.git` file pointing at the parent's gitdir).
/// The first ancestor that has one wins, and its folder name becomes
/// the label. With no `.git` anywhere along the chain, the launch
/// directory's own folder name is used. A final fallback to "cImp"
/// covers degenerate paths like a drive root with no file_name segment.
fn project_label_for(cwd: &Path) -> String {
    let mut dir = cwd;
    loop {
        if dir.join(".git").exists() {
            if let Some(name) = dir.file_name().and_then(|s| s.to_str()) {
                return name.to_string();
            }
            break;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    cwd.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("cImp")
        .to_string()
}

/// Find the persisted layout's focused pane and return its active tab
/// id. Returns `None` if the focused-pane id doesn't match any pane
/// (the integrity check at load time normally repairs this) or if the
/// focused pane has no active tab (transient empty pane).
fn layout_focused_active_tab_id(layout: &LayoutPersisted) -> Option<String> {
    fn find<'a>(node: &'a LayoutNodePersisted, target: &str) -> Option<&'a Option<String>> {
        match node {
            LayoutNodePersisted::Pane {
                id, active_tab_id, ..
            } => {
                if id == target {
                    Some(active_tab_id)
                } else {
                    None
                }
            }
            LayoutNodePersisted::Split { first, second, .. } => {
                find(first, target).or_else(|| find(second, target))
            }
        }
    }
    find(&layout.tree, &layout.focused_pane_id)
        .and_then(|opt| opt.clone())
}

/// Build the launch-seed `Vec<TabMeta>` from a settings snapshot.
/// Reserved AI ids map to their corresponding `TabId` variants;
/// everything else is a Shell tab. The integrity check has already
/// guaranteed every id named in `enabled_ai_tabs` is present, so the
/// result always has at least one AI builtin (and a `shell-default-1`
/// on fresh installs unless the user has closed it).
fn build_tab_metas_from_settings(settings: &Settings) -> Vec<TabMeta> {
    settings
        .tabs
        .iter()
        .map(|cfg| {
            let tab_id = TabId::from_str(cfg.id());
            let kind = match cfg {
                TabConfig::AiTool(_) => TabKind::AiTool,
                TabConfig::Shell(_) => TabKind::Shell,
                TabConfig::Preview(_) => TabKind::Preview,
            };
            TabMeta {
                id: tab_id,
                kind,
                name: cfg.name().to_string(),
            }
        })
        .collect()
}

fn init_tts_pipeline(
    app: AppHandle,
    tts_rx: mpsc::Receiver<TtsRequest>,
    state_signals: mpsc::Sender<StateSignal>,
    settings: SettingsHandle,
    audio_slot: Arc<RwLock<Option<Arc<AudioOutput>>>>,
    active: ActiveTab,
    initial_active: TabId,
    speak_session: SpeakSession,
    ai_tts_suppressed: AiTtsSuppressed,
) {
    let audio = match AudioOutput::new(state_signals.clone(), settings.clone(), active.clone()) {
        Ok(a) => Arc::new(a),
        Err(e) => {
            warn!(error = %e, "audio output unavailable; TTS will be silent");
            let _ = state_signals.try_send(StateSignal::AudioError { tab: initial_active.clone() });
            drop(tts_rx);
            return;
        }
    };

    if let Ok(mut slot) = audio_slot.write() {
        *slot = Some(audio.clone());
    }

    spawn_amplitude_streamer(app, audio.clone());

    // The worker owns the engine lifecycle now: it loads the Kokoro model when
    // `tts.enabled` is on (and reloads/unloads it as that toggles), so this
    // setup no longer constructs the engine eagerly. (`initial_active` above
    // labels the audio-error signal.)
    spawn_tts_worker(
        audio,
        tts_rx,
        state_signals,
        settings,
        active,
        speak_session,
        ai_tts_suppressed,
    );
}

fn spawn_settings_broadcast(app: AppHandle, settings: SettingsHandle) {
    tauri::async_runtime::spawn(async move {
        let mut rx = settings.subscribe();
        let initial = settings.current();
        let mut current_log_level = initial.logging.level;
        let mut current_retention: LogRetention = initial.logging.retention;
        let mut current_content_enabled = initial.logging.content_capture.enabled;
        let mut current_content_retention: LogRetention = initial.logging.content_capture.retention;
        let _ = app.emit("settings-changed", initial);
        loop {
            match rx.recv().await {
                Ok(s) => {
                    if s.logging.level != current_log_level {
                        current_log_level = s.logging.level;
                        logging::set_level(current_log_level);
                    }
                    if s.logging.retention != current_retention {
                        current_retention = s.logging.retention;
                        logging::run_cleanup(current_retention);
                    }
                    if s.logging.content_capture.enabled != current_content_enabled {
                        current_content_enabled = s.logging.content_capture.enabled;
                        content::set_enabled(current_content_enabled);
                    }
                    if s.logging.content_capture.retention != current_content_retention {
                        current_content_retention = s.logging.content_capture.retention;
                        content::run_cleanup(current_content_retention);
                    }
                    let _ = app.emit::<Settings>("settings-changed", s);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "settings broadcast lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

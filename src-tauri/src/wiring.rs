//! Startup wiring — every service `main`'s Tauri `setup` hook brings up, in
//! the order it brings them up in.
//!
//! **R16 (V42).** `main()` was 1 137 lines. Inside it, one `.setup()` closure
//! ran to ~380 and was preceded by 22 consecutive `let x_for_y = h.clone()`
//! aliases whose only purpose was to give that closure (and the two other
//! `move` closures around it) each their own copy of the same dozen handles.
//! The aliases are now the fields of one `#[derive(Clone)] struct Wiring`, and
//! the closure is one `wire_*` call per service. Nothing here is new code:
//! each method is a block of the old `setup` body, unindented.
//!
//! # The order is load-bearing and stays literal
//!
//! [`Wiring::wire_workbench`] runs **before** [`Wiring::wire_graph`] because
//! `GraphService::reindex_paths` looks the workbench service up through
//! `AppHandle::state` on every watcher batch — construct it first and that
//! lookup can never race an empty state table during startup. The reason is
//! spelled at the call site in `main`, where a reordering has to delete it to
//! happen.
//!
//! Two more orderings that are not obvious from the names:
//! [`Wiring::wire_offload`] must precede [`Wiring::wire_graph`] because it
//! yields the session-push bus and the warm MCP host that the graph service and
//! the audit runner are constructed with (V30 Phase C, V38 Phase F), and the
//! state manager is wired first so that a failure anywhere later still leaves
//! the avatar/permission state machine running.
//!
//! # What is NOT here
//!
//! Command handlers. This is bootstrap: `#[tauri::command]` surfaces are
//! V42-A's business and none of them moved.

use std::sync::{Arc, Mutex, RwLock};

use tauri::{App, AppHandle, Emitter, Manager};
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

use crate::audio::{spawn_amplitude_streamer, AudioOutput};
use crate::offload::mcp_host::McpHost;
use crate::offload::service::PushRegistry;
use crate::settings::{LogRetention, Settings, SettingsHandle};
use crate::state::{
    spawn_state_manager, InputLengths, ReadOnlyTabs, StateEvent, StateManagerWiring, StateSignal,
    TabActivity, TabId, TabMeta,
};
use crate::stt::SttRuntime;
use crate::tts::{spawn_tts_worker, ActiveTab, AiTtsSuppressed, SpeakSession, TtsRequest};
use crate::{content, logging, notifications, stt};

/// Everything the `setup` blocks share.
///
/// Every field is cheap to clone (a channel sender, an `Arc`, a settings
/// handle, a small owned value) and every field used to be cloned by hand into
/// one or more `_for_<consumer>` locals — which is why `Clone` is derived
/// rather than each method taking a reference: the `move` closures the wiring
/// spawns each need their own copy, exactly as before.
///
/// The three `Arc<Mutex<Option<…>>>` slots are receivers parked at construction
/// time and `take`n by whichever block owns them, because they cannot be
/// created inside `setup` (they are the other end of channels `AppState`
/// already holds) and cannot be cloned.
#[derive(Clone)]
pub struct Wiring {
    pub settings: SettingsHandle,
    /// The in-process fanout of every `StateEvent` the manager emits.
    pub state_events: broadcast::Sender<StateEvent>,
    /// The state machine's input channel, for the producers wired here (the
    /// audio thread's error edges).
    pub state_signals: mpsc::Sender<StateSignal>,
    /// The state machine's receiver, parked until `setup` runs.
    pub state_rx: Arc<Mutex<Option<mpsc::Receiver<StateSignal>>>>,
    /// The TTS work queue's sender, for the notification manager (AppState
    /// holds the original).
    pub tts_tx: mpsc::Sender<TtsRequest>,
    /// The TTS worker's receiver, parked until `setup` runs.
    pub tts_rx: Arc<Mutex<Option<mpsc::Receiver<TtsRequest>>>>,
    /// The STT capture/transcription runtime, parked until `setup` runs.
    pub stt_runtime: Arc<Mutex<Option<SttRuntime>>>,
    /// Filled in by [`Self::wire_tts_and_notifications`] if audio comes up.
    pub audio: Arc<RwLock<Option<Arc<AudioOutput>>>>,
    pub input_lengths: InputLengths,
    pub read_only: ReadOnlyTabs,
    pub tab_activity: TabActivity,
    pub tab_metas: Vec<TabMeta>,
    pub initial_active: TabId,
    pub tts_active: ActiveTab,
    pub speak_session: SpeakSession,
    pub ai_tts_suppressed: AiTtsSuppressed,
    /// `"<project> - cImp"`, applied to the main window during `setup`.
    pub window_title: String,
}

/// What [`Wiring::wire_offload`] hands the producers wired after it.
///
/// V30 Phase C: the block yields the session-push bus
/// (`OffloadService::push_registry`) so the producers constructed further down
/// — the graph service and the audit runner — can announce their long-running
/// completions into channel-armed sessions. Only the send half travels; nothing
/// holds the service itself, so no Arc cycle.
///
/// V38 Phase F: and the MCP host, for the audit runner's tier-2 provider tools.
///
/// V42 Phase A2: and the supervisor itself, which the graph service's memory
/// distiller and file-digest cache run their local-only prompts through. That
/// one used to be an `AppHandle::try_state` reach from inside `GraphService`;
/// the handle travels the same way the push bus already did, and for the same
/// reason — the producer is wired after the layer it needs.
pub struct OffloadHandoff {
    pub pushes: Arc<PushRegistry>,
    pub mcp_host: Arc<McpHost>,
    pub supervisor: Arc<crate::offload::OffloadSupervisor>,
}

impl Wiring {
    /// Spawn the state-manager task.
    pub fn wire_state_manager(&self, app: &App) {
        // Recover a poisoned guard rather than `.ok()` skipping it: silently
        // not spawning the state manager would leave the whole avatar /
        // permission state machine dead with no diagnostic.
        if let Some(rx) = self
            .state_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            spawn_state_manager(
                app.handle().clone(),
                rx,
                StateManagerWiring {
                    state_events: self.state_events.clone(),
                    input_lengths: self.input_lengths.clone(),
                    activity: self.tab_activity.clone(),
                    tab_metas: self.tab_metas.clone(),
                    initial_active: self.initial_active.clone(),
                    ai_tts_suppressed: self.ai_tts_suppressed.clone(),
                },
            );
        }
    }

    /// Bring up the audio output + TTS worker, and — only if audio came up —
    /// the notification manager on top of it.
    pub fn wire_tts_and_notifications(&self, app: &App) {
        if let Some(rx) = self.tts_rx.lock().ok().and_then(|mut g| g.take()) {
            self.init_tts_pipeline(app.handle().clone(), rx);
            // Notification manager piggybacks on the audio output we
            // just built. If audio init failed above, audio_slot is
            // None and we skip — without audio there's nothing to
            // play and nothing to wait on.
            if let Some(audio) = self.audio.read().ok().and_then(|g| g.as_ref().cloned()) {
                notifications::spawn_notification_manager(
                    self.state_events.subscribe(),
                    audio,
                    self.tts_tx.clone(),
                    self.settings.clone(),
                    self.tts_active.clone(),
                    self.initial_active.clone(),
                );
            }
        }
    }

    /// The audio output and the TTS worker. Failures are non-fatal — the app
    /// launches with TTS silent and a warning logged.
    fn init_tts_pipeline(&self, app: AppHandle, tts_rx: mpsc::Receiver<TtsRequest>) {
        let audio = match AudioOutput::new(
            self.state_signals.clone(),
            self.settings.clone(),
            self.tts_active.clone(),
        ) {
            Ok(a) => Arc::new(a),
            Err(e) => {
                warn!(error = %e, "audio output unavailable; TTS will be silent");
                let _ = self.state_signals.try_send(StateSignal::AudioError {
                    tab: self.initial_active.clone(),
                });
                drop(tts_rx);
                return;
            }
        };

        if let Ok(mut slot) = self.audio.write() {
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
            self.state_signals.clone(),
            self.settings.clone(),
            self.tts_active.clone(),
            self.speak_session.clone(),
            self.ai_tts_suppressed.clone(),
        );
    }

    /// V6-01 STT: spawn the capture + transcription threads. The
    /// engine is constructed lazily on the first recording, so a
    /// missing model never blocks launch — it surfaces as an `error`
    /// state on the first record attempt instead.
    pub fn wire_stt(&self, app: &App) {
        if let Some(rt) = self.stt_runtime.lock().ok().and_then(|mut g| g.take()) {
            stt::spawn(app.handle().clone(), self.settings.clone(), rt);
        }
    }

    /// V8-01 offload: construct the supervisor (needs the
    /// AppHandle for `offload-state` events) and manage it as its
    /// own state. With `enabled` + `autostart`, kick off a
    /// non-blocking start; otherwise it stays Stopped/Disabled and
    /// the user starts it from Settings (or it's lazy on first
    /// offload). Fail-soft: a bad command surfaces as an Error
    /// status, never blocks launch.
    pub fn wire_offload(&self, app: &App) -> OffloadHandoff {
        let supervisor =
            crate::offload::OffloadSupervisor::new(app.handle().clone(), self.settings.clone());
        app.manage(supervisor.clone());

        // V8-03: the app-side offload service — owns the warm pool,
        // the global concurrency gate, the router, and the MCP host.
        // Managed unconditionally so the IPC + loopback can reach it;
        // the heavy machinery (warm host, loopback endpoint, health
        // watch) only spins up when offload is enabled.
        let service = crate::offload::OffloadService::new(
            app.handle().clone(),
            self.settings.clone(),
            supervisor.clone(),
        );
        app.manage(service.clone());

        // The offload runtime (autostart, warm host, loopback discovery
        // endpoint, health watch, metrics poller) is started by a
        // single idempotent helper. `started` guards against a double
        // start — both the launch path and the runtime-enable watcher
        // below call it, but the loopback binds a port and the pollers
        // spawn tasks, so it must run at most once.
        let offload_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Start the runtime (loopback + warm host) when ANY feature
        // whose out-of-process children dial back in needs it: offload
        // enabled, an MCP server exposed to Claude Code/OpenCode, the
        // graph (its tools ride the injected cimp-offload server and
        // the hook shims), or Code Audit exposed to a stdio consumer
        // (`cimp --code-audit-mcp` proxies to `/audit/run`). Gating on
        // offload alone stranded audit-only/graph-only projects with
        // "cImp is not running" tool errors.
        if self.settings.current().loopback_needed()
            && !offload_started.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            start_offload_runtime(app.handle().clone(), service.clone(), supervisor.clone());
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
            let watch = self.settings.clone();
            let started = offload_started.clone();
            tauri::async_runtime::spawn(async move {
                let mut rx = watch.subscribe();
                loop {
                    // H2-R1 (2026-08-05 review): a LAGGED receiver has
                    // DROPPED frames, and one of them can be the very
                    // false→true `loopback_needed` edge this task
                    // exists to catch — leaving the runtime unstarted
                    // while newly-spawned tabs inject hooks against
                    // `current()`, self-healing only on the next
                    // settings save. So Lagged is treated as "changed,
                    // re-check" and re-reads the authoritative current
                    // settings (the standard tokio broadcast pattern),
                    // instead of `continue`-ing past the edge.
                    let s = match rx.recv().await {
                        Ok(s) => s,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(
                                dropped = n,
                                "offload: settings broadcast lagged — re-checking current settings"
                            );
                            watch.current()
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
                    // Start-once / never-stop stays monotonic: the
                    // atomic swap is the only gate, so a replayed or
                    // re-read `true` after a start is a no-op, and a
                    // `false` never tears the runtime down.
                    if s.loopback_needed()
                        && !started.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        info!("offload: MCP host needed at runtime — starting offload runtime");
                        start_offload_runtime(app_handle.clone(), svc.clone(), sup.clone());
                    }
                }
            });
        }
        // V30 Phase C: hand the push bus to the producers below.
        // V38 Phase F: and the MCP host, for the audit runner's tier-2
        // provider tools.
        // V42 Phase A2: and the supervisor, for the graph service's two
        // local-only prompt paths.
        OffloadHandoff {
            pushes: service.push_registry(),
            mcp_host: service.mcp_host(),
            supervisor,
        }
    }

    /// V13 Phase A: the Workbench service (fs-batch broadcast today;
    /// checkpoint scheduling and worktree bookkeeping in later phases).
    ///
    /// Managed unconditionally, and **before** [`Self::wire_graph`] — see the
    /// module docs and the call site.
    ///
    /// V42 Phase A2: hands the service back, because `wire_graph` now
    /// *constructs* the graph service with it rather than leaving it to find
    /// one in the managed-state table on every watcher batch. That is what
    /// makes the ordering above a hand-off instead of a race.
    pub fn wire_workbench(&self, app: &App) -> Arc<crate::workbench::WorkbenchService> {
        let workbench_service = crate::workbench::WorkbenchService::new(
            std::sync::Arc::new(crate::service::sink::TauriEventSink::new(
                app.handle().clone(),
            )),
            self.settings.clone(),
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
        app.manage(workbench_service.clone());
        workbench_service
    }

    /// V9-01 code knowledge graph: the app-owned graph service that
    /// builds `<root>/<db_subdir>/graph.db` so the `graph_*` MCP tools
    /// have data to read. Managed unconditionally (the IPC reaches it
    /// either way); a full build only runs when the feature is enabled.
    /// Like the supervisor, it's fail-soft — a build error surfaces as
    /// an `error` status, never blocks launch.
    ///
    /// Carries the Code Audit runner and the tool-plugin store with it: both
    /// are constructed from the same handoff and both are published as process
    /// globals before `manage` moves them, for the reason each states.
    pub fn wire_graph(
        &self,
        app: &App,
        offload: &OffloadHandoff,
        workbench: &Arc<crate::workbench::WorkbenchService>,
    ) {
        let graph_service = crate::graph::GraphService::new(
            Arc::new(crate::service::sink::TauriEventSink::new(
                app.handle().clone(),
            )),
            self.settings.clone(),
            // V30 Phase C: announce expensive full index builds.
            Some(offload.pushes.clone()),
            // V42 Phase A2: the two `try_state` reaches this service used to
            // make, as constructor arguments. Both are `Some` here — this is
            // the only wiring that has an app to be wired into.
            Some(offload.supervisor.clone()),
            Some(workbench.clone()),
        );
        app.manage(graph_service.clone());

        // V23 Code Audit: the concurrent scan runner. Managed
        // unconditionally (the IPC reaches it either way); a scan only
        // runs when the user triggers one from the enabled tab. Root =
        // the launch project directory every scan runs against.
        {
            let audit_root =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let audit_state = crate::audit::AuditState::new(
                app.handle().clone(),
                self.settings.clone(),
                audit_root,
                // V30 Phase C: announce GUI-initiated scan completions.
                Some(offload.pushes.clone()),
                // V38 Phase F: the warm MCP host, for tier-2 provider
                // tools. The host and not the service — the runner needs
                // exactly one thing from that layer and holding the
                // service would be a cycle.
                Some(offload.mcp_host.clone()),
            );
            // V26: publish the runner as the process global BEFORE
            // `manage` moves it — this is how the offload worker's native
            // audit tools (which run outside any Tauri command context)
            // reach the state. The managed `Arc` and the global point at
            // the same runner.
            crate::audit::set_global(audit_state.clone());
            app.manage(audit_state);
        }

        // V38 Phase A: discover drop-in tool plugins from
        // `<exe-dir>/plugins/`. Managed unconditionally (the settings
        // section reads it either way) and published as the process
        // global BEFORE `manage` moves the handle — the audit seam's
        // reason, unchanged: Phase C/D's consumers run outside any
        // Tauri command context and cannot reach a managed state.
        //
        // The scan itself is off the setup thread: it walks a directory
        // and reads every file in it, and nothing on the startup path
        // needs the result synchronously — the store starts empty and
        // the settings pane reads whatever is there when it mounts.
        {
            let plugin_store = crate::plugins::PluginStore::new();
            crate::plugins::set_global(plugin_store.clone());
            app.manage(plugin_store.clone());
            tauri::async_runtime::spawn_blocking(move || {
                plugin_store.rescan();
            });
        }

        // Build the launch project's graph in the background on startup
        // so a session opened immediately after launch finds an index.
        // Runtime enable (false→true) also kicks one build via the
        // settings watcher below.
        if self.settings.current().graph.enabled {
            if let Ok(root) = std::env::current_dir() {
                // Startup housekeeping — never a session push.
                graph_service.spawn_rebuild(root.clone(), crate::graph::RebuildOrigin::Automatic);
                // Phase D: keep the index live as files change.
                graph_service.start_watch(root);
            }
        }
        {
            let svc = graph_service.clone();
            let watch = self.settings.clone();
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
                                    // Side effect of a settings save, not
                                    // a rebuild request — no push.
                                    svc.spawn_rebuild(
                                        root.clone(),
                                        crate::graph::RebuildOrigin::Automatic,
                                    );
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

    /// Re-emit every settings save to the frontend, and apply the live-settable
    /// halves of it (log level + retention, content capture, the read-only tab
    /// mirror) as it goes.
    pub fn wire_settings_broadcast(&self, app: &App) {
        let app = app.handle().clone();
        let settings = self.settings.clone();
        let read_only = self.read_only.clone();
        tauri::async_runtime::spawn(async move {
            let mut rx = settings.subscribe();
            let initial = settings.current();
            let mut current_log_level = initial.logging.level;
            let mut current_retention: LogRetention = initial.logging.retention;
            let mut current_content_enabled = initial.logging.content_capture.enabled;
            let mut current_content_retention: LogRetention =
                initial.logging.content_capture.retention;
            let _ = app.emit("settings-changed", initial);
            loop {
                match rx.recv().await {
                    Ok(s) => {
                        // V39 Phase A: `read_only` is a persisted per-tab field, so
                        // it can move through the Settings window, a project-overlay
                        // switch or a hand edit as well as through
                        // `tab_set_read_only`. Re-syncing here keeps the runtime map
                        // `pty_write` enforces from becoming a second source of
                        // truth that drifts from the file.
                        read_only.sync_users(crate::user_read_only_tabs(&s));
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

    /// V32 Phase C3 (locked decision 13): keep the detection data fresh
    /// — a debounced launch check plus a periodic due-ness poll, per
    /// component, against a curated manifest. Spawned unconditionally,
    /// and gated INSIDE the tick rather than here (#46): the task
    /// re-reads settings every tick, so the Phase G L1 master switch and
    /// the per-component modes both take effect at the next tick with no
    /// restart, and a tick under a disabled master (or with both
    /// components `off`) returns before touching the network or the
    /// disk. A spawn-time gate would have made protection a spawn-baked
    /// setting for no gain. Deliberately NOT gated on `offload.enabled`
    /// — detection guards content that reaches Claude/OpenCode tabs
    /// through the proxy too, so its data must stay current whatever the
    /// worker is doing.
    pub fn wire_detection_updater(&self) {
        crate::offload::detection::updater::spawn_scheduler(self.settings.clone());
    }

    /// V35 Phase F, trigger (b): if the installed Claude Code moved
    /// while cImp was closed — the common case, since the CLI
    /// self-updates on its own schedule — nothing observed the change,
    /// so the in-session trigger cannot fire. Run the canaries once now
    /// and let a clean result advance `claude_last_verified` by itself.
    /// Cheap and self-gating: two string comparisons against a
    /// mtime-cached read, no thread at all when the versions already
    /// match, and the work itself is a detached OS thread so startup
    /// never waits on it.
    pub fn wire_harness_verify(&self) {
        crate::harness::verify::spawn_startup_check();
    }

    /// Apply the project-derived window title. The hardcoded
    /// "cImp" from tauri.conf.json is what the OS sees before
    /// this fires; this overwrite happens during setup so the
    /// user only briefly sees the bare default.
    pub fn wire_window_title(&self, app: &App) {
        if let Some(win) = app.get_webview_window("main") {
            if let Err(e) = win.set_title(&self.window_title) {
                warn!(error = %e, "set_title for main window failed");
            }
        }
    }

    /// V1.4-04 D.6: orphan-prune the scrollback dir so files
    /// for tabs deleted between sessions don't accumulate. We
    /// ask the registry for its sanitized known IDs (matches
    /// exactly what `pty::scrollback::scrollback_file_for`
    /// writes).
    pub fn wire_scrollback_prune(&self, app: &App) {
        let app_handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            let state = app_handle.state::<crate::ipc::AppState>();
            let registry = state.tabs.lock().await;
            let known = registry.known_scrollback_ids();
            drop(registry);
            crate::pty::scrollback::prune_orphans(&known);
        });
    }
}

/// Start the offload runtime: autostart opted-in Local backends, warm the MCP
/// host, spawn the health watch + metrics poller, and bring up the loopback
/// discovery endpoint. Call at most once (guarded by the caller) — the loopback
/// binds a port and the pollers spawn long-lived tasks.
fn start_offload_runtime(
    app_handle: AppHandle,
    service: Arc<crate::offload::OffloadService>,
    supervisor: Arc<crate::offload::OffloadSupervisor>,
) {
    // V8-02: autostart every Local backend that opted in.
    tauri::async_runtime::spawn(async move {
        supervisor.autostart_all().await;
    });
    // V8-03: warm the MCP host, start the loopback endpoint (writes the
    // discovery files), and watch backend health so `/events` →
    // `tools/list_changed` tracks up/down.
    tauri::async_runtime::spawn(async move {
        service.warm_host().await;
        service.spawn_health_watch();
        // V37 C6: the MCP health checker — its own cadence, and it never
        // reconciles (see `spawn_mcp_health_watch`).
        service.spawn_mcp_health_watch();
        service.spawn_metrics_poller();
        // The launch root rides the discovery entry so MCP children spawned
        // by a DIFFERENT project's agent can't misroute to this instance
        // (per-instance `.cimp-discovery/<pid>.json`; see loopback.rs).
        let root = app_handle.state::<crate::ipc::AppState>().launch.cwd.clone();
        match crate::offload::loopback::Loopback::start(service.clone(), app_handle.clone(), &root)
            .await
        {
            Ok(lb) => {
                app_handle.manage(lb);
            }
            Err(e) => {
                warn!(error = %e, "offload: loopback endpoint failed to start")
            }
        }
    });
}

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod activity;
mod advisor;
mod attach;
mod audio;
mod audit;
mod checks;
mod content;
mod delegation;
mod error;
mod fsutil;
mod graph;
mod harness;
mod ipc;
mod logging;
mod mcp_stdio;
mod notifications;
mod offload;
mod plugins;
mod preview;
mod pricing;
mod process_guard;
mod processing;
mod procutil;
mod pty;
/// Test-only: reading this crate's own source as text, for the three source
/// scanners that gate on it (see the module docs).
#[cfg(test)]
mod rustsrc;
mod sandbox;
mod settings;
mod shell;
mod spawn_gate;
mod spawn_ledger;
mod state;
mod stt;
mod sysmon;
mod tabs;
mod theming;
mod tts;
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
    acknowledge_error, activity_clear, activity_delete, activity_detail, activity_list,
    advisor_dismiss, advisor_mark_applied, ai_tool_tab_defaults, checks_apply_proposals,
    checks_detect, checks_dismiss_suggestion, checks_suggestion, checks_test,
    checks_validate_pattern, close_settings_window, compose_attach_image, compose_content_changed,
    compose_templates, compose_templates_global_get, compose_templates_global_set,
    compose_templates_project_get, consume_settings_deep_link, contamination_events, content_clear,
    content_open_folder, detection_check_now, detection_open_rules_folder, detection_revert,
    detection_status, get_system_stats, graph_architecture,
    graph_context_preview, graph_cycles, graph_dead_exports, graph_fact_add, graph_fact_update,
    graph_facts, graph_history,
    graph_ignore_pick, graph_impact, graph_language_census, graph_memory, graph_memory_clear,
    graph_note_review, graph_note_set_pinned, graph_path, graph_rebuild, graph_rebuild_embeddings,
    graph_session_usage, graph_set_language_enabled, graph_set_watch_paused, graph_status,
    graph_tab_session, graph_test_embedder, graph_usage, graph_usage_advice, graph_viz_ego, graph_viz_file_status,
    advisor_rules, graph_viz_snapshot, harness_instructions, harness_mark_verified, harness_usage, harness_list, harness_run_checks,
    harness_versions_get, injection_status,
    latch_override, latch_status,
    list_tabs, list_voices,
    llm_pricing_get, llm_pricing_set, offload_backend_restart, offload_backend_start,
    offload_backend_stop, offload_derive_local_provider, offload_enable_readonly_commands,
    offload_reload_mcp, offload_server_log, offload_server_metrics, offload_server_restart,
    offload_server_start, offload_server_stop, offload_service_status, offload_status,
    offload_statuses, offload_test, open_settings_window, open_settings_window_to_section,
    open_settings_window_to_tab, pty_get_scrollback, pty_rebind_channel, pty_resize, pty_restart,
    pty_start, pty_write, request_tab_restart, restart_shell_tab, set_active_tab,
    set_window_square_corners, settings_get, settings_update, stt_cancel, stt_list_input_devices,
    delegation_statuses, delegation_status, delegation_take_over, stt_list_models,
    stt_start_recording, stt_stop_recording, tab_activate, tab_set_delegation_backend,
    tab_set_delegation_role,
    tab_set_read_only, tts_set_paused,
    tts_speak, tts_speak_selection, tts_stop, tts_test, workbench_checkpoint_diff,
    workbench_checkpoint_now, workbench_checkpoints, workbench_commit_diff, workbench_diff_file,
    workbench_diff_summary, workbench_git_graph, workbench_restore, workbench_revert_hunk,
    workbench_send_hunk, workbench_session_commit_counts, workbench_session_commits,
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
    default_shell_spec, get_shell_tab_config, open_note_tab, open_tool_tab, reconfigure_shell_tab,
    rename_tab, set_enabled_ai_tabs,
};
use crate::ipc::{AppState, LaunchContext};
use crate::preview::{
    preview_capture, preview_close, preview_hide, preview_navigate, preview_open, preview_reload,
    preview_set_rect, preview_show, preview_update_config,
};
use crate::settings::{
    LayoutNodePersisted, LayoutPersisted, LogLevel, LogRetention, Settings, SettingsHandle,
    TabConfig,
};
use crate::state::{
    spawn_state_manager, ReadOnlyTabs, StateEvent, StateSignal, TabId, TabKind, TabMeta,
};
use crate::stt::SttHandle;
use crate::tabs::{TabRegistry, TabRegistryHandle};
use crate::tts::{spawn_tts_worker, ActiveTab, AiTtsSuppressed, SpeakSession, TtsRequest};

/// Usage text for `cimp --help`. Lists the drop-in forwarding contract and the
/// service flags so an agent probing the CLI learns the surface instead of
/// launching a GUI window per probe.
///
/// **V40 Phase E (locked decision 26): the forwarding sentence is composed from
/// the registry**, not written here. Exactly one harness declares
/// `accepts_passthrough_argv`, and that is the harness the args actually reach —
/// so the promise a user reads and the tab they land in cannot disagree, and a
/// build whose passthrough harness changed does not ship a help text about the
/// old one.
fn help_text() -> String {
    let (label, binary) = crate::harness::registry::passthrough_harness()
        .and_then(|h| h.descriptor())
        .map(|d| (d.label, d.binaries.first().copied().unwrap_or(d.id)))
        .unwrap_or(("the AI", "the harness"));
    let usage = format!(
        "  cimp [ARGS...]          launch the GUI in the current directory; unrecognized\n\
                          args are forwarded verbatim to the {label} tab\n\
                          (drop-in `{binary}` replacement, e.g. `cimp --resume <id>`)"
    );
    format!(
        "\
cimp {} — code Imp: a TTS/avatar terminal for AI coding agents

USAGE:
{usage}

INFO:
  -h, --help              print this help and exit
  -V, --version           print the cimp version and exit

MAINTENANCE:
  --harness-canary [--json]
                          probe the INSTALLED harness CLIs against cImp's harness
                          capability registry and print one line per
                          capability. Needs no running cImp. Exits non-zero ONLY on
                          real drift — an absent CLI or an upstream improvement
                          reports `unknown` / `transition` and exits 0.
  --harness-capture [--json]
                          run the same probes and FILE what they observed, scrubbed
                          for credentials, under <app-data>/harness-captures/<harness>/
                          <cli-version>/ — so a future breakage is a diff against the
                          last known-good capture instead of an investigation. A run
                          that found drift lands in `<cli-version>-failing/` and never
                          overwrites the known-good corpus. Prints where it wrote;
                          exits non-zero only if it could write nothing at all.

SERVICE FLAGS (spawned by agent harnesses over stdio; not for interactive use):
  --statusline                           a harness status-line renderer
  --offload-mcp [--consumer <name>] [--tab <id>] [--channel-push]
                                         stdio MCP server (offload + graph + proxied servers)
  --code-audit-mcp [--consumer <name>] [--tab <id>]
                                         stdio MCP server (security_audit / quality_audit)

The MCP servers and hooks proxy to a RUNNING cImp instance launched in the
project directory; they are injected automatically into the AI tabs cImp
spawns and have no standalone mode.",
        env!("CARGO_PKG_VERSION")
    )
}

/// Best-effort attach to the parent's console so `--help`/`--version` output
/// is visible when a release build (`windows_subsystem = "windows"`) is run
/// from an interactive terminal. Only attaches when stdout has no handle at
/// all — when stdio is piped (agents, hooks, statusline) the inherited pipe
/// handles must stay untouched.
#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
    };
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}

/// Resolve `--consumer <name>` against the registry (V40 locked decision 2).
///
/// The **default stays `"claude"`** on the command line: a shim or a hand-run
/// child from before the flag existed omits it, and refusing those would break
/// backward compatibility for no gain — see
/// [`crate::harness::DEFAULT_HARNESS`], which carries the full rationale. What
/// changed is that the value is now *resolved*: a token nobody declared fails
/// the proxy start with the registered list in the message, instead of silently
/// serving Claude's tool set to a child that asked for something else.
fn resolve_consumer(args: &[String]) -> Result<&'static str, String> {
    let named = args
        .iter()
        .position(|a| a == "--consumer")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.trim());
    let Some(named) = named.filter(|s| !s.is_empty()) else {
        return Ok(crate::harness::DEFAULT_HARNESS
            .id()
            .expect("DEFAULT_HARNESS names a registered harness"));
    };
    // `offload` is cImp's OWN in-app consumer, not a harness: the offload worker
    // calls the proxy with it. It is a registered token in the MCP host's
    // vocabulary and has no `harness/<id>/` directory, so it is accepted here
    // beside the registry's ids rather than smuggled into the registry.
    if named.eq_ignore_ascii_case("offload") {
        return Ok("offload");
    }
    match crate::harness::HarnessId::from_consumer(named) {
        Some(h) => Ok(h.id().expect("from_consumer never answers ANY")),
        None => {
            let known: Vec<&str> = crate::harness::registry::harness_ids();
            Err(format!(
                "cimp: --consumer {named:?} names no registered harness. Registered: {} (plus                  `offload`, cImp's own in-app consumer). A consumer token decides which MCP                  servers this child may reach, so an unrecognised one is refused rather than                  defaulted.",
                known.join(", ")
            ))
        }
    }
}

fn main() {
    // `--help`/`--version` guard: agents reflexively probe unknown CLIs with
    // `cimp --help`, and before this guard every such invocation fell through
    // to the full GUI launch — each probe opened a real window (observed with
    // Claude Code running `cimp --help`, `cimp help`, `cimp code-audit --help`
    // in a project where the audit MCP server wasn't advertised). Handled
    // first and GUI-free like the service shims below. Everything else still
    // falls through: `cimp` is a drop-in replacement for whichever harness
    // declares `accepts_passthrough_argv`, and forwards unrecognized args to
    // that harness's tabs (V40 locked decision 26).
    {
        let early: Vec<String> = std::env::args().skip(1).collect();
        let wants_help = early.iter().any(|a| a == "--help" || a == "-h")
            || early.first().is_some_and(|a| a == "help");
        if wants_help {
            attach_parent_console();
            println!("{}", help_text());
            return;
        }
        if early.iter().any(|a| a == "--version" || a == "-V") {
            attach_parent_console();
            println!("cimp {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        // V35 Phase D: the L2 harness live probe. Drives the INSTALLED Claude
        // Code / OpenCode CLIs and reports one line per capability from the
        // `harness::contract` registry. Handled here, GUI-free like the service
        // shims, because it deliberately needs no app instance, no loopback
        // server and no settings file — a maintenance script (or Phase F's
        // auto-run on a CLI version change) must be able to run it on a machine
        // where cImp is not running at all.
        //
        // It is the ONE early-dispatch branch that sets an exit code:
        // **non-zero iff a probe found real drift**. `unknown` (CLI absent, no
        // session to tail) and `transition` (upstream improved) are never
        // failures — milestone locked decision 8, which is what keeps this from
        // becoming the version tripwire that cried wolf.
        if early.iter().any(|a| a == "--harness-canary") {
            attach_parent_console();
            std::process::exit(harness::probe::run(&early));
        }
        // V35 Phase H: the capture corpus's manual trigger. Same probes, same
        // GUI-free reasoning as above — and deliberately a SECOND command
        // rather than a flag on the first, because the two answer different
        // questions and only one of them owns an exit code. `--harness-canary`
        // says whether the harness drifted; this says whether a capture was
        // filed, and it files one whether or not anything drifted (a failing
        // run goes to a marked sibling directory, so the last known-good
        // capture — the thing you would diff against — survives).
        if early.iter().any(|a| a == "--harness-capture") {
            attach_parent_console();
            std::process::exit(harness::capture::run(&early));
        }
    }

    // Plugin-registered subcommands (V40 Phase D, locked decision 19). Claude
    // Code invokes `cimp --statusline`, pipes the session JSON to our stdin and
    // reads the rendered context bar from our stdout — a contract that belongs
    // to that harness, so the flag, the handler and the shell quoting around it
    // all live in `harness/claude/statusline.rs` and this loop asks rather than
    // matches. Handled before any Tauri/audio/settings init so a subcommand is
    // instant and never spins up the GUI; works under the release `windows`
    // subsystem too — inherited stdio pipes stay usable, only console
    // allocation is suppressed.
    {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        if let Some(sub) = harness::registry::subcommand_for(&argv) {
            (sub.run)();
            return;
        }
    }

    // ── V35 Phase J: the hook shims are GONE, and these are TOMBSTONES ───────
    //
    // `--context-hook`, `--precompact-hook`, `--read-hook`, `--postedit-hook`
    // and `--notify-hook` were five stateless binaries that carried a payload
    // from stdin to the loopback and a reply back to stdout. Claude Code 2.1.63's
    // `type: "http"` hooks let the harness do that itself, so the overlay now
    // emits `type: "http"` entries pointing at `/claude/hook/*` and the shim
    // logic lives in `harness::claude::hook` + `offload::loopback`.
    //
    // **2026-08-17 added the last two.** `--taint-beacon` and
    // `--checkpoint-beacon` were the two Phase J left behind; both are now
    // `type: "http"` entries too (`/claude/hook/pre_tool_use_taint`,
    // `/claude/hook/pre_tool_use_checkpoint`) and `taint_beacon.rs` /
    // `checkpoint_beacon.rs` are deleted.
    //
    // **The flags must not simply vanish, and this is why.** A `--settings`
    // overlay is written at TAB LAUNCH: a Claude tab open across this upgrade is
    // still configured to run `cimp --context-hook` on every prompt and
    // `cimp --taint-beacon` on every `WebFetch`. If a flag stopped being
    // recognised, `main()` would fall through to the normal startup path and
    // every such call would launch a whole second cImp GUI. So each flag still
    // terminates the process quietly: drain stdin (the harness writes the payload
    // and can block on a full pipe otherwise), print nothing, exit 0 — which for
    // every one of the seven is the documented fail-open answer. The old tab
    // keeps working, inert: no injection, no advisor, no auto-check, no taint
    // beacon, no pre-tool checkpoint, and permission detection falls back to the
    // TUI regex. Restart the tab and it gets the http hooks.
    //
    // The inert beacons are the one consequence worth naming, because they are
    // security-relevant: until such a tab is restarted its native `WebFetch` is
    // unobserved (the PROXIED half of the latch still catches everything routed
    // through cImp) and its edits get no per-call rewind point (the prompt-level
    // checkpoints remain). *Harness health* reports the tab as `old_plugin` for
    // exactly as long as that is true.
    //
    // REMOVABLE ONE RELEASE AFTER the release that deletes each shim — by then no
    // overlay that names it can still be in force, because a tab cannot outlive
    // two upgrades unrestarted without the *Harness health* panel having reported
    // it as `old_plugin` the whole time (`chp::expects_chp` answers `true` for
    // Claude from Phase J).
    const RETIRED_HOOK_FLAGS: [&str; 7] = [
        "--context-hook",
        "--precompact-hook",
        "--read-hook",
        "--postedit-hook",
        "--notify-hook",
        "--taint-beacon",
        "--checkpoint-beacon",
    ];
    if std::env::args()
        .skip(1)
        .any(|a| RETIRED_HOOK_FLAGS.contains(&a.as_str()))
    {
        let mut sink = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut sink);
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
    //
    // Collected once — shared by the two MCP-child checks below and the normal
    // launch path's `extra_args`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--offload-mcp") {
        let consumer = match resolve_consumer(&args) {
            Ok(c) => c,
            Err(msg) => {
                eprintln!("{msg}");
                std::process::exit(2);
            }
        };
        // V28: `--tab <tab-id>` names the cImp tab this per-tab child serves, so
        // the app can resolve the tab's CURRENT session for the `context_*`
        // memory tools. Absent (hand-run child, or one spawned before the
        // upgrade) simply falls back to the pre-V28 scoping.
        let tab = args
            .iter()
            .position(|a| a == "--tab")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str);
        // V30 (M5): `--channel-push` is the session-push gate, decided ONCE per
        // tab spawn in `tabs/config.rs::build_pre_args` — the same read of the
        // same settings snapshot that decides whether Claude itself gets
        // `--dangerously-load-development-channels`. Baking it into argv (rather
        // than having the child re-read settings at `initialize`) is what keeps
        // the two halves of the gate from desyncing across a child restart.
        // Absent on a hand-run child ⇒ no channel declaration.
        let channel_push = args.iter().any(|a| a == "--channel-push");
        offload::mcp::run(consumer, tab, channel_push);
        return;
    }

    // V26 code-audit MCP server: a host agent invokes `cimp --code-audit-mcp`
    // (Claude via `--mcp-config`; OpenCode via the injected `mcp` block) and
    // speaks newline-delimited JSON-RPC over stdio, exposing exactly two
    // zero-argument tools (`security_audit` / `quality_audit`). Like the offload
    // child it stays GUI-free and proxies to the running app's loopback
    // (`POST /audit/run`) — the audit needs the app's live `AuditState`, so
    // there is no headless fallback. Handled before any Tauri init, and it takes
    // the same optional `--consumer <name>` (default `claude`); the app's
    // loopback re-checks that consumer's expose toggle on every run.
    if args.iter().any(|a| a == "--code-audit-mcp") {
        let consumer = match resolve_consumer(&args) {
            Ok(c) => c,
            Err(msg) => {
                eprintln!("{msg}");
                std::process::exit(2);
            }
        };
        // V32 C-1b: `--tab <id>` names the cImp tab this child serves, so
        // `/audit/run` can gate the scan on that tab's taint latch. Until the
        // 2026-08-07 review this child deliberately carried no identity and the
        // route held no latch call at all — see `audit::mcp::TAB`.
        let tab = args
            .iter()
            .position(|a| a == "--tab")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str);
        audit::mcp::run(consumer, tab);
        return;
    }

    let launch_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let extra_args: Vec<String> = args;
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

    // Apply the user's saved log level to the live filter — unless a valid
    // RUST_LOG was locked in by `logging::init`, in which case the env
    // override stays in effect until the user picks a new level LIVE in
    // Settings (the broadcast loop below reloads on change; an explicit
    // mid-session pick wins over the env var). Calling `set_level`
    // unconditionally here used to clobber the RUST_LOG filter with the
    // saved level milliseconds after startup. The cleanup
    // pass deletes old rolled files per the user's retention setting.
    // Content capture is disabled by default — `set_enabled` mirrors the
    // saved flag, and the cleanup pass also runs against the content
    // subdirectory.
    {
        let snap = settings_handle.current();
        if !logging::env_override_active() {
            logging::set_level(snap.logging.level);
        }
        logging::run_cleanup(snap.logging.retention);
        content::set_enabled(snap.logging.content_capture.enabled);
        content::run_cleanup(snap.logging.content_capture.retention);
    }

    // V14 Phase B: sweep compose-attach directories orphaned by a previous
    // run that crashed or was killed before its own exit-time prune (below,
    // in the `CloseRequested` handler) ran. Fixed 3-day age cap — not a
    // user setting, this is opportunistic disk hygiene, not a feature.
    attach::prune(3);

    // V32 Phase C: compile the injection-detection signature rules from
    // `<exe-dir>/detection/rules.d/` and report whether the classifier's
    // weights are installed. Started here, before any tool call can happen, so
    // the very first fetched page is screened by a ready layer — and so a broken
    // rules file is a startup WARN the user can act on rather than a surprise
    // later. Infallible: both layers degrade to inert (see `detection::init`).
    //
    // V33 stage 3: this **returns immediately** and the compile runs on its own
    // thread. It used to run inline right here, which made one crafted file in
    // the user-writable `rules.d/local/` a permanent launch hang. A fetch that
    // beats the compile waits for it rather than screening nothing, so the
    // ordering property above survives the move — see `detection::init`.
    offload::detection::init();

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

    // V39 Phase A: the per-tab read-only locks `pty_write` enforces. Seeded
    // from the persisted `AiToolTabConfig::read_only` flags — a user lock is
    // sticky across restarts — and kept in step with later settings writes by
    // the broadcast watcher in `spawn_settings_watcher`. No tab is `Driven` at
    // startup: that source is never persisted.
    let read_only_tabs = ReadOnlyTabs::seeded(user_read_only_tabs(&settings_handle.current()));

    // V39 Phase B: the readable mirror of the per-tab prompt / output-burst /
    // exit flags. Written only by the state manager (which owns the edges),
    // read by `pty_write`'s siblings and by the delegation engine's preflight —
    // the same shape and the same reason as `read_only_tabs` above.
    let tab_activity = crate::state::TabActivity::default();

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
    let layout_active_id: Option<String> =
        snap.layout.as_ref().and_then(layout_focused_active_tab_id);
    let session_active_id: Option<&str> = snap.session.active_tab_id.as_deref();
    let resolved_id: Option<String> =
        layout_active_id.or_else(|| session_active_id.map(String::from));
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
                // V40 Phase E (locked decision 26): the registry's own default,
                // not a named harness — see `TabId::first_harness_default`.
                .unwrap_or_else(TabId::first_harness_default)
        });
    drop(snap);
    let tts_active: ActiveTab = Arc::new(RwLock::new(initial_active.clone()));
    // Shared selection-read session id (0 = none). Shared between the
    // `tts_speak_selection`/`tts_stop` commands and the TTS worker.
    let speak_session: SpeakSession = Arc::new(AtomicU64::new(0));
    // Shared "suppress AI-tag TTS" flag. Set by Esc (`tts_stop`), cleared by
    // the state manager on the next `HarnessOutputStarted`.
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
        read_only: read_only_tabs.clone(),
        tab_activity: tab_activity.clone(),
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
    let read_only_for_setup = read_only_tabs.clone();
    let tab_activity_for_setup = tab_activity.clone();
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
                    tab_activity_for_setup.clone(),
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
            //
            // V30 Phase C: the block yields the session-push bus
            // (`OffloadService::push_registry`) so the producers constructed
            // further down — the graph service and the audit runner — can
            // announce their long-running completions into channel-armed
            // sessions. Only the send half travels; nothing holds the service
            // itself, so no Arc cycle.
            let (push_registry, mcp_host) = {
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
                // Start the runtime (loopback + warm host) when ANY feature
                // whose out-of-process children dial back in needs it: offload
                // enabled, an MCP server exposed to Claude Code/OpenCode, the
                // graph (its tools ride the injected cimp-offload server and
                // the hook shims), or Code Audit exposed to a stdio consumer
                // (`cimp --code-audit-mcp` proxies to `/audit/run`). Gating on
                // offload alone stranded audit-only/graph-only projects with
                // "cImp is not running" tool errors.
                if settings_for_offload.current().loopback_needed()
                    && !offload_started.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    start_offload_runtime(
                        app.handle().clone(),
                        service.clone(),
                        supervisor.clone(),
                    );
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
                                start_offload_runtime(
                                    app_handle.clone(),
                                    svc.clone(),
                                    sup.clone(),
                                );
                            }
                        }
                    });
                }
                // V30 Phase C: hand the push bus to the producers below.
                // V38 Phase F: and the MCP host, for the audit runner's tier-2
                // provider tools.
                (service.push_registry(), service.mcp_host())
            };

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
                    // V30 Phase C: announce expensive full index builds.
                    Some(push_registry.clone()),
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
                        settings_for_graph.clone(),
                        audit_root,
                        // V30 Phase C: announce GUI-initiated scan completions.
                        Some(push_registry.clone()),
                        // V38 Phase F: the warm MCP host, for tier-2 provider
                        // tools. The host and not the service — the runner needs
                        // exactly one thing from that layer and holding the
                        // service would be a cycle.
                        Some(mcp_host.clone()),
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
                if settings_for_graph.current().graph.enabled {
                    if let Ok(root) = std::env::current_dir() {
                        // Startup housekeeping — never a session push.
                        graph_service
                            .spawn_rebuild(root.clone(), crate::graph::RebuildOrigin::Automatic);
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

            spawn_settings_broadcast(
                app.handle().clone(),
                settings_for_setup.clone(),
                read_only_for_setup.clone(),
            );

            // V32 Phase C3 (locked decision 13): keep the detection data fresh
            // — a debounced launch check plus a periodic due-ness poll, per
            // component, against a curated manifest. Spawned unconditionally,
            // and gated INSIDE the tick rather than here (#46): the task
            // re-reads settings every tick, so the Phase G L1 master switch and
            // the per-component modes both take effect at the next tick with no
            // restart, and a tick under a disabled master (or with both
            // components `off`) returns before touching the network or the
            // disk. A spawn-time gate would have made protection a spawn-baked
            // setting for no gain. Deliberately NOT gated on `offload.enabled`
            // — detection guards content that reaches Claude/OpenCode tabs
            // through the proxy too, so its data must stay current whatever the
            // worker is doing.
            offload::detection::updater::spawn_scheduler(settings_for_setup.clone());

            // V35 Phase F, trigger (b): if the installed Claude Code moved
            // while cImp was closed — the common case, since the CLI
            // self-updates on its own schedule — nothing observed the change,
            // so the in-session trigger cannot fire. Run the canaries once now
            // and let a clean result advance `claude_last_verified` by itself.
            // Cheap and self-gating: two string comparisons against a
            // mtime-cached read, no thread at all when the versions already
            // match, and the work itself is a detached OS thread so startup
            // never waits on it.
            harness::verify::spawn_startup_check();

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
            tab_set_read_only,
            tab_set_delegation_role,
            tab_set_delegation_backend,
            delegation_take_over,
            delegation_status,
            delegation_statuses,
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
            harness_usage,
            get_system_stats,
            settings_get,
            settings_update,
            ai_tool_tab_defaults,
            list_voices,
            list_tabs,
            open_settings_window,
            open_settings_window_to_tab,
            open_settings_window_to_section,
            consume_settings_deep_link,
            close_settings_window,
            request_tab_restart,
            restart_shell_tab,
            compose_content_changed,
            compose_templates,
            compose_templates_global_get,
            compose_templates_global_set,
            compose_templates_project_get,
            llm_pricing_get,
            llm_pricing_set,
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
            offload_derive_local_provider,
            offload_enable_readonly_commands,
            offload_service_status,
            offload_reload_mcp,
            detection_status,
            detection_check_now,
            detection_revert,
            detection_open_rules_folder,
            // V32 Phase F: the per-tab taint badge + its override popover.
            latch_status,
            latch_override,
            injection_status,
            offload_server_log,
            offload_server_metrics,
            graph_status,
            graph_rebuild,
            graph_rebuild_embeddings,
            graph_ignore_pick,
            checks_detect,
            checks_apply_proposals,
            checks_suggestion,
            checks_dismiss_suggestion,
            checks_test,
            checks_validate_pattern,
            graph_set_watch_paused,
            graph_language_census,
            graph_set_language_enabled,
            graph_test_embedder,
            graph_history,
            activity_list,
            activity_detail,
            activity_delete,
            activity_clear,
            graph_dead_exports,
            graph_cycles,
            graph_impact,
            graph_path,
            graph_architecture,
            graph_viz_snapshot,
            graph_viz_file_status,
            graph_viz_ego,
            graph_memory,
            graph_memory_clear,
            graph_note_set_pinned,
            graph_note_review,
            graph_facts,
            graph_fact_update,
            graph_fact_add,
            graph_context_preview,
            graph_usage,
            graph_session_usage,
            graph_usage_advice,
            graph_tab_session,
            advisor_dismiss,
            advisor_mark_applied,
            harness_versions_get,
            harness_list,
            advisor_rules,
            harness_instructions,
            harness_mark_verified,
            harness_run_checks,
            workbench_status,
            workbench_diff_summary,
            workbench_diff_file,
            workbench_revert_hunk,
            workbench_send_hunk,
            workbench_checkpoints,
            // V33 step 5: the Timeline's contamination evidence rows.
            contamination_events,
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
            workbench_session_commits,
            workbench_session_commit_counts,
            workbench_commit_diff,
            workbench_git_graph,
            theming::themes_list,
            theming::palettes_list,
            audit::audit_detect_tool,
            audit::audit_start_scan,
            audit::audit_cancel_scan,
            audit::audit_snapshot,
            audit::audit_refresh_census,
            audit::audit_effective_roster,
            // V38: tool-plugin discovery (read + Rescan) and the key the
            // settings pane stores this project's path overrides under.
            // Nothing here RUNS a plugin — the pipelines that consume one
            // are Phase C/D.
            plugins::plugins_snapshot,
            plugins::plugins_rescan,
            plugins::plugins_project_key,
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
        let root = app_handle.state::<AppState>().launch.cwd.clone();
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
    find(&layout.tree, &layout.focused_pane_id).and_then(|opt| opt.clone())
}

/// V39 Phase A: the tabs whose persisted `read_only` flag is set — the
/// `ReadOnlySource::User` locks, as the settings file currently states them.
/// One helper for both the startup seed and the per-broadcast re-sync so the
/// two can never disagree about what "the file says locked" means.
fn user_read_only_tabs(settings: &Settings) -> Vec<TabId> {
    settings
        .tabs
        .iter()
        .filter_map(|cfg| match cfg {
            TabConfig::AiTool(c) if c.read_only => Some(TabId::from_str(&c.id)),
            _ => None,
        })
        .collect()
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

// Wiring function called once from setup: every argument is a distinct handle
// or channel end the pipeline owns, with no natural grouping.
#[allow(clippy::too_many_arguments)]
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
            let _ = state_signals.try_send(StateSignal::AudioError {
                tab: initial_active.clone(),
            });
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

fn spawn_settings_broadcast(app: AppHandle, settings: SettingsHandle, read_only: ReadOnlyTabs) {
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
                    // V39 Phase A: `read_only` is a persisted per-tab field, so
                    // it can move through the Settings window, a project-overlay
                    // switch or a hand edit as well as through
                    // `tab_set_read_only`. Re-syncing here keeps the runtime map
                    // `pty_write` enforces from becoming a second source of
                    // truth that drifts from the file.
                    read_only.sync_users(user_read_only_tabs(&s));
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

#[cfg(test)]
mod read_only_seed_tests {
    use super::*;
    use crate::settings::TabConfig;
    use crate::state::ReadOnlySource;

    /// `Settings::default()` carries no tabs (the integrity check seeds the
    /// builtins at load time), so the fixtures supply one.
    fn settings_with_ai_tab() -> Settings {
        let mut s = Settings::default();
        s.tabs.push(crate::settings::default_claude_tab());
        s.tabs.push(crate::settings::default_claude_local_tab());
        s
    }

    fn lock_first_ai_tab(s: &mut Settings) -> TabId {
        for cfg in s.tabs.iter_mut() {
            if let TabConfig::AiTool(c) = cfg {
                c.read_only = true;
                return TabId::from_str(&c.id);
            }
        }
        panic!("Settings::default() seeds at least one AI tab");
    }

    /// **V39 Phase A: a user read-only lock survives an app restart.** This is
    /// the restart path itself — the persisted flag, read at startup, becomes
    /// the runtime lock `pty_write` enforces.
    #[test]
    fn startup_seeds_the_user_locks_from_settings() {
        let mut s = settings_with_ai_tab();
        let locked = lock_first_ai_tab(&mut s);
        let ro = crate::state::ReadOnlyTabs::seeded(user_read_only_tabs(&s));
        assert_eq!(ro.read_only(&locked), Some(ReadOnlySource::User));
        for cfg in &s.tabs {
            let id = TabId::from_str(cfg.id());
            if id != locked {
                assert_eq!(ro.read_only(&id), None, "only the locked tab is locked");
            }
        }
    }

    /// …and nothing is `Driven` at startup: that source is never persisted, so
    /// a crash mid-delegation cannot leave a lock with no owner to lift it.
    #[test]
    fn startup_never_seeds_a_driven_lock() {
        let mut s = settings_with_ai_tab();
        let locked = lock_first_ai_tab(&mut s);
        let ro = crate::state::ReadOnlyTabs::seeded(user_read_only_tabs(&s));
        assert!(matches!(
            ro.read_only(&locked),
            Some(ReadOnlySource::User)
        ));
    }

    /// A Shell tab can never carry the flag — it is a field on
    /// `AiToolTabConfig` only, and the seed walks that variant alone.
    #[test]
    fn the_seed_only_looks_at_ai_tabs() {
        let mut s = settings_with_ai_tab();
        let _ = lock_first_ai_tab(&mut s);
        for tab in user_read_only_tabs(&s) {
            assert_eq!(tab.kind(), TabKind::AiTool, "{tab:?} is not an AI tab");
        }
    }
}

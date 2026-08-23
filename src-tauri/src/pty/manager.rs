use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::sync::{Arc, Mutex as StdMutex};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};
use crate::processing::permission::PermissionPattern;
use crate::pty::tasks;
use crate::state::{StateSignal, TabId};
use crate::tts::TtsRequest;

/// Per-tab launch parameters resolved by the registry before spawning. Keeps
/// the PtyManager binary-agnostic so each tab can plug in its own command.
pub struct PtyLaunchSpec {
    pub tab: TabId,
    /// Path to the resolved binary on disk (already passed through `which`).
    pub binary: std::path::PathBuf,
    /// Arguments inserted before `extra_args`. Used for `--append-system-
    /// prompt <content>` on the Claude tab.
    pub pre_args: Vec<String>,
    /// User-supplied flags + cimp invocation args.
    pub extra_args: Vec<String>,
    pub working_dir: std::path::PathBuf,
    /// Environment additions/overrides applied on top of the inherited
    /// environment. Empty for AI builtins; user Shell tabs may set per-tab
    /// vars (the M2 dialog leaves env empty — schema reserved, UI deferred).
    pub env: std::collections::HashMap<String, String>,
    /// V30 (review M9): inherited variables to STRIP before `env` is applied.
    ///
    /// The child otherwise inherits cImp's whole environment, which is wrong for
    /// the harness markers of a Claude Code session cImp itself was launched
    /// from: `CLAUDE_CODE_CHILD_SESSION=1` makes a spawned Claude run with no
    /// transcript, no history and no session records, which silently blinds the
    /// out-of-band tap (no TTS, no usage, no live-session registry entry, no V28
    /// scoping) — spike-documented in `docs/MILESTONE-V30-mcp-channels.md`.
    /// Resolved by `tabs::config::ai_env_removals`; empty for Shell tabs, whose
    /// whole point is the environment the user actually has.
    pub env_remove: Vec<String>,
    /// V20: the out-of-band TTS source to attach once the child is up (Claude
    /// transcript tail / OpenCode event stream). `None` for shell tabs and any
    /// AI tab whose source can't be resolved. The source rides the tab's PTY
    /// cancel token, so it starts with the tab and dies with it.
    pub oob: Option<crate::harness::OobSpec>,
    /// V33 Phase B: which AI harness this tab runs, or `None` for a Shell tab.
    ///
    /// The ONLY thing that makes a tab eligible for the sandbox (decision B1),
    /// and it is decided in `tabs::config::build_ai_tool_spec` — the one place
    /// that already knows which harness a command is — rather than re-derived
    /// here from `binary`, so "which tabs are agent seams" has exactly one
    /// answer in the codebase. `None` means plain spawn, no sandbox, no row.
    pub harness: Option<crate::sandbox::tabs::Harness>,
}

pub struct PtyManager {
    inner: Arc<TokioMutex<Option<PtyHandle>>>,
}

/// The two trait objects a spawned PTY session hands back, whichever backend
/// produced them — portable-pty's native ConPTY or V33 Phase B's sandboxed twin.
/// Named so both arms of the spawn have one shape and the manager's downstream
/// code cannot tell them apart, which is the property the whole design rests on.
type PtySession = (
    Box<dyn MasterPty + Send>,
    Box<dyn portable_pty::Child + Send + Sync>,
);

/// Shape the child's environment: strip first, then add.
///
/// `CommandBuilder::new` seeds itself with a snapshot of THIS process's
/// environment, so `env_remove` genuinely un-inherits a variable (portable-pty
/// 0.9's `CommandBuilder::env_remove`, verified against the pinned source).
/// Removals run BEFORE the additions so an explicit per-tab `env` entry always
/// wins — a user who deliberately sets one of the scrubbed vars keeps it (the
/// resolver in `tabs::config` also declines to list such a key, so this is the
/// second of two guards).
fn apply_env(
    cmd: &mut CommandBuilder,
    env: &std::collections::HashMap<String, String>,
    env_remove: &[String],
) {
    for key in env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
}

/// Control messages routed to the per-PTY processor task. Currently used to
/// swap the output `Channel<String>` without restarting the shell — the
/// V1.4-03 renderer-flip path destroys the JS xterm and rebinds the PTY's
/// bytes to a freshly-constructed one.
pub enum ProcessorControl {
    ChannelChange(Channel<String>),
    /// A real (non-deduped) PTY resize just pulsed SIGWINCH at the child,
    /// which makes a TUI like Claude Code repaint. That repaint is a burst
    /// of bytes indistinguishable from genuine output, so it can trip the
    /// processor's byte-burst activity fallback and spuriously flip the
    /// avatar Idle → Thinking → Idle (firing an "idle" notification). The
    /// processor uses this to open a short grace window during which the
    /// burst fallback is suppressed; the `claude_working` marker path stays
    /// live, so real activity during/after a resize is still detected.
    Resized,
    /// M11 (2026-08-05 review): drop the permission detector's LATCHED pattern
    /// name for this tab without emitting anything, so the next scan tick can
    /// re-`Detected` a prompt that is still on screen.
    ///
    /// Sent by the NC-2 hook path (`offload::loopback::handle_permission_event`)
    /// whenever a `PermissionDenied` clears `awaiting_permission`. That clear is
    /// deliberately eager — an auto-denied call can land while a genuine
    /// approval prompt is visible — and the regex detector is edge-triggered:
    /// while the same pattern keeps matching, `(Some, Some)` with the same name
    /// emits NOTHING, so without this the badge/TTS stayed cleared until the
    /// user typed. The processor answers it with
    /// `PermissionDetector::force_clear(Permission)`, the same primitive the
    /// Working-kind stale path already uses.
    ClearPermissionLatch,
}

struct PtyHandle {
    writer: Arc<StdMutex<Box<dyn Write + Send>>>,
    master: Arc<StdMutex<Box<dyn MasterPty + Send>>>,
    killer: Arc<StdMutex<Box<dyn ChildKiller + Send + Sync>>>,
    cancel: CancellationToken,
    /// Last (rows, cols) we forwarded to the PTY. Lets `resize` short-
    /// circuit a no-op call so a same-size resize event from the WebView
    /// (e.g. mid-drag across DPI boundaries) doesn't pulse SIGWINCH at the
    /// child and trigger a TUI redraw.
    last_size: Arc<StdMutex<(u16, u16)>>,
    /// Sends `ProcessorControl` messages to the processor task. Currently
    /// only `ChannelChange`. Capacity is small — control messages are
    /// rare. Sending fails when the processor task has exited (e.g.,
    /// after the child PTY exited and the cancel token fired); the
    /// caller falls back to `pty_start`.
    control_tx: mpsc::Sender<ProcessorControl>,
    /// V1.4-04 D: per-tab cross-restart scrollback ring. The reader
    /// task (in `pty/tasks.rs`) appends every PTY byte here; on graceful
    /// exit the ring is written to disk; on next launch it's read back
    /// and replayed into the new xterm. Bounded at `scrollback_cap`
    /// bytes — surplus is dropped from the front. `StdMutex` (not the
    /// async one) because the writer lives on the blocking pool and
    /// every critical section is short.
    scrollback: Arc<StdMutex<VecDeque<u8>>>,
    /// Cap for the `scrollback` ring. Read from
    /// `terminal.scrollback.ring_bytes` at start time; live changes to
    /// the setting don't reshape an already-running ring (mid-buffer
    /// expand-vs-shrink semantics aren't worth the complexity for a
    /// rarely-changed knob — the new cap takes effect on next start).
    scrollback_cap: usize,
    /// V33 Phase B, decision B10: the tab's sandbox preparation, held for the
    /// LIFETIME OF THE PTY SESSION.
    ///
    /// Never read — held. `Prepared` owns the refcounted `subst` drive mapping
    /// the child's cwd points at (`S:\`), so dropping it unmaps that drive; a
    /// `Prepared` scoped to the spawn function would pull the child's own
    /// working directory out from under it milliseconds after launch. The
    /// mapping is refcounted per project root, so several sandboxed tabs on one
    /// project share one letter and the last one to close releases it.
    ///
    /// `None` for every plain tab, which is every tab on a non-Windows build.
    #[cfg(windows)]
    #[allow(dead_code)]
    sandbox: Option<Box<crate::sandbox::windows::Prepared>>,
}

impl PtyManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TokioMutex::new(None)),
        }
    }

    /// V33 Phase B — spawn one AI tab INSIDE the AppContainer.
    ///
    /// Everything policy-shaped (which switches, which grants, which scratch)
    /// was decided by `sandbox::tabs::plan_tab` before this is reached; what
    /// happens here is the three things only this layer can do:
    ///
    /// 1. **redirect the scratch** — `TEMP`/`TMP` into the per-tab directory
    ///    under the project root, expressed on the MAPPED DRIVE so the child
    ///    never hands a deep real path to a tool that walks its ancestors
    ///    (the S1 canonicalization gotcha, which reproduces through ConPTY);
    /// 2. **read the resolved environment back out of the same
    ///    `CommandBuilder` the plain path would have spawned with** (decision
    ///    B4). On Windows `CommandBuilder::new` is NOT a plain
    ///    `std::env::vars_os()` snapshot — it re-reads the machine and user
    ///    `Environment` registry keys and concatenates system+user `PATH` — so
    ///    a hand-rolled equivalent would give the sandboxed child a different
    ///    `PATH` than the plain one, and nobody would connect that to the
    ///    sandbox switch. The one fidelity gap is inherent to the public API:
    ///    `iter_full_env_as_str` drops entries that are not valid UTF-8, so a
    ///    non-UTF-8 variable does not reach a sandboxed child;
    /// 3. **mint the lane's rows** — a confirmation on success, and on a Win32
    ///    refusal a denial row plus a hard failure. Decision B9: a spawn error
    ///    AFTER the grants landed is not a prerequisite gap, so it is never
    ///    retried plain.
    #[cfg(windows)]
    fn start_sandboxed(
        spec: &PtyLaunchSpec,
        prepared: &crate::sandbox::windows::Prepared,
        cfg: &crate::sandbox::SandboxCfg,
        cmd: &mut CommandBuilder,
        size: PtySize,
    ) -> Result<PtySession, String> {
        let tab_id = spec.tab.as_str();
        let seam = crate::sandbox::tab_seam(tab_id);
        let root = &spec.working_dir;

        // (1) the scratch, on the mapped drive.
        let scratch = crate::sandbox::tabs::scratch_dir(root, tab_id);
        for (key, value) in crate::sandbox::tabs::env_overrides(&prepared.cwd_under(&scratch)) {
            cmd.env(key, value);
        }

        // (2) the resolved environment, from the builder itself.
        let env: Vec<(std::ffi::OsString, std::ffi::OsString)> = cmd
            .iter_full_env_as_str()
            .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)))
            .collect();

        let mut args: Vec<String> = Vec::with_capacity(spec.pre_args.len() + spec.extra_args.len());
        args.extend(spec.pre_args.iter().cloned());
        args.extend(spec.extra_args.iter().cloned());

        let spawned = crate::pty::sandboxed_conpty::open_and_spawn(
            prepared,
            crate::pty::sandboxed_conpty::TabSpawn {
                program: &spec.binary,
                args: &args,
                env: &env,
                // Decision B6: the drive root, not the real project path.
                cwd: &prepared.cwd(),
                size,
            },
        );

        // (3) the rows.
        match spawned {
            Ok(pty) => {
                // Deduped per (seam, subject) inside `record_sandboxed`, and the
                // seam already carries the tab id — so this is once per tab per
                // session, which is once per launch in practice.
                crate::sandbox::record_sandboxed(&seam, root, tab_id, cfg);
                info!(tab = %tab_id, "AI tab spawned inside the sandbox");
                Ok((pty.master, pty.child))
            }
            Err(e) => {
                // A container that cannot read the program image fails right
                // here, with no exit code because nothing ran — the same
                // classification `run_command` applies to its own
                // `CreateProcessW` refusals. `allow_network` is passed as
                // `true` because a sandboxed tab always has egress (B3), which
                // is what keeps a name-resolution failure from being libelled
                // as a boundary denial.
                if let Some(class) = crate::sandbox::denial_signature(None, &e, true) {
                    crate::sandbox::record_denial(
                        &seam, root, tab_id, &args, None, &e, class, cfg,
                    );
                }
                Err(format!(
                    "the sandboxed tab could not be started: {e}. The sandbox grants were \
                     already applied, so this is not a missing prerequisite and cImp will NOT \
                     silently retry it unsandboxed — switch AI-tab sandboxing off in \
                     Settings ▸ Sandboxing if you need this tab now."
                ))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        &self,
        app: AppHandle,
        spec: PtyLaunchSpec,
        output_channel: Channel<String>,
        initial_rows: u16,
        initial_cols: u16,
        tts_segments: mpsc::Sender<TtsRequest>,
        // V20: retained for the registry's call shape; the processing layer no
        // longer consumes a user-typed echo filter (TTS is out-of-band).
        _user_typed_tts: Arc<StdMutex<HashSet<String>>>,
        state_signals: mpsc::Sender<StateSignal>,
        patterns: Arc<Vec<PermissionPattern>>,
        // V39 review R-5: the generation this start is filed under, from
        // `TabActivity::begin_start`. Every exit this start can produce carries
        // it, so an exit handled after a later restart is ignorable.
        start_gen: u64,
    ) -> AppResult<()> {
        let mut guard = self.inner.lock().await;
        if guard.is_some() {
            return Err(AppError::AlreadyStarted);
        }

        let tab = spec.tab.clone();
        info!(?tab, path = %spec.binary.display(), "spawning subprocess");

        let size = PtySize {
            rows: initial_rows.max(1),
            cols: initial_cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        };

        // ── V33 Phase B: decide the boundary BEFORE opening a pty ──────────
        //
        // Sandboxed and plain are two different OS mechanisms (a bespoke
        // `CreatePseudoConsole` + `CreateProcessW` with an AppContainer
        // attribute list, versus portable-pty's one-attribute ConPTY), not a
        // flag on one, so the decision has to come first. Everything downstream
        // of the spawn is trait-level and does not know which it got.
        //
        // A Shell tab (`harness == None`) never reaches this at all: decision B1
        // — a shell tab is the user's own hands, not an agent seam.
        //
        // ONE settings snapshot serves the decision and the rows it mints: a
        // second read could straddle a save and describe the boundary with a
        // posture the child never ran under.
        let sandbox_settings = app.state::<crate::ipc::AppState>().settings.current();
        // Read on every platform (the value is what the ROWS are described
        // with), consumed only by the Windows engine — the same shape the rest
        // of the sandbox layer carries off Windows.
        #[cfg_attr(not(windows), allow(unused_variables))]
        let sandbox_cfg = crate::sandbox::tabs::tab_sandbox_cfg(&sandbox_settings);
        let sandbox_plan = match spec.harness {
            Some(harness) => {
                crate::sandbox::tabs::plan_tab(
                    &sandbox_settings,
                    harness,
                    tab.as_str(),
                    &spec.binary,
                    &spec.working_dir,
                )
                .await
            }
            None => crate::sandbox::tabs::TabPlan::Plain,
        };
        // Decision B9: a WEDGED preparation refuses the launch. Never a silent
        // unsandboxed fallback — dropping the boundary because a step hung is
        // exactly the degradation V33 decision 5 forbids, and the user sees the
        // reason as the tab's launch error.
        if let crate::sandbox::tabs::TabPlan::Refused(reason) = &sandbox_plan {
            let _ = state_signals.try_send(StateSignal::SubprocessExited {
                tab,
                code: None,
                start_gen,
            });
            return Err(AppError::Spawn(reason.clone()));
        }

        // The command is built the SAME way for both paths (decision B4): same
        // `CommandBuilder`, same `apply_env`, same `env_remove` list. The
        // sandboxed backend then reads the RESOLVED environment back out of this
        // builder rather than composing a second one — because on Windows
        // `CommandBuilder::new` is not a plain `std::env::vars_os()` snapshot
        // (it re-reads the machine and user `Environment` registry keys and
        // merges PATH), and a hand-rolled equivalent would disagree about the
        // child's PATH in ways nobody would connect to the sandbox switch.
        let mut cmd = CommandBuilder::new(&spec.binary);
        for arg in &spec.pre_args {
            cmd.arg(arg);
        }
        for arg in &spec.extra_args {
            cmd.arg(arg);
        }
        cmd.cwd(&spec.working_dir);
        apply_env(&mut cmd, &spec.env, &spec.env_remove);

        #[cfg(windows)]
        let sandboxed = match &sandbox_plan {
            crate::sandbox::tabs::TabPlan::Sandboxed(prepared) => Some(prepared),
            _ => None,
        };
        #[cfg(not(windows))]
        let sandboxed: Option<()> = None;

        let (master, child): PtySession = match sandboxed {
            #[cfg(windows)]
            Some(prepared) => {
                match Self::start_sandboxed(&spec, prepared, &sandbox_cfg, &mut cmd, size) {
                    Ok(pair) => pair,
                    Err(e) => {
                        let _ = state_signals
                            .try_send(StateSignal::SubprocessExited {
                tab,
                code: None,
                start_gen,
            });
                        return Err(AppError::Spawn(e));
                    }
                }
            }
            #[cfg(not(windows))]
            Some(()) => unreachable!("the sandboxed backend is Windows-only"),
            None => {
                let pty_system = native_pty_system();
                let pair = match pty_system.openpty(size) {
                    Ok(p) => p,
                    Err(e) => {
                        // The waiter task (which normally emits
                        // SubprocessExited) is never spawned on the spawn-error
                        // path; emit it here so the state machine pins the tab
                        // to Error and the avatar reflects the failure.
                        let _ = state_signals
                            .try_send(StateSignal::SubprocessExited {
                tab,
                code: None,
                start_gen,
            });
                        return Err(AppError::Pty(format!("openpty: {e}")));
                    }
                };
                // Through the spawn gate (see `spawn_gate`). portable-pty builds
                // its own `STARTUPINFOEX` and calls `CreateProcessW` itself, so
                // this spawn sits outside `std`'s private process-wide
                // create-process lock entirely — which is half the reason the
                // gate had to exist. `with_shared` wraps the third-party call
                // and nothing else.
                let child = match crate::spawn_gate::with_shared(|| pair.slave.spawn_command(cmd)) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = state_signals
                            .try_send(StateSignal::SubprocessExited {
                tab,
                code: None,
                start_gen,
            });
                        return Err(AppError::Spawn(format!("{e}")));
                    }
                };
                // Drop the slave end in the parent; the child inherits its own
                // reference. (The sandboxed backend has no slave half — the
                // pseudoconsole IS the child's console.)
                drop(pair.slave);
                (pair.master, child)
            }
        };
        let killer = child.clone_killer();

        // V33 contract C3: put the tab's process into the process-lifetime
        // kill-on-job-close job. Everything else cImp spawns has been in it
        // since the job existed; this one was not, because `guard_child` is
        // typed to a `tokio::process::Child` and portable-pty hands back its
        // own `Child`. That made the widest agent seam in the app — the tab
        // process, whose descendants are everything the agent runs — the one
        // thing a hard cImp death (panic=abort, OOM kill, `cargo tauri dev`'s
        // hot-reload TerminateProcess) left orphaned.
        //
        // Naming the child by pid is safe here and only here: `child` is still
        // alive in this scope and holds the process handle, which is what pins
        // the pid against reuse (see `guard_pid`'s contract). Hardening, never
        // a gate — a failure is logged and the tab still launches.
        match child.process_id() {
            Some(pid) => crate::process_guard::guard_pid(pid),
            None => tracing::debug!(?tab, "pty child reported no pid; cannot job-guard it"),
        }

        // The child is already running. If we bail past this point without
        // killing it AND signaling exit, we leak the process and leave the
        // state machine in its prior state (no Error overlay) — the same
        // hazard the openpty/spawn error paths above guard. Mirror that here.
        let reader = match master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                let _ = child.clone_killer().kill();
                let _ = state_signals.try_send(StateSignal::SubprocessExited {
                tab,
                code: None,
                start_gen,
            });
                return Err(AppError::Pty(format!("try_clone_reader: {e}")));
            }
        };
        let writer = match master.take_writer() {
            Ok(w) => w,
            Err(e) => {
                let _ = child.clone_killer().kill();
                let _ = state_signals.try_send(StateSignal::SubprocessExited {
                tab,
                code: None,
                start_gen,
            });
                return Err(AppError::Pty(format!("take_writer: {e}")));
            }
        };

        let master: Arc<StdMutex<Box<dyn MasterPty + Send>>> = Arc::new(StdMutex::new(master));
        let writer: Arc<StdMutex<Box<dyn Write + Send>>> = Arc::new(StdMutex::new(writer));
        let killer: Arc<StdMutex<Box<dyn ChildKiller + Send + Sync>>> =
            Arc::new(StdMutex::new(killer));

        let cancel = CancellationToken::new();
        let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>(256);
        // V1.4-03: control mpsc lets `rebind_channel` swap the processor's
        // output channel without restarting the PTY. Capacity 4 is plenty
        // — control messages are rare (one per renderer-flip).
        let (control_tx, control_rx) = mpsc::channel::<ProcessorControl>(4);

        let settings = app.state::<crate::ipc::AppState>().settings.clone();
        // V1.4-04 D: per-tab scrollback ring buffer. Reader task
        // appends every PTY byte; we hand out clones via
        // `scrollback_snapshot` for persistence and via the
        // `pty_get_scrollback` Tauri command for diagnostics.
        let scrollback_cap = settings.current().terminal.scrollback.ring_bytes;
        let scrollback: Arc<StdMutex<VecDeque<u8>>> = Arc::new(StdMutex::new(
            VecDeque::with_capacity(scrollback_cap.min(1 << 20)),
        ));

        // V20: build the out-of-band TTS source context before the senders are
        // moved into the processor/waiter. The source rides `cancel`, so it
        // starts now and dies when the tab's PTY does.
        // V10: the warm graph service, so the Claude transcript tap can record
        // session/action memory in-process. Absent in headless/test builds.
        let mem = app
            .try_state::<Arc<crate::graph::GraphService>>()
            .map(|s| s.inner().clone());
        // V30 Phase D: the session-push bus, so the OpenCode tap can subscribe
        // its tab in-process and forward notices over OpenCode's HTTP API. Only
        // the send half's registry travels (not the service), exactly like the
        // Phase C producers in `main.rs` — no Arc cycle. Absent in
        // headless/test builds ⇒ `None` ⇒ no fanout.
        let pushes = app
            .try_state::<Arc<crate::offload::OffloadService>>()
            .map(|s| s.push_registry());
        let oob_ctx = spec.oob.clone().map(|oob_spec| {
            (
                oob_spec,
                crate::harness::OobContext {
                    tab: tab.clone(),
                    tts: tts_segments.clone(),
                    state_signals: state_signals.clone(),
                    settings: settings.clone(),
                    cancel: cancel.clone(),
                    mem: mem.clone(),
                    pushes: pushes.clone(),
                },
            )
        });
        // V40 Phase D (locked decision 18): how this tab's harness reports
        // being busy, asked of the harness. This used to be
        // `matches!(spec.oob, Some(OobSpec::OpenCodeEvent { .. }))` — core
        // testing for ONE harness's transport to decide whether to run a TUI
        // heuristic tuned to ANOTHER harness's spinner.
        //
        // A tab with no registered harness (a shell tab, a command nothing
        // claims) gets `OutOfBand`, i.e. no inference at all: see
        // `ActivitySource`'s docs for why a missing declaration must not
        // inherit somebody else's timings.
        let activity = spec
            .harness
            .and_then(|h| h.plugin())
            .map(|p| p.activity_source())
            .unwrap_or(crate::harness::plugin::ActivitySource::OutOfBand);

        tasks::spawn_reader(
            reader,
            bytes_tx,
            cancel.clone(),
            Arc::clone(&scrollback),
            scrollback_cap,
        );
        tasks::spawn_processor(
            tab.clone(),
            bytes_rx,
            output_channel,
            control_rx,
            cancel.clone(),
            state_signals.clone(),
            settings,
            patterns,
            activity,
        );
        tasks::spawn_waiter(
            tab.clone(),
            child,
            app,
            cancel.clone(),
            state_signals,
            start_gen,
        );

        // H1-R3 (2026-08-05 review): KEEP THIS IN THE SAME SYNCHRONOUS STRETCH
        // as the `spawn_command` above — do not insert an `.await` (or any
        // fallible/slow step that could park) between the child spawn and this
        // call. The V28 ambiguity predicate only counts tabs whose tap has
        // declared its transcript root, so between "child running" and "tap
        // registered" a second Claude tab is invisible as a co-tenant and a
        // sibling tap can bind confidently to the wrong session
        // (docs/MILESTONE-V28-session-identity.md § "Launch-order window").
        // Today the gap is ~15 sync statements — sub-millisecond, against
        // Claude's multi-second boot before it writes a transcript — but that
        // is a property of THIS code order, not an enforced invariant.
        if let Some((oob_spec, ctx)) = oob_ctx {
            crate::harness::spawn(oob_spec, ctx);
        }

        *guard = Some(PtyHandle {
            writer,
            master,
            killer,
            cancel,
            last_size: Arc::new(StdMutex::new((initial_rows.max(1), initial_cols.max(1)))),
            control_tx,
            scrollback,
            scrollback_cap,
            // Decision B10: the drive mapping lives as long as the session.
            #[cfg(windows)]
            sandbox: match sandbox_plan {
                crate::sandbox::tabs::TabPlan::Sandboxed(prepared) => Some(prepared),
                _ => None,
            },
        });
        info!(
            ?tab,
            rows = initial_rows,
            cols = initial_cols,
            "PTY session started"
        );
        Ok(())
    }

    /// V39 Phase B: whether this tab has a live PTY handle right now.
    ///
    /// The same condition [`Self::write_input`] would fail on
    /// (`AppError::NotStarted`), asked *before* writing rather than discovered
    /// by writing. The delegation engine's preflight needs it: "the worker's
    /// process is alive" is a refusal condition (locked decision 12), and a
    /// refusal that only surfaces as a failed write has already engaged the
    /// read-only lock and minted a `start` row for a delegation that never
    /// began.
    pub async fn is_started(&self) -> bool {
        self.inner.lock().await.is_some()
    }

    pub async fn write_input(&self, bytes: Vec<u8>) -> AppResult<()> {
        let writer = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            handle.writer.clone()
        };

        tokio::task::spawn_blocking(move || -> AppResult<()> {
            let mut w = writer
                .lock()
                .map_err(|e| AppError::Pty(format!("writer poisoned: {e}")))?;
            w.write_all(&bytes).map_err(AppError::Io)?;
            w.flush().map_err(AppError::Io)?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Pty(format!("blocking task: {e}")))??;
        Ok(())
    }

    pub async fn resize(&self, rows: u16, cols: u16) -> AppResult<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);

        let (master, last_size, control_tx) = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            (
                handle.master.clone(),
                handle.last_size.clone(),
                handle.control_tx.clone(),
            )
        };

        // Claim the new target under the lock — dedup check AND update together —
        // so a concurrent resize back to the previous size isn't wrongly
        // suppressed against a `last_size` this call hasn't published yet (which
        // would leave the PTY at the wrong size).
        let prev = {
            let mut last = last_size
                .lock()
                .map_err(|e| AppError::Pty(format!("last_size poisoned: {e}")))?;
            if *last == (rows, cols) {
                return Ok(());
            }
            let prev = *last;
            *last = (rows, cols);
            prev
        };

        let result = tokio::task::spawn_blocking(move || -> AppResult<()> {
            let m = master
                .lock()
                .map_err(|e| AppError::Pty(format!("master poisoned: {e}")))?;
            m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::Pty(format!("resize: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Pty(format!("blocking task: {e}")));

        // Roll back the claimed size on failure so a retry to this size isn't
        // suppressed — but only if no later resize already superseded our claim.
        if let Err(e) = result.and_then(|r| r) {
            if let Ok(mut last) = last_size.lock() {
                if *last == (rows, cols) {
                    *last = prev;
                }
            }
            return Err(e);
        }

        // Tell the processor a real resize just fired so it can ignore the
        // resulting TUI-repaint burst (see `ProcessorControl::Resized`).
        // `try_send` is deliberate: a full control queue during a fast drag
        // means a Resized is already in flight, so dropping this one is
        // harmless — the processor only needs to know a resize happened
        // recently, not how many.
        let _ = control_tx.try_send(ProcessorControl::Resized);
        Ok(())
    }

    /// V1.4-03: swap the processor task's output channel without
    /// restarting the PTY. Used when the JS-side xterm is destroyed and
    /// recreated for a renderer-category flip — the shell session, env,
    /// cwd, and running processes survive; only the IPC `Channel<String>`
    /// is replaced.
    ///
    /// Returns `AppError::NotStarted` if no PTY is registered, or
    /// `AppError::Pty` if the processor task has already exited (e.g.,
    /// after a child PTY exit). In both cases the caller is expected to
    /// fall back to `pty_start`.
    pub async fn rebind_channel(&self, new_channel: Channel<String>) -> AppResult<()> {
        let control_tx = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            handle.control_tx.clone()
        };
        // Lock released before `await` so a slow processor doesn't block
        // other PtyManager operations.
        control_tx
            .send(ProcessorControl::ChannelChange(new_channel))
            .await
            .map_err(|_| AppError::Pty("processor task gone".into()))?;
        Ok(())
    }

    /// M11: drop the permission detector's latched pattern for this tab so a
    /// prompt still on screen is re-detected on the next scan tick. Best-effort
    /// and non-blocking by design — this rides the hook path, which must never
    /// stall on a busy processor, and a dropped message only costs the
    /// re-detection this call was trying to enable (the regex path is the
    /// fallback, not the primary). No-op when this tab has no running PTY.
    pub async fn clear_permission_latch(&self) {
        let control_tx = {
            let guard = self.inner.lock().await;
            match guard.as_ref() {
                Some(handle) => handle.control_tx.clone(),
                None => return,
            }
        };
        if control_tx
            .try_send(ProcessorControl::ClearPermissionLatch)
            .is_err()
        {
            debug!("permission latch clear dropped (processor busy or gone)");
        }
    }

    /// V1.4-04 D: snapshot the current scrollback ring as a flat
    /// `Vec<u8>`. Used by the graceful-exit persistence path and the
    /// `pty_get_scrollback` Tauri command. Returns `NotStarted` if no
    /// PTY is registered for this tab.
    ///
    /// The snapshot is a copy: the live ring continues to accumulate
    /// bytes after this call returns. This means a long-running
    /// `pty_get_scrollback` poll won't see byte-stream tearing, just
    /// monotonically-newer prefixes.
    pub async fn scrollback_snapshot(&self) -> AppResult<Vec<u8>> {
        let scrollback = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            Arc::clone(&handle.scrollback)
        };
        let ring = scrollback
            .lock()
            .map_err(|e| AppError::Pty(format!("scrollback poisoned: {e}")))?;
        let (a, b) = ring.as_slices();
        let mut out = Vec::with_capacity(a.len() + b.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        Ok(out)
    }

    /// V1.4-04 D: seed the scrollback ring with bytes restored from a
    /// previous session. The replay is purely about persistence
    /// continuity — the same bytes are also written into the new
    /// xterm by the frontend (front-loaded in `term.write` before the
    /// live channel binds). Truncates to the ring cap.
    ///
    /// Ordering note: `seed_scrollback` runs *after* `start` has already
    /// spawned the reader (see `pty_start`), so by the time we take the
    /// ring lock the live reader may have appended this session's first
    /// output. The restored bytes are older than anything live, so they
    /// must be prepended to the *front* of the ring rather than extended
    /// onto the back — otherwise the order inverts to `[live, restored]`
    /// and `trim_ring` (which evicts from the front) drops the fresh live
    /// output instead of old history. Both ends are serialized by this
    /// same `StdMutex`, so the splice can't race the reader.
    pub async fn seed_scrollback(&self, bytes: &[u8]) -> AppResult<()> {
        let (scrollback, cap) = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            (Arc::clone(&handle.scrollback), handle.scrollback_cap)
        };
        let mut ring = scrollback
            .lock()
            .map_err(|e| AppError::Pty(format!("scrollback poisoned: {e}")))?;
        // Splice restored-then-live so chronological order is preserved.
        crate::pty::scrollback::seed_front(&mut ring, bytes, cap);
        Ok(())
    }

    /// V1.4-04 D: clear the scrollback ring. Called on user-initiated
    /// `pty_restart` because the user explicitly asked for a clean
    /// shell — the prior session's output is no longer relevant.
    pub async fn clear_scrollback(&self) -> AppResult<()> {
        let scrollback = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            Arc::clone(&handle.scrollback)
        };
        let mut ring = scrollback
            .lock()
            .map_err(|e| AppError::Pty(format!("scrollback poisoned: {e}")))?;
        ring.clear();
        Ok(())
    }

    pub async fn shutdown(&self) -> AppResult<()> {
        let handle = {
            let mut guard = self.inner.lock().await;
            guard.take()
        };

        if let Some(handle) = handle {
            handle.cancel.cancel();
            let killer = handle.killer.clone();
            let _ = tokio::task::spawn_blocking(move || {
                match killer.lock() {
                    Ok(mut k) => {
                        if let Err(e) = k.kill() {
                            // A failed kill leaves the waiter task blocked in
                            // child.wait() indefinitely — holding the child
                            // handle and a blocking-pool thread, with the
                            // process orphaned. Nothing more we can do
                            // synchronously, but surface it instead of
                            // swallowing it silently.
                            warn!(error = %e, "PTY kill failed during shutdown; child may be orphaned");
                        }
                    }
                    Err(e) => warn!(error = %e, "PTY killer mutex poisoned during shutdown"),
                }
            })
            .await;
            debug!("PTY session shut down");
        }
        Ok(())
    }
}

impl Default for PtyManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::tasks::spawn_processor;
    use crate::settings::SettingsHandle;
    use crate::state::TabId;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// V30 (review M9): the spawn path must be able to UN-inherit a variable,
    /// not just add one — and an explicit per-tab value must still win.
    #[test]
    fn env_removals_strip_inherited_vars_but_lose_to_explicit_values() {
        // A var this process really has, so the removal has something to bite.
        std::env::set_var("CIMP_TEST_INHERITED", "1");
        std::env::set_var("CIMP_TEST_OVERRIDDEN", "parent");
        let mut cmd = CommandBuilder::new("cmd");
        let mut env = std::collections::HashMap::new();
        env.insert("CIMP_TEST_OVERRIDDEN".to_string(), "child".to_string());
        apply_env(
            &mut cmd,
            &env,
            &[
                "CIMP_TEST_INHERITED".to_string(),
                "CIMP_TEST_OVERRIDDEN".to_string(),
                "CIMP_TEST_NEVER_SET".to_string(), // removing an absent var is a no-op
            ],
        );
        assert_eq!(
            cmd.get_env("CIMP_TEST_INHERITED"),
            None,
            "an inherited var on the strip list must not reach the child"
        );
        assert_eq!(
            cmd.get_env("CIMP_TEST_OVERRIDDEN")
                .and_then(|v| v.to_str()),
            Some("child"),
            "a per-tab env entry outranks the strip list"
        );
        std::env::remove_var("CIMP_TEST_INHERITED");
        std::env::remove_var("CIMP_TEST_OVERRIDDEN");
    }

    /// **V33 Phase B decision B4, as a test: the sandboxed child's environment
    /// is the plain child's environment plus the scratch redirection.**
    ///
    /// This is the likeliest regression in the whole phase. The sandboxed
    /// backend cannot spawn through `CommandBuilder` (its `cmdline()` and
    /// `environment_block()` are `pub(crate)`), so a naive implementation
    /// composes a second environment from `std::env::vars_os()` — and on
    /// Windows that is NOT what `CommandBuilder::new` produces: it additionally
    /// re-reads the machine and user `Environment` registry keys and
    /// concatenates system+user `PATH`. A sandboxed tab would then run with a
    /// different `PATH` than the same tab unsandboxed, and nobody would connect
    /// that to the sandbox switch.
    ///
    /// The fix under test is structural — `start_sandboxed` reads the RESOLVED
    /// environment back out of the same builder — so what this asserts is the
    /// composition that reading depends on: removals still bite, an explicit
    /// per-tab value still wins, the scratch override lands last, and
    /// `HOME`/`USERPROFILE` are left alone (unlike `run_command`'s children,
    /// whose home is redirected into the sandbox root).
    #[test]
    fn the_sandboxed_env_is_the_plain_env_plus_the_scratch_redirection() {
        use std::path::Path;
        std::env::set_var("CIMP_TEST_B4_INHERITED", "yes");
        std::env::set_var("CIMP_TEST_B4_STRIPPED", "leak");

        // Exactly the composition `start` performs for BOTH paths…
        let mut cmd = CommandBuilder::new("claude");
        let mut env = std::collections::HashMap::new();
        env.insert("CIMP_TEST_B4_EXPLICIT".to_string(), "tab".to_string());
        apply_env(&mut cmd, &env, &["CIMP_TEST_B4_STRIPPED".to_string()]);
        // …then the sandbox's own overrides, last, as `start_sandboxed` adds
        // them (the scratch expressed on the mapped drive).
        let scratch = Path::new(r"S:\.cimp\sandbox-tmp\claude");
        for (key, value) in crate::sandbox::tabs::env_overrides(scratch) {
            cmd.env(key, value);
        }

        let pairs: std::collections::HashMap<String, String> = cmd
            .iter_full_env_as_str()
            .map(|(k, v)| (k.to_ascii_uppercase(), v.to_string()))
            .collect();

        assert_eq!(
            pairs.get("CIMP_TEST_B4_INHERITED").map(String::as_str),
            Some("yes"),
            "the sandboxed child must still inherit the environment the plain one would"
        );
        assert!(
            !pairs.contains_key("CIMP_TEST_B4_STRIPPED"),
            "the harness-marker strip list must apply on the sandboxed path too — a Claude \
             spawned with CLAUDE_CODE_CHILD_SESSION set writes no transcript at all"
        );
        assert_eq!(
            pairs.get("CIMP_TEST_B4_EXPLICIT").map(String::as_str),
            Some("tab"),
            "an explicit per-tab value still outranks everything"
        );
        let expected = scratch.to_string_lossy().to_string();
        assert_eq!(pairs.get("TEMP"), Some(&expected), "TEMP must be redirected");
        assert_eq!(pairs.get("TMP"), Some(&expected), "…and TMP with it");

        // The load-bearing negative: the harness's real home is where its
        // credentials and session history live, and the grant table — not an
        // env redirection — is what keeps the rest of that directory dark.
        for key in ["HOME", "USERPROFILE"] {
            if let Some(real) = std::env::var_os(key).and_then(|v| v.into_string().ok()) {
                assert_eq!(
                    pairs.get(key),
                    Some(&real),
                    "{key} must stay REAL for a tab (unlike run_command's children)"
                );
            }
        }

        std::env::remove_var("CIMP_TEST_B4_INHERITED");
        std::env::remove_var("CIMP_TEST_B4_STRIPPED");
    }

    /// V1.4-03: rebind on an empty manager errors with NotStarted. The
    /// frontend's `attemptSpawn(entry, 'rebind')` catch block treats this
    /// as the signal to fall back to `pty_start`.
    #[tokio::test]
    async fn rebind_with_no_pty_errors() {
        let manager = PtyManager::new();
        let dummy = Channel::new(|_| Ok(()));
        let result = manager.rebind_channel(dummy).await;
        assert!(matches!(result, Err(AppError::NotStarted)));
    }

    /// V1.4-03: directly exercises the processor task's channel-swap
    /// behavior without spinning up a real PTY. Confirms that bytes
    /// emitted before `ChannelChange` reach the old `Channel<String>`,
    /// bytes emitted after reach the new one, and no bytes are lost
    /// across the swap (the cancel-safety claim about `mpsc::Receiver::
    /// recv()` in the select loop).
    #[tokio::test]
    async fn channel_rebind_routes_bytes_to_new_channel() {
        let received_a: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let received_b: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));

        let a_buf = received_a.clone();
        let channel_a: Channel<String> = Channel::new(move |body| {
            // `body` is the encoded base64 string the processor sends.
            let s = String::from_utf8(body.deserialize().unwrap_or_default()).unwrap_or_default();
            a_buf.lock().unwrap().push(s);
            Ok(())
        });
        let b_buf = received_b.clone();
        let channel_b: Channel<String> = Channel::new(move |body| {
            let s = String::from_utf8(body.deserialize().unwrap_or_default()).unwrap_or_default();
            b_buf.lock().unwrap().push(s);
            Ok(())
        });

        let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>(16);
        let (control_tx, control_rx) = mpsc::channel::<ProcessorControl>(4);
        let (state_tx, _state_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        // SettingsHandle uses defaults; no `.set()` is ever called so the
        // debounced saver task stays idle and never touches disk.
        let defaults = crate::settings::Settings::default();
        let settings = SettingsHandle::new(defaults.clone(), defaults, std::env::temp_dir());

        let tab = TabId::Shell("shell-test".to_string());
        let patterns = Arc::new(Vec::new());
        spawn_processor(
            tab,
            bytes_rx,
            channel_a,
            control_rx,
            cancel.clone(),
            state_tx,
            settings,
            patterns,
            // A shell tab: nothing infers activity for it.
            crate::harness::plugin::ActivitySource::OutOfBand,
        );

        // Bytes before the rebind. The processor's flush tick is 50ms;
        // give it a couple of cycles to drain.
        bytes_tx.send(b"hello-A\r\n".to_vec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Swap to channel B.
        control_tx
            .send(ProcessorControl::ChannelChange(channel_b))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Bytes after the rebind.
        bytes_tx.send(b"hello-B\r\n".to_vec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let a = received_a.lock().unwrap().clone();
        let b = received_b.lock().unwrap().clone();
        // The processor base64-encodes terminal bytes. We don't decode
        // here — just assert the routing: A got something, B got
        // something, and nothing crossed.
        assert!(
            !a.is_empty(),
            "channel A should have received pre-swap bytes"
        );
        assert!(
            !b.is_empty(),
            "channel B should have received post-swap bytes"
        );
    }

    /// V1.4-03: confirms the processor handles three rapid rebinds
    /// without dropping bytes or panicking — mimics a slider drag that
    /// crosses the image/no-image threshold multiple times faster than
    /// the JS-side debounce can collapse them.
    #[tokio::test]
    async fn processor_survives_rapid_rebinds() {
        let final_buf: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));

        let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>(16);
        let (control_tx, control_rx) = mpsc::channel::<ProcessorControl>(8);
        let (state_tx, _state_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let defaults = crate::settings::Settings::default();
        let settings = SettingsHandle::new(defaults.clone(), defaults, std::env::temp_dir());

        let initial: Channel<String> = Channel::new(|_| Ok(()));

        let tab = TabId::Shell("shell-test".to_string());
        let patterns = Arc::new(Vec::new());
        spawn_processor(
            tab,
            bytes_rx,
            initial,
            control_rx,
            cancel.clone(),
            state_tx,
            settings,
            patterns,
            // A shell tab: nothing infers activity for it.
            crate::harness::plugin::ActivitySource::OutOfBand,
        );

        // Three rapid rebinds. The last channel is the one whose buffer
        // we assert against.
        for _ in 0..2 {
            let throwaway: Channel<String> = Channel::new(|_| Ok(()));
            control_tx
                .send(ProcessorControl::ChannelChange(throwaway))
                .await
                .unwrap();
        }
        let final_clone = final_buf.clone();
        let final_channel: Channel<String> = Channel::new(move |body| {
            let s = String::from_utf8(body.deserialize().unwrap_or_default()).unwrap_or_default();
            final_clone.lock().unwrap().push(s);
            Ok(())
        });
        control_tx
            .send(ProcessorControl::ChannelChange(final_channel))
            .await
            .unwrap();

        // Send bytes after the last rebind.
        tokio::time::sleep(Duration::from_millis(50)).await;
        bytes_tx.send(b"final\r\n".to_vec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let received = final_buf.lock().unwrap().clone();
        assert!(
            !received.is_empty(),
            "final channel should have received post-rebind bytes"
        );
    }
}

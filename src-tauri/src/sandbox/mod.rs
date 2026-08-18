//! V33 Phase A — the OS sandbox layer for **agent-initiated child processes**.
//!
//! Engine: Windows AppContainer, chosen by spike S1
//! (`docs/reviews/SPIKE-S1-appcontainer-2026-08-15.md`, user decision
//! 2026-08-15 closing milestone decision 2). Linux gets Landlock in Phase D;
//! until then non-Windows reports `Unavailable` and children run exactly as
//! before — loudly, per decision 5, never silently.
//!
//! **Scope.** This layer wraps three of the four [`SpawnClass::AgentSpawn`]
//! seams: `run_command` children (`offload/tools/run_command.rs`, Phase A), the
//! `run_check` shell (`checks/mod.rs`) and the audit scanners
//! (`audit/runner.rs`). Tab spawns (ConPTY) are Phase B behind spike S3. Host
//! spawns (`spawn_ledger::SpawnClass::HostSpawn`) are **never** sandboxed — see
//! the ledger's reasons column.
//!
//! **One switch, every seam** (milestone decision 17's membership test): there
//! is no per-seam sandbox toggle. `sandbox.enabled` governs the OS boundary
//! wherever a model's request reaches a spawn, because the question the setting
//! answers ("does the OS confine what agents run?") does not have three
//! different answers. What differs per seam is only what has to: which program
//! is spawned, which directories grant-on-first-use infers, and which `seam`
//! label its rows carry.
//!
//! **What the boundary is (and honestly is not).** A sandboxed child can
//! read+write the project root, read the OS dirs and each granted tool's own
//! code/config/caches — and nothing else. Writes outside the root are denied,
//! reads of credential dirs, other projects and cImp's own secrets are denied.
//! It is *not* "reads nothing outside the root": executing `git` means reading
//! `git.exe`. Every widening beyond the root is a reviewed grant with a reason
//! (decision 3), stamped once per machine against the **stable** container SID.
//!
//! **The off switch reaches this layer only** (decision 17): `sandbox.enabled =
//! false` removes the AppContainer wrapper and nothing more. Job objects, the
//! minimal environment and the injection-layer fixes are lifecycle/containment
//! correctness and remain unconditional. The two negative states are distinct
//! and stay distinct (C10): [`SkipReason::OffUser`] is a choice,
//! [`SkipReason::Unavailable`] is a missing prerequisite; collapsing them is
//! how a broken prerequisite hides behind a deliberate setting.
//!
//! **Degradation is loud, never silent** (decision 5): every skip reason is
//! recorded as a `sandbox` activity row (its own retention lane, per #51's
//! pick-a-lane-on-purpose rule) the first time it occurs in a session, and the
//! child then runs unsandboxed — this is a hardening layer over a working
//! product, not a gate that bricks tool calls on a missing prerequisite.

pub mod child_env;
#[cfg(windows)]
pub mod windows;

use std::path::{Path, PathBuf};
use std::time::Duration;

// ── the seam labels ─────────────────────────────────────────────────────────
//
// Every row this module writes names the seam it came from in the activity
// record's `source` column, so the Events lane distinguishes "a model ran a
// command", "a model ran a configured check" and "a model ran an audit
// scanner" without opening a row. They are `&'static str` rather than an enum
// because the audit seam's label carries the tool name (`audit:semgrep`) and an
// enum with a `String` payload would buy nothing over this.

/// `offload/tools/run_command.rs` — the model names program and arguments.
pub const SEAM_RUN_COMMAND: &str = "run_command";
/// `checks/mod.rs` — the model selects one of the operator's configured checks,
/// which cImp runs through the platform shell.
pub const SEAM_RUN_CHECK: &str = "run_check";
/// `audit/runner.rs` — the label is `audit:<tool>` (see [`audit_seam`]).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn audit_seam(tool: &str) -> String {
    format!("audit:{tool}")
}

// ── the caller-side backstops (the 2026-08-18 wedges) ───────────────────────

/// How much longer than its own child timeout a sandboxed spawn is given to
/// *settle*: terminate the child, wait for the job to reap the tree, drain both
/// pipes (up to `DRAIN_GRACE + DRAIN_CANCEL_GRACE` each, worst case ~14 s
/// serial) and return.
#[cfg_attr(not(windows), allow(dead_code))]
pub const SANDBOX_SETTLE_SLACK: Duration = Duration::from_secs(30);

/// The caller-side backstop for a sandboxed child whose own deadline is
/// `child_timeout` (2026-08-18 incident).
///
/// The sandboxed path is a hand-rolled Win32 dance on a blocking thread; if any
/// step of it ever fails to return, the tool call never completes, the calling
/// worker/scan stays pinned and NOTHING is recorded — which is exactly how the
/// first live sandboxed `run_command` spent 22 minutes invisible. The engine
/// bounds its own internals now, but a backstop that exists only inside the
/// thing it is backstopping is not a backstop.
///
/// A `const fn` so each seam's constant timeout still yields a constant, and so
/// the three seams cannot each invent their own slack: `run_command` has a
/// fixed 120 s cap, a check has its per-check floored timeout and an audit tool
/// has its per-tool budget, but all three derive from this one expression.
#[cfg_attr(not(windows), allow(dead_code))]
pub const fn backstop_for(child_timeout: Duration) -> Duration {
    Duration::from_secs(child_timeout.as_secs() + SANDBOX_SETTLE_SLACK.as_secs())
}

/// The caller-side backstop on sandbox *preparation* (2026-08-18, second
/// incident of the same day). The first wedge taught us to bound
/// `spawn_and_capture` — but preparation (profile creation, ACL grants, drive
/// mapping) ran unbounded ahead of it on a blocking thread, and a deadlock in
/// `map_drive` pinned the worker slot forever with the grant row as the only
/// trace. Same rule as [`backstop_for`]: a path whose only deadline lives
/// inside itself has no deadline at all.
///
/// Shared by every seam and independent of the child's timeout, because
/// preparation happens BEFORE the child exists and costs the same everywhere.
///
/// Generous, because a wrong elapse here refuses a healthy sandbox: first-time
/// ACL stamps on a toolchain directory and AppContainer profile creation can
/// take seconds each on a slow disk or with sluggish SID lookups.
pub const PREPARE_BACKSTOP: Duration = Duration::from_secs(60);

/// A cooperative cancel signal a sandboxed spawn polls while it waits.
///
/// Platform-neutral so a seam can build one without a `cfg(windows)` arm, and a
/// plain `AtomicBool` rather than a `CancellationToken` because the consumer is
/// a **blocking** Win32 wait loop with no runtime to await on. The audit
/// runner's scan token is bridged onto one of these at the call site; see
/// `audit::runner::spawn_sandboxed`.
#[cfg_attr(not(windows), allow(dead_code))]
pub type CancelFlag = std::sync::Arc<std::sync::atomic::AtomicBool>;

/// The runtime slice of `Settings::sandbox` a spawn seam needs. Carried on
/// `ToolCtx` beside the command allowlist rather than read from a global, so
/// the headless `--offload-mcp` child and unit tests get exactly the config
/// their constructor passed — the same plumbing discipline as
/// `command_allowlist` itself.
#[derive(Debug, Clone, Default)]
pub struct SandboxCfg {
    /// Master switch (decision 17; OS layer only).
    pub enabled: bool,
    /// Grant the `internetClient` capability to sandboxed children. Default
    /// off: a read-only probe needs no egress, and S1 measured that on a
    /// Public-profile NIC this capability opens the LAN too — per-host
    /// scoping is S4/WFP work, so the honest granularity today is all/none.
    pub allow_network: bool,
    /// Extra directories granted read+execute inside the sandbox — the
    /// user-curated rows of the decision-3 grant table (tools in
    /// non-standard locations that grant-on-first-use can't infer).
    pub extra_grant_dirs: Vec<PathBuf>,
}

impl SandboxCfg {
    /// The context-provides-none default: sandbox off, labeled as such.
    /// `ToolCtx::new` uses this so a new construction site gets the safe,
    /// honest shape; real paths opt in with the settings value.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Translate the persisted `SandboxSettings` into the runtime config the
    /// spawn seams consume.
    ///
    /// A function rather than a `From` impl because the two types are
    /// deliberately not 1:1 — settings hold user-facing strings
    /// (`extra_grant_dirs`), the runtime wants `PathBuf`s, and a future
    /// engine-selection field will resolve here rather than widening the
    /// settings struct.
    ///
    /// It lives here (rather than beside one seam) because all three seams
    /// build their config from the same settings snapshot: one master switch,
    /// one translation.
    pub fn from_settings(s: &crate::settings::Settings) -> Self {
        Self {
            enabled: s.sandbox.enabled,
            allow_network: s.sandbox.allow_network,
            extra_grant_dirs: s
                .sandbox
                .extra_grant_dirs
                .iter()
                .filter(|d| !d.trim().is_empty())
                .map(PathBuf::from)
                .collect(),
        }
    }
}

/// Why a spawn is NOT running inside the sandbox. The two variants are the
/// C10 states and must never merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `sandbox.enabled = false` — the user's choice.
    OffUser,
    /// A prerequisite is missing or still becoming ready; the reason string
    /// is user-facing (Events row) and starts with what failed.
    Unavailable(String),
}

impl SkipReason {
    /// The Events-row label. Kept short and stable; the detail rides the
    /// record's response payload.
    pub fn label(&self) -> &'static str {
        match self {
            SkipReason::OffUser => "off (user choice)",
            SkipReason::Unavailable(_) => "unavailable",
        }
    }
}

/// The decision for one spawn: run it inside the container, or run it plain
/// and say why.
pub enum Plan {
    #[cfg(windows)]
    Sandboxed(windows::Prepared),
    Plain(SkipReason),
}

/// What a seam needs granted **beyond** what [`plan`] infers from the spawned
/// program itself (whose own install directory is always granted read+execute,
/// and whose project root is always granted full access).
///
/// Every field widens the boundary, so every field is a reviewed decision — the
/// decision-3 grant ladder applied to the seams that cannot express their needs
/// as "the program I spawn". Seams with nothing extra pass
/// [`GrantHints::default`], which grants nothing.
///
/// Owned rather than borrowed because both lists are tiny (0–2 entries) and
/// `prepare` has to move them onto a blocking thread anyway.
#[derive(Debug, Clone, Default)]
pub struct GrantHints {
    /// Resolved program paths whose **parent directory** gets read+execute,
    /// exactly as the spawned program's does.
    ///
    /// The grant-inference hook for a seam where the spawned program is not the
    /// program that does the work: `run_check` spawns `cmd.exe`, and the tool
    /// the check invokes (`cargo` in `cargo test --bin cimp`) lives somewhere
    /// else entirely.
    pub programs: Vec<PathBuf>,
    /// Directories the child must be able to **write**, granted full access.
    ///
    /// Today's only user is the audit runner's report directory: a
    /// `Transport::ReportFile` scanner (gitleaks, cppcheck, dotnet-analyzers)
    /// is handed an absolute SARIF path under cImp's own temp scratch and
    /// writes its findings there. Without this grant those three tools fail
    /// with an access denial the moment the sandbox is switched on — correctly
    /// reported, but a working feature turned into a denial row.
    ///
    /// **Only cImp-owned scratch belongs here.** The project root is already
    /// granted; the user's tree is not a place cImp adds write ACEs to on a
    /// tool's behalf.
    pub full_dirs: Vec<PathBuf>,
}

/// Decide how to run one agent-initiated child, doing all sandbox preparation
/// (profile, grants, drive mapping) that the decision needs.
///
/// `program` is what cImp actually spawns; its install directory is granted
/// read+execute. `hints` widens that — see [`GrantHints`].
///
/// `env` is the exact minimal-environment pair list the plain spawn would
/// use — the sandbox path adds its redirections on top (TEMP and tool caches
/// into the root) rather than composing a second environment.
pub async fn plan(
    cfg: &SandboxCfg,
    seam: &str,
    program: &Path,
    hints: &GrantHints,
    root: &Path,
    env: &[(&str, std::ffi::OsString)],
) -> Plan {
    if !cfg.enabled {
        return Plan::Plain(SkipReason::OffUser);
    }
    #[cfg(windows)]
    {
        match windows::prepare(cfg, seam, program, hints, root, env).await {
            Ok(prepared) => Plan::Sandboxed(prepared),
            Err(reason) => Plan::Plain(SkipReason::Unavailable(reason)),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (seam, program, hints, root, env);
        Plan::Plain(SkipReason::Unavailable(
            "no OS sandbox engine on this platform yet (Linux Landlock is V33 Phase D)".into(),
        ))
    }
}

/// Record one skip loudly, once per distinct reason **per seam** per session —
/// repeat occurrences are the same fact, and a row per spawn would just let
/// this lane crowd itself out of its retention window.
///
/// The seam is part of the dedup key, not only of the row: "run_command runs
/// unsandboxed" and "run_check runs unsandboxed" are two facts, and keying on
/// the reason alone would let whichever seam spawned first silence the others.
pub fn record_skip(seam: &str, reason: &SkipReason, subject: &str, root: &Path) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static EMITTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let key = match reason {
        SkipReason::OffUser => format!("{seam}|off"),
        SkipReason::Unavailable(r) => format!("{seam}|{r}"),
    };
    if let Ok(mut guard) = EMITTED.lock() {
        let set = guard.get_or_insert_with(HashSet::new);
        if !set.insert(key) {
            return;
        }
    }
    // `off (user choice)` is still recorded — once — so the Events feed
    // answers "was this run sandboxed?" without the user having to remember
    // what the switch was set to at the time (C10's two-states rule).
    let detail = match reason {
        SkipReason::OffUser => String::new(),
        SkipReason::Unavailable(r) => r.clone(),
    };
    crate::activity::record_bg(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::Sandbox,
            crate::activity::now_ms(),
            root.to_string_lossy().into_owned(),
            seam.to_string(),
            "unsandboxed".into(),
            state_target(reason.label(), subject),
            0,
            0,
            // `ok` mirrors whether this state is a chosen one: a user choice is
            // not a failure; a missing prerequisite is.
            matches!(reason, SkipReason::OffUser),
            crate::activity::Attribution::Headless,
            None,
        ),
        request: String::new(),
        response: detail,
    });
}

/// Record a sandbox-side lifecycle fact — the one-time ACL grants that prepare
/// a machine (`tool = "grant"`), a confirmation, a denial, a wedge — into the
/// same lane, tagged with the `seam` it came from.
///
/// `#[allow(dead_code)]` off Windows: the only caller is the AppContainer
/// engine, and Landlock (Phase D) will be the second.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn record_event(seam: &str, root: &Path, tool: &str, target: String, detail: String, ok: bool) {
    crate::activity::record_bg(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::Sandbox,
            crate::activity::now_ms(),
            root.to_string_lossy().into_owned(),
            seam.to_string(),
            tool.to_string(),
            target,
            0,
            0,
            ok,
            crate::activity::Attribution::Headless,
            None,
        ),
        request: String::new(),
        response: detail,
    });
}

/// How much of a failed child's stderr rides along in a denial row. Long
/// enough that the actual error line survives, short enough that a chatty
/// tool cannot push its own row past the activity store's payload cap.
///
/// The `allow(dead_code)` on this and the helpers below is the same one
/// [`record_event`] carries and for the same reason: the only non-test caller
/// today is the Windows AppContainer engine, and Landlock (Phase D) will be
/// the second.
#[cfg_attr(not(windows), allow(dead_code))]
const DENIAL_STDERR_TAIL_CHARS: usize = 500;

/// Substrings whose presence means the OS refused a **file or object** access.
/// Matched case-insensitively against a failed child's stderr.
///
/// `os error 5` is Rust's rendering of `ERROR_ACCESS_DENIED`; `Access is
/// denied` is what the Win32 tools print for the same thing; `Permission
/// denied` is the POSIX spelling, kept here because a cross-compiled or
/// MSYS-linked tool prints it on Windows too.
#[cfg_attr(not(windows), allow(dead_code))]
const FILESYSTEM_DENIAL_MARKERS: &[&str] = &["os error 5", "access is denied", "permission denied"];

/// Substrings whose presence means the OS refused a **socket** operation.
/// `10013` is `WSAEACCES`; the "forbidden by its access permissions" phrasing
/// is the message Windows renders for it, which is exactly what an
/// AppContainer without `internetClient` produces on `connect()`.
#[cfg_attr(not(windows), allow(dead_code))]
const SOCKET_DENIAL_MARKERS: &[&str] = &[
    "os error 10013",
    "wsaeacces",
    "forbidden by its access permissions",
];

/// Substrings that mean name resolution died. These are **conditional** — see
/// [`denial_signature`] — because with egress allowed they are ordinary
/// network weather, and claiming them as boundary denials would be dishonest.
#[cfg_attr(not(windows), allow(dead_code))]
const NAME_RESOLUTION_MARKERS: &[&str] = &[
    "could not resolve host",
    "getaddrinfo",
    "temporary failure in name resolution",
    "curle_couldnt_resolve_host",
];

/// Classify one failed child's output: does it *look like* the sandbox
/// boundary refused something?
///
/// Pure and cross-platform on purpose — the AppContainer engine is
/// Windows-only, but the judgement it feeds is plain string work, so it is
/// testable (and reviewable) on any machine, and Landlock's Phase D failures
/// will be classified by this same function rather than a second copy.
///
/// # This is a heuristic, and the caller must say so
///
/// cImp cannot observe the OS's ACL decision: a sandboxed child is a separate
/// process whose `NtCreateFile` returned `STATUS_ACCESS_DENIED` to *itself*.
/// All we have is the exit code and whatever the tool chose to print. So the
/// return value is a *signature class*, never a verdict, and every row minted
/// from it is worded as "matches an access-denial signature — likely the
/// sandbox boundary". A false positive here (a tool that genuinely hit a
/// permission problem of its own) costs one over-eager Events row; asserting
/// certainty would cost the lane its credibility.
///
/// # Why the network markers are conditional
///
/// Inside an AppContainer without `internetClient`, DNS is the *usual* place a
/// network-touching tool dies — the resolver socket is refused before any
/// connect is attempted, so "could not resolve host" is the boundary's most
/// common fingerprint. But when the user has granted egress
/// (`allow_network = true`) those same strings mean the network is simply
/// broken or the host is wrong, and the sandbox had nothing to do with it.
/// One flag, two meanings — so the flag is an argument, not an assumption.
///
/// Returns `None` for a clean exit (whatever the stderr says — a passing run
/// that mentions "permission denied" in a test name is not a boundary event)
/// and for an ordinary nonzero exit with no matching marker (a failing test
/// suite is not a boundary event either).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn denial_signature(
    exit_code: Option<i32>,
    stderr: &str,
    allow_network: bool,
) -> Option<&'static str> {
    // A child that succeeded did not get denied, no matter what it printed.
    // `None` (no code — a spawn failure or an abnormal termination) is NOT a
    // success and still gets classified.
    if exit_code == Some(0) {
        return None;
    }
    let hay = stderr.to_ascii_lowercase();
    let hit = |markers: &[&str]| markers.iter().any(|m| hay.contains(m));
    if hit(FILESYSTEM_DENIAL_MARKERS) {
        return Some("filesystem/OS access denied");
    }
    if hit(SOCKET_DENIAL_MARKERS) {
        return Some("socket access denied");
    }
    if !allow_network && hit(NAME_RESOLUTION_MARKERS) {
        return Some("name resolution failed (no network capability)");
    }
    None
}

/// The capability posture a sandboxed child ran under, rendered for a row's
/// detail. Both new row types carry it: a denial is only interpretable next to
/// what the boundary was actually configured to allow.
#[cfg_attr(not(windows), allow(dead_code))]
fn posture(cfg: &SandboxCfg) -> String {
    format!(
        "network={}, extra grants={}",
        if cfg.allow_network { "on" } else { "off" },
        cfg.extra_grant_dirs.len()
    )
}

/// **What a sandbox-lane row is about**, rendered from a program path: the file
/// name. This is what lands in the scannable `target` column and what the
/// confirmation row dedups on.
///
/// Two of the three seams use this. The `run_check` seam deliberately does NOT:
/// it always spawns `cmd.exe`, so a program-derived subject would render every
/// check identically and collapse them all into one confirmation row. It passes
/// the CHECK NAME instead — the thing the user configured and the thing they
/// would look for in the lane. That is why these helpers take a `&str` subject
/// rather than a `&Path`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn program_subject(program: &Path) -> String {
    program
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// The dedup key for a confirmation row: the subject, lowercased.
/// `git.exe` and `GIT.EXE` are one subject; `git.exe` and `cargo.exe` are two.
#[cfg_attr(not(windows), allow(dead_code))]
fn subject_key(subject: &str) -> String {
    subject.to_ascii_lowercase()
}

/// Insert `key`, returning whether it was new. Split out from the statics so
/// the dedup *policy* is testable without touching a process-wide set (which
/// no test can reset, and which every other test in the binary shares).
#[cfg_attr(not(windows), allow(dead_code))]
fn first_time(set: &mut std::collections::HashSet<String>, key: String) -> bool {
    set.insert(key)
}

/// Record that a program is running INSIDE the sandbox — once per program per
/// session, mirroring [`record_skip`]'s dedup and for the same reason: a row
/// per spawn would let this lane crowd itself out of its retention window.
///
/// # Why a positive row exists at all
///
/// Before this, the lane recorded only *skips*, which made an empty lane
/// ambiguous in exactly the way that confused live testing: "everything ran
/// sandboxed" and "nothing ever spawned" produced the same empty list. A lane
/// that only speaks when something is wrong cannot be read as evidence that
/// nothing is wrong. One affirmative row per program removes the ambiguity —
/// the lane now says which programs the boundary is actually wrapping, and
/// under what capability posture.
///
/// # Column shape
///
/// `tool` is the state label (`"sandboxed"`), matching [`record_skip`]'s
/// `"unsandboxed"` and the engine's `"grant"` — the lane's rows are read by
/// scanning that column, and the frontend's `rowStatus` keys on it. `target`
/// is the human summary in the skip row's shape (`"<label> — <program>"`), so
/// a glance answers "which program?" without opening the row; the posture and
/// the rest of the facts ride the detail payload.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn record_sandboxed(seam: &str, root: &Path, subject: &str, cfg: &SandboxCfg) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static EMITTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    if let Ok(mut guard) = EMITTED.lock() {
        let set = guard.get_or_insert_with(HashSet::new);
        // Per subject per SEAM. Both halves earn their place:
        //
        // * the SEAM, because `run_check` and `run_command` can both spawn the
        //   same program (`cmd.exe`) and "checks are sandboxed" is not the same
        //   fact as "commands are sandboxed";
        // * the SUBJECT, which is a program name for `run_command`/audit but the
        //   CHECK NAME for `run_check` — so each configured check confirms once
        //   per session instead of the first one speaking for all of them.
        if !first_time(set, format!("{seam}|{}", subject_key(subject))) {
            return;
        }
    }
    record_event(
        seam,
        root,
        "sandboxed",
        state_target("sandboxed", subject),
        format!("{subject} is running inside the sandbox — {}", posture(cfg)),
        true,
    );
}

/// Record that a sandboxed child failed with output matching an access-denial
/// signature — the boundary being hit, as best as this process can tell.
///
/// # Every occurrence is recorded — deliberately unlike [`record_skip`]
///
/// A skip is one standing fact ("the switch is off"), so repeating it adds
/// nothing and dedup protects the lane. A denial is an *event*: the pattern
/// the user asked to be able to see is a child hitting the boundary again and
/// again — a probe walking the filesystem, a tool retrying egress. Collapsing
/// those into one row would delete exactly the signal. The lane's own
/// retention is what bounds the cost, and a flood here is itself the finding.
///
/// `class` comes from [`denial_signature`]; the wording below never asserts
/// that the sandbox denied anything, only that the failure matches the shape.
///
/// # Column shape
///
/// `tool = "denied"` (the state label the lane is scanned by, beside
/// `"unsandboxed"` / `"sandboxed"` / `"grant"`), `target` = the signature class
/// and the program in the skip row's `"<label> — <program>"` shape, so a
/// repeated boundary hit is visible as a repeated *line*, not as something you
/// have to open a row to find. Everything else — the bounded invocation, the
/// exit code, the posture, the screened stderr tail — rides the detail payload.
#[cfg_attr(not(windows), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
pub fn record_denial(
    seam: &str,
    root: &Path,
    subject: &str,
    args: &[String],
    exit_code: Option<i32>,
    stderr: &str,
    class: &str,
    cfg: &SandboxCfg,
) {
    let exit = exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "none (did not run or terminated abnormally)".into());
    let detail = format!(
        "`{}` exit {} — matches an access-denial signature ({}) — likely the sandbox boundary, \
         but cImp cannot observe the OS's decision directly, so this is a labeled heuristic, not \
         proof. Posture: {}.\nstderr tail: {}",
        summarize_invocation(subject, args),
        exit,
        class,
        posture(cfg),
        stderr_tail(stderr)
    );
    record_event(
        seam,
        root,
        "denied",
        state_target(class, subject),
        detail,
        false,
    );
}

/// The `target` column for a sandbox-lane row: `"<label> — <program>"`, the
/// shape [`record_skip`] established ("off (user choice) — git.exe"). Kept as
/// one function so the new row types cannot drift from the skip row's
/// layout — a lane whose rows format their scannable column four different
/// ways is a lane nobody scans.
///
/// `pub(crate)` because the `wedged` row is minted by the *caller*
/// (`run_command::run_sandboxed`) rather than by a `record_*` helper here: the
/// fact it records is "the engine never returned", which is only observable
/// from outside the engine.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn state_target(label: &str, subject: &str) -> String {
    format!("{label} — {subject}")
}

/// `git rev-parse --show-toplevel …(+2 more)` — the invocation, bounded.
/// Three args is enough to tell one probe from another; the rest would just
/// be an unbounded model-controlled string in a security row.
#[cfg_attr(not(windows), allow(dead_code))]
fn summarize_invocation(subject: &str, args: &[String]) -> String {
    const SHOWN: usize = 3;
    const ARG_CHARS: usize = 60;
    let mut out = truncate_chars(subject, ARG_CHARS);
    for arg in args.iter().take(SHOWN) {
        out.push(' ');
        out.push_str(&truncate_chars(arg, ARG_CHARS));
    }
    if args.len() > SHOWN {
        out.push_str(&format!(" …(+{} more)", args.len() - SHOWN));
    }
    out
}

/// The last [`DENIAL_STDERR_TAIL_CHARS`] characters of `stderr`, credential-
/// screened.
///
/// The tail rather than the head: a tool prints its progress first and its
/// error last, so the bytes that explain the denial are at the end.
///
/// Screened through the capture path's scrubber
/// ([`crate::processing::scrub_payload`]) and **fail-closed** exactly as that
/// path is: if the credential rule set does not compile there is no screen, and
/// a row we cannot screen is a row we do not write text into. Allowlisted
/// read-only probes have low-secret stderr, so the loss is small and the
/// alternative — an unscreened child's output landing in a JSONL file — is not
/// a trade worth making for a diagnostic nicety.
#[cfg_attr(not(windows), allow(dead_code))]
fn stderr_tail(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        return "(empty)".into();
    }
    let tail = tail_chars(trimmed, DENIAL_STDERR_TAIL_CHARS);
    match crate::processing::scrub_payload(&tail) {
        Some(scrubbed) => scrubbed.text,
        None => "(withheld: the credential screen is unavailable)".into(),
    }
}

/// Last `n` characters (not bytes — never split a code point).
#[cfg_attr(not(windows), allow(dead_code))]
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    let skipped = count - n;
    let mut out = String::from("…");
    out.extend(s.chars().skip(skipped));
    out
}

/// First `n` characters, with an ellipsis when anything was cut.
#[cfg_attr(not(windows), allow(dead_code))]
fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_cfg_yields_off_user() {
        let cfg = SandboxCfg::disabled();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let plan = rt.block_on(plan(
            &cfg,
            SEAM_RUN_COMMAND,
            Path::new("C:/x/y.exe"),
            &GrantHints::default(),
            Path::new("C:/proj"),
            &[],
        ));
        match plan {
            Plan::Plain(SkipReason::OffUser) => {}
            _ => panic!("disabled cfg must plan a plain spawn with OffUser"),
        }
    }

    #[test]
    fn skip_labels_stay_distinct() {
        // C10: the two negative states must never collapse into one string.
        assert_ne!(
            SkipReason::OffUser.label(),
            SkipReason::Unavailable("x".into()).label()
        );
    }

    /// Decision 17, as a test rather than a comment: the master switch governs
    /// the OS layer ONLY. `SandboxCfg` is the whole of what the switch reaches
    /// at the spawn seam, so anything unconditional must be absent from it —
    /// if a future change moves the minimal environment or the job object
    /// behind this struct, this test is what notices.
    #[test]
    fn the_off_switch_reaches_the_os_layer_only() {
        let cfg = SandboxCfg::disabled();
        // The struct carries only OS-boundary knobs. Job objects
        // (`process_guard`), the C2 minimal environment
        // (`sandbox::child_env::CHILD_ENV`) and the injection-layer fixes are
        // deliberately not
        // representable here — they stay on regardless of `enabled`.
        assert!(!cfg.enabled);
        assert!(!cfg.allow_network);
        assert!(cfg.extra_grant_dirs.is_empty());
        // Guard the shape itself: a three-field struct is the contract. Adding
        // a field is fine; adding one that switches OFF a non-OS protection is
        // the thing decision 17 forbids, and the reviewer of that diff sees
        // this assertion's comment.
        let SandboxCfg {
            enabled: _,
            allow_network: _,
            extra_grant_dirs: _,
        } = cfg;
    }

    /// A settings-off sandbox must be reported as a *choice*, and an engine
    /// failure as *unavailable* — never the reverse, because collapsing them
    /// is how a broken prerequisite hides behind a deliberate setting (C10).
    #[test]
    fn off_is_a_choice_not_an_unavailability() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let plan = rt.block_on(plan(
            &SandboxCfg::disabled(),
            SEAM_RUN_COMMAND,
            Path::new("C:/x/git.exe"),
            &GrantHints::default(),
            Path::new("C:/proj"),
            &[],
        ));
        match plan {
            Plan::Plain(SkipReason::OffUser) => {}
            Plan::Plain(SkipReason::Unavailable(r)) => {
                panic!("a disabled switch was reported as unavailable: {r}")
            }
            #[cfg(windows)]
            Plan::Sandboxed(_) => panic!("a disabled switch still sandboxed the spawn"),
        }
    }

    // ---- V33 Phase A follow-up: what happens INSIDE the boundary ----

    /// Every unconditional marker must classify on its own, in whatever case
    /// the tool happened to print it. These are the strings that mean the OS
    /// refused an object access regardless of the network posture, so they
    /// must fire with egress allowed too.
    #[test]
    fn filesystem_and_socket_markers_are_unconditional() {
        let cases = [
            ("failed to open C:\\x: os error 5", "filesystem/OS access denied"),
            ("Access is denied.", "filesystem/OS access denied"),
            ("ACCESS IS DENIED", "filesystem/OS access denied"),
            ("open /etc/shadow: Permission denied", "filesystem/OS access denied"),
            ("connect failed (os error 10013)", "socket access denied"),
            ("WSAEACCES", "socket access denied"),
            (
                "An attempt was made to access a socket in a way forbidden by its access permissions.",
                "socket access denied",
            ),
        ];
        for (stderr, class) in cases {
            for allow_network in [false, true] {
                assert_eq!(
                    denial_signature(Some(1), stderr, allow_network),
                    Some(class),
                    "{stderr:?} must classify with allow_network={allow_network}"
                );
            }
        }
    }

    /// The honesty rule, as a test: name-resolution failures are the
    /// AppContainer's usual death shape ONLY when egress was withheld. With
    /// `allow_network = true` the very same strings are ordinary network
    /// errors, and claiming them as boundary denials would be a lie the user
    /// cannot check.
    #[test]
    fn name_resolution_is_a_denial_only_when_network_is_off() {
        for stderr in [
            "fatal: Could not resolve host: github.com",
            "getaddrinfo ENOTFOUND registry.npmjs.org",
            "Temporary failure in name resolution",
            "CURLE_COULDNT_RESOLVE_HOST (6)",
        ] {
            assert_eq!(
                denial_signature(Some(128), stderr, false),
                Some("name resolution failed (no network capability)"),
                "{stderr:?} must classify with the network capability off"
            );
            assert_eq!(
                denial_signature(Some(128), stderr, true),
                None,
                "{stderr:?} must NOT be called a boundary denial when egress was granted"
            );
        }
    }

    /// A failing test suite is not a boundary event. An ordinary nonzero exit
    /// with nothing denial-shaped in its output mints no row — this is the
    /// assertion that keeps the lane readable.
    #[test]
    fn an_ordinary_failure_is_not_a_denial() {
        assert_eq!(
            denial_signature(Some(101), "test result: FAILED. 1 failed; 40 passed", false),
            None
        );
        assert_eq!(denial_signature(Some(1), "", false), None);
    }

    /// A child that SUCCEEDED was not denied, whatever it printed — a passing
    /// run whose output happens to quote "Access is denied" (a test name, a
    /// grep hit, a log line) must not be reported as a boundary hit.
    #[test]
    fn a_clean_exit_is_never_a_denial() {
        for allow_network in [false, true] {
            assert_eq!(
                denial_signature(Some(0), "Access is denied. os error 5", allow_network),
                None
            );
            assert_eq!(
                denial_signature(Some(0), "Could not resolve host: x", allow_network),
                None
            );
        }
    }

    /// A spawn failure has no exit code at all (decision 4 routes
    /// `CreateProcessW`'s error string through the same classifier), so
    /// `None` must be classified rather than skipped as "not a failure".
    #[test]
    fn a_missing_exit_code_is_still_classified() {
        assert_eq!(
            denial_signature(None, "CreateProcessW failed: os error 5", false),
            Some("filesystem/OS access denied")
        );
    }

    /// The confirmation row's dedup policy: one key per subject, matched
    /// case-insensitively and independent of where the binary lives, so a
    /// second `git` spawn is silent and a first `cargo` spawn is not.
    #[test]
    fn confirmation_rows_dedup_per_program_not_per_spawn() {
        let key = |p: &str| subject_key(&program_subject(Path::new(p)));
        let mut set = std::collections::HashSet::new();
        assert!(first_time(&mut set, key("C:/bin/git.exe")));
        assert!(!first_time(&mut set, key("C:/bin/git.exe")));
        // Same program, different path and case — still the same fact.
        assert!(!first_time(&mut set, key("D:/other/GIT.EXE")));
        // A different program is a different fact and must be recorded.
        assert!(first_time(&mut set, key("C:/bin/cargo.exe")));
        assert_eq!(set.len(), 2);
    }

    /// …and the `run_check` seam's subject is the CHECK NAME, not the shell it
    /// runs through. Every check spawns the same `cmd.exe`, so a program-derived
    /// subject would render every row identically AND let the first sandboxed
    /// check speak for all of them. Each configured check confirms once.
    #[test]
    fn a_check_is_identified_by_its_configured_name_not_by_the_shell() {
        let mut set = std::collections::HashSet::new();
        // Two different checks, one shell: two facts, two rows.
        assert!(first_time(&mut set, subject_key("cargo")));
        assert!(first_time(&mut set, subject_key("tsc")));
        // …and re-running a check is the same fact, whatever its case.
        assert!(!first_time(&mut set, subject_key("Cargo")));
        assert_eq!(set.len(), 2);
        // The row a user scans names the check, not `cmd.exe`.
        assert_eq!(state_target("sandboxed", "cargo"), "sandboxed — cargo");
        assert!(!state_target("sandboxed", "cargo").contains("cmd"));
    }

    /// Denials are NOT deduped — the repeated boundary hit is the signal the
    /// user asked to be able to see. Guarded as a policy assertion because the
    /// obvious "make it consistent with record_skip" refactor would delete it.
    #[test]
    fn the_denial_path_has_no_dedup_key() {
        let src = include_str!("mod.rs");
        let body = src
            .split("pub fn record_denial(")
            .nth(1)
            .expect("record_denial exists");
        // The function ends at the first `}` in column 0 — every brace inside
        // the body is indented. (Line-ending-blind: `\r\n}` contains `\n}`.)
        let body = body.split("\n}").next().unwrap_or(body);
        assert!(
            !body.contains("EMITTED") && !body.contains("first_time"),
            "record_denial must record every occurrence — repeated boundary hits are the signal"
        );
    }

    /// The stderr tail is bounded, keeps the END of the output (where the
    /// error is), and never splits a UTF-8 code point.
    #[test]
    fn stderr_tail_is_bounded_and_keeps_the_end() {
        let long = format!("{}THE-ERROR", "é".repeat(2_000));
        let tail = stderr_tail(&long);
        assert!(tail.ends_with("THE-ERROR"), "tail must keep the end: {tail}");
        assert!(
            tail.chars().count() <= DENIAL_STDERR_TAIL_CHARS + 1,
            "tail is {} chars",
            tail.chars().count()
        );
        assert_eq!(stderr_tail("   "), "(empty)");
    }

    /// The invocation summary is bounded: the model controls these strings, so
    /// three args and a per-arg cap is what lands in a security row.
    #[test]
    fn invocation_summary_is_bounded() {
        let args: Vec<String> = (0..10).map(|i| format!("--flag-{i}")).collect();
        let got = summarize_invocation(&program_subject(Path::new("C:/bin/git.exe")), &args);
        assert!(got.starts_with("git.exe --flag-0 --flag-1 --flag-2"), "{got}");
        assert!(got.contains("(+7 more)"), "{got}");
        assert!(!got.contains("--flag-3"), "{got}");
        let huge = vec!["x".repeat(500)];
        let got = summarize_invocation("git", &huge);
        assert!(got.chars().count() < 100, "{got}");
        // The subject is model-adjacent too (a check name comes from settings,
        // but the shell tail beside it does not), so it is bounded as well.
        let got = summarize_invocation(&"n".repeat(500), &huge);
        assert!(got.chars().count() < 200, "{got}");
    }

    /// The lane is scanned by its `target` column, so all four row types must
    /// lay it out the same way: `"<state label> — <subject>"`. The skip row set
    /// that shape ("off (user choice) — git.exe") and the rest follow it — a
    /// subject that lives only in an unopened detail payload is a subject
    /// nobody sees.
    #[test]
    fn every_row_type_puts_the_program_in_the_target_column() {
        assert_eq!(
            state_target("sandboxed", &program_subject(Path::new("C:/bin/git.exe"))),
            "sandboxed — git.exe"
        );
        assert_eq!(
            state_target(
                "filesystem/OS access denied",
                &program_subject(Path::new("/usr/bin/curl"))
            ),
            "filesystem/OS access denied — curl"
        );
        // Same separator the skip row uses, so the column reads as one list.
        assert!(state_target("x", "git").contains(" — "));
        // A program path with no file name still yields something scannable
        // rather than a panic or an empty half-row.
        assert_eq!(state_target("x", &program_subject(Path::new(""))), "x — ");
    }

    /// Both new rows must state the capability posture — a denial is only
    /// interpretable next to what the boundary was configured to allow.
    #[test]
    fn posture_names_the_capabilities() {
        let cfg = SandboxCfg::disabled();
        assert_eq!(posture(&cfg), "network=off, extra grants=0");
        let cfg = SandboxCfg {
            enabled: true,
            allow_network: true,
            extra_grant_dirs: vec![PathBuf::from("C:/tools")],
        };
        assert_eq!(posture(&cfg), "network=on, extra grants=1");
    }

    /// On a platform with no engine, the reason must NAME the missing thing —
    /// decision 5's "loud, never silent" applied to the string a user reads in
    /// the Events row.
    #[cfg(not(windows))]
    #[test]
    fn non_windows_says_what_is_missing() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut cfg = SandboxCfg::disabled();
        cfg.enabled = true;
        let plan = rt.block_on(plan(
            &cfg,
            SEAM_RUN_COMMAND,
            Path::new("/usr/bin/git"),
            &GrantHints::default(),
            Path::new("/proj"),
            &[],
        ));
        match plan {
            Plan::Plain(SkipReason::Unavailable(r)) => {
                assert!(r.contains("Landlock"), "reason must name the gap: {r}");
            }
            _ => panic!("an enabled sandbox on a platform with no engine must be Unavailable"),
        }
    }
}

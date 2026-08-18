//! V33 Phase A — the OS sandbox layer for **agent-initiated child processes**.
//!
//! Engines, one per platform:
//!
//! * **Windows — AppContainer** ([`windows`]), chosen by spike S1
//!   (`docs/reviews/SPIKE-S1-appcontainer-2026-08-15.md`, user decision
//!   2026-08-15 closing milestone decision 2). Covers all four seams.
//! * **Linux — Landlock** ([`linux`], V33 Phase D). Covers the three non-PTY
//!   tool seams; AI *tabs* stay unsandboxed there and say so, because a
//!   confined PTY child needs a spawn hook `portable_pty` does not expose.
//! * **Everything else (macOS)** reports `Unavailable` and children run exactly
//!   as before — loudly, per decision 5, never silently.
//!
//! The two engines meet the seams differently and deliberately: Windows must
//! hand-roll a `CreateProcessW` (no `std`/`tokio` `Command` can attach an
//! AppContainer attribute list), so it owns the whole spawn; Linux applies its
//! boundary *to the seam's own spawn* through `pre_exec`, so the plain path and
//! the sandboxed path are the same code with two extra lines. That is why only
//! the Windows path carries settle-slack backstops, cancel flags and bounded
//! drains — those bound a bespoke blocking dance that does not exist on Linux.
//!
//! **Scope.** This layer wraps all four [`SpawnClass::AgentSpawn`] seams:
//! `run_command` children (`offload/tools/run_command.rs`, Phase A), the
//! `run_check` shell (`checks/mod.rs`), the audit scanners (`audit/runner.rs`)
//! and — since Phase B, on the engine spike S3 proved — the **AI-tool tab**
//! itself (`pty/manager.rs` + `pty::sandboxed_conpty`, see
//! [`tabs`](crate::sandbox::tabs)). Host spawns
//! (`spawn_ledger::SpawnClass::HostSpawn`) are **never** sandboxed — see the
//! ledger's reasons column.
//!
//! **One switch, every TOOL seam** (milestone decision 17's membership test):
//! there is no per-seam sandbox toggle. `sandbox.enabled` governs the OS
//! boundary wherever a model's request reaches a spawn, because the question the
//! setting answers ("does the OS confine what agents run?") does not have three
//! different answers. What differs per seam is only what has to: which program
//! is spawned, which directories grant-on-first-use infers, and which `seam`
//! label its rows carry.
//!
//! **Tabs are the one documented exception, and it is a SCOPE switch, not a
//! second master** (Phase B decision B2). `sandbox.tabs` widens the same
//! boundary to the tab process; it is inert unless `sandbox.enabled` is also on.
//! It earns its own checkbox because confining the tab confines *everything the
//! agent afterwards runs* — including tools whose credentials the boundary
//! deliberately withholds — which is a materially larger step than confining one
//! allowlisted probe, and a user who says yes to the second has not thereby said
//! yes to the first.
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
/// The Linux engine. Declared on **every** platform, unlike [`windows`]: its
/// grant ladder, environment redirection and posture wording are pure functions
/// with their own tests, and the machine this project is developed on cannot
/// compile the Linux target at all — so the half that can be reviewed and run
/// everywhere is. Only the parts that need the kernel are `cfg`'d inside.
pub mod linux;
pub mod tabs;
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
/// `pty/manager.rs` — V33 Phase B. The label is `tab:<tab id>` and the row's
/// SUBJECT is the tab id too.
///
/// Per-tab rather than one flat `tab` label, because unlike the other three
/// seams a tab is long-lived and identity-bearing: two Claude tabs on one
/// project are two boundaries with two lifetimes, and a lane that called both
/// `tab` would let the first tab's confirmation speak for the second's. The
/// subject repeats the id (rather than naming `claude.exe`) for the same reason
/// `run_check` names the check instead of `cmd.exe` — every AI tab of one
/// harness spawns the same binary.
pub fn tab_seam(tab_id: &str) -> String {
    format!("tab:{tab_id}")
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
    /// The two `Sandboxed` variants are mutually exclusive by `cfg` — exactly
    /// one exists in any build, and neither does on a platform with no engine.
    /// Same name on purpose: a seam's `if let Plan::Sandboxed(prepared)` arm
    /// reads identically on both platforms, and what differs (a bespoke spawn
    /// vs. a hook on the plain one) differs where it has to, in the seam's own
    /// `cfg` block.
    #[cfg(target_os = "linux")]
    Sandboxed(linux::Prepared),
    Plain(SkipReason),
}

/// The **interpreter root** behind a launcher directory, when the directory
/// follows the one convention that hides one.
///
/// A Windows Python distribution puts every `pip`-installed console script in
/// `<install-root>\Scripts\<tool>.exe`. That `.exe` is a tiny launcher stub: it
/// loads `python3XX.dll` and the standard library **from the install root**,
/// which is the `Scripts` directory's PARENT. Granting only the program's own
/// directory therefore hands the container an executable it cannot initialize.
///
/// Live rc.9, `audit:semgrep`: the grant row named
/// `…\pythoncore-3.14-64\Scripts` and nothing else, and the child exited **1
/// with empty stdout AND empty stderr** — no interpreter, no message, no denial
/// signature to classify. The adapter reads exit 1 as "findings present", so the
/// whole thing surfaced as "the SARIF report was empty — findings were lost".
/// Adding the install root to the grants turns that into a working
/// `semgrep --version` (measured: exit 0, `1.170.0`).
///
/// **Keyed on the directory NAME, not on a tool id**, so it is a rule rather
/// than a semgrep special-case — and keyed on that name *only*, because the
/// general form ("grant the grandparent") would grant `C:\Windows` for anything
/// in `System32` and `C:\Users\<user>` for a tool in `<user>\bin`. Two guards
/// keep it narrow: the parent must literally be named `Scripts`, and the root it
/// yields must not be a volume root (`C:\Scripts\x.exe` grants nothing).
///
/// Deliberately Windows-only in its callers: the POSIX equivalent directory is
/// `bin`, and `/usr/bin/tool` would yield `/usr` — the exact over-grant this
/// rule is shaped to avoid.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn interpreter_root(program_dir: &Path) -> Option<&Path> {
    let name = program_dir.file_name()?.to_str()?;
    if !name.eq_ignore_ascii_case("Scripts") {
        return None;
    }
    let root = program_dir.parent()?;
    // A volume root has no parent of its own; granting one is never the
    // narrow answer this rule promises.
    root.parent()?;
    Some(root)
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
    /// V33 Phase B: the reviewed grant TABLE — one row per widening, each
    /// carrying its own access width, whether it names a file or a directory,
    /// and the reason it exists.
    ///
    /// The two lists above are shorthands that predate this and stay as they
    /// are (`programs` = "and this program's install dir too", `full_dirs` =
    /// "and cImp's own scratch"). Rows are what a seam uses when the grant is a
    /// *decision* rather than an inference — the tab seam's per-harness state
    /// paths, where the reviewer's question is "why is `~/.claude` readable?"
    /// and the answer has to live beside the path.
    pub rows: Vec<GrantRow>,
}

/// How wide one [`GrantRow`] opens the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantAccess {
    /// Read + execute. Enough to run code and read config.
    ReadExecute,
    /// Read + write + execute. Only for state the tool genuinely owns and
    /// rewrites (a session store, a credentials file it refreshes).
    Full,
}

/// One reviewed widening of the sandbox boundary.
///
/// Data in code with a reason per row, deliberately in the shape of
/// `sandbox::child_env::CHILD_ENV` and `spawn_ledger::LEDGER`: the reviewer of
/// a diff that adds a row sees the path, the width and the justification in one
/// place, and a row with no reason does not compile.
#[derive(Debug, Clone)]
pub struct GrantRow {
    /// The absolute path. Resolved by the seam (usually from `%USERPROFILE%`),
    /// never a pattern.
    pub path: PathBuf,
    pub access: GrantAccess,
    /// `true` when `path` names a FILE rather than a directory. Both go through
    /// `SE_FILE_OBJECT`, but only a directory grant is made inheritable — an
    /// inheritable ACE on a file is meaningless, and asking for one on a
    /// per-file grant is how a reader concludes the directory around it was
    /// granted too.
    pub is_file: bool,
    /// Why the boundary is wider because of this row. User-visible: it is what
    /// the grant Events row prints beside the path.
    pub reason: &'static str,
    /// `false` ⇒ a path that does not exist is skipped rather than failing the
    /// whole preparation. Most harness state is created on first use, so
    /// "absent" is the normal state of half this table on a fresh machine, and
    /// refusing to sandbox a tab because the user has no `~/.config/git` would
    /// be the prerequisite check punishing a perfectly fine machine.
    pub required: bool,
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
    // **A relative root is not a boundary.** Every engine resolves `root`
    // against the *cImp process's* working directory — which is cImp's own
    // install directory, never the caller's project. A relative root therefore
    // asks the sandbox to grant, map and confine the wrong tree entirely:
    // AppContainer stamps an inheritable read+WRITE ACE on cImp's install
    // directory and maps a drive letter to a name no NT lookup can serve
    // (`\??\.`), and Landlock's `root.exists()` check passes for the same wrong
    // directory and writes its rules against it.
    //
    // Live, rc.9: `POST /graph_run` with no `cwd` in the body defaults the
    // caller's working directory to `"."`, `run_graph_tool` falls back to that
    // cwd as the project root when no graph root is found, and `run_check` then
    // died with a bare `CreateProcessW failed (267)` — a Win32 error code for
    // what is really "cImp was told to sandbox a directory it cannot name".
    //
    // Checked HERE rather than in each engine because it is a property of the
    // request, not of the OS: one guard, before any ACL is stamped or any
    // kernel rule is written, and it degrades through the loud path every other
    // prerequisite failure already uses.
    if !root.is_absolute() {
        return Plan::Plain(SkipReason::Unavailable(format!(
            "the project root `{}` is not an absolute path, so there is nothing the sandbox can \
             grant or map (the calling session supplied no working directory)",
            root.display()
        )));
    }
    #[cfg(windows)]
    {
        match windows::prepare(cfg, seam, program, hints, root, env).await {
            Ok(prepared) => Plan::Sandboxed(prepared),
            Err(reason) => Plan::Plain(SkipReason::Unavailable(reason)),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match linux::prepare(cfg, seam, program, hints, root, env).await {
            Ok(prepared) => Plan::Sandboxed(prepared),
            Err(reason) => Plan::Plain(SkipReason::Unavailable(reason)),
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (seam, program, hints, root, env);
        Plan::Plain(SkipReason::Unavailable(
            "no OS sandbox engine on this platform yet — Windows uses AppContainer and Linux uses \
             Landlock; macOS has neither"
                .into(),
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
    record_skip_noting(seam, reason, subject, root, "");
}

/// [`record_skip`] plus a seam-supplied `note` appended to the row's detail.
///
/// Exists for the V33 Phase B tab seam, where "off" has TWO causes — the master
/// switch, or `sandbox.tabs` alone — and a row that says only "off (user
/// choice)" would leave the user hunting for which of two checkboxes they left
/// unticked. The note is a constant per seam, so it is deliberately NOT part of
/// the dedup key: the fact recorded is still "this seam runs unsandboxed", once.
pub fn record_skip_noting(
    seam: &str,
    reason: &SkipReason,
    subject: &str,
    root: &Path,
    note: &str,
) {
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
        SkipReason::OffUser => note.to_string(),
        SkipReason::Unavailable(r) if note.is_empty() => r.clone(),
        SkipReason::Unavailable(r) => format!("{r}\n{note}"),
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
/// `#[allow(dead_code)]` off Windows: the callers are the AppContainer engine
/// and, since V33 Phase D, the Landlock one — so the attribute is now only
/// there for the platform with neither (macOS), where an `allow` on a used item
/// costs nothing and a missing one would warn.
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
/// [`record_event`] carries and for the same reason: the non-test callers are
/// the two engines (AppContainer, and Landlock since V33 Phase D), so the
/// attribute now only covers the platform with neither.
#[cfg_attr(not(windows), allow(dead_code))]
const DENIAL_STDERR_TAIL_CHARS: usize = 500;

/// Substrings whose presence means the OS refused a **file or object** access.
/// Matched case-insensitively against a failed child's stderr.
///
/// `os error 5` is Rust's rendering of `ERROR_ACCESS_DENIED`; `Access is
/// denied` is what the Win32 tools print for the same thing; `Permission
/// denied` is the POSIX spelling, kept here because a cross-compiled or
/// MSYS-linked tool prints it on Windows too.
///
/// V33 Phase D adds the Linux spellings. **A Landlock denial is `EACCES`** —
/// for the filesystem and, since ABI 4, for a refused TCP `bind`/`connect` as
/// well — so `os error 13` and the bare `EACCES` token join `permission
/// denied`, which already covered the rendered form.
#[cfg_attr(not(windows), allow(dead_code))]
const FILESYSTEM_DENIAL_MARKERS: &[&str] = &[
    "os error 5",
    "access is denied",
    "permission denied",
    "os error 13",
    "eacces",
];

/// Substrings whose presence means the OS refused a **socket** operation.
/// `10013` is `WSAEACCES`; the "forbidden by its access permissions" phrasing
/// is the message Windows renders for it, which is exactly what an
/// AppContainer without `internetClient` produces on `connect()`.
///
/// **The Linux entries name the OPERATION, not the errno, and that is the whole
/// point.** Landlock refuses a scoped TCP `bind`/`connect` with `EACCES` — the
/// same errno a denied `open()` returns — so on Linux the number cannot tell a
/// socket denial from a file one. What can is the syscall the tool printed
/// beside it, which is why these are compound phrases and why
/// [`denial_signature`] checks this list FIRST.
///
/// `EPERM` (`os error 1`) is deliberately absent. It is Linux's most generic
/// refusal, it is what this crate's own `pre_exec` returns when it refuses to
/// exec an unconfined child, and claiming it as a socket denial would put a
/// confident wrong label on the one row a user needs to trust.
#[cfg_attr(not(windows), allow(dead_code))]
const SOCKET_DENIAL_MARKERS: &[&str] = &[
    "os error 10013",
    "wsaeacces",
    "forbidden by its access permissions",
    "connect: permission denied",
    "bind: permission denied",
    "socket: permission denied",
];

/// Substrings that mean a **program could not be started** — the shape a
/// confined *shell* dies in, as opposed to a confined tool being refused a file.
///
/// Measured on Windows, 2026-08-18 (rc.9 live-verify): a process running under
/// cImp's AppContainer **cannot create a child process at all**. `cmd.exe` runs
/// its builtins fine (`echo`, `cd`, `dir` and `type` all work, on the mapped
/// drive and off it) and every `CreateProcess` it attempts is refused —
/// including `C:\Windows\System32\where.exe`, which carries
/// `ALL APPLICATION PACKAGES:(RX)`, from a cwd of `System32`, with no drive
/// mapping involved. A plain `cmd.exe` in the same job object spawns the same
/// grandchild successfully, so neither the job, the grants, the drive mapping
/// nor PATH resolution is the cause.
///
/// The user therefore sees one of two messages, and neither says "sandbox":
///
/// * `'cargo' is not recognized as an internal or external command` — the PATH
///   *search* failing, because probing `C:\Users\<u>\.cargo\bin\cargo.exe`
///   traverses ancestors the container has no ACE on;
/// * `Access is denied.` — the search having succeeded and `CreateProcess`
///   being refused (already covered by [`FILESYSTEM_DENIAL_MARKERS`]).
///
/// The first one used to classify as nothing at all, so the lane stayed empty
/// while the check failed. It is listed here rather than folded into the
/// filesystem set because it is a *different fact* and deserves its own label.
#[cfg_attr(not(windows), allow(dead_code))]
const PROGRAM_START_DENIAL_MARKERS: &[&str] = &[
    "is not recognized as an internal or external command",
    "the system cannot execute the specified program",
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

/// Whether a name-resolution failure can be the boundary's fingerprint **on
/// this platform at all** — the second condition on [`NAME_RESOLUTION_MARKERS`],
/// and the one that is not a runtime flag.
///
/// * **Windows: yes.** An AppContainer without `internetClient` refuses the
///   resolver's socket, so DNS is where a network-touching tool dies first.
/// * **Linux: no.** Landlock scopes **TCP only**; UDP is untouched, so a
///   confined child with egress denied still resolves names perfectly well. A
///   resolver failure there is ordinary network weather, and labelling it a
///   boundary denial would be a claim the user cannot check and we cannot
///   support (V33 Phase D, decision D6's honesty rule).
///
/// A `const` rather than a `cfg` inside [`denial_signature`] so the *test* can
/// branch on the same fact the function does, and stays truthful on both
/// platforms instead of being disabled on one.
#[cfg_attr(not(windows), allow(dead_code))]
const NAME_RESOLUTION_IS_A_BOUNDARY_SIGNAL: bool = cfg!(windows);

/// Classify one failed child's output: does it *look like* the sandbox
/// boundary refused something?
///
/// Pure and (almost) cross-platform on purpose — the engines are
/// platform-specific, but the judgement they feed is plain string work, so it
/// is testable and reviewable on any machine, and Landlock's denials are
/// classified by this same function rather than by a second copy. The one
/// platform-dependent term is
/// [`NAME_RESOLUTION_IS_A_BOUNDARY_SIGNAL`], which is a fact about the
/// mechanism rather than a preference — see it for why.
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
/// The *platform* is the second condition and it is not a flag:
/// [`NAME_RESOLUTION_IS_A_BOUNDARY_SIGNAL`] is false on Linux, where Landlock
/// scopes TCP only and DNS therefore keeps working inside the boundary.
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
    // SOCKET first, FILESYSTEM second — the socket list is the more SPECIFIC
    // one (its Linux entries name a syscall, e.g. `connect: Permission
    // denied`), and on Linux both denials share the `EACCES` errno, so a
    // filesystem-first order would swallow every network denial into the wrong
    // class. The two Windows sets are disjoint, so the order costs nothing
    // there — `filesystem_and_socket_markers_are_unconditional` pins that.
    if hit(SOCKET_DENIAL_MARKERS) {
        return Some("socket access denied");
    }
    if hit(FILESYSTEM_DENIAL_MARKERS) {
        return Some("filesystem/OS access denied");
    }
    // Checked AFTER the two access sets, so every string that classified before
    // this list existed still classifies exactly as it did.
    if hit(PROGRAM_START_DENIAL_MARKERS) {
        return Some("a program could not be started");
    }
    if NAME_RESOLUTION_IS_A_BOUNDARY_SIGNAL && !allow_network && hit(NAME_RESOLUTION_MARKERS) {
        return Some("name resolution failed (no network capability)");
    }
    None
}

/// The capability posture a sandboxed child ran under, rendered for a row's
/// detail. Both new row types carry it: a denial is only interpretable next to
/// what the boundary was actually configured to allow.
///
/// The first clause is what the USER asked for and reads the same everywhere.
/// On Linux a second clause states what the KERNEL is actually enforcing
/// ([`linux::posture_note`]) — the ABI, and which of the two network holes
/// applies — because `network=off` on a Landlock box means "TCP is scoped, UDP
/// is not" or, below ABI 4, "nothing is scoped", and a posture line that stops
/// at `off` would be promising confinement the kernel is not providing.
#[cfg_attr(not(windows), allow(dead_code))]
fn posture(cfg: &SandboxCfg) -> String {
    format!(
        "network={}, extra grants={}{}",
        if cfg.allow_network { "on" } else { "off" },
        cfg.extra_grant_dirs.len(),
        engine_posture(cfg)
    )
}

/// The engine-specific half of [`posture`], or the empty string where the
/// engine has nothing to add beyond what the user configured.
///
/// Empty on Windows on purpose: an AppContainer's `internetClient` capability
/// is all-or-nothing and `network=on/off` says the whole truth about it. Linux
/// is the platform where it does not — see [`linux::posture_note`].
#[cfg_attr(not(windows), allow(dead_code))]
fn engine_posture(cfg: &SandboxCfg) -> String {
    #[cfg(target_os = "linux")]
    {
        linux::posture_note(cfg.allow_network)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cfg;
        String::new()
    }
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

/// Record a sandboxed spawn that produced **no child at all** — the one
/// funnel all three seams route their `Err` from the spawn engine through.
///
/// # Why this exists
///
/// Each seam used to classify the engine's error itself and mint a `denied`
/// row only when [`denial_signature`] matched. An error it could not classify
/// — rc.9's `CreateProcessW failed (267)` is exactly that shape, and so is
/// every future unattributable Win32/`libc` code — minted **nothing**, so the
/// sandbox lane's silence meant two different things again: "no sandboxed
/// spawn failed" or "one failed in a way nobody taught the classifier". The
/// failure was visible only inside the calling tool's own result text, which
/// is precisely where a user auditing the boundary is not looking.
///
/// So an unclassified refusal now mints a `refused` row. It deliberately does
/// NOT claim the boundary denied anything — it asserts only the fact cImp can
/// actually observe: the child never started, and this is the error the OS
/// gave. A classified one still goes to [`record_denial`], unchanged.
///
/// Every occurrence is recorded, for [`record_denial`]'s reason: a spawn
/// refused again and again IS the signal, and dedup would delete it.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn record_spawn_failure(
    seam: &str,
    root: &Path,
    subject: &str,
    args: &[String],
    err: &str,
    cfg: &SandboxCfg,
) {
    if let Some(class) = denial_signature(None, err, cfg.allow_network) {
        record_denial(seam, root, subject, args, None, err, class, cfg);
        return;
    }
    record_event(
        seam,
        root,
        "refused",
        state_target("refused", subject),
        refused_detail(subject, args, err, cfg),
        false,
    );
}

/// Record a sandboxed child that **ran, failed, and said nothing at all** —
/// no stdout, no stderr, just a non-zero exit code.
///
/// # Why this is its own row
///
/// [`denial_signature`] classifies a failure by what the child *printed*. A
/// child that prints nothing is unclassifiable by construction, so the lane
/// stayed silent for the one failure shape that is most likely to be the
/// boundary: a program the container cannot fully load exits without ever
/// reaching its own error handling.
///
/// Live rc.9, `audit:semgrep`: `semgrep.exe` (a pip console-script launcher)
/// was granted its own `Scripts` directory but not the Python install root it
/// loads `python3XX.dll` and the standard library from. It exited **1 with both
/// streams empty**. The audit adapter reads exit 1 as "findings present", so the
/// scan reported "the SARIF report was empty — findings were lost" while the
/// sandbox lane said nothing whatsoever. Granting the interpreter root
/// ([`interpreter_root`]) fixes that particular tool; this row is what makes the
/// *shape* visible the next time some other tool hits it.
///
/// The row asserts only the observable and names the boundary as a candidate,
/// never as a finding — the same posture as [`record_denial`].
#[cfg_attr(not(windows), allow(dead_code))]
pub fn record_silent_exit(
    seam: &str,
    root: &Path,
    subject: &str,
    args: &[String],
    exit_code: Option<i32>,
    cfg: &SandboxCfg,
) {
    record_event(
        seam,
        root,
        "silent",
        state_target("no output", subject),
        silent_exit_detail(subject, args, exit_code, cfg),
        false,
    );
}

/// [`record_silent_exit`]'s wording, pure so the row can be asserted directly.
#[cfg_attr(not(windows), allow(dead_code))]
fn silent_exit_detail(
    subject: &str,
    args: &[String],
    exit_code: Option<i32>,
    cfg: &SandboxCfg,
) -> String {
    format!(
        "`{}` exited {} and produced NOTHING on either stream — no output, no error text. \
         A tool that cannot finish loading (a runtime or interpreter directory the sandbox does \
         not grant) exits exactly like this, and it leaves no message for the classifier to read, \
         so this row records the shape rather than a cause. Posture: {}.",
        summarize_invocation(subject, args),
        exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "with no code".into()),
        posture(cfg),
    )
}

/// Whether a sandboxed **shell's** output carries the fingerprint of the
/// AppContainer's no-child-processes rule, and the note to hand the user if so.
///
/// See [`PROGRAM_START_DENIAL_MARKERS`] for the measurement. The note exists
/// because the two messages a user actually sees — `'cargo' is not recognized`
/// and `Access is denied.` — both point them at PATH or at file permissions,
/// and neither is the problem: no external program can run inside this boundary
/// at all, so a check that shells out cannot pass while it is on. Stating that
/// is worth more than any amount of retrying.
///
/// Returns `None` for anything else, so a check that genuinely failed on its own
/// terms is never handed an explanation it did not earn.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn sandboxed_shell_note(exit_code: Option<i32>, stderr: &str) -> Option<&'static str> {
    if exit_code == Some(0) {
        return None;
    }
    let hay = stderr.to_ascii_lowercase();
    let hit = PROGRAM_START_DENIAL_MARKERS
        .iter()
        .chain(FILESYSTEM_DENIAL_MARKERS.iter())
        .any(|m| hay.contains(m));
    hit.then_some(
        "\n[sandbox: this check ran inside the OS sandbox, where a shell cannot start ANY child \
         process — on Windows an AppContainer is refused CreateProcess for every image, including \
         ones it has read+execute on. If this check invokes an external tool (cargo, tsc, npm, a \
         linter), that is the most likely reason it failed, and no grant will change it: turn the \
         sandbox off for this run, or run the check from a tab.]",
    )
}

/// [`record_spawn_failure`]'s `refused` wording, as a pure function so the row
/// it writes can be asserted without an activity store.
///
/// `err` is **cImp's own** error string (the engine's, e.g. `CreateProcessW
/// failed (267)`), not a child's output — nothing ran, so there is no child
/// output to screen. It is still bounded, because an engine error can carry a
/// path and this lane is not the place to grow unbounded rows.
#[cfg_attr(not(windows), allow(dead_code))]
fn refused_detail(subject: &str, args: &[String], err: &str, cfg: &SandboxCfg) -> String {
    format!(
        "`{}` never started: {} — the sandboxed spawn was refused with an error that matches no \
         access-denial signature, so this row asserts only that NO child ran; whether the \
         boundary is the cause is not something cImp can tell from this. Posture: {}.",
        summarize_invocation(subject, args),
        truncate_chars(err.trim(), DENIAL_STDERR_TAIL_CHARS),
        posture(cfg),
    )
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
            #[cfg(any(windows, target_os = "linux"))]
            Plan::Sandboxed(_) => panic!("a disabled switch still sandboxed the spawn"),
        }
    }

    /// **The rc.9 defect at its widest point.** A root that is not absolute
    /// resolves against cImp's OWN working directory, so every engine would
    /// build its boundary around cImp's install directory: an inheritable
    /// read+write ACE for the container on Windows, Landlock rules on the wrong
    /// tree on Linux, and a drive letter mapped to `\??\.` whose only symptom
    /// was `CreateProcessW failed (267)`.
    ///
    /// It must therefore be refused HERE — before any engine runs, so nothing
    /// is stamped or mapped — and refused as `Unavailable` (a broken
    /// prerequisite), never as `OffUser` (a deliberate setting). The test runs
    /// with the switch ON, on every platform, and touches nothing: no engine is
    /// reached.
    #[test]
    fn a_relative_project_root_is_never_sandboxed() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let cfg = SandboxCfg {
            enabled: true,
            allow_network: false,
            extra_grant_dirs: Vec::new(),
        };
        // `"."` is the live shape (a `/graph_run` body with no `cwd`); `""` is
        // the same mistake spelled as an absent value.
        for root in [".", "", "src-tauri"] {
            let plan = rt.block_on(plan(
                &cfg,
                SEAM_RUN_CHECK,
                Path::new("C:/x/cmd.exe"),
                &GrantHints::default(),
                Path::new(root),
                &[],
            ));
            match plan {
                Plan::Plain(SkipReason::Unavailable(r)) => assert!(
                    r.contains("not an absolute path"),
                    "the reason must name the real problem: {r}"
                ),
                Plan::Plain(SkipReason::OffUser) => {
                    panic!("a broken root was reported as the user's choice (root {root:?})")
                }
                #[cfg(any(windows, target_os = "linux"))]
                Plan::Sandboxed(_) => {
                    panic!("a relative root {root:?} was accepted as a sandbox boundary")
                }
            }
        }
    }

    /// A spawn error the classifier does not recognize must still leave a row.
    ///
    /// `CreateProcessW failed (267)` — the live rc.9 error — matches no denial
    /// marker, so before [`record_spawn_failure`] it minted nothing at all and
    /// the failure existed only inside the calling tool's own result text. The
    /// two branches are asserted through the pure wording function plus the
    /// classifier, so no activity store is needed.
    #[test]
    fn an_unclassifiable_spawn_error_still_says_no_child_ran() {
        let cfg = SandboxCfg::disabled();
        // The routing premise: 267 is genuinely unclassifiable, so it takes the
        // `refused` branch rather than being mislabeled a denial.
        assert_eq!(
            denial_signature(None, "CreateProcessW failed (267)", false),
            None,
            "if this ever classifies, the `refused` branch is no longer the one 267 takes"
        );
        let detail = refused_detail(
            "cmd.exe",
            &["cargo check".to_string()],
            "CreateProcessW failed (267)",
            &cfg,
        );
        assert!(detail.contains("never started"), "{detail}");
        assert!(detail.contains("267"), "{detail}");
        assert!(detail.contains("NO child ran"), "{detail}");
        // …and it must NOT claim the boundary denied anything.
        assert!(
            !detail.contains("access-denial signature ("),
            "a refusal must not borrow the denial row's claim: {detail}"
        );
        // A classifiable spawn error still goes the denial route.
        assert_eq!(
            denial_signature(None, "CreateProcessW failed: Access is denied.", false),
            Some("filesystem/OS access denied")
        );
    }

    /// **The `Scripts` convention, and its two guards.** A pip console-script
    /// launcher cannot initialize without the install root beside it (rc.9:
    /// `semgrep.exe` exited 1 with both streams empty), so the parent of a
    /// `Scripts` directory is granted too — and ONLY there, because the general
    /// "grant the grandparent" rule would hand out `C:\Windows` for anything in
    /// `System32`.
    ///
    /// Forward slashes throughout: `Path` treats `/` as a separator on BOTH
    /// platforms, while a backslash is an ordinary character on Linux — a
    /// `C:\…` fixture would make `file_name()` answer the whole string on the
    /// Linux CI runner and this test would pass locally and fail there. (The
    /// same trap `the_first_token_of_a_check_command_is_what_gets_a_grant`
    /// documents.)
    #[test]
    fn the_interpreter_root_rule_is_the_scripts_convention_and_nothing_wider() {
        // The live shape, and the case-insensitivity Windows paths need.
        assert_eq!(
            interpreter_root(Path::new(
                "C:/Users/me/AppData/Local/Python/pythoncore-3.14-64/Scripts"
            )),
            Some(Path::new("C:/Users/me/AppData/Local/Python/pythoncore-3.14-64"))
        );
        assert_eq!(
            interpreter_root(Path::new("C:/py/venv/scripts")),
            Some(Path::new("C:/py/venv"))
        );
        // Everything else yields nothing — most importantly the directories a
        // grandparent rule would over-grant.
        for narrow in [
            "C:/Windows/System32",
            "C:/Users/me/.cargo/bin",
            "C:/Program Files/Git/cmd",
            "/usr/bin",
        ] {
            assert_eq!(
                interpreter_root(Path::new(narrow)),
                None,
                "{narrow} must not widen the boundary"
            );
        }
        // A `Scripts` directory sitting at a volume root would yield the volume
        // itself; the answer there is no grant, not the whole drive. Spelled
        // per-platform, because "the root" is the one path shape that cannot be
        // written portably.
        #[cfg(windows)]
        assert_eq!(interpreter_root(Path::new(r"C:\Scripts")), None);
        #[cfg(not(windows))]
        assert_eq!(interpreter_root(Path::new("/Scripts")), None);
    }

    /// **The two rc.9 lane silences, both closed.**
    ///
    /// A sandboxed child that fails says one of three things, and until now only
    /// the first one produced a row: output the classifier recognizes, output it
    /// does not, or *nothing at all*. The last is the most dangerous, because it
    /// is what a tool that cannot finish loading looks like.
    #[test]
    fn a_child_that_fails_silently_still_produces_a_row() {
        let cfg = SandboxCfg::disabled();
        // Nothing to classify — by construction, an empty stderr matches no
        // marker, which is exactly why this needed its own row.
        assert_eq!(denial_signature(Some(1), "", false), None);
        let detail = silent_exit_detail("semgrep.exe", &["scan".into()], Some(1), &cfg);
        assert!(detail.contains("produced NOTHING on either stream"), "{detail}");
        assert!(detail.contains("interpreter"), "{detail}");
        // It records a shape, never a verdict.
        assert!(
            !detail.contains("denied"),
            "a silent exit is not evidence of a denial: {detail}"
        );
        // A child with no exit code at all is still describable.
        assert!(silent_exit_detail("x", &[], None, &cfg).contains("with no code"));
    }

    /// `'cargo' is not recognized as an internal or external command` is what a
    /// sandboxed shell prints when its PATH search cannot reach the tool, and it
    /// classified as **nothing** — so a check that could not start its compiler
    /// left the sandbox lane empty. It is its own class, not the filesystem one:
    /// the fact is "no program started", which is a different thing to tell a
    /// user than "a file was refused".
    #[test]
    fn a_program_that_never_started_classifies_as_its_own_denial_shape() {
        assert_eq!(
            denial_signature(
                Some(1),
                "'cargo' is not recognized as an internal or external command,\r\noperable \
                 program or batch file.",
                false
            ),
            Some("a program could not be started")
        );
        // The access sets keep their own labels — this list is checked after
        // them, so nothing that classified before reclassifies now.
        assert_eq!(
            denial_signature(Some(1), "Access is denied.", false),
            Some("filesystem/OS access denied")
        );
        // And a clean exit still classifies as nothing, whatever it printed.
        assert_eq!(
            denial_signature(Some(0), "'cargo' is not recognized as an internal", false),
            None
        );
    }

    /// **The measured AppContainer limit, stated where the user reads.** A
    /// confined `cmd.exe` cannot start ANY child process — measured against
    /// `System32\where.exe`, which the container has read+execute on, from a
    /// `System32` cwd, with no drive mapping in play; a plain `cmd.exe` in the
    /// same job object runs the same grandchild fine. Both messages the user
    /// actually sees point at PATH or at file permissions instead, so the note
    /// exists to say what no amount of grant-tuning will change.
    #[test]
    fn a_sandboxed_shell_gets_told_why_it_cannot_start_a_program() {
        for stderr in [
            "'cargo' is not recognized as an internal or external command,",
            "Access is denied.",
        ] {
            let note = sandboxed_shell_note(Some(1), stderr)
                .unwrap_or_else(|| panic!("no note for {stderr:?}"));
            assert!(note.contains("cannot start ANY child process"), "{note}");
            assert!(note.contains("no grant will change it"), "{note}");
        }
        // A check that failed on its own terms is not handed an excuse.
        assert_eq!(
            sandboxed_shell_note(Some(1), "error[E0425]: cannot find value `x` in this scope"),
            None
        );
        // Nor is a successful one, whatever it printed along the way.
        assert_eq!(sandboxed_shell_note(Some(0), "Access is denied."), None);
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
            // V33 Phase D — the Linux spellings. A Landlock filesystem denial
            // is EACCES, which tools render either way round.
            ("open /home/me/.ssh/id_ed25519: os error 13", "filesystem/OS access denied"),
            ("cat: /etc/shadow: EACCES", "filesystem/OS access denied"),
            // …and a denied TCP operation is EACCES *too*, so what makes it a
            // socket row is the syscall the tool named beside it. This is the
            // pair the marker ORDER exists for: `connect: Permission denied`
            // matches BOTH lists, and the socket class is the true one.
            ("curl: (7) Failed to connect: connect: Permission denied", "socket access denied"),
            ("bind: Permission denied", "socket access denied"),
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

    /// The honesty rule, as a test, in its two dimensions.
    ///
    /// 1. **The flag.** Name-resolution failures are the AppContainer's usual
    ///    death shape ONLY when egress was withheld; with `allow_network =
    ///    true` the same strings are ordinary network errors.
    /// 2. **The platform.** On Linux they are never the boundary's fingerprint,
    ///    because Landlock scopes TCP and DNS is UDP — so the expectation below
    ///    is read from the same constant the classifier reads, and this test
    ///    asserts the truth on both platforms rather than being switched off on
    ///    one.
    #[test]
    fn name_resolution_is_a_denial_only_when_the_platform_and_the_flag_both_allow_it() {
        let expected = NAME_RESOLUTION_IS_A_BOUNDARY_SIGNAL
            .then_some("name resolution failed (no network capability)");
        for stderr in [
            "fatal: Could not resolve host: github.com",
            "getaddrinfo ENOTFOUND registry.npmjs.org",
            "Temporary failure in name resolution",
            "CURLE_COULDNT_RESOLVE_HOST (6)",
        ] {
            assert_eq!(
                denial_signature(Some(128), stderr, false),
                expected,
                "{stderr:?} with the network capability off"
            );
            assert_eq!(
                denial_signature(Some(128), stderr, true),
                None,
                "{stderr:?} must NOT be called a boundary denial when egress was granted"
            );
        }
        // The constant is not a free parameter: it must match the platform's
        // actual mechanism, or the rule above is enforcing nothing.
        assert_eq!(NAME_RESOLUTION_IS_A_BOUNDARY_SIGNAL, cfg!(windows));
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
        // `starts_with`, not equality: on Linux the engine appends what the
        // KERNEL is enforcing (see `posture`), and pinning the exact string
        // would either forbid that clause or make this test platform-specific.
        assert!(
            posture(&cfg).starts_with("network=off, extra grants=0"),
            "{}",
            posture(&cfg)
        );
        let cfg = SandboxCfg {
            enabled: true,
            allow_network: true,
            extra_grant_dirs: vec![PathBuf::from("C:/tools")],
        };
        assert!(
            posture(&cfg).starts_with("network=on, extra grants=1"),
            "{}",
            posture(&cfg)
        );
        // …and where there IS an engine-specific clause, it must name the
        // engine — a posture nobody can attribute is a posture nobody trusts.
        #[cfg(target_os = "linux")]
        assert!(
            posture(&cfg).to_ascii_lowercase().contains("landlock"),
            "{}",
            posture(&cfg)
        );
    }

    /// On a platform with no engine, the reason must NAME the missing thing —
    /// decision 5's "loud, never silent" applied to the string a user reads in
    /// the Events row.
    ///
    /// Since V33 Phase D that platform is macOS only; Linux has an engine and
    /// its own coverage in [`linux`].
    #[cfg(not(any(windows, target_os = "linux")))]
    #[test]
    fn a_platform_with_no_engine_says_what_is_missing() {
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

    /// V33 Phase D — on Linux an enabled switch must reach the Landlock engine:
    /// either it prepares a boundary, or it says why it could not, naming
    /// Landlock. What it may never be is `OffUser` (the switch is on) or a
    /// silent success.
    ///
    /// The kernel is not a given — a container without Landlock is a normal
    /// place for this to run — so the unavailable arm is a PASS with a loud
    /// note rather than a failure. What is asserted either way is that the two
    /// states stay distinguishable (C10).
    #[cfg(target_os = "linux")]
    #[test]
    fn an_enabled_switch_on_linux_reaches_the_landlock_engine() {
        let root = std::env::temp_dir();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let cfg = SandboxCfg {
            enabled: true,
            allow_network: false,
            extra_grant_dirs: Vec::new(),
        };
        let plan = rt.block_on(plan(
            &cfg,
            SEAM_RUN_COMMAND,
            Path::new("/bin/sh"),
            &GrantHints::default(),
            &root,
            &[],
        ));
        match plan {
            Plan::Sandboxed(prepared) => {
                // A boundary with no grants would confine the child out of its
                // own project; the root is always the first rule.
                assert!(
                    !prepared.grants.is_empty(),
                    "a prepared Landlock boundary must grant at least the root"
                );
            }
            Plan::Plain(SkipReason::Unavailable(r)) => {
                // Skip or fail, decided by `CIMP_EXPECT_LANDLOCK` — the same
                // policy the engine's own live tests use, reached through the
                // same function so the two cannot disagree about what a
                // kernel-less run means.
                assert!(!r.is_empty(), "an unavailable engine must say why");
                linux::skip_or_fail("an_enabled_switch_on_linux_reaches_the_landlock_engine", &r);
            }
            Plan::Plain(SkipReason::OffUser) => {
                panic!("an ENABLED switch was reported as the user's choice to be off")
            }
        }
    }
}

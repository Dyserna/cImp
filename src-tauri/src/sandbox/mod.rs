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

/// The runtime-profile table and the inference over it (R17, V42).
mod runtime;

/// The activity rows this layer writes (R17, V42).
mod events;

// R17 (V42): `sandbox/mod.rs` was 4 113 lines of three unrelated concerns —
// the sandbox MODEL (this file: the plan, the grant screen, the two engines'
// shared vocabulary), the runtime-profile table and its inference
// (`runtime.rs`, ~965 lines), and the activity rows the layer writes
// (`events.rs`, ~1 020). The two were lifted out with no logic change and are
// re-exported here, so `crate::sandbox::X` still names all three files' items
// and no caller had to learn a new path. This module is TCB-adjacent; the split
// was pure motion on purpose, and the one source scanner that reads a moved
// function by file (`the_denial_path_has_no_dedup_key`) was re-pointed in the
// same commit.
pub use events::*;
pub use runtime::*;


/// One shape of directory that `sandbox.extra_grant_dirs` is **not** allowed to
/// open, identified by the components it ends in, with the reason the user's
/// refusal row carries.
///
/// Data in code with a reason per row, the shape [`GrantRow`] and
/// [`child_env::CHILD_ENV`] use: the reviewer of a diff that adds a row sees
/// the pattern and the justification together, and a row with no reason does
/// not compile.
pub(crate) struct GrantRefusalRule {
    /// The TRAILING path components that identify the directory. Compared
    /// lowercased on both platforms — a broader match than Linux's
    /// case-sensitive filesystem strictly needs, which is the safe direction
    /// for a deny rule.
    pub(crate) suffix: &'static [&'static str],
    pub(crate) why: &'static str,
}

/// The credential stores no reviewed grant row may name.
///
/// Deliberately short and literal. This is not an attempt to enumerate every
/// secret on a machine — the structural rules in [`extra_grant_refusal`] (a
/// volume root, a user-profile root, the Windows install directory) are what
/// stop the wholesale cases, and these rows cover the specific directories a
/// plausible-looking "just grant my toolchain" entry would otherwise reach.
// `pub(crate)` so `plugins::spec` can pin it against docs/TOOL-PLUGINS.md.
pub(crate) const GRANT_REFUSAL_RULES: &[GrantRefusalRule] = &[
    GrantRefusalRule {
        suffix: &[".ssh"],
        why: "an SSH key store — private keys, agent config and known-hosts",
    },
    GrantRefusalRule {
        suffix: &[".aws"],
        why: "AWS long-lived credentials and cached session tokens",
    },
    GrantRefusalRule {
        suffix: &[".gnupg"],
        why: "a GnuPG private keyring",
    },
    GrantRefusalRule {
        suffix: &[".config", "gh"],
        why: "the GitHub CLI's OAuth token store",
    },
    GrantRefusalRule {
        suffix: &["microsoft", "credentials"],
        why: "the Windows credential store (AppData\\Roaming\\Microsoft\\Credentials)",
    },
    GrantRefusalRule {
        suffix: &["microsoft", "protect"],
        why: "the DPAPI master keys that decrypt the Windows credential store",
    },
    GrantRefusalRule {
        suffix: &["microsoft", "vault"],
        why: "the Windows Vault credential store",
    },
];

/// Why a `sandbox.extra_grant_dirs` row must NOT be granted — `None` means it
/// is fine to grant.
///
/// # Why a settings row needs screening at all
///
/// This is the second of the two independent V33 mitigations for
/// *"the settings file that configures the boundary lives inside the
/// boundary"* (2026-08-18). The first is
/// `settings::persistence::OVERLAY_BANNED_KEYS`, which stops a project overlay
/// carrying a `sandbox` block at all. This one is what still holds if the
/// **global** settings file is the thing that goes wrong: whatever names the
/// row, cImp runs as the user and `grant_dir` stamps a **durable, inheritable**
/// ACE — so `extra_grant_dirs: ["C:\\Users\\<u>\\.ssh"]` would not merely let
/// one child read the keys, it would leave the container able to read them
/// after cImp exits. Neither mitigation covers the other's case, which is why
/// both exist.
///
/// # The rules
///
/// Structural first, then the [`GRANT_REFUSAL_RULES`] table:
///
/// 1. a **rootless (relative)** path — it would resolve against cImp's own
///    working directory, which is not a boundary anyone reviewed. `has_root`
///    rather than `is_absolute` on purpose: a POSIX-rooted row is a legitimate
///    Linux grant, and `is_absolute` calls it relative on Windows, which would
///    make this rule platform-dependent for no security gain;
/// 2. a **volume / filesystem root** — everything on the machine is beneath it;
/// 3. a **user-profile root or an ancestor of one** (`C:\Users\<u>`,
///    `C:\Users`, `/home`) — it contains every credential store there is, so
///    the table below would be decoration;
/// 4. the **Windows install directory** or anything under it — already readable
///    inside the container (`ALL APPLICATION PACKAGES` covers `System32`), so a
///    grant buys nothing and an ACE on the OS is a durable machine change;
/// 5. a directory whose trailing components match [`GRANT_REFUSAL_RULES`].
///
/// `home` and `system_root` are parameters rather than direct `env` reads for
/// the same reason [`child_env::minimal_env`]'s lookup is: the tests drive a
/// synthetic machine, on either platform, without touching the process's own
/// environment. [`extra_grant_refusal_live`] is the production wrapper.
///
/// **A refusal is not a failure.** The engines skip the row, record it, and
/// carry on with the rest — a bad settings row must not brick the sandbox, and
/// refusing to run *unsandboxed* over one is the wrong direction too.
pub fn extra_grant_refusal(
    path: &Path,
    home: Option<&Path>,
    system_root: Option<&Path>,
) -> Option<&'static str> {
    if path.as_os_str().is_empty() || !path.has_root() {
        return Some(
            "not a rooted path — a relative grant row resolves against cImp's own working \
             directory, which is not a reviewed boundary",
        );
    }
    let comps = lower_components(path);
    if comps.is_empty() || path.parent().is_none() {
        return Some(
            "a volume/filesystem root — everything on the machine is beneath it, so this is not \
             a grant, it is switching the sandbox off",
        );
    }
    if let Some(home) = home {
        // `path` is the profile root itself, or an ancestor of it.
        if starts_with(&lower_components(home), &comps) {
            return Some(
                "a user-profile root (or an ancestor of one) — it contains every credential \
                 store on the machine",
            );
        }
    }
    if let Some(system_root) = system_root {
        // `path` is the Windows directory itself, or something under it.
        if starts_with(&comps, &lower_components(system_root)) {
            return Some(
                "the Windows install directory — already readable inside the container, and an \
                 ACE stamped there is a durable change to the OS",
            );
        }
    }
    GRANT_REFUSAL_RULES
        .iter()
        .find(|rule| ends_with(&comps, rule.suffix))
        .map(|rule| rule.why)
}

/// [`extra_grant_refusal`] against the machine cImp is running on.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn extra_grant_refusal_live(path: &Path) -> Option<&'static str> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    let system_root = std::env::var_os("SystemRoot").map(PathBuf::from);
    extra_grant_refusal(path, home.as_deref(), system_root.as_deref())
}

/// A path's components, lowercased — the comparison unit for the rules above.
pub(super) fn lower_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect()
}

/// Is `prefix` the leading run of `comps` (component-wise, so `C:\Users\amirx`
/// does not "start with" `C:\Users\amir`)?
pub(super) fn starts_with(comps: &[String], prefix: &[String]) -> bool {
    !prefix.is_empty() && comps.len() >= prefix.len() && comps[..prefix.len()] == *prefix
}

/// Does `comps` END in `suffix` (already lowercase, component-wise)?
pub(super) fn ends_with(comps: &[String], suffix: &[&str]) -> bool {
    comps.len() >= suffix.len()
        && comps[comps.len() - suffix.len()..]
            .iter()
            .zip(suffix)
            .all(|(a, b)| a == b)
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
    /// V38: which runtime profile the SPAWNED PROGRAM's grants come from.
    ///
    /// Applies to `program` only, never to [`programs`](Self::programs): those
    /// are paths cImp inferred from a command line and nobody declared anything
    /// about them, so they keep inference. Defaults to [`RuntimeSelect::Infer`],
    /// which is what every pre-V38 seam does.
    pub runtime: RuntimeSelect,
}

/// Which [`RUNTIME_PROFILES`] rows a spawn's grants come from.
///
/// The runtime half of V38's manifest sandbox fields, as a type the sandbox
/// layer owns: a seam that has nothing to declare passes the default and keeps
/// today's inference exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RuntimeSelect {
    /// Infer from the resolved program — V33's behaviour, and every built-in
    /// seam's.
    #[default]
    Infer,
    /// A named profile, from a table cImp owns. Inference still runs as a
    /// CROSS-CHECK at the calling seam (see [`inferred_runtime_ids`]); it does
    /// not decide.
    Profile(&'static str),
    /// "This is a single static binary": its own directory is the whole grant,
    /// and no profile applies even if one would have detected.
    None,
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
            &fxp("C:/x/y.exe"),
            &GrantHints::default(),
            &fxp("C:/proj"),
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

    /// **The Events lane is prose, and prose is a contract too** (Phase C
    /// review, B-C3). Three V38 row texts shipped with fourteen-space runs
    /// mid-clause, because a single-line `format!` string literal indented to
    /// match its call site keeps every one of those spaces. Nothing read them
    /// back, so nothing noticed.
    ///
    /// This reads all four bodies and asserts they are sentences: no run of
    /// three or more spaces, no embedded newline, and a real ending. It is the
    /// consumer that makes the wording a checked signal rather than a hope.
    #[test]
    fn row_texts_read_as_sentences() {
        let p = Path::new("C:\\Users\\x\\.ssh");
        let bodies = [
            runtime_mismatch_body("python", "node, rust"),
            declared_unsandboxed_body("acme@1.0.0/scan"),
            sandbox_required_refusal_body("acme@1.0.0/scan", "the sandbox is off"),
            grant_refused_body(GrantSource::Settings, p, "an SSH key store"),
            grant_refused_body(GrantSource::Manifest, p, "an SSH key store"),
        ];
        for body in &bodies {
            assert!(
                !body.contains("   "),
                "a user-visible row must not carry a run of spaces: {body}"
            );
            assert!(!body.contains('\n'), "one row, one line: {body}");
            assert!(body.ends_with('.'), "a row text is a sentence: {body}");
        }
    }

    /// The two grant sources say DIFFERENT things about where to go and fix it.
    /// A manifest-sourced refusal that named `sandbox.extra_grant_dirs` sent the
    /// reader hunting for a settings entry that does not exist (B-C1).
    #[test]
    fn a_manifest_grant_refusal_does_not_blame_settings() {
        let p = Path::new("C:\\Users\\x\\.aws");
        let from_manifest = grant_refused_body(GrantSource::Manifest, p, "AWS credentials");
        assert!(
            !from_manifest.contains("is listed in sandbox.extra_grant_dirs"),
            "{from_manifest}"
        );
        assert!(from_manifest.contains("extra_grants"), "{from_manifest}");
        assert!(from_manifest.contains("Tool Plugins"), "{from_manifest}");

        // …and the settings wording is unchanged, which is what keeps the
        // pre-V38 row identical for the population that always had it.
        let from_settings = grant_refused_body(GrantSource::Settings, p, "AWS credentials");
        assert!(
            from_settings.contains("is listed in sandbox.extra_grant_dirs"),
            "{from_settings}"
        );
        assert!(!from_settings.contains("Tool Plugins"), "{from_settings}");
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
            &fxp("C:/x/git.exe"),
            &GrantHints::default(),
            &fxp("C:/proj"),
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
                &fxp("C:/x/cmd.exe"),
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

    // ── the runtime table's fixtures ────────────────────────────────────────
    //
    // Every runtime test drives a SYNTHETIC machine: a fixed environment and a
    // fixed set of directories that exist. Nothing here reads the filesystem or
    // the process environment — the first would make the result depend on which
    // toolchains the runner happens to have installed, the second is a
    // process-wide mutation under a 32-thread suite.
    //
    // Forward slashes throughout: `Path` treats `/` as a separator on BOTH
    // platforms, while a backslash is an ordinary character on Linux — a `C:\…`
    // fixture would make `file_name()` answer the whole string on the Linux CI
    // runner and these tests would pass locally and fail there. The `C:\`-shaped
    // cases live in their own `cfg(windows)` test.
    //
    // Forward slashes are not enough on their own, though: the DRIVE PREFIX is
    // what roots a path, and `C:/x` has no root on Linux, so every derived
    // grant would be refused there and the assertions would be about nothing.
    // Every fixture path therefore goes through [`fx`] (and back through
    // [`unfx`] where an assertion compares one) — see its doc comment.

    /// A drive-lettered fixture path, given a root the RUNNING OS recognizes.
    ///
    /// The rows below encode Windows conventions (`…\.cargo`, `…\Scripts`, a
    /// JDK on its own volume) and the fixtures are written that way on
    /// purpose — but every path the table derives passes
    /// [`extra_grant_refusal`], whose first rule refuses a path with no root,
    /// and `C:/x` has NO root on Linux (there is no drive prefix there; it is
    /// just a directory called `C:`). Without this the whole runtime table
    /// would silently produce zero grants on the Linux runner and the
    /// assertions would be about nothing.
    ///
    /// So: identity on Windows (the drive letter is the real shape there), and
    /// on Linux the WSL spelling — `C:/Users/me` → `/c/Users/me`. Same
    /// components, same rules fire, same assertions, both platforms.
    /// [`unfx`] is the inverse, applied where an assertion compares against
    /// the Windows spelling.
    fn fx(p: &str) -> String {
        let b = p.as_bytes();
        if cfg!(windows)
            || !(b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        {
            return p.to_string();
        }
        format!("/{}/{}", (b[0] as char).to_ascii_lowercase(), p[2..].trim_start_matches(['/', '\\']))
    }

    /// [`fx`] as a `PathBuf`, for the program paths a test drives.
    fn fxp(p: &str) -> PathBuf {
        PathBuf::from(fx(p))
    }

    /// The inverse of [`fx`]: `/c/Users/me` → `C:/Users/me` on Linux, identity
    /// on Windows. Lets the assertions keep the Windows spelling they document.
    fn unfx(p: &str) -> String {
        if cfg!(windows) {
            return p.to_string();
        }
        let rest = match p.strip_prefix('/') {
            Some(r) => r,
            None => return p.to_string(),
        };
        let (head, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let mut it = head.chars();
        match (it.next(), it.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => {
                format!("{}:/{tail}", c.to_ascii_uppercase())
            }
            _ => p.to_string(),
        }
    }

    /// "These directories exist, nothing else does."
    fn dirs_exist(list: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        let dirs: Vec<PathBuf> = list.iter().map(|d| fxp(d)).collect();
        move |p: &Path| dirs.iter().any(|d| d == p)
    }

    /// "These variables are set, nothing else is."
    fn env_of(
        list: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        let vars: Vec<(&str, String)> = list.iter().map(|(k, v)| (*k, fx(v))).collect();
        move |name: &str| {
            vars.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| std::ffi::OsString::from(v.clone()))
        }
    }

    /// The runtimes one program matched, by id.
    fn runtime_ids(matches: &[RuntimeMatch]) -> Vec<&'static str> {
        matches.iter().map(|m| m.runtime).collect()
    }

    /// One runtime's match, or a panic naming what did match instead.
    fn only<'a>(matches: &'a [RuntimeMatch], id: &str) -> &'a RuntimeNeeds {
        &matches
            .iter()
            .find(|m| m.runtime == id)
            .unwrap_or_else(|| panic!("no `{id}` row fired; got {:?}", runtime_ids(matches)))
            .needs
    }

    /// The granted directories, back in the Windows spelling the fixtures use.
    fn granted_dirs(n: &RuntimeNeeds) -> Vec<String> {
        n.grants
            .iter()
            .map(|g| unfx(&g.dir.to_string_lossy().replace('\\', "/")))
            .collect()
    }

    fn env_named<'a>(n: &'a RuntimeNeeds, name: &str) -> Option<&'a RuntimeEnv> {
        n.env.iter().find(|v| v.name == name).map(|v| &v.value)
    }


    /// **V38: a DECLARED runtime selects a row detection would not have fired,
    /// and takes the same screen.**
    ///
    /// The case the manifest field exists for: a scanner installed somewhere no
    /// convention recognizes (`C:\tools\acme\acme.exe` is a Python program, and
    /// nothing about that path says so). Inference sees nothing; the
    /// declaration selects the `python` row; the grants it produces are the
    /// row's own, screened exactly as an inferred one would be.
    #[test]
    fn a_declared_runtime_selects_a_profile_inference_would_have_missed() {
        let env = env_of(&[("USERPROFILE", "C:/Users/me")]);
        let is_dir = dirs_exist(&["C:/tools/acme"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let program = &fxp("C:/tools/acme/acme.exe");

        // Inference: nothing. A path convention is all it has, and there is none.
        assert!(inferred_runtime_ids(program, &m).is_empty());
        assert!(runtime_matches(&RuntimeSelect::Infer, program, &m).is_empty());

        // The declaration fires the row, whose scratch redirections apply even
        // where no install tree was found.
        let got = runtime_matches(&RuntimeSelect::Profile("python"), program, &m);
        assert_eq!(runtime_ids(&got), vec!["python"]);

        // `none` is the positive statement "single static binary": no row, ever,
        // even for a program inference WOULD have matched.
        let stub = &fxp("C:/Users/me/AppData/Local/Programs/Python/Scripts/acme.exe");
        assert_eq!(inferred_runtime_ids(stub, &m), vec!["python"]);
        assert!(runtime_matches(&RuntimeSelect::None, stub, &m).is_empty());

        // An id no row carries selects nothing — a manifest from a newer cImp
        // asking for a runtime this build has no rules for gets the gap, never
        // an invented grant.
        assert!(runtime_matches(&RuntimeSelect::Profile("cobol"), stub, &m).is_empty());
    }

    /// Every id the manifest's `runtime` enum can name is a real row in this
    /// table — the two vocabularies are the same one, checked from the sandbox
    /// side as well as from the manifest side.
    #[test]
    fn every_runtime_select_id_names_a_profile() {
        for id in ["python", "node", "java", "dotnet", "go", "rust"] {
            assert!(
                RUNTIME_PROFILES.iter().any(|p| p.id == id),
                "`{id}` is selectable by a manifest but names no profile"
            );
        }
    }
    /// **The rustup convention, ported into the table unchanged.**
    ///
    /// Measured 2026-08-18: a sandboxed `cargo` with only `…\.cargo\bin`
    /// granted dies on `C:\Users\<u>\.rustup`; with both state directories
    /// granted it runs, resolves offline and compiles. The row must therefore
    /// yield BOTH homes for a rustup shim — and nothing at all for a directory
    /// that merely happens to be called `bin`, because every such grant is a
    /// durable ACE on the user's machine.
    #[test]
    fn the_rust_row_is_the_rustup_convention_and_nothing_wider() {
        let env = env_of(&[("USERPROFILE", "C:/Users/me")]);
        let is_dir = dirs_exist(&["C:/Users/me/.cargo", "C:/Users/me/.rustup"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(&fxp("C:/Users/me/.cargo/bin/cargo.exe"), &m);
        assert_eq!(runtime_ids(&got), vec!["rust"]);
        let n = only(&got, "rust");
        assert_eq!(
            granted_dirs(n),
            vec!["C:/Users/me/.cargo", "C:/Users/me/.rustup"]
        );
        assert_eq!(
            env_named(n, "CARGO_HOME"),
            Some(&RuntimeEnv::Dir(fxp("C:/Users/me/.cargo")))
        );
        assert_eq!(
            env_named(n, "RUSTUP_HOME"),
            Some(&RuntimeEnv::Dir(fxp("C:/Users/me/.rustup")))
        );
        assert!(n.gaps.is_empty(), "{:?}", n.gaps);

        // An explicitly-set pointer wins over the convention, both halves
        // independently — a user who moved either home is still served.
        let env = env_of(&[
            ("USERPROFILE", "C:/Users/me"),
            ("RUSTUP_HOME", "D:/rust/toolchains"),
        ]);
        let is_dir = dirs_exist(&["C:/Users/me/.cargo", "D:/rust/toolchains"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(&fxp("C:/Users/me/.cargo/bin/cargo.exe"), &m);
        assert_eq!(
            granted_dirs(only(&got, "rust")),
            vec!["C:/Users/me/.cargo", "D:/rust/toolchains"]
        );

        // A pointer to a directory that does not exist is neither granted nor
        // set: no rule here may invent state the user never created.
        let env = env_of(&[("USERPROFILE", "C:/Users/me")]);
        let is_dir = dirs_exist(&["C:/Users/me/.cargo"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let n = &runtime_needs(&fxp("C:/Users/me/.cargo/bin/cargo.exe"), &m)[0].needs;
        assert_eq!(granted_dirs(n), vec!["C:/Users/me/.cargo"]);
        assert_eq!(env_named(n, "RUSTUP_HOME"), None);
    }

    /// **The `Scripts` convention, ported into the table.** A pip
    /// console-script launcher cannot initialize without the install root
    /// beside it (rc.9: `semgrep.exe` exited 1 with both streams empty), so the
    /// parent of a `Scripts` directory is granted too — and ONLY there, because
    /// the general "grant the grandparent" rule would hand out `C:\Windows` for
    /// anything in `System32`.
    #[test]
    fn the_python_row_is_the_scripts_convention_and_nothing_wider() {
        let env = env_of(&[]);
        let is_dir = dirs_exist(&["C:/Users/me/AppData/Local/Python/pythoncore-3.14-64"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        // The live shape, and the case-insensitivity Windows paths need.
        let got = runtime_needs(
            &fxp("C:/Users/me/AppData/Local/Python/pythoncore-3.14-64/Scripts/semgrep.exe"),
            &m,
        );
        assert_eq!(
            granted_dirs(only(&got, "python")),
            vec!["C:/Users/me/AppData/Local/Python/pythoncore-3.14-64"]
        );

        // The interpreter's OWN directory is never re-asked for: the engine
        // grants `program.parent()` before the table is consulted at all.
        let is_dir = dirs_exist(&["C:/Python314", "C:/Python314/Lib", "C:/Python314/DLLs"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(&fxp("C:/Python314/python3.14.exe"), &m);
        let n = only(&got, "python");
        assert!(n.grants.is_empty(), "{:?}", n.grants);
        assert!(n.gaps.is_empty(), "{:?}", n.gaps);
        // …and the caches still move into the sandbox root, which is the only
        // writable place a bytecode cache can land.
        assert_eq!(
            env_named(n, "PYTHONPYCACHEPREFIX"),
            Some(&RuntimeEnv::Scratch("pycache"))
        );

        // A virtual environment: `Lib` but no `DLLs`. Its base interpreter is
        // named by `pyvenv.cfg` and cannot be inferred, so the row SAYS SO
        // rather than letting the stub exit silently.
        let is_dir = dirs_exist(&["C:/py/venv", "C:/py/venv/Lib"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(&fxp("C:/py/venv/scripts/ruff.exe"), &m);
        let n = only(&got, "python");
        assert_eq!(granted_dirs(n), vec!["C:/py/venv"]);
        assert_eq!(n.gaps.len(), 1, "{:?}", n.gaps);
        assert!(n.gaps[0].why.contains("pyvenv.cfg"), "{:?}", n.gaps[0]);

        // Everything else yields no python row at all — most importantly the
        // directories a grandparent rule would over-grant.
        let is_dir = dirs_exist(&[]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        for narrow in [
            "C:/Windows/System32/where.exe",
            "C:/Program Files/Git/cmd/git.exe",
            "/usr/bin/env",
        ] {
            assert!(
                !runtime_ids(&runtime_needs(&fxp(narrow), &m)).contains(&"python"),
                "{narrow} must not widen the boundary"
            );
        }
        // A `Scripts` directory at a volume root would yield the volume itself;
        // the answer there is no grant, not the whole drive.
        let is_dir = dirs_exist(&["/"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(Path::new("/Scripts/tool.exe"), &m);
        assert!(only(&got, "python").grants.is_empty());
    }

    /// **Node.** The runtime a JS tool shim starts lives outside the project
    /// root even when the shim itself does not, and when it cannot be inferred
    /// that is a row rather than a silence.
    #[test]
    fn the_node_row_finds_the_runtime_a_project_local_shim_starts() {
        // A `node_modules\.bin` shim resolves INSIDE the project root (already
        // granted full access) — what it needs is node itself.
        let env = env_of(&[("npm_config_prefix", "C:/nvm4w/nodejs")]);
        let is_dir = dirs_exist(&["C:/nvm4w/nodejs"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(&fxp("C:/proj/node_modules/.bin/eslint.cmd"), &m);
        let n = only(&got, "node");
        assert_eq!(granted_dirs(n), vec!["C:/nvm4w/nodejs"]);
        assert_eq!(
            env_named(n, "NODE_OPTIONS"),
            Some(&RuntimeEnv::Literal(
                "--preserve-symlinks --preserve-symlinks-main"
            ))
        );
        assert_eq!(
            env_named(n, "npm_config_cache"),
            Some(&RuntimeEnv::Scratch("npm"))
        );
        assert!(n.gaps.is_empty(), "{:?}", n.gaps);

        // With no pointer to node, the row states what is missing — and does
        // NOT go looking for it in the profile root.
        let env = env_of(&[("USERPROFILE", "C:/Users/me")]);
        let is_dir = dirs_exist(&[]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(&fxp("C:/proj/node_modules/.bin/knip.cmd"), &m);
        let n = only(&got, "node");
        assert!(n.grants.is_empty(), "{:?}", n.grants);
        assert_eq!(n.gaps.len(), 1);
        assert_eq!(n.gaps[0].what, "node.exe");

        // `node.exe` itself needs no extra grant: its own directory is what the
        // engine already grants.
        let got = runtime_needs(&fxp("C:/nvm4w/nodejs/node.exe"), &m);
        let n = only(&got, "node");
        assert!(n.grants.is_empty() && n.gaps.is_empty(), "{n:?}");
    }

    /// **Java.** A real JVM launcher derives its home from its own layout; a
    /// launcher SCRIPT must not, because deriving from `pmd-bin-7\bin\pmd.bat`
    /// would set `JAVA_HOME` to PMD's own directory.
    #[test]
    fn the_java_row_separates_a_jvm_launcher_from_a_launcher_script() {
        let env = env_of(&[]);
        let is_dir = dirs_exist(&["P:/WorkSync/JavaJDK"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(&fxp("P:/WorkSync/JavaJDK/bin/java.exe"), &m);
        let n = only(&got, "java");
        assert_eq!(granted_dirs(n), vec!["P:/WorkSync/JavaJDK"]);
        assert_eq!(
            env_named(n, "JAVA_HOME"),
            Some(&RuntimeEnv::Dir(fxp("P:/WorkSync/JavaJDK")))
        );

        // The script: JAVA_HOME comes from the environment, never from the
        // script's own `bin`.
        let env = env_of(&[("JAVA_HOME", "P:/WorkSync/JavaJDK")]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(&fxp("C:/tools/pmd-bin-7.0.0/bin/pmd.bat"), &m);
        assert_eq!(granted_dirs(only(&got, "java")), vec!["P:/WorkSync/JavaJDK"]);

        // …and with no JAVA_HOME anywhere, the JVM it starts is a stated gap.
        let env = env_of(&[]);
        let is_dir = dirs_exist(&[]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(&fxp("C:/tools/pmd-bin-7.0.0/bin/pmd.bat"), &m);
        let n = only(&got, "java");
        assert!(n.grants.is_empty());
        assert_eq!(n.gaps[0].what, "JAVA_HOME");
    }

    /// **.NET and Go — the two halves of rule (b).** State the tool WRITES
    /// moves into the sandbox root; state it READS keeps pointing at the real
    /// directory, which is then granted read-only beside the pointer.
    #[test]
    fn the_dotnet_and_go_rows_move_what_is_written_and_grant_what_is_read() {
        let env = env_of(&[("USERPROFILE", "C:/Users/me")]);
        let is_dir = dirs_exist(&[
            "C:/Users/me/.nuget/packages",
            "C:/Program Files/Go",
            "C:/Users/me/go/pkg/mod",
        ]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };

        let got = runtime_needs(&fxp("C:/Program Files/dotnet/dotnet.exe"), &m);
        let n = only(&got, "dotnet");
        assert_eq!(granted_dirs(n), vec!["C:/Users/me/.nuget/packages"]);
        assert_eq!(
            env_named(n, "NUGET_PACKAGES"),
            Some(&RuntimeEnv::Dir(fxp("C:/Users/me/.nuget/packages")))
        );
        assert_eq!(
            env_named(n, "DOTNET_CLI_HOME"),
            Some(&RuntimeEnv::Scratch("dotnet"))
        );
        assert_eq!(
            env_named(n, "DOTNET_CLI_TELEMETRY_OPTOUT"),
            Some(&RuntimeEnv::Literal("1"))
        );

        let got = runtime_needs(&fxp("C:/Program Files/Go/bin/go.exe"), &m);
        let n = only(&got, "go");
        assert_eq!(
            granted_dirs(n),
            vec!["C:/Program Files/Go", "C:/Users/me/go/pkg/mod"]
        );
        for (name, sub) in [
            ("GOCACHE", "gocache"),
            ("GOTMPDIR", "gotmp"),
            ("GOPATH", "gopath"),
        ] {
            assert_eq!(env_named(n, name), Some(&RuntimeEnv::Scratch(sub)), "{name}");
        }
        assert_eq!(
            env_named(n, "GOMODCACHE"),
            Some(&RuntimeEnv::Dir(fxp("C:/Users/me/go/pkg/mod")))
        );

        // A Go TOOL with no toolchain in sight says so; it never derives a
        // GOROOT from its own directory.
        let env = env_of(&[]);
        let is_dir = dirs_exist(&[]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(&fxp("C:/tools/golangci-lint.exe"), &m);
        let n = only(&got, "go");
        assert!(n.grants.is_empty(), "{:?}", n.grants);
        assert_eq!(n.gaps[0].what, "GOROOT");
    }

    /// **A runtime that simply is not viable in-container.** S1 measured the
    /// Store interpreter aliases as unworkable — a reparse point in unlistable
    /// profile territory — and no grant fixes it. The honest answer is a row
    /// that says so, not an ACE stamped on an alias directory in the hope it
    /// helps.
    #[test]
    fn an_unsupported_runtime_is_a_row_not_a_grant() {
        let env = env_of(&[("USERPROFILE", "C:/Users/me")]);
        let is_dir = dirs_exist(&["C:/Users/me/AppData/Local/Microsoft/WindowsApps"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(
            &fxp("C:/Users/me/AppData/Local/Microsoft/WindowsApps/python.exe"),
            &m,
        );
        let n = only(&got, "windows-store-alias");
        assert!(n.grants.is_empty() && n.env.is_empty(), "{n:?}");
        assert_eq!(n.gaps.len(), 1);
        assert!(n.gaps[0].why.contains("reparse point"), "{:?}", n.gaps[0]);
        // The python row fires too (it is a `python*.exe`) and must not grant
        // the alias directory on the strength of the file name.
        assert!(only(&got, "python").grants.is_empty());
    }

    /// **The final screen, on a path the TABLE produced.** Every path here is
    /// inferred from the machine rather than read from a reviewed constant, so
    /// it goes through the same refusal rules a settings row gets — and a
    /// refused path is dropped WITH A ROW while the rest of the same runtime's
    /// grants still apply.
    #[test]
    fn a_table_produced_path_that_hits_the_refusal_screen_is_dropped_with_a_row() {
        // `CARGO_HOME` pointed at the profile root: refused, recorded, and the
        // sibling `.rustup` grant is untouched.
        let env = env_of(&[("HOME", "/home/me"), ("CARGO_HOME", "/home/me")]);
        let is_dir = dirs_exist(&["/home/me", "/home/me/.rustup"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(Path::new("/home/me/.cargo/bin/cargo"), &m);
        let n = only(&got, "rust");
        assert_eq!(granted_dirs(n), vec!["/home/me/.rustup"]);
        assert_eq!(n.gaps.len(), 1, "{:?}", n.gaps);
        assert!(n.gaps[0].why.contains("user-profile root"), "{:?}", n.gaps[0]);
        // The pointer goes with the grant: a variable naming a directory the
        // container cannot read turns a clean failure into a confusing one.
        assert_eq!(env_named(n, "CARGO_HOME"), None);
        assert!(env_named(n, "RUSTUP_HOME").is_some());

        // A volume root, the other shape no rule may ever hand over.
        let env = env_of(&[("HOME", "/home/me"), ("CARGO_HOME", "/")]);
        let is_dir = dirs_exist(&["/"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(Path::new("/home/me/.cargo/bin/cargo"), &m);
        let n = only(&got, "rust");
        assert!(n.grants.is_empty(), "{:?}", n.grants);
        assert!(n.gaps[0].why.contains("volume/filesystem root"));

        // A credential store reached through a moved pointer is refused by the
        // same table that refuses it in settings.
        let env = env_of(&[("HOME", "/home/me"), ("JAVA_HOME", "/home/me/.ssh")]);
        let is_dir = dirs_exist(&["/home/me/.ssh"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(Path::new("/opt/pmd/bin/pmd"), &m);
        let n = only(&got, "java");
        assert!(n.grants.is_empty(), "{:?}", n.grants);
        assert!(n.gaps[0].why.contains("SSH"), "{:?}", n.gaps[0]);
    }

    /// **The ordering invariant.** The engine redirects `HOME`/`USERPROFILE`
    /// into the sandbox root, so a toolchain resolving `%USERPROFILE%\.cargo`
    /// after that redirect finds an empty scratch directory. Every runtime
    /// pointer must therefore be applied AFTER the redirect, and scratch
    /// pointers must resolve inside the root rather than beside it.
    #[test]
    fn runtime_pointers_land_after_the_home_redirect() {
        let env = env_of(&[("USERPROFILE", "C:/Users/me")]);
        let is_dir = dirs_exist(&["C:/Users/me/.cargo", "C:/Users/me/.rustup"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let matches = runtime_needs(&fxp("C:/Users/me/.cargo/bin/cargo.exe"), &m);
        let root = &fxp("S:/");
        let composed = compose_env_overrides(root, &matches);
        let names: Vec<&str> = composed.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(&names[..4], &["TEMP", "TMP", "HOME", "USERPROFILE"]);
        let home_at = names.iter().position(|n| *n == "USERPROFILE").unwrap();
        for pointer in ["CARGO_HOME", "RUSTUP_HOME"] {
            let at = names
                .iter()
                .position(|n| *n == pointer)
                .unwrap_or_else(|| panic!("{pointer} never reached the child"));
            assert!(
                at > home_at,
                "{pointer} lands BEFORE the home redirect, which would overwrite it"
            );
        }
        // The redirect itself still points at the root, and the real pointer
        // still points at the real directory.
        let value = |name: &str| {
            composed
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| unfx(&v.to_string_lossy().replace('\\', "/")))
                .unwrap()
        };
        assert_eq!(value("USERPROFILE"), "S:/");
        assert_eq!(value("CARGO_HOME"), "C:/Users/me/.cargo");

        // Scratch resolves INSIDE the root, under one obvious directory.
        let matches = runtime_needs(&fxp("C:/Program Files/Go/bin/go.exe"), &m);
        let composed = compose_env_overrides(root, &matches);
        let gocache = composed
            .iter()
            .find(|(k, _)| k == "GOCACHE")
            .map(|(_, v)| unfx(&v.to_string_lossy().replace('\\', "/")))
            .expect("GOCACHE");
        assert_eq!(gocache, format!("S:/{SANDBOX_SCRATCH_DIR}/gocache"));
    }

    /// Every row carries a pattern and a reason a user can act on — the same
    /// bar [`GrantRow`], `GRANT_REFUSAL_RULES` and `child_env::CHILD_ENV` are
    /// held to. A row with no reason is a widening nobody reviewed.
    #[test]
    fn every_runtime_row_carries_a_pattern_and_a_reason() {
        for profile in RUNTIME_PROFILES {
            assert!(!profile.detect.is_empty(), "`{}` matches nothing", profile.id);
            assert_eq!(
                profile.id,
                profile.id.to_ascii_lowercase(),
                "row ids appear in grant rows verbatim; keep them lowercase"
            );
            assert!(
                profile.why.len() > 40,
                "`{}` widens the boundary without a reason a reviewer can check",
                profile.id
            );
            for detect in profile.detect {
                match detect {
                    Detect::Program(names) => {
                        for n in *names {
                            assert_eq!(
                                *n,
                                n.to_ascii_lowercase(),
                                "`{n}` is compared lowercased and would never match"
                            );
                            assert!(
                                n.matches('*').count() <= 1,
                                "`{n}`: one wildcard only — a richer matcher is a richer way to \
                                 fire on the wrong program"
                            );
                        }
                    }
                    Detect::DirTail(tail) => {
                        assert!(!tail.is_empty(), "`{}` has an empty tail", profile.id);
                        for c in *tail {
                            assert_eq!(*c, c.to_ascii_lowercase(), "`{c}` would never match");
                        }
                    }
                }
            }
        }
        // The wildcard is exactly as narrow as it claims to be.
        assert!(glob1("python*.exe", "python3.14.exe"));
        assert!(glob1("python*.exe", "python.exe"));
        assert!(!glob1("python*.exe", "pythonw.dll"));
        assert!(!glob1("node.exe", "nodemon.exe"));
    }

    /// **Detection is a layout, never a guess.** One near-miss per row, chosen
    /// to be the shape a looser rule would have swallowed.
    #[test]
    fn a_near_miss_fires_no_runtime_row() {
        let env = env_of(&[("USERPROFILE", "C:/Users/me"), ("JAVA_HOME", "C:/jdk")]);
        let is_dir = dirs_exist(&["C:/jdk", "C:/Users/me/.cargo"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        for near_miss in [
            // `bin` alone is not the rustup convention…
            "C:/Program Files/Git/usr/bin/bash.exe",
            "/usr/bin/env",
            // …`System32` is not an interpreter root…
            "C:/Windows/System32/where.exe",
            // …a name that merely starts like one is not a JVM or a node…
            "C:/tools/javascript-obfuscator.exe",
            "C:/tools/nodemon.exe",
            "C:/tools/gopls.exe",
            // …and a `.bin` that is not `node_modules\.bin` is just a directory.
            "C:/tools/.bin/thing.exe",
        ] {
            assert!(
                runtime_needs(&fxp(near_miss), &m).is_empty(),
                "{near_miss} matched {:?}",
                runtime_ids(&runtime_needs(&fxp(near_miss), &m))
            );
        }
        // A program with no directory at all cannot be matched against any
        // layout, and guessing is how a rule fires on the wrong tree.
        assert!(runtime_needs(Path::new("cargo.exe"), &m).is_empty());
    }

    /// The Windows shapes of the same screen — the drive prefix and the profile
    /// ancestry, which are one opaque component on the Linux runner.
    #[cfg(windows)]
    #[test]
    fn the_runtime_table_is_screened_with_windows_shapes_too() {
        let env = env_of(&[
            ("USERPROFILE", r"C:\Users\me"),
            ("SystemRoot", r"C:\Windows"),
            ("CARGO_HOME", r"C:\Users\me"),
        ]);
        let is_dir = dirs_exist(&[r"C:\Users\me", r"C:\Users\me\.rustup"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(Path::new(r"C:\Users\me\.cargo\bin\cargo.exe"), &m);
        let n = only(&got, "rust");
        assert_eq!(granted_dirs(n), vec!["C:/Users/me/.rustup"]);
        assert!(n.gaps[0].why.contains("user-profile root"));

        // The Windows directory is already readable inside the container and an
        // ACE stamped there is a durable change to the OS — no inferred path
        // may reach it either.
        let env = env_of(&[
            ("USERPROFILE", r"C:\Users\me"),
            ("SystemRoot", r"C:\Windows"),
            ("JAVA_HOME", r"C:\Windows\System32"),
        ]);
        let is_dir = dirs_exist(&[r"C:\Windows\System32"]);
        let m = Machine {
            env: &env,
            is_dir: &is_dir,
        };
        let got = runtime_needs(Path::new(r"C:\tools\pmd\bin\pmd.bat"), &m);
        let n = only(&got, "java");
        assert!(n.grants.is_empty(), "{:?}", n.grants);
        assert!(n.gaps[0].why.contains("Windows install directory"));
    }

    /// **The grant-site screen (V33, 2026-08-18).** `extra_grant_dirs` is the
    /// one input to the engines a settings file supplies verbatim, and
    /// `grant_dir` stamps a DURABLE inheritable ACE — so a row naming `~/.ssh`
    /// would outlive the run. Both directions, and the near-misses that a
    /// sloppy string comparison would get wrong.
    #[test]
    fn a_grant_row_that_opens_credentials_or_the_world_is_refused() {
        let home = Path::new("/home/me");
        for (path, needle) in [
            ("/home/me/.ssh", "SSH"),
            ("/home/me/.aws", "AWS"),
            ("/home/me/.gnupg", "GnuPG"),
            ("/home/me/.config/gh", "GitHub"),
            ("/home/me", "user-profile root"),
            ("/home", "user-profile root"),
            ("/", "volume/filesystem root"),
            ("tools", "not a rooted path"),
            ("", "not a rooted path"),
        ] {
            let why = extra_grant_refusal(Path::new(path), Some(home), None)
                .unwrap_or_else(|| panic!("{path} must be refused"));
            assert!(why.contains(needle), "{path}: {why}");
        }
        // Case and a trailing separator do not launder a refusal.
        assert!(extra_grant_refusal(Path::new("/home/me/.SSH"), Some(home), None).is_some());
        assert!(extra_grant_refusal(Path::new("/home/me/.ssh/"), Some(home), None).is_some());
        // …and the legitimate rows this setting exists for still pass.
        for ok in [
            "/opt/toolchains/gcc/bin",
            "/home/me/.cargo/bin",
            "/home/me/.local/share/pnpm",
            "/usr/lib/llvm-18",
        ] {
            assert_eq!(
                extra_grant_refusal(Path::new(ok), Some(home), None),
                None,
                "{ok} is exactly what extra_grant_dirs is for"
            );
        }
        // Component-wise, not string-prefix: another user's tree is not this
        // user's profile root, and `.sshkeys` is not `.ssh`.
        assert_eq!(
            extra_grant_refusal(Path::new("/home/melissa/tools"), Some(home), None),
            None
        );
        assert_eq!(
            extra_grant_refusal(Path::new("/home/me/.sshkeys"), Some(home), None),
            None
        );
        // With no home known, the structural profile rule simply does not fire
        // — the table still does.
        assert_eq!(extra_grant_refusal(Path::new("/opt/x"), None, None), None);
        assert!(extra_grant_refusal(Path::new("/opt/x/.ssh"), None, None).is_some());
    }

    /// The Windows shapes of the same rule — the drive prefix, the profile
    /// ancestry and the three credential stores under `AppData\Roaming`.
    /// Split off because `C:\…` is one opaque component on the Linux runner.
    #[cfg(windows)]
    #[test]
    fn a_grant_row_is_screened_with_windows_shapes_too() {
        let home = Path::new(r"C:\Users\me");
        let sys = Path::new(r"C:\Windows");
        for p in [
            r"C:\",
            r"C:\Users",
            r"C:\Users\me",
            r"C:\Users\me\.ssh",
            r"C:\Users\me\AppData\Roaming\Microsoft\Credentials",
            r"C:\Users\me\AppData\Roaming\Microsoft\Protect",
            r"C:\Users\me\AppData\Roaming\Microsoft\Vault",
            r"C:\Windows",
            r"C:\Windows\System32",
            r"tools\bin",
        ] {
            assert!(
                extra_grant_refusal(Path::new(p), Some(home), Some(sys)).is_some(),
                "{p} must be refused"
            );
        }
        for p in [
            r"C:\Users\me\.cargo\bin",
            r"D:\toolchains\llvm\bin",
            r"C:\Program Files\nodejs",
            r"C:\Users\meredith\shared",
        ] {
            assert_eq!(
                extra_grant_refusal(Path::new(p), Some(home), Some(sys)),
                None,
                "{p}"
            );
        }
        // Windows paths are case-insensitive, and so is this screen.
        assert!(extra_grant_refusal(Path::new(r"c:\users\ME\.SSH"), Some(home), Some(sys)).is_some());
        assert!(extra_grant_refusal(Path::new(r"c:\WINDOWS\system32"), Some(home), Some(sys)).is_some());
    }

    /// Every refusal row carries a reason a user can act on — the same bar
    /// [`GrantRow`] and `child_env::CHILD_ENV` are held to.
    #[test]
    fn every_grant_refusal_rule_carries_a_reason_and_a_pattern() {
        for rule in GRANT_REFUSAL_RULES {
            assert!(
                !rule.suffix.is_empty(),
                "a rule with no pattern matches nothing"
            );
            assert!(
                rule.why.len() > 20,
                "`{:?}` is refused without a reason a user can act on",
                rule.suffix
            );
            for seg in rule.suffix {
                assert_eq!(
                    *seg,
                    seg.to_ascii_lowercase(),
                    "patterns are compared lowercased; `{seg}` would never match"
                );
            }
        }
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
        // It records a shape, never a verdict. Only the NARRATIVE half is
        // checked: the `Posture:` tail is a capability inventory, and on Linux
        // it legitimately says "TCP bind+connect denied" about the boundary —
        // which is not the row calling this child's exit a denial.
        let narrative = detail.split("Posture:").next().unwrap();
        assert!(
            !narrative.contains("denied"),
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

    /// **The retraction, pinned.** The note used to assert that no child
    /// process can run inside the boundary; that was measured false on
    /// 2026-08-18 (see [`PROGRAM_START_DENIAL_MARKERS`]), and a note that tells
    /// a user their situation is hopeless is worse than no note at all. What it
    /// must say instead is what they can act on — reachability, and the
    /// drive-qualified-path rule — and what it must never say again is that a
    /// grant cannot help.
    #[test]
    fn a_sandboxed_shell_gets_told_why_it_cannot_start_a_program() {
        for stderr in [
            "'cargo' is not recognized as an internal or external command,",
            "Access is denied.",
        ] {
            let note = sandboxed_shell_note(Some(1), stderr)
                .unwrap_or_else(|| panic!("no note for {stderr:?}"));
            // The measured truth, and the two actionable causes.
            assert!(note.contains("Programs DO run inside the boundary"), "{note}");
            assert!(note.contains("volume root"), "{note}");
            assert!(note.contains("extra grants"), "{note}");
            // The retracted claims must not come back.
            let lower = note.to_ascii_lowercase();
            assert!(!lower.contains("cannot start any child process"), "{note}");
            assert!(!lower.contains("no grant will change"), "{note}");
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
        // R17: driven through the real `once_per_session`, against this test's
        // OWN slot — which is exactly the per-site static the helper documents,
        // and is why no test needs to reset a process-wide set.
        static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
            std::sync::Mutex::new(None);
        let key = |p: &str| subject_key(&program_subject(Path::new(p)));
        assert!(once_per_session(&SEEN, key("C:/bin/git.exe")));
        assert!(!once_per_session(&SEEN, key("C:/bin/git.exe")));
        // Same program, different path and case — still the same fact.
        assert!(!once_per_session(&SEEN, key("D:/other/GIT.EXE")));
        // A different program is a different fact and must be recorded.
        assert!(once_per_session(&SEEN, key("C:/bin/cargo.exe")));
        assert_eq!(SEEN.lock().unwrap().as_ref().map(|s| s.len()), Some(2));
    }

    /// …and the `run_check` seam's subject is the CHECK NAME, not the shell it
    /// runs through. Every check spawns the same `cmd.exe`, so a program-derived
    /// subject would render every row identically AND let the first sandboxed
    /// check speak for all of them. Each configured check confirms once.
    #[test]
    fn a_check_is_identified_by_its_configured_name_not_by_the_shell() {
        static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
            std::sync::Mutex::new(None);
        // Two different checks, one shell: two facts, two rows.
        assert!(once_per_session(&SEEN, subject_key("cargo")));
        assert!(once_per_session(&SEEN, subject_key("tsc")));
        // …and re-running a check is the same fact, whatever its case.
        assert!(!once_per_session(&SEEN, subject_key("Cargo")));
        assert_eq!(SEEN.lock().unwrap().as_ref().map(|s| s.len()), Some(2));
        // The row a user scans names the check, not `cmd.exe`.
        assert_eq!(state_target("sandboxed", "cargo"), "sandboxed — cargo");
        assert!(!state_target("sandboxed", "cargo").contains("cmd"));
    }

    /// Denials are NOT deduped — the repeated boundary hit is the signal the
    /// user asked to be able to see. Guarded as a policy assertion because the
    /// obvious "make it consistent with record_skip" refactor would delete it.
    #[test]
    fn the_denial_path_has_no_dedup_key() {
        let src = include_str!("events.rs");
        let body = src
            .split("pub fn record_denial(")
            .nth(1)
            .expect("record_denial exists");
        // The function ends at the first `}` in column 0 — every brace inside
        // the body is indented. (Line-ending-blind: `\r\n}` contains `\n}`.)
        let body = body.split("\n}").next().unwrap_or(body);
        assert!(
            // `once_per_session` joined this list when R17 wrote it: a "make
            // this consistent with its siblings" refactor now reaches for THAT
            // name, so a tripwire that only knew the old two would wave it
            // through.
            !body.contains("EMITTED")
                && !body.contains("first_time")
                && !body.contains("once_per_session"),
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
        let got = summarize_invocation(&program_subject(&fxp("C:/bin/git.exe")), &args);
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
            state_target("sandboxed", &program_subject(&fxp("C:/bin/git.exe"))),
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
            extra_grant_dirs: vec![fxp("C:/tools")],
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

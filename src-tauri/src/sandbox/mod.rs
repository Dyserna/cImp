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

// ── the runtime profile table (V33, 2026-08-19) ─────────────────────────────
//
// **The problem this table generalizes.** A sandboxed child is granted its own
// install directory and the project root, and nothing else. That is enough for
// a self-contained binary (`git.exe`, `typos.exe`) and it is *never* enough for
// a program that is really a front end to a RUNTIME: a pip console-script stub
// loads `python3XX.dll` and the standard library from the install root one
// directory up; a rustup shim resolves `rustc` through `RUSTUP_HOME`; a
// `node_modules\.bin\*.cmd` shim starts `node.exe` from somewhere else
// entirely; a JVM launcher cannot start without the runtime image beside it.
// Worse, the engine redirects `HOME`/`USERPROFILE` into the sandbox root
// (windows::prepare_blocking), so a runtime whose state pointer is UNSET
// resolves it against an empty scratch directory and a runtime whose pointer IS
// set names a directory the container was never granted. Either way the tool
// starts and then dies for a reason that looks nothing like a sandbox — often
// with both streams empty (see [`record_silent_exit`]).
//
// Until 2026-08-19 this module answered that with exactly two hardcoded
// special cases: `interpreter_root` (the Python `Scripts` convention) and
// `toolchain_state` (the rustup convention). Both were right and neither
// generalized. The table below is those two rules plus the rest of the S1
// addendum's toolchain matrix
// (`docs/reviews/SPIKE-S1-appcontainer-2026-08-15.md`), in the house shape
// [`child_env::CHILD_ENV`] and [`GRANT_REFUSAL_RULES`] already use: data in
// code, one row per runtime, a reason on every widening, and a reviewer of the
// diff that adds a row sees the pattern and the justification together.
//
// **Two rules from S1 shape every row.** (a) *Install location decides the
// grant*: anything under `Program Files` or `%SystemRoot%` is already readable
// by `ALL APPLICATION PACKAGES`, so those rows cost nothing
// (`windows::is_app_package_readable` short-circuits them); a user-owned tree
// costs one RX ACE; an Administrators-owned tree cannot be granted unelevated
// at all and degrades through the loud ladder (`grant_dir` errors →
// `prepare` errors → [`Plan::Plain`] → the child runs unsandboxed and says so).
// (b) *State directories get env-redirected*: a cache or scratch directory
// moves INTO the sandbox root ([`RuntimeEnv::Scratch`]), while read-only state
// a tool must actually find keeps pointing at the real thing
// ([`RuntimeEnv::Dir`]) and is granted read+execute beside the pointer.
//
// **What was measured before this shipped, and what was reasoned.** S1 supplies
// the in-container half (go/dotnet/clang/python/java all execute under
// AppContainer; npm needs `--preserve-symlinks`; `DOTNET_CLI_HOME` and the Go
// cache trio must be redirected). What S1 did NOT establish is that this
// table's exact composition is one a runtime accepts, so every row's variables
// were run against the real toolchains on this machine on 2026-08-19 — outside
// the container, which is where an environment contract can be falsified
// without stamping anything: `go env` confirmed that an explicit `GOMODCACHE`
// really does survive a redirected `GOPATH` (the split this table depends on,
// and the one thing here that could not be deduced), `node -e` accepted
// `NODE_OPTIONS`, `dotnet --version` ran with `DOTNET_CLI_HOME` redirected,
// `java -version` with `JAVA_HOME` re-asserted, and `python -c` with
// `PYTHONPYCACHEPREFIX` pointed into a scratch tree. What remains reasoned
// rather than measured is whether a read-only NuGet/module cache is *enough*
// for a restore inside the boundary — a restore that needs to WRITE one fails
// with a denial the classifier recognizes, which is the honest outcome either
// way.
//
// **What no row may ever do.** Grant a volume root, a user-profile root,
// `%SystemRoot%` or a credential store — not because a row would want to, but
// because every path here is INFERRED from the machine (an environment
// variable, a directory name) rather than read from a reviewed constant, and an
// inference is exactly the kind of input that should not be trusted with a
// durable inheritable ACE. So every path the table produces goes through
// [`extra_grant_refusal`], the same screen the settings-supplied rows get.
// Defence in depth on purpose: cImp-derived grants are not screened anywhere
// else, and these are the cImp-derived grants that a hostile environment
// variable can steer.
//
// **Machine-wide ACL weakening is not on the menu.** A runtime that cannot work
// without `C:\`, `C:\Users` or `%USERPROFILE%` being opened stays unsupported
// and says so in its row's gap text: widening those would widen the boundary
// for every AppContainer on the machine, browser renderers included, which is a
// far larger change than anything cImp is entitled to make on a tool's behalf.

/// The machine a runtime rule may look at — **injected, never read directly**,
/// so every rule below is a pure function that both platforms' test runs can
/// drive with a synthetic machine and no filesystem. Exactly the discipline
/// [`extra_grant_refusal`] and [`child_env::minimal_env`] already follow.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Machine<'a> {
    /// One environment variable's value.
    ///
    /// Production reads the **composed child environment first, cImp's own
    /// process environment second** (`windows::prepare_blocking`). Both halves
    /// are load-bearing: the child's copy is what the tool will actually see,
    /// so a seam that forced `CARGO_HOME` wins; but the child's environment is
    /// the C2 *ceiling* ([`child_env::CHILD_ENV`]) and most runtime pointers —
    /// `JAVA_HOME`, `GOPATH`, `NUGET_PACKAGES` — are deliberately not on it, so
    /// a table that read only the child's copy would be blind to every runtime
    /// except rust and npm. Reading cImp's copy is not a hole: the value is
    /// re-asserted onto the child through a reviewed row with a reason, which
    /// is precisely the shape the C2 table exists to force.
    pub env: &'a dyn Fn(&str) -> Option<std::ffi::OsString>,
    /// Does this path name an existing DIRECTORY?
    ///
    /// A parameter for the same reason: "the user has a `.rustup`" is a fact
    /// about a machine, not about a convention, and no rule here may invent
    /// state. A pointer to a directory that does not exist is neither granted
    /// nor set — stamping (or naming) a path the user never created would be
    /// cImp manufacturing state rather than reaching the state that is there.
    pub is_dir: &'a dyn Fn(&Path) -> bool,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Machine<'_> {
    /// One variable as a `PathBuf`, empty values dropped.
    fn path_var(&self, name: &str) -> Option<PathBuf> {
        let v = (self.env)(name)?;
        (!v.is_empty()).then(|| PathBuf::from(v))
    }
    /// One variable as a directory that EXISTS.
    fn dir_var(&self, name: &str) -> Option<PathBuf> {
        self.path_var(name).filter(|p| (self.is_dir)(p))
    }
    /// The user's profile directory, Windows spelling first.
    fn home(&self) -> Option<PathBuf> {
        self.path_var("USERPROFILE").or_else(|| self.path_var("HOME"))
    }
    /// The Windows install directory, for the refusal screen.
    fn system_root(&self) -> Option<PathBuf> {
        self.path_var("SystemRoot")
    }
}

/// The program a grant is being inferred from, pre-chewed into the two forms
/// every rule matches on.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Program<'a> {
    /// The program's own file name, lowercased (`node.exe`).
    pub file: String,
    /// The directory it lives in — the one the engine always grants R+X, so a
    /// row never has to ask for it.
    pub dir: &'a Path,
    /// `dir`'s components, lowercased. Precomputed because every
    /// [`Detect::DirTail`] arm of every row reads it.
    dir_comps: Vec<String>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl<'a> Program<'a> {
    /// `None` for a program with no directory or a non-UTF-8 name — neither can
    /// be matched against a rule, and guessing is how a rule fires on the wrong
    /// tree.
    pub fn at(program: &'a Path) -> Option<Self> {
        let file = program.file_name()?.to_str()?.to_ascii_lowercase();
        let dir = program.parent()?;
        if dir.as_os_str().is_empty() {
            return None;
        }
        Some(Self {
            file,
            dir,
            dir_comps: lower_components(dir),
        })
    }

    /// Is the program's own directory named exactly this (lowercase)?
    fn dir_named(&self, name: &str) -> bool {
        self.dir_comps.last().is_some_and(|c| c == name)
    }
}

/// How a [`RuntimeProfile`] recognizes that a program belongs to its runtime.
///
/// Both arms are *layout* facts, never tool identities: a rule keyed on "this
/// is semgrep" would have to be extended for every tool that ever ships, while
/// a rule keyed on "this is a pip console-script stub" already covers the ones
/// nobody has installed yet. Where a row does name a specific launcher
/// (`pmd.bat`, `golangci-lint.exe`) it says in its reason that the row is about
/// what that launcher STARTS, not about the tool itself.
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Detect {
    /// The program's file name is one of these, compared lowercased. A single
    /// `*` is a wildcard for the varying middle of a family name
    /// (`python*.exe` matches `python3.14.exe`, `python.exe`, `python3.exe`).
    /// Both the `.exe` and bare spellings are listed where a row is meant to
    /// fire on POSIX too.
    Program(&'static [&'static str]),
    /// The program's directory chain ENDS in these components, outermost
    /// first — `&[".cargo", "bin"]` matches `…\.cargo\bin` and nothing else.
    ///
    /// The trailing-components form rather than "the parent is called X",
    /// because the narrowness is the whole point: `bin` alone would fire for
    /// `C:\Program Files\Git\usr\bin` and `/usr/bin`, and the rule behind it
    /// would then grant `/usr` on the strength of a directory name.
    DirTail(&'static [&'static str]),
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Detect {
    fn matches(&self, p: &Program) -> bool {
        match self {
            Detect::Program(names) => names.iter().any(|n| glob1(n, &p.file)),
            Detect::DirTail(tail) => ends_with(&p.dir_comps, tail),
        }
    }
}

/// A one-`*` glob, both sides already lowercase. Deliberately not a regex and
/// deliberately not multi-`*`: the only variation these names have is a version
/// in the middle, and a richer matcher is a richer way to fire on the wrong
/// program.
#[cfg_attr(not(windows), allow(dead_code))]
fn glob1(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((pre, post)) => {
            name.len() >= pre.len() + post.len()
                && name.starts_with(pre)
                && name.ends_with(post)
        }
    }
}

/// What one environment pointer a runtime needs must be set to.
///
/// The three arms are the whole design: a pointer either names REAL state the
/// tool has to find (and which therefore also needs a grant), or it names
/// scratch that must be REDIRECTED into the sandbox's one writable place, or it
/// is not a path at all. Collapsing them would lose exactly the distinction
/// that makes the boundary work.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum RuntimeEnv {
    /// A real directory on the machine. Always paired with a grant for the same
    /// directory — a pointer the container cannot read is worse than no pointer.
    Dir(PathBuf),
    /// A subdirectory of [`SANDBOX_SCRATCH_DIR`] inside the sandbox root, for
    /// caches and scratch the tool WRITES. Resolved by
    /// [`compose_env_overrides`] once the root's drive letter exists.
    Scratch(&'static str),
    /// A literal value that is not a path — a flag string.
    ///
    /// **Never sourced from cImp's own environment**, which is the entire
    /// reason `NODE_OPTIONS` can appear here while [`child_env::CHILD_ENV`]
    /// deliberately refuses to pass it through: the C2 omission is about not
    /// INHERITING a variable that names files the child would then load, and
    /// this is a reviewed constant with a measurement behind it.
    Literal(&'static str),
}

/// One directory a runtime needs granted read+execute, with the reason the
/// user's grant row prints beside the path.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RuntimeGrant {
    pub dir: PathBuf,
    pub why: &'static str,
}

/// One environment pointer a runtime needs set on the child.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RuntimeVar {
    pub name: &'static str,
    pub value: RuntimeEnv,
    pub why: &'static str,
}

/// A need this boundary does **not** meet, stated rather than dropped.
///
/// Decision 5's loud-degradation rule applied one level down: "the sandbox is
/// on" and "the sandbox is on and this runtime is missing half of what it
/// needs" are two different states, and a user whose tool exits 1 with no
/// output has no other way to tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RuntimeGap {
    /// What is missing — a path, or the thing that could not be inferred.
    pub what: String,
    pub why: &'static str,
}

/// Everything one runtime asks for, before the refusal screen runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RuntimeNeeds {
    pub grants: Vec<RuntimeGrant>,
    pub env: Vec<RuntimeVar>,
    pub gaps: Vec<RuntimeGap>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl RuntimeNeeds {
    /// Real state: grant the directory AND point its variable at it. Skipped
    /// entirely when the directory does not exist (see [`Machine::is_dir`]).
    fn state(&mut self, m: &Machine, dir: PathBuf, name: &'static str, why: &'static str) {
        if !(m.is_dir)(&dir) {
            return;
        }
        self.env.push(RuntimeVar {
            name,
            value: RuntimeEnv::Dir(dir.clone()),
            why,
        });
        self.grants.push(RuntimeGrant { dir, why });
    }
    /// A tree to grant with no pointer of its own (a runtime image, an
    /// interpreter root the launcher finds by relative path).
    fn tree(&mut self, m: &Machine, dir: PathBuf, why: &'static str) {
        if (m.is_dir)(&dir) {
            self.grants.push(RuntimeGrant { dir, why });
        }
    }
    /// Scratch redirected into the sandbox root. No existence check and no
    /// grant: the root is already granted read+write and the tool creates the
    /// directory itself on first use.
    fn scratch(&mut self, name: &'static str, sub: &'static str, why: &'static str) {
        self.env.push(RuntimeVar {
            name,
            value: RuntimeEnv::Scratch(sub),
            why,
        });
    }
    /// A non-path constant.
    fn literal(&mut self, name: &'static str, value: &'static str, why: &'static str) {
        self.env.push(RuntimeVar {
            name,
            value: RuntimeEnv::Literal(value),
            why,
        });
    }
    /// A need that cannot be met — recorded, never silent.
    fn gap(&mut self, what: impl Into<String>, why: &'static str) {
        self.gaps.push(RuntimeGap {
            what: what.into(),
            why,
        });
    }
    fn is_empty(&self) -> bool {
        self.grants.is_empty() && self.env.is_empty() && self.gaps.is_empty()
    }
}

/// One runtime, its detection and what it needs.
///
/// `needs` is a function rather than more data because the answers are
/// *derived* — from the program's own directory, from an environment pointer,
/// from what exists on the machine — while the parts a reviewer has to check
/// (which programs this fires for, and why the row exists at all) are data.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RuntimeProfile {
    /// The runtime's name in a grant row ("… — for node").
    pub id: &'static str,
    /// Any one match fires the row.
    pub detect: &'static [Detect],
    pub needs: fn(&Program, &Machine) -> RuntimeNeeds,
    /// Why this row exists, for the reviewer of the diff that changes it.
    pub why: &'static str,
}

/// The subdirectory of the sandbox root every redirected cache lands under.
///
/// One directory rather than six, and named after cImp so nobody wonders what
/// put it in their repository. It appears in the project root because the
/// project root is the only place a sandboxed child may write — the same
/// reason `TEMP`/`TMP` already point at the mapped drive root.
///
/// **It lives UNDER `.cimp/`, and that is load-bearing rather than tidy.**
/// cImp already writes `.cimp/` in every project (`config.json`,
/// `shadow.git`, the graph store) and projects ignore it as one rule — this
/// repo's own `.gitignore` carries `**/.cimp/`. A sibling top-level directory
/// would be a SECOND thing every user has to learn to ignore, and would show
/// up as untracked noise in `git status` the first time anyone enables
/// sandboxing. `sandbox::tabs::scratch_dir` already made this choice for the
/// per-tab `TEMP` (`.cimp/sandbox-tmp/<tab>`); this is the same rule for the
/// runtime caches, so the two cannot drift apart.
#[cfg_attr(not(windows), allow(dead_code))]
pub const SANDBOX_SCRATCH_DIR: &str = ".cimp/sandbox-cache";

/// **The table.** Order is presentation only; every matching row applies.
#[cfg_attr(not(windows), allow(dead_code))]
pub const RUNTIME_PROFILES: &[RuntimeProfile] = &[
    RuntimeProfile {
        id: "rust",
        // rustup's published layout: the shims live in `<CARGO_HOME>\bin`, and
        // `bin` ALONE is not the convention — the parent must be `.cargo`, or
        // this would grant `C:\Program Files\Git\usr` and `/usr` on the
        // strength of a directory name.
        detect: &[Detect::DirTail(&[".cargo", "bin"])],
        needs: rust_needs,
        why: "a rustup shim is a launcher: measured 2026-08-18, a sandboxed `cargo` with only \
              `…\\.cargo\\bin` granted dies on `could not create home directory: …\\.rustup`, and \
              with both homes granted it resolves offline and compiles",
    },
    RuntimeProfile {
        id: "python",
        // Either end of the convention: the interpreter itself, or one of the
        // console-script stubs `pip` writes next to it.
        detect: &[
            Detect::Program(&["python*.exe", "pythonw*.exe", "python", "python3"]),
            Detect::DirTail(&["scripts"]),
        ],
        needs: python_needs,
        why: "a pip console-script `.exe` is a stub that loads `python3XX.dll` and the standard \
              library from the install root one directory up — live rc.9, `audit:semgrep` was \
              granted only its `Scripts` directory and exited 1 with BOTH streams empty",
    },
    RuntimeProfile {
        id: "node",
        detect: &[
            Detect::Program(&[
                "node.exe", "node", "npm.cmd", "npx.cmd", "pnpm.cmd", "yarn.cmd", "npm", "npx",
                "pnpm", "yarn",
            ]),
            Detect::DirTail(&["node_modules", ".bin"]),
        ],
        needs: node_needs,
        why: "a `node_modules\\.bin` shim resolves INSIDE the project root (already granted full \
              access) but starts `node.exe`, which does not — and S1 measured npm needing \
              `--preserve-symlinks` to resolve its own shims through the boundary",
    },
    RuntimeProfile {
        id: "java",
        detect: &[Detect::Program(&[
            "java.exe", "javaw.exe", "javac.exe", "jar.exe", "jshell.exe", "javadoc.exe", "java",
            "javac", "pmd.bat", "pmd.cmd", "pmd",
        ])],
        needs: java_needs,
        why: "a JVM cannot start without the runtime image (`lib\\modules`, `conf`, the JNI DLLs) \
              beside its launcher; `pmd.bat` is in this list because of what it STARTS — it is a \
              JVM launcher script, not a special-cased tool",
    },
    RuntimeProfile {
        id: "dotnet",
        detect: &[Detect::Program(&["dotnet.exe", "dotnet"])],
        needs: dotnet_needs,
        why: "S1 measured the .NET SDK working under AppContainer once `DOTNET_CLI_HOME` is \
              redirected into the root; the package cache is the other half, because a restore \
              that cannot read it re-downloads a graph the boundary denies egress for",
    },
    RuntimeProfile {
        id: "go",
        detect: &[Detect::Program(&[
            "go.exe",
            "gofmt.exe",
            "go",
            "gofmt",
            "golangci-lint.exe",
            "golangci-lint",
        ])],
        needs: go_needs,
        why: "S1 verified a full `go build` inside the container with GOCACHE/GOPATH/GOTMPDIR \
              redirected into the root; `golangci-lint` is listed because it drives the same \
              toolchain and writes the same caches",
    },
    RuntimeProfile {
        id: "windows-store-alias",
        detect: &[Detect::DirTail(&["microsoft", "windowsapps"])],
        needs: store_alias_needs,
        why: "S1: the Store interpreter aliases are reparse points in unlistable profile \
              territory — the container's PATH search never resolves them and no grant fixes it, \
              so this row exists ONLY to say so out loud",
    },
];

/// rustup: `<CARGO_HOME>\bin\<shim>.exe`, whose sibling `RUSTUP_HOME` defaults
/// to the same profile directory. An explicitly-set pointer wins over the
/// convention, both halves independently, so a user who moved either home is
/// served by the same rule.
#[cfg_attr(not(windows), allow(dead_code))]
fn rust_needs(p: &Program, m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    let Some(cargo_home) = p.dir.parent() else {
        return n;
    };
    n.state(
        m,
        m.path_var("CARGO_HOME")
            .unwrap_or_else(|| cargo_home.to_path_buf()),
        "CARGO_HOME",
        "the crate cache and registry index this toolchain reads",
    );
    // `<profile>` — the directory `.cargo` sits in, which is where rustup puts
    // `.rustup` too, because both default to the same `$HOME`.
    if let Some(rustup) = m
        .path_var("RUSTUP_HOME")
        .or_else(|| cargo_home.parent().map(|p| p.join(".rustup")))
    {
        n.state(
            m,
            rustup,
            "RUSTUP_HOME",
            "the toolchains the rustup shim resolves rustc through",
        );
    }
    n
}

/// Python: the install root behind a `Scripts` launcher directory, plus the two
/// caches that would otherwise be written outside the boundary.
///
/// The root's grant is **inheritable**, so `Lib`, `DLLs` and `site-packages`
/// under it are covered by that one ACE — listing them as rows of their own
/// would stamp three more durable changes on the user's machine to reach
/// directories the first grant already reaches.
#[cfg_attr(not(windows), allow(dead_code))]
fn python_needs(p: &Program, m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    // The install root: `…\Scripts\tool.exe` → its parent; `…\python.exe` →
    // its own directory, which the engine ALREADY grants, so only the first
    // case asks for anything. That asymmetry is deliberate and it is what keeps
    // this row off `…\Microsoft\WindowsApps\python.exe`: an alias directory is
    // not an install root and must not collect an ACE on the strength of
    // holding a file called `python.exe`.
    let scripts = p.dir_named("scripts");
    let root = if scripts { p.dir.parent() } else { Some(p.dir) };
    if let Some(root) = root {
        // A `Scripts` directory sitting at a volume root would yield the volume
        // itself; the answer there is no grant, not the whole drive. (The
        // refusal screen would catch it too — this keeps the row from ever
        // asking.)
        if root.parent().is_some() {
            if scripts {
                n.tree(
                    m,
                    root.to_path_buf(),
                    "the interpreter root a pip console-script stub loads `python3XX.dll` and the \
                     standard library from",
                );
            }
            // A Windows virtual environment has `Scripts` and `Lib` but no
            // `DLLs` — its interpreter is a shim onto a BASE install named by
            // `pyvenv.cfg`'s `home`, which is not derivable from any path or
            // pointer this rule is allowed to read. Saying so is the whole
            // point: without the base install granted the stub exits silently,
            // which is the exact failure this table was built to stop being
            // invisible.
            if (m.is_dir)(&root.join("Lib")) && !(m.is_dir)(&root.join("DLLs")) {
                n.gap(
                    root.display().to_string(),
                    "this looks like a virtual environment (a `Lib` but no `DLLs`): its BASE \
                     interpreter is named by `pyvenv.cfg`'s `home` and is NOT granted. If the \
                     tool exits with no output, add that directory under \
                     Settings ▸ Sandboxing ▸ extra grants",
                );
            }
        }
    }
    n.scratch(
        "PYTHONPYCACHEPREFIX",
        "pycache",
        "so the interpreter's bytecode cache lands in the sandbox's one writable place instead of \
         being denied beside a read-only standard library",
    );
    n.scratch(
        "PIP_CACHE_DIR",
        "pip",
        "pip's download cache — the real one lives in the profile, which the boundary does not \
         open for writing",
    );
    n
}

/// Node: the runtime a JS tool shim starts, its cache, and the symlink flags S1
/// measured npm needing.
#[cfg_attr(not(windows), allow(dead_code))]
fn node_needs(p: &Program, m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    // `node.exe` itself: its own directory is already granted, so there is
    // nothing to add. Anything else (a `.cmd` shim, a `node_modules\.bin`
    // entry) starts a node that lives somewhere the boundary has never heard
    // of, and the only pointer to it that cImp is allowed to read is npm's own
    // prefix.
    let is_node = p.file == "node.exe" || p.file == "node";
    if !is_node {
        match m
            .dir_var("npm_config_prefix")
            .or_else(|| m.dir_var("NPM_CONFIG_PREFIX"))
        {
            Some(prefix) => n.tree(
                m,
                prefix,
                "the Node runtime this shim starts — npm's global prefix, which is where its \
                 `node.exe` and global packages live",
            ),
            None => n.gap(
                "node.exe",
                "the Node runtime this shim starts could not be inferred (no `npm_config_prefix` \
                 is set). If node is under Program Files it is already readable; otherwise add \
                 its directory under Settings ▸ Sandboxing ▸ extra grants. cImp will NOT grant \
                 `%USERPROFILE%` or a volume root to find it",
            ),
        }
    }
    n.scratch(
        "npm_config_cache",
        "npm",
        "npm's cache — it is written on every install, and the real one lives in the profile the \
         boundary keeps read-only",
    );
    n.literal(
        "NODE_OPTIONS",
        "--preserve-symlinks --preserve-symlinks-main",
        "S1-measured: npm's shims are symlinks, and resolving through them walks the container \
         into the ancestor-canonicalization wall unless node keeps the link path",
    );
    n
}

/// Java: the JDK/JRE tree behind a launcher in `bin`, or the one `JAVA_HOME`
/// already names.
#[cfg_attr(not(windows), allow(dead_code))]
fn java_needs(p: &Program, m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    // Only a real JVM launcher may derive its home from its own layout. A
    // launcher SCRIPT (`pmd.bat`) also lives in a `bin`, and deriving from it
    // would set `JAVA_HOME` to the tool's own directory — which is not merely
    // useless, it is actively wrong.
    let is_jvm = matches!(
        p.file.as_str(),
        "java.exe"
            | "javaw.exe"
            | "javac.exe"
            | "jar.exe"
            | "jshell.exe"
            | "javadoc.exe"
            | "java"
            | "javac"
    );
    let home = if is_jvm && p.dir_named("bin") {
        p.dir.parent().map(Path::to_path_buf)
    } else {
        None
    }
    .or_else(|| m.dir_var("JAVA_HOME"));
    match home {
        Some(home) => n.state(
            m,
            home,
            "JAVA_HOME",
            "the JDK/JRE tree — `lib\\modules` (the runtime image), `conf` and the JNI DLLs \
             beside the launcher, without which a JVM does not start",
        ),
        None => n.gap(
            "JAVA_HOME",
            "this is a JVM launcher and no JDK could be inferred — its own directory is granted \
             but the runtime it starts is not. Set JAVA_HOME, or add the JDK directory under \
             Settings ▸ Sandboxing ▸ extra grants",
        ),
    }
    n
}

/// .NET: the SDK's CLI state moves into the root; the package cache does not.
///
/// **The judgement call, stated.** `NUGET_PACKAGES` gets a read+execute grant
/// on the REAL cache rather than a redirect into the sandbox root, because a
/// redirect makes every restore start from an empty cache — and the boundary
/// denies egress by default, so "start from empty" means "fail". Read-only is
/// enough for restore-from-cache; a restore that must ADD a package fails with
/// a denial the classifier recognizes, which is the honest outcome for an
/// offline boundary. The cost is one first-time ACL walk of the package cache,
/// and it was measured before this row shipped (2026-08-19, this machine):
/// `~\.nuget\packages` is 1,472 files / 859 directories and the Go module cache
/// is 10,463 / 1,402 — both an order of magnitude *below* the `.rustup` tree
/// (54,457 files) whose ~10 s stamp the ladder already absorbs inside
/// [`PREPARE_BACKSTOP`]. A machine with a far larger cache degrades the way any
/// other slow grant does: unsandboxed, loudly, once.
#[cfg_attr(not(windows), allow(dead_code))]
fn dotnet_needs(_p: &Program, m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    if let Some(pkgs) = m.path_var("NUGET_PACKAGES").or_else(|| {
        m.home()
            .map(|h| h.join(".nuget").join("packages"))
    }) {
        n.state(
            m,
            pkgs,
            "NUGET_PACKAGES",
            "the global package cache `dotnet restore` reads — read-only, because redirecting it \
             into the root would make every restore re-download a graph the boundary denies \
             egress for",
        );
    }
    n.scratch(
        "DOTNET_CLI_HOME",
        "dotnet",
        "S1-named: the SDK writes its first-run sentinel and extracted bundles here, and the real \
         one is in the profile the boundary keeps read-only",
    );
    n.literal(
        "DOTNET_CLI_TELEMETRY_OPTOUT",
        "1",
        "the telemetry uploader is the one part of a build that reaches the network; with egress \
         denied its retries would be the loudest thing in the log and none of it is wanted",
    );
    n
}

/// Go: everything Go writes moves into the root; the module cache it READS
/// stays where it is.
#[cfg_attr(not(windows), allow(dead_code))]
fn go_needs(p: &Program, m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    // `go.exe` in a `bin` implies GOROOT one level up. On the standard install
    // that is `C:\Program Files\Go`, which ALL APPLICATION PACKAGES already
    // reads — the grant is then a no-op and costs nothing.
    let is_go = matches!(p.file.as_str(), "go.exe" | "gofmt.exe" | "go" | "gofmt");
    let goroot = if is_go && p.dir_named("bin") {
        p.dir.parent().map(Path::to_path_buf)
    } else {
        None
    }
    .or_else(|| m.dir_var("GOROOT"));
    if let Some(goroot) = goroot {
        if goroot.parent().is_some() {
            n.tree(
                m,
                goroot,
                "the Go toolchain tree — `pkg`, `src` and the compiler binaries the driver execs",
            );
        }
    } else if !is_go {
        n.gap(
            "GOROOT",
            "this tool drives the Go toolchain and no GOROOT could be inferred. If Go is under \
             Program Files it is already readable; otherwise set GOROOT or add the directory \
             under Settings ▸ Sandboxing ▸ extra grants",
        );
    }
    // The module cache is READ by an offline build and Go marks its contents
    // read-only itself, so a read+execute grant on the real one is both
    // sufficient and honest — while GOPATH below still moves, so anything Go
    // WRITES (`go install` output, the build cache) lands inside the root.
    if let Some(modcache) = m.path_var("GOMODCACHE").or_else(|| {
        m.path_var("GOPATH")
            .or_else(|| m.home().map(|h| h.join("go")))
            .map(|g| g.join("pkg").join("mod"))
    }) {
        n.state(
            m,
            modcache,
            "GOMODCACHE",
            "the module cache an offline `go build` resolves its dependencies from — read-only, \
             which is how Go marks it anyway",
        );
    }
    n.scratch(
        "GOCACHE",
        "gocache",
        "S1-verified: the build cache is written on every compile and must be inside the one \
         writable place",
    );
    n.scratch(
        "GOTMPDIR",
        "gotmp",
        "S1-verified: the linker's scratch, for the same reason",
    );
    n.scratch(
        "GOPATH",
        "gopath",
        "S1-verified: everything else Go writes (`go install` output, `bin`) lands here; the \
         module cache is pointed back at the real one by GOMODCACHE above",
    );
    n
}

/// The Store aliases: a row whose entire content is a gap.
#[cfg_attr(not(windows), allow(dead_code))]
fn store_alias_needs(p: &Program, _m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    n.gap(
        p.dir.display().to_string(),
        "an app-execution-alias directory: the alias is a reparse point in unlistable profile \
         territory, so a sandboxed PATH search never resolves it. S1 measured this and found no \
         workaround short of installing a real interpreter — no grant fixes it, and cImp will not \
         open the profile to try",
    );
    n
}

/// One runtime's needs after the screen, ready for the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RuntimeMatch {
    pub runtime: &'static str,
    pub why: &'static str,
    pub needs: RuntimeNeeds,
}

/// **The entry point.** Everything [`RUNTIME_PROFILES`] infers for one program,
/// screened.
///
/// Pure and IO-free — `m` is the whole of the machine this is allowed to see —
/// so the engines can be trusted with it and the tests can drive it on either
/// platform without a filesystem.
///
/// # The screen
///
/// Every path a row produces goes through [`extra_grant_refusal`], the same
/// rules a settings-supplied grant row gets, and a refused path is DROPPED —
/// with a [`RuntimeGap`] the engine records — while every other grant from the
/// same row still applies. An environment pointer that named a refused
/// directory is dropped with it: handing a tool a pointer to a directory the
/// container cannot read only converts a clean failure into a confusing one.
///
/// This is defence in depth and it is deliberate. [`GrantHints`] rows are cImp
/// constants a reviewer approved; these paths are INFERRED from environment
/// variables and directory names, which is exactly the class of input that
/// should not be trusted with a durable inheritable ACE.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn runtime_needs(program: &Path, m: &Machine) -> Vec<RuntimeMatch> {
    let Some(p) = Program::at(program) else {
        return Vec::new();
    };
    let detected: Vec<&'static RuntimeProfile> = RUNTIME_PROFILES
        .iter()
        .filter(|profile| profile.detect.iter().any(|d| d.matches(&p)))
        .collect();
    screened_needs(&detected, &p, m)
}

/// Which runtime profiles **detection** fires for, by id — the raw answer,
/// before any screening drops a refused grant.
///
/// V38 Phase C's declaration/inference cross-check reads this: a manifest that
/// declares `runtime: node` for a program detection sees as `python` is drift
/// worth a row, and the comparison has to be about what fired, not about what
/// survived the screen (a profile whose every grant was refused still fired).
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn inferred_runtime_ids(program: &Path, m: &Machine) -> Vec<&'static str> {
    let _ = m;
    let Some(p) = Program::at(program) else {
        return Vec::new();
    };
    RUNTIME_PROFILES
        .iter()
        .filter(|profile| profile.detect.iter().any(|d| d.matches(&p)))
        .map(|profile| profile.id)
        .collect()
}

/// Which runtime profiles apply to one spawn — inference, a DECLARED profile,
/// or none at all (V38 Phase C's manifest `runtime` field).
///
/// `Profile` takes the row's `id` rather than a path for the reason the
/// manifest field is a closed enum at all: the value selects from a table cImp
/// owns, so the worst a lying manifest achieves is a grant the user can see
/// named at enable time. An id no row carries selects nothing — a manifest from
/// a newer cImp asks for a runtime this build has no rules for, and inventing
/// one would be worse than the gap.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn runtime_matches(select: &RuntimeSelect, program: &Path, m: &Machine) -> Vec<RuntimeMatch> {
    match select {
        RuntimeSelect::Infer => runtime_needs(program, m),
        RuntimeSelect::None => Vec::new(),
        RuntimeSelect::Profile(id) => {
            let Some(p) = Program::at(program) else {
                return Vec::new();
            };
            let declared: Vec<&'static RuntimeProfile> = RUNTIME_PROFILES
                .iter()
                .filter(|profile| profile.id == *id)
                .collect();
            screened_needs(&declared, &p, m)
        }
    }
}

/// The screen every profile's needs pass, whichever way the profile was chosen.
///
/// Split out of [`runtime_needs`] so a DECLARED profile cannot take a shorter
/// path to a grant than an inferred one: the manifest is attacker-controlled
/// input and its declaration selects a row, never a rule.
#[cfg_attr(not(windows), allow(dead_code))]
fn screened_needs(
    profiles: &[&'static RuntimeProfile],
    p: &Program,
    m: &Machine,
) -> Vec<RuntimeMatch> {
    let home = m.home();
    let system_root = m.system_root();
    let mut out: Vec<RuntimeMatch> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for profile in profiles {
        let RuntimeNeeds {
            grants,
            env,
            mut gaps,
        } = (profile.needs)(p, m);
        let mut kept = Vec::new();
        let mut refused: Vec<PathBuf> = Vec::new();
        for g in grants {
            match extra_grant_refusal(&g.dir, home.as_deref(), system_root.as_deref()) {
                Some(why) => {
                    gaps.push(RuntimeGap {
                        what: g.dir.display().to_string(),
                        why,
                    });
                    refused.push(g.dir);
                }
                // Two rows can derive the same directory (a Go tool and `go`
                // itself); one grant, one row.
                None if !seen.contains(&g.dir) => {
                    seen.push(g.dir.clone());
                    kept.push(g);
                }
                None => {}
            }
        }
        let env: Vec<RuntimeVar> = env
            .into_iter()
            .filter(|v| match &v.value {
                RuntimeEnv::Dir(d) => !refused.contains(d),
                _ => true,
            })
            .collect();
        let needs = RuntimeNeeds {
            grants: kept,
            env,
            gaps,
        };
        if !needs.is_empty() {
            out.push(RuntimeMatch {
                runtime: profile.id,
                why: profile.why,
                needs,
            });
        }
    }
    out
}

/// The complete environment the sandbox engine overrides on a child, **in the
/// one order that works**.
///
/// 1. `TEMP`/`TMP` — scratch into the mapped root, the only writable place;
/// 2. `HOME`/`USERPROFILE` — the home redirect, so a child that writes config
///    lands there too and `getcwd` stays shallow;
/// 3. every runtime pointer, LAST.
///
/// **(3) after (2) is the invariant**, not a detail: a toolchain resolving
/// `%USERPROFILE%\.cargo` after the redirect finds an empty scratch directory,
/// so the pointers that undo that must be applied where the redirect cannot
/// reach them. Same last-writer-wins composition the seams use
/// ([`child_env::ChildEnv`]), so a seam that forces one of these itself still
/// loses to the engine — which is correct: the engine is the half that knows
/// what the container can reach.
///
/// A free function here rather than inline in the engine so the order is a
/// property with a test on both platforms, instead of four lines in the middle
/// of a Win32 routine no Linux run ever compiles.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn compose_env_overrides(
    drive_root: &Path,
    matches: &[RuntimeMatch],
) -> Vec<(String, std::ffi::OsString)> {
    let mut out: Vec<(String, std::ffi::OsString)> = Vec::new();
    for name in ["TEMP", "TMP", "HOME", "USERPROFILE"] {
        out.push((name.to_string(), drive_root.as_os_str().to_os_string()));
    }
    let scratch = drive_root.join(SANDBOX_SCRATCH_DIR);
    for m in matches {
        for v in &m.needs.env {
            let value = match &v.value {
                RuntimeEnv::Dir(d) => d.as_os_str().to_os_string(),
                RuntimeEnv::Scratch(sub) => scratch.join(sub).into_os_string(),
                RuntimeEnv::Literal(s) => std::ffi::OsString::from(*s),
            };
            out.push((v.name.to_string(), value));
        }
    }
    out
}

/// Record one runtime need the boundary did **not** meet — once per
/// (seam, runtime, subject) per session, for [`record_grant_refused`]'s reason:
/// it is re-derived on every spawn and a line per spawn would push the rest of
/// this lane out of its retention window.
///
/// `ok = false`: a detected runtime missing half of what it needs is a state the
/// user may have to fix, not a choice they made. It is deliberately NOT a
/// failure of preparation — the child still runs, still sandboxed, and this row
/// is what explains it if the child then dies without a word.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn record_runtime_gap(seam: &str, root: &Path, runtime: &str, what: &str, why: &str) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static EMITTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    if let Ok(mut guard) = EMITTED.lock() {
        let set = guard.get_or_insert_with(HashSet::new);
        if !set.insert(format!("{seam}|{runtime}|{what}")) {
            return;
        }
    }
    record_event(
        seam,
        root,
        "runtime-gap",
        state_target(&format!("{runtime} runtime"), what),
        format!(
            "The {runtime} runtime was detected behind this program, and `{what}` is something it \
             needs inside the sandbox that was NOT provided: {why}. The child still runs, still \
             sandboxed — this row is here so that if it exits without a word, the reason is \
             already written down."
        ),
        false,
    );
}

/// Record that a manifest DECLARED a runtime that inference disagrees with.
///
/// The doc's cross-check, as a row rather than as a tie-break: cImp runs with
/// the declaration (a plugin author knows what their tool is, and inference
/// cannot know a runtime it has never heard of) and says so, because a stale
/// declaration is drift and drift that nothing reports is drift nobody fixes.
///
/// It lands in the **sandbox** lane, not the plugin one, on purpose. The plugin
/// lane is a LOAD lane — manifests that would not parse, identities that
/// collide, a rescan's summary — and this is not a fact about the file; it is a
/// fact about one spawn, whose seam tag (`audit:<tool>`) is what a reader
/// correlates it with. Every other row explaining which grants a child got is
/// here, and splitting the same question across two lanes is how the second one
/// stops being read.
///
/// Once per (seam, runtime pair) per session — [`record_runtime_gap`]'s reason:
/// it is re-derived on every spawn.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn record_runtime_mismatch(
    seam: &str,
    root: &Path,
    subject: &str,
    declared: &str,
    inferred: &[&str],
) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static EMITTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let seen = inferred.join(", ");
    if let Ok(mut guard) = EMITTED.lock() {
        let set = guard.get_or_insert_with(HashSet::new);
        if !first_time(set, format!("{seam}|{declared}|{seen}|{}", subject_key(subject))) {
            return;
        }
    }
    record_event(
        seam,
        root,
        "runtime-mismatch",
        state_target("runtime mismatch", subject),
        runtime_mismatch_body(declared, &seen),
        false,
    );
}

/// The row text [`record_runtime_mismatch`] writes.
///
/// A function rather than an inline `format!` for the reason
/// [`grant_refused_body`] states: these bodies are user-visible prose, and
/// prose nothing can read back is prose nothing can check
/// (`row_texts_read_as_sentences`).
fn runtime_mismatch_body(declared: &str, seen: &str) -> String {
    format!(
        "the manifest declares the `{declared}` runtime; detection recognizes `{seen}` behind \
         this program. cImp ran with the DECLARED profile — a declaration is the author's \
         statement and inference cannot know a runtime it has never met — but the two \
         disagreeing is drift: either the manifest names the wrong runtime, or the tool's \
         layout changed under it. If the tool then fails to start, this is the first row to \
         read."
    )
}

/// Record that a tool ran OUTSIDE the boundary because its manifest declares
/// `sandbox: unsupported`.
///
/// Its own row rather than [`record_skip`]'s, because the two states are not
/// the same: a skip says cImp could not provide the boundary, this says the
/// tool asked not to be inside one and the user granted that by enabling it
/// (the permission summary shows the ask at enable time). The verb stays
/// `unsandboxed` so the feed's existing chip is correct — what changed is the
/// *reason*, which is what the target text carries.
///
/// Once per (seam, subject) per session: a standing fact about a configured
/// tool, not an event.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn record_declared_unsandboxed(seam: &str, root: &Path, subject: &str) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static EMITTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    if let Ok(mut guard) = EMITTED.lock() {
        let set = guard.get_or_insert_with(HashSet::new);
        if !first_time(set, format!("{seam}|{}", subject_key(subject))) {
            return;
        }
    }
    record_event(
        seam,
        root,
        "unsandboxed",
        state_target("declared unsupported", subject),
        declared_unsandboxed_body(subject),
        false,
    );
}

/// The row text [`record_declared_unsandboxed`] writes.
fn declared_unsandboxed_body(subject: &str) -> String {
    format!(
        "{subject} ran OUTSIDE the OS sandbox because its plugin manifest declares \
         `sandbox: unsupported` — the boundary was not attempted, whether or not it was \
         available. That declaration is shown as a permission where the tool is enabled; \
         disabling the tool is the way to withdraw it."
    )
}

/// Record that a tool was NOT RUN because its manifest declares
/// `sandbox: required` and the boundary could not be provided.
///
/// The refusal is the point: `required` is a manifest saying "never run me
/// unprotected", and the honest answer to a missing boundary is a failed tool
/// with a reason, not a quiet unsandboxed run. Deduped per (seam, reason)
/// because the cause is a standing condition (the switch is off, a prerequisite
/// is missing) — the per-run surface is the tool's own error, which is not
/// deduped.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn record_sandbox_required_refusal(seam: &str, root: &Path, subject: &str, why: &str) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static EMITTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    if let Ok(mut guard) = EMITTED.lock() {
        let set = guard.get_or_insert_with(HashSet::new);
        if !first_time(set, format!("{seam}|{why}|{}", subject_key(subject))) {
            return;
        }
    }
    record_event(
        seam,
        root,
        "refused",
        state_target("refused (sandbox required)", subject),
        sandbox_required_refusal_body(subject, why),
        false,
    );
}

/// The row text [`record_sandbox_required_refusal`] writes.
fn sandbox_required_refusal_body(subject: &str, why: &str) -> String {
    format!(
        "{subject} was NOT run: its plugin manifest declares `sandbox: required`, and the OS \
         boundary could not be provided here — {why}. Running it anyway would have delivered \
         findings from a tool the manifest says must never run unprotected, which is a worse \
         outcome than this tool being missing from the report."
    )
}

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
fn lower_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect()
}

/// Is `prefix` the leading run of `comps` (component-wise, so `C:\Users\amirx`
/// does not "start with" `C:\Users\amir`)?
fn starts_with(comps: &[String], prefix: &[String]) -> bool {
    !prefix.is_empty() && comps.len() >= prefix.len() && comps[..prefix.len()] == *prefix
}

/// Does `comps` END in `suffix` (already lowercase, component-wise)?
fn ends_with(comps: &[String], suffix: &[&str]) -> bool {
    comps.len() >= suffix.len()
        && comps[comps.len() - suffix.len()..]
            .iter()
            .zip(suffix)
            .all(|(a, b)| a == b)
}

/// Where a refused grant was ASKED FOR — the fact the refusal row has to carry
/// if the reader is to have anywhere to go and fix it.
///
/// V38 Phase C gave [`GrantRow`] a second population (a tool plugin manifest's
/// `extra_grants`) and kept one row text, which then told every reader of a
/// manifest-sourced refusal that the path "is listed in
/// `sandbox.extra_grant_dirs`" — sending them to hunt for a settings entry that
/// does not exist. The two sources are fixed at the call site and cannot be
/// re-derived from a path, so they travel with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub enum GrantSource {
    /// A `sandbox.extra_grant_dirs` row in cImp's own settings — the user typed
    /// it, and the user is the one who can remove it.
    Settings,
    /// A tool plugin manifest's `extra_grants` entry (V38). The user consented
    /// to it by enabling the tool; the *author* is who would narrow it.
    Manifest,
}

/// Record one refused grant row — once per (seam, source, path) per session,
/// because the row is re-read on every spawn and a line per spawn would push
/// the rest of this lane out of its retention window.
///
/// `ok = false`: a grant that cannot be honored is a state someone has to fix,
/// not a choice they made.
///
/// **Only call this when a boundary is actually being prepared.** A refusal row
/// promises "this path was not granted, everything else was" — which is true
/// inside `prepare` and false when the sandbox is off or the tool declared
/// `sandbox: unsupported`, where NOTHING is granted because there is no
/// container, and the child can read the refused directory freely. Screening
/// the list there is still right (a refused path must never reach a
/// [`GrantRow`]); saying so in the lane is not, and the honest row for that run
/// is the unsandboxed/skip one its seam already mints.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn record_grant_refused(
    seam: &str,
    root: &Path,
    path: &Path,
    why: &str,
    source: GrantSource,
) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static EMITTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    // The source is part of the key: the same directory refused from both a
    // settings row and a manifest is two different things to fix, and deduping
    // them together would silence whichever arrived second.
    let key = format!("{seam}|{source:?}|{}", path.display());
    if let Ok(mut guard) = EMITTED.lock() {
        let set = guard.get_or_insert_with(HashSet::new);
        if !set.insert(key) {
            return;
        }
    }
    record_event(
        seam,
        root,
        "grant-refused",
        state_target("grant refused", &path.display().to_string()),
        grant_refused_body(source, path, why),
        false,
    );
}

/// The row text [`record_grant_refused`] writes, per source.
///
/// Pure and named so the prose is testable. A user-visible sentence assembled
/// inline inside a `format!` argument list is a sentence no test ever reads,
/// which is how three of these shipped with fourteen-space gaps mid-clause
/// (Phase C review, B-C3) — `row_texts_read_as_sentences` now reads all four.
fn grant_refused_body(source: GrantSource, path: &Path, why: &str) -> String {
    match source {
        GrantSource::Settings => format!(
            "`{}` is listed in sandbox.extra_grant_dirs and was NOT granted: {why}. Nothing was \
             written to that directory's ACL. Every other grant was applied and the run \
             continued — one unusable settings row does not switch the boundary off. If a tool \
             genuinely needs something in there, name the narrower directory it actually reads.",
            path.display()
        ),
        GrantSource::Manifest => format!(
            "`{}` is requested by a tool plugin manifest's `extra_grants` and was NOT granted: \
             {why}. Nothing was written to that directory's ACL. Every other grant was applied \
             and the tool still ran — one refused grant does not switch the boundary off. This \
             is NOT a cImp settings row: it comes from the plugin's definition file, so the fix \
             is either the plugin naming the narrower directory it actually reads, or disabling \
             the tool in Settings → Tool Plugins.",
            path.display()
        ),
    }
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
            None,
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
            None,
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
/// # The 2026-08-18 retraction (read this before trusting the old story)
///
/// An earlier rc.9 note recorded here claimed that a process under cImp's
/// AppContainer "cannot create a child process at all". **That is false, and
/// it was measured false on the same machine and build** (Windows 11 Pro
/// 26200.9168) with a harness that reproduces this engine's spawn dance
/// exactly — `CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT |
/// EXTENDED_STARTUPINFO_PRESENT`, the two-attribute list (security
/// capabilities + handle list), piped stdio, a hand-built environment block,
/// the kill-on-close job, the `cimp.worker` profile itself, and a cwd on the
/// mapped drive. Under all of it:
///
/// * a container child spawns grandchildren and great-grandchildren freely
///   (`where.exe`, `cmd.exe`, `cargo.exe` → `rustc.exe` → a build script →
///   `link.exe`);
/// * `cargo --version` / `rustc --version` run **inside** the container once
///   the toolchain's state directory is granted (see [`RUNTIME_PROFILES`]);
/// * the spike S1/S3 results (npm's node grandchild, a token-proven ConPTY
///   grandchild) reproduce unchanged.
///
/// What actually produces the two user-visible messages:
///
/// * `'cargo' is not recognized …` — a genuine PATH-search miss, or the
///   toolchain shim dying before it prints anything of its own;
/// * `Access is denied.` from a sandboxed `cmd.exe` — **not** a refused
///   `CreateProcess`. `cmd` resolves a *drive-qualified* path (`C:\…`, and
///   even `C:x`) through the VOLUME ROOT, and `C:\` carries no
///   `ALL APPLICATION PACKAGES` ACE on a stock install, so
///   `GetVolumeInformation("C:\")` returns error 5 and `cmd` reports that
///   before it creates anything. The same command spelled without a drive
///   (`\Windows\System32\where.exe`, `.\tool.exe`), or by bare name through
///   PATH, or on the sandbox's own mapped drive (whose root IS granted), runs
///   normally. Granting the volume root would need elevation, so the practical
///   rule is: inside the sandbox, spell programs by bare name or on the mapped
///   drive.
///
/// The marker list itself is unchanged — the classification was always right;
/// only the *explanation* was wrong.
///
/// # What the two halves of the fix each removed
///
/// Both causes above are now handled at their own seam, and the markers stay
/// for what is left:
///
/// * the drive-qualified spelling — `checks::sandboxed_raw_tail` never hands a
///   sandboxed `cmd.exe` a program token that designates a drive, and leads the
///   child's `PATH` with the directory the sandbox granted;
/// * the state directory — [`runtime_needs`] grants it and re-asserts its
///   pointer, so a shim cannot resolve its home into the redirected scratch.
///
/// The residual, and why these markers still earn their place: a compound
/// command line's LATER tokens are not rewritten (only the first is resolved
/// and granted at all), and a tool whose own tree cImp has no layout knowledge
/// of still dies exactly this way until the user adds it under
/// `Settings ▸ Sandboxing ▸ extra grants`.
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
/// (the `python` row of [`RUNTIME_PROFILES`]) fixes that particular tool; this
/// row is what makes the
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

/// Whether a sandboxed **shell's** output carries the fingerprint of a program
/// that never started, and the note to hand the user if so.
///
/// See [`PROGRAM_START_DENIAL_MARKERS`] for the measurement — including the
/// retraction of the "no child processes" claim this note used to carry.
/// Programs DO run inside the boundary; what stops them is narrower, and the
/// note now names the two things a user can actually act on: an ungranted
/// toolchain state directory, and a drive-qualified path in a sandboxed shell.
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
        "\n[sandbox: this check ran inside the OS sandbox and a program it invoked did not \
         start. Programs DO run inside the boundary, so this is a reachability problem with a \
         cause: either the tool's own files are not granted (its install dir is granted \
         automatically, its STATE directory only for toolchains cImp knows the layout of), or \
         the command spells a drive-qualified path — a sandboxed `cmd.exe` resolves `C:\\…` \
         through the volume root, which no AppContainer can read on a stock Windows install, \
         and reports `Access is denied.` before starting anything. Spell the program by bare \
         name (PATH works) or by a path on the sandbox's mapped drive, add its directory under \
         Settings ▸ Sandboxing ▸ extra grants, or turn the sandbox off for this run.]",
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

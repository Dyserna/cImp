//! V33 Phase D — the **Linux Landlock engine**.
//!
//! The Windows sibling (`sandbox::windows`) had to hand-roll a `CreateProcessW`
//! because no `std`/`tokio` `Command` can attach an AppContainer attribute list.
//! Linux needs no such thing, and that difference is the whole shape of this
//! file: **the sandboxed Linux path IS the seam's ordinary spawn**, plus two
//! additions applied to the very same `tokio::process::Command` the plain path
//! built —
//!
//! 1. the composed environment (the C2 base, the seam's forced variables, then
//!    this module's redirections last — [`Prepared::compose_env`]), and
//! 2. an `unsafe pre_exec` hook that applies a Landlock ruleset to the child
//!    *after* `fork` and *before* `exec` ([`Prepared::confine`]).
//!
//! Everything else the seam already does — the spawn gate, `process_group(0)`,
//! `kill_on_drop`, `guard_child`, the per-seam timeout and output caps — is
//! untouched, which is why this engine needs no `CancelFlag`, no settle-slack
//! backstop and no bespoke drain machinery. Those exist on Windows because the
//! spawn there is a blocking Win32 dance; here there is nothing extra to wedge.
//!
//! # Where the boundary is, honestly
//!
//! A confined child may **read+write** the project root (and the seam's own
//! write scratch), **read+execute** the OS/toolchain directories and the
//! directory each granted program lives in, and **open device nodes for
//! read/write** under `/dev`. Everything else is denied — including `$HOME`,
//! and therefore `~/.ssh`, other projects and cImp's own state. Same bar spike
//! S1 set on Windows.
//!
//! Three holes are deliberate, named here so nobody discovers them by surprise:
//!
//! * **Toolchain state under `$HOME` is not auto-granted.** `~/.cargo`,
//!   `~/.rustup`, `~/.npm` stay unreadable; a `cargo` probe that needs its
//!   registry surfaces as a denial row and the user opts in with a
//!   `sandbox.extra_grant_dirs` entry. That is the Windows grant ladder's honest
//!   degradation, not an oversight.
//! * **Landlock's network scoping is TCP-only** (ABI 4+): `bind()` and
//!   `connect()` on TCP are denied with `allow_network = false`, but **UDP is
//!   not restricted at all**, so a direct-socket DNS query still leaves the
//!   machine. On a kernel below ABI 4 there is no network confinement
//!   whatsoever. Both facts are printed in the row detail and in the sandbox
//!   lane's posture line rather than papered over — claiming a confinement the
//!   kernel is not providing is the one thing this lane may never do.
//! * **`/tmp` is not granted.** `TMPDIR`/`TEMP`/`TMP`/`HOME` are redirected into
//!   the project root (the one writable place), exactly as the Windows engine
//!   redirects them onto the mapped drive. A tool that hardcodes `/tmp` instead
//!   of honoring `TMPDIR` gets a denial row.
//!
//! # Why half of this file compiles everywhere
//!
//! The dev machine for this project is Windows and Linux correctness rides CI,
//! so everything that *can* be platform-neutral is: the grant ladder
//! ([`grant_list`]), the environment redirection ([`env_overrides`]) and the
//! environment composition are pure functions with their own tests, reviewed and
//! run on every platform. Only the parts that genuinely need the kernel — the
//! ABI probe, the ruleset build and `restrict_self` — are `cfg(target_os =
//! "linux")`.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

// ── the grant ladder (Phase D decision D4) ──────────────────────────────────

/// Directories granted **read+execute**: the OS and its shared libraries, the
/// system configuration a tool reads, and the pseudo-filesystems every runtime
/// stats on startup.
///
/// Data in code with the same discipline as `child_env::CHILD_ENV` and
/// `spawn_ledger::LEDGER`: a reviewer of a diff that widens this list sees
/// exactly what was widened. Each entry is filtered on existence before it
/// becomes a rule (a Landlock rule needs an openable path, and `/lib32` exists
/// on roughly no modern distribution).
///
/// **`/tmp` and `/home` are deliberately absent** — see the module header.
pub const SYSTEM_DIRS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32", "/etc", "/opt", "/run", "/proc", "/sys",
];

/// Directories whose contents are **devices**, granted read+execute *plus*
/// `WriteFile` (and `Truncate`/`IoctlDev` where the ABI has them).
///
/// `/dev` is split out of [`SYSTEM_DIRS`] rather than sharing its read-only
/// tier, and the reason is not hardening theatre: a read-only `/dev` denies
/// `open("/dev/null", O_WRONLY)`, which means `2>/dev/null` — the single most
/// common construct in the shell commands the `run_check` seam runs — fails
/// with a permission error that looks nothing like a sandbox. What the wider
/// tier does *not* include is `MakeReg`/`MakeDir`/`RemoveFile`: the child may
/// open the device nodes that exist, never create or unlink one.
pub const DEVICE_DIRS: &[&str] = &["/dev"];

/// How wide one [`Grant`] opens the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Everything the ABI defines — read, write, create, remove, truncate.
    /// The project root and the seam's own scratch, and nothing else.
    ReadWrite,
    /// Read + execute (the crate's `from_read` set: `Execute`, `ReadFile`,
    /// `ReadDir`). Enough to run a program and read its configuration.
    ReadExecute,
    /// Read + execute + `WriteFile` — see [`DEVICE_DIRS`].
    Devices,
}

impl Tier {
    /// The user-facing name printed beside a path in the grant row.
    pub fn label(self) -> &'static str {
        match self {
            Tier::ReadWrite => "read+write",
            Tier::ReadExecute => "read+execute",
            Tier::Devices => "read+execute, and write to open device nodes",
        }
    }

    /// How wide this tier is. Used to collapse a duplicate path onto its widest
    /// grant — the same directory named twice (a root that is also a settings
    /// `extra_grant_dirs` row) must not end up read-only because the narrower
    /// mention happened to come second.
    fn rank(self) -> u8 {
        match self {
            Tier::ReadWrite => 2,
            Tier::Devices => 1,
            Tier::ReadExecute => 0,
        }
    }
}

/// One reviewed widening of the Linux boundary: an existing path, a tier and
/// the reason it is there. `why` is a fixed string from a closed set rather
/// than free text, so the grant row reads as a table and a new *kind* of grant
/// is a diff a reviewer notices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub path: PathBuf,
    pub tier: Tier,
    pub why: &'static str,
}

/// Build the complete grant list for one child, in the order the rules are
/// added: widest tier first, so the dedup below keeps the widest mention of a
/// path that appears twice.
///
/// `exists` is a parameter rather than a direct `Path::exists` call so the
/// ladder is testable on a machine that has no `/usr` — the same discipline
/// `child_env::minimal_env` uses for its environment lookup and
/// `checks::check_program_hint` uses for PATH resolution. Production passes
/// `|p| p.exists()`.
///
/// Non-existent paths are dropped: a Landlock rule needs a file descriptor, and
/// `path_beneath_rules` would silently skip them anyway — filtering here is
/// what makes the *recorded* list honest about what was actually granted.
pub fn grant_list(
    cfg: &super::SandboxCfg,
    program: &Path,
    hints: &super::GrantHints,
    root: &Path,
    exists: &dyn Fn(&Path) -> bool,
) -> Vec<Grant> {
    let mut out: Vec<Grant> = Vec::new();
    let mut push = |path: &Path, tier: Tier, why: &'static str| {
        if !exists(path) {
            return;
        }
        out.push(Grant {
            path: path.to_path_buf(),
            tier,
            why,
        });
    };

    // (1) The one writable area.
    push(root, Tier::ReadWrite, "the project root");
    for dir in &hints.full_dirs {
        push(
            dir,
            Tier::ReadWrite,
            "cImp-owned scratch a tool writes its report into",
        );
    }
    // (2) Devices — narrower than the root, wider than read-only.
    for dir in DEVICE_DIRS {
        push(Path::new(dir), Tier::Devices, "device nodes");
    }
    // (3) Everything read-only: the OS, then the programs.
    for dir in SYSTEM_DIRS {
        push(
            Path::new(dir),
            Tier::ReadExecute,
            "an OS directory (executables, shared libraries, system configuration)",
        );
    }
    if let Some(install) = program.parent() {
        push(
            install,
            Tier::ReadExecute,
            "the directory the spawned program lives in",
        );
    }
    for extra in &hints.programs {
        if let Some(install) = extra.parent() {
            push(
                install,
                Tier::ReadExecute,
                "inferred from the command line — the tool the seam actually runs",
            );
        }
    }
    // The V33 Phase B reviewed grant TABLE. The three tool seams pass none
    // today (it is the tab seam's per-harness state), but honoring it here
    // rather than ignoring it is what stops a seam that later adds a row from
    // getting a silently narrower boundary on Linux than on Windows.
    for row in &hints.rows {
        push(
            &row.path,
            match row.access {
                super::GrantAccess::ReadExecute => Tier::ReadExecute,
                super::GrantAccess::Full => Tier::ReadWrite,
            },
            row.reason,
        );
    }
    // The user's own grant rows, screened exactly as the Windows engine screens
    // them (`super::extra_grant_refusal`): a credential directory, a profile
    // root, a filesystem root or a relative path is dropped rather than
    // granted. Nothing here is durable the way an ACE is, but a Landlock rule
    // that opens `~/.ssh` to a confined child is the same disclosure while the
    // child runs, and the two engines must not disagree about which rows are
    // honorable. The refusal ROW is minted by `prepare` (see `refused_extras`),
    // because this function is pure.
    for extra in &cfg.extra_grant_dirs {
        if super::extra_grant_refusal_live(extra).is_some() {
            continue;
        }
        push(
            extra,
            Tier::ReadExecute,
            "a reviewed row from the user's sandbox settings",
        );
    }

    // Collapse duplicates onto the widest tier. Landlock's own behaviour for
    // two rules on one path is not something this layer should be relying on,
    // and a reader of the grant row should not see the same directory twice
    // with two different widths.
    let mut kept: Vec<Grant> = Vec::with_capacity(out.len());
    for grant in out {
        match kept.iter_mut().find(|k| k.path == grant.path) {
            Some(existing) => {
                if grant.tier.rank() > existing.tier.rank() {
                    *existing = grant;
                }
            }
            None => kept.push(grant),
        }
    }
    kept
}

/// The grant list rendered for the Events row — one path per line with its
/// width and its reason, exactly like the Windows engine's `granted` list.
pub fn grant_summary(grants: &[Grant]) -> String {
    grants
        .iter()
        .map(|g| format!("{} ({}, {})", g.path.display(), g.tier.label(), g.why))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The scratch/home redirections a confined child gets, pointed at the project
/// root — the only place it can write.
///
/// Mirrors the Windows engine's `env_overrides` with the Unix spellings, and
/// keeps the two Windows names as well: they cost nothing, a cross-compiled or
/// Wine-hosted tool reads them, and dropping them would be a difference between
/// the platforms with no reason behind it.
///
/// `USERPROFILE` is deliberately *not* set: it is Windows' name for `HOME`, and
/// a Linux child that reads it is reading a variable nothing on the platform
/// defines.
pub fn env_overrides(root: &Path) -> Vec<(String, OsString)> {
    let value = root.as_os_str().to_os_string();
    ["TMPDIR", "TEMP", "TMP", "HOME"]
        .iter()
        .map(|name| (name.to_string(), value.clone()))
        .collect()
}

// ── the prepared spawn ──────────────────────────────────────────────────────

/// Everything one confined spawn needs, assembled by [`prepare`].
///
/// Unlike the Windows engine's `Prepared` this owns no OS
/// resource — no drive mapping, no profile, no handle — so dropping it releases
/// nothing and a caller may hold it across an `await` freely. The ruleset file
/// descriptor is created per spawn inside [`confine`](Prepared::confine),
/// because `RulesetCreated::restrict_self` consumes the value and a `Prepared`
/// that could be confined only once would be a footgun aimed at whichever seam
/// grows a retry first.
pub struct Prepared {
    /// What the kernel answered the ABI probe: its own Landlock version.
    ///
    /// A plain `i32` rather than a `landlock::ABI` so this struct — and its
    /// tests — are platform-neutral. It is the RAW answer, un-clamped, because
    /// `landlock::ABI::from(i32)` does the clamping to this binding's ceiling
    /// on the way back in and there is no supported way to turn an `ABI` into a
    /// number again.
    pub(crate) abi_raw: i32,
    /// The ABI this build actually confines with, as the binding names it
    /// (`"V4"`). Rendered once at probe time; the row and the posture line
    /// print it rather than a number, so a kernel newer than the binding is
    /// described by what is being *enforced*, not by what it could support.
    pub(crate) abi_label: String,
    /// The kernel is at or above the newest ABI this binding understands, so
    /// the confinement uses the older, smaller access set. Informational, and
    /// worth saying out loud in the row.
    pub(crate) abi_at_ceiling: bool,
    /// TCP `bind`+`connect` are handled with zero allow rules, i.e. denied.
    /// False when the user granted egress, and false on a kernel below ABI 4 —
    /// which is why it is a stored fact rather than a re-derivation of
    /// `!cfg.allow_network` at the point of use.
    pub(crate) net_denied: bool,
    /// The user's egress setting, kept so the row can state the posture without
    /// re-deriving it from [`Prepared::net_denied`] — which would be wrong on a
    /// kernel below ABI 4, where nothing is denied *and* nothing was allowed.
    pub(crate) allow_network: bool,
    pub(crate) grants: Vec<Grant>,
    pub(crate) env_overrides: Vec<(String, OsString)>,
    /// Row bookkeeping: which seam asked, which root, and what the row's
    /// scannable subject is.
    pub(crate) seam: String,
    pub(crate) root: PathBuf,
    pub(crate) subject: String,
}

impl Prepared {
    /// The final environment a confined child gets, in the order
    /// [`super::child_env::ChildEnv`] documents as the contract: the C2 base,
    /// then the seam's forced variables, then this sandbox's redirections last.
    ///
    /// Platform-neutral on purpose — the ordering is the part that has bitten
    /// this codebase, and it is testable without a kernel.
    pub fn compose_env<K, V>(
        &self,
        base: &[(&str, OsString)],
        seam_env: impl IntoIterator<Item = (K, V)>,
    ) -> Vec<(OsString, OsString)>
    where
        K: AsRef<str>,
        V: Into<OsString>,
    {
        let mut env = super::child_env::ChildEnv::from_base(base);
        env.overlay(seam_env);
        env.overlay(
            self.env_overrides
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone())),
        );
        env.into_pairs()
    }

    /// The one call a seam makes: compose the environment and install the
    /// Landlock hook on the command it was going to spawn anyway.
    ///
    /// `Err` means **do not spawn** — the caller propagates it as a spawn
    /// failure. There is no unconfined fallback here by construction: the
    /// choice between "confined" and "plain" is made in [`super::plan`], before
    /// this point, and is recorded there.
    #[cfg(target_os = "linux")]
    pub fn apply<K, V>(
        &self,
        cmd: &mut tokio::process::Command,
        base: &[(&str, OsString)],
        seam_env: impl IntoIterator<Item = (K, V)>,
    ) -> Result<(), String>
    where
        K: AsRef<str>,
        V: Into<OsString>,
    {
        let env = self.compose_env(base, seam_env);
        self.confine(cmd, env)
    }

    /// What THIS boundary enforces, for its grant row.
    ///
    /// Rendered from the values the ruleset was built from rather than by
    /// re-asking the kernel: a row that describes a different probe than the one
    /// the boundary used is a row that can be wrong.
    pub fn boundary_note(&self) -> String {
        render_posture(
            &self.abi_label,
            self.abi_raw,
            self.abi_at_ceiling,
            self.allow_network,
        )
    }

    /// Install the boundary on `cmd`: replace its environment wholesale and add
    /// the `pre_exec` hook that restricts the child.
    ///
    /// # The fork-safety question, and why the work is split the way it is
    ///
    /// A `pre_exec` closure runs in the child after `fork`, where the process
    /// has one thread but may hold locks another thread was holding at fork
    /// time — allocation is the classic way to deadlock there. Building a
    /// Landlock ruleset **allocates**: `Ruleset::default().handle_access(..)`,
    /// `create()` and every `PathFd::new` behind `path_beneath_rules` build
    /// owned values. So the ruleset is built **here, in the parent**, and the
    /// closure does exactly one thing: `restrict_self`, which is a `prctl` and
    /// a `landlock_restrict_self` syscall with no allocation on either the
    /// success or the failure path (verified against rust-landlock 0.4.7's
    /// `ruleset.rs`). The ruleset file descriptor survives `fork` as a copy of
    /// the parent's, and `restrict_self` consumes the value — so the child's
    /// copy is closed by the drop at the end of the closure, before `exec`
    /// replaces the program image. Nothing leaks into the confined program.
    ///
    /// # Refusing is the only alternative to confining
    ///
    /// Both failure shapes return an error to the parent's `spawn()` rather
    /// than exec'ing:
    ///
    /// * `restrict_self` failed outright;
    /// * it "succeeded" with `RulesetStatus::NotEnforced` — which
    ///   rust-landlock returns *without calling the syscall at all* when its
    ///   compatibility state collapsed. That is the silent-unconfined outcome,
    ///   and it is the one result this engine may never let through.
    ///
    /// The child cannot report a reason back through `pre_exec` (the mechanism
    /// carries one `errno`), so both are `EPERM` and the parent's message is
    /// the seam's ordinary spawn error. `EPERM` is deliberately *not* one of
    /// [`super::denial_signature`]'s markers, so this refusal is never
    /// mislabelled as the child hitting the boundary.
    ///
    /// # Safety
    ///
    /// The closure is `async-signal-safe`: it moves an already-built value out
    /// of an `Option` and makes two syscalls. It allocates nothing, takes no
    /// lock, and touches no state shared with the parent beyond the ruleset fd
    /// it consumes.
    #[cfg(target_os = "linux")]
    pub fn confine(
        &self,
        cmd: &mut tokio::process::Command,
        env: Vec<(OsString, OsString)>,
    ) -> Result<(), String> {
        if self.abi_raw < 1 {
            return Err(self.refuse("the Landlock ABI probe reports no usable version"));
        }
        let ruleset = self.build_ruleset().map_err(|e| self.refuse(&e))?;

        // The confined child gets the composed environment and nothing else —
        // locked decision L4, the same narrowing the Windows engine applies.
        cmd.env_clear();
        for (key, value) in env {
            cmd.env(key, value);
        }

        let mut pending = Some(ruleset);
        // SAFETY: see this function's `# Safety` section — the closure
        // allocates nothing and only consumes a value built above, in the
        // parent, before `fork`.
        unsafe {
            cmd.pre_exec(move || {
                let ruleset = match pending.take() {
                    Some(r) => r,
                    // Unreachable in practice (one `apply` per spawn), and a
                    // refusal rather than an unconfined exec if it ever is not.
                    None => return Err(std::io::Error::from_raw_os_error(libc::EPERM)),
                };
                let status = ruleset
                    .restrict_self()
                    .map_err(|_| std::io::Error::from_raw_os_error(libc::EPERM))?;
                if status.ruleset == landlock::RulesetStatus::NotEnforced {
                    return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                }
                Ok(())
            });
        }
        Ok(())
    }

    /// Build the ruleset for this spawn: handle every access the ABI defines
    /// (so anything not named below is denied), then add one rule per tier.
    #[cfg(target_os = "linux")]
    fn build_ruleset(&self) -> Result<landlock::RulesetCreated, String> {
        use landlock::{
            path_beneath_rules, Access, AccessFs, AccessNet, BitFlags, Ruleset, RulesetAttr,
            RulesetCreatedAttr, RulesetError, ABI,
        };

        let abi = ABI::from(self.abi_raw);
        let all = AccessFs::from_all(abi);
        let read = AccessFs::from_read(abi);
        // Intersected with `all` rather than trusting best-effort adjustment:
        // `Truncate` (ABI 3) and `IoctlDev` (ABI 5) do not exist on an older
        // kernel, and a rule naming an access the ruleset does not handle is a
        // kernel `EINVAL`, not a warning.
        let devices: BitFlags<AccessFs> =
            (read | AccessFs::WriteFile | AccessFs::Truncate | AccessFs::IoctlDev) & all;

        let mut ruleset = Ruleset::default()
            .handle_access(all)
            .map_err(|e| format!("landlock could not handle the filesystem access set: {e}"))?;
        if self.net_denied {
            // Handled with ZERO allow rules — every TCP bind and connect is
            // refused. `handle_access` is only reached when the probe said
            // ABI >= 4, so best-effort has nothing to downgrade here.
            ruleset = ruleset
                .handle_access(AccessNet::from_all(abi))
                .map_err(|e| format!("landlock could not handle the network access set: {e}"))?;
        }
        let mut created = ruleset
            .create()
            .map_err(|e| format!("landlock ruleset creation failed: {e}"))?;

        for tier in [Tier::ReadWrite, Tier::Devices, Tier::ReadExecute] {
            let paths: Vec<&Path> = self
                .grants
                .iter()
                .filter(|g| g.tier == tier)
                .map(|g| g.path.as_path())
                .collect();
            if paths.is_empty() {
                continue;
            }
            let access = match tier {
                Tier::ReadWrite => all,
                Tier::Devices => devices,
                Tier::ReadExecute => read,
            };
            created = created
                .add_rules(path_beneath_rules(paths, access))
                .map_err(|e: RulesetError| {
                    format!("landlock rejected the {} rules: {e}", tier.label())
                })?;
        }
        Ok(created)
    }

    /// Record a refusal in the sandbox lane and render the caller's error.
    ///
    /// Every caller of this is a path that will NOT run the child, which is the
    /// deliberate half of Phase D decision D3: between "confined" and "error"
    /// we choose error, because the third option — running the agent's command
    /// with the boundary quietly missing — is the failure mode the whole lane
    /// exists to make impossible.
    #[cfg(target_os = "linux")]
    fn refuse(&self, why: &str) -> String {
        super::record_event(
            &self.seam,
            &self.root,
            "refused",
            super::state_target("refused", &self.subject),
            format!(
                "the Landlock boundary could not be applied ({why}). `{}` was NOT run — \
                 refusing rather than silently dropping the sandbox boundary.",
                self.subject
            ),
            false,
        );
        format!("sandbox: {why} — `{}` was not run", self.subject)
    }
}

// ── the kernel probe ────────────────────────────────────────────────────────

/// `LANDLOCK_CREATE_RULESET_VERSION` — the flag that turns
/// `landlock_create_ruleset` from "make me a ruleset" into "tell me which ABI
/// you speak". With it, the call takes a null attribute pointer, a zero size,
/// creates nothing and returns the version as a positive integer.
#[cfg(target_os = "linux")]
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;

/// What one ABI probe learned about the running kernel.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
struct Probe {
    /// The kernel's own version number, as the syscall returned it.
    raw: i32,
    /// How this binding names the version it will confine with (`"V4"`).
    ///
    /// A `Debug`-rendered [`landlock::ABI`] rather than a number, because there
    /// is no `From<ABI> for i32` and casting a foreign `#[non_exhaustive]` enum
    /// back to its discriminant is not a contract this layer should lean on.
    label: String,
    /// The kernel is at or beyond this binding's newest known ABI.
    at_ceiling: bool,
}

/// The running kernel's Landlock ABI.
///
/// # Why this is a raw syscall and not a crate call
///
/// rust-landlock performs exactly this probe internally
/// (`LandlockStatus::current`) and the comment above it reads "Must remain
/// private" — the crate deliberately exposes **no** public way to ask the
/// kernel its ABI, because its own model is "request a maximum, let best-effort
/// downgrade, then read `RestrictionStatus` afterwards". That model cannot
/// serve this layer: the status is only knowable *inside the restricted
/// process*, and what [`super::plan`] has to decide — sandbox or loudly-plain —
/// is a decision the parent must make before it forks anything. So the version
/// query is issued here, in fifteen lines of `libc`, and the crate's public
/// `From<i32> for ABI` does the clamping. The syscall's contract (`landlock(7)`,
/// stable since 5.13) is the part being depended on, not an internal.
#[cfg(target_os = "linux")]
fn probe() -> Result<Probe, String> {
    // SAFETY: the version query is defined to take a null attribute pointer and
    // a zero size; it only reads kernel state and creates no descriptor.
    let raw = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if raw < 0 {
        let err = std::io::Error::last_os_error();
        return Err(match err.raw_os_error() {
            Some(libc::ENOSYS) => "this kernel has no Landlock support (needs Linux 5.13 or \
                                   newer, built with CONFIG_SECURITY_LANDLOCK)"
                .to_string(),
            Some(libc::EOPNOTSUPP) => "this kernel has Landlock but it is not enabled — it must \
                                       be listed in the `lsm=` boot parameter"
                .to_string(),
            _ => format!("the Landlock ABI probe failed ({err})"),
        });
    }
    let kernel = raw as i32;
    if kernel < 1 {
        return Err(format!(
            "the Landlock ABI probe returned {kernel}, which names no usable version"
        ));
    }
    // `From<i32> for ABI` clamps to the newest ABI this binding understands, so
    // a kernel ahead of us confines with the older (smaller) access set rather
    // than failing — and says so. `ABI::from(i32::MAX)` IS that ceiling, which
    // is how "the kernel is ahead of us" is detected without a numeric cast.
    let effective = landlock::ABI::from(kernel);
    Ok(Probe {
        raw: kernel,
        label: format!("{effective:?}"),
        at_ceiling: effective == landlock::ABI::from(i32::MAX),
    })
}

/// The probe, once per process. Called from the row/posture path as well as
/// from [`prepare`], and the answer cannot change while the kernel is running.
#[cfg(target_os = "linux")]
fn cached_probe() -> &'static Result<Probe, String> {
    static PROBE: std::sync::OnceLock<Result<Probe, String>> = std::sync::OnceLock::new();
    PROBE.get_or_init(probe)
}

// ── how a live test behaves on a kernel that cannot sandbox ─────────────────

/// The environment variable by which a runner **promises** that this machine
/// can sandbox. Set by the `test-linux` job in `.github/workflows/tests.yml`.
#[cfg(test)]
pub(crate) const EXPECT_LANDLOCK_VAR: &str = "CIMP_EXPECT_LANDLOCK";

/// The skip-or-fail decision and the text that goes with it, as a pure
/// function: `Ok` is the line a skip prints, `Err` is the message a broken
/// promise panics with.
///
/// Split out from [`skip_or_fail`] for the reason `spawn_ledger`'s
/// `audit_against` is split out from its tripwire: **a tripwire whose failure
/// path is never exercised is an assumption, not a test.** On the CI runner the
/// promise is kept, so the panic branch never fires there — and would never
/// fire anywhere until the day it matters, which is far too late to discover
/// that the message was wrong or the condition inverted. Being pure, it is
/// tested in both directions on every platform, including this Windows dev box
/// where no Landlock exists at all.
#[cfg(test)]
pub(crate) fn skip_or_fail_verdict(promised: bool, test: &str, reason: &str) -> Result<String, String> {
    if promised {
        return Err(format!(
            "{EXPECT_LANDLOCK_VAR} is set, so this environment promised a kernel with usable \
             Landlock — but the ABI probe says: {reason}.\n`{test}` therefore verified NOTHING. \
             Either the runner/container lost Landlock (fix the environment), or this machine \
             genuinely cannot sandbox and the promise must be withdrawn by removing \
             {EXPECT_LANDLOCK_VAR} from that job's `env:` in .github/workflows/tests.yml — in a \
             diff someone reviews, never by letting a skip go on passing as a pass."
        ));
    }
    Ok(format!(
        "SKIPPED `{test}` (no usable Landlock on this kernel): {reason}\n  \
         Set {EXPECT_LANDLOCK_VAR}=1 to make this a failure instead of a skip."
    ))
}

/// What a live test does when the kernel has no usable Landlock — one policy,
/// in one place, because two copies of it would drift and only one of them
/// would be the one that mattered.
///
/// Lives at module level rather than inside `mod tests` because the sandbox
/// layer's own `plan()` test needs it too, and a second copy over there is
/// exactly the drift this function exists to prevent.
///
/// # Why a `println!` alone was not enough
///
/// That was the first shape of this, and it was a quality signal with no
/// consumer. `cargo test` **captures the stdout of passing tests**, so in a CI
/// log a skip and a genuinely exercised pass are byte-identical: `test … ok`.
/// The Landlock tests would have gone on reporting green forever if a runner
/// image, a container flag or a kernel `lsm=` list ever dropped Landlock —
/// which is the precise regression they exist to catch.
///
/// So the skip is conditional on who is asking:
///
/// * [`EXPECT_LANDLOCK_VAR`] set ⇒ the environment PROMISED a Landlock kernel
///   and did not deliver. That is a broken promise about the test environment,
///   not a missing feature, and it **panics** — a red run, visible without
///   anyone having to read the log.
/// * unset ⇒ a developer's laptop, an old kernel, a container. Print and skip,
///   which is the honest answer there.
///
/// The variable is checked rather than "am I on CI?", because CI is not the
/// claim being made. A local `CIMP_EXPECT_LANDLOCK=1 cargo test` is a perfectly
/// good way to demand the strict behaviour, and a future runner that genuinely
/// cannot sandbox is opted out by editing one line of YAML — deliberately, in a
/// diff someone reviews, rather than by a green run nobody questioned.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn skip_or_fail(test: &str, reason: &str) {
    let promised = std::env::var_os(EXPECT_LANDLOCK_VAR).is_some();
    match skip_or_fail_verdict(promised, test, reason) {
        Ok(note) => println!("{note}"),
        Err(broken_promise) => panic!("{broken_promise}"),
    }
}

/// The Landlock half of [`super::posture`]: what this kernel is actually
/// enforcing, in the row detail beside every confirmation and every denial.
///
/// Spells out the two ways the network is *not* confined, because a posture
/// line that says only "network=off" would read as a promise Landlock does not
/// make (UDP is never restricted; below ABI 4 nothing is).
#[cfg(target_os = "linux")]
pub fn posture_note(allow_network: bool) -> String {
    match cached_probe() {
        Err(e) => format!(", landlock unavailable ({e})"),
        Ok(probe) => format!(
            ", {}",
            render_posture(&probe.label, probe.raw, probe.at_ceiling, allow_network)
        ),
    }
}

/// What the kernel is enforcing, as one sentence.
///
/// Takes the facts rather than reading them, so the two callers cannot drift:
/// [`posture_note`] renders the *current kernel's* answer for a row that has no
/// [`Prepared`] to hand, and [`Prepared::boundary_note`] renders the answer the
/// boundary in question was actually built from. Platform-neutral, so the
/// sentence a user reads is reviewed and tested everywhere.
pub fn render_posture(
    abi_label: &str,
    abi_raw: i32,
    at_ceiling: bool,
    allow_network: bool,
) -> String {
    format!(
        "landlock ABI {abi_label}{}, {}",
        if at_ceiling {
            format!(" (this build's newest known ABI; the kernel reports {abi_raw})")
        } else {
            String::new()
        },
        network_note(allow_network, abi_raw)
    )
}

/// What the boundary does — and does not — do to the network, in one clause.
///
/// Split out and platform-neutral so the sentence a user reads is reviewable
/// (and testable) without a kernel: the failure mode this guards against is a
/// posture line that implies confinement the kernel is not providing.
pub fn network_note(allow_network: bool, abi_raw: i32) -> &'static str {
    if allow_network {
        "network not restricted (the user granted egress)"
    } else if abi_raw >= 4 {
        "TCP bind+connect denied — UDP, and therefore direct-socket DNS, is NOT restricted by \
         Landlock"
    } else {
        "network NOT confined: this kernel is below Landlock ABI 4 and cannot scope sockets at all"
    }
}

/// Decide and prepare one confined child, or return the user-facing reason
/// [`super::plan`] turns into a loud `Unavailable`.
///
/// Cheap by construction — one syscall and a handful of `stat`s — so unlike the
/// Windows engine there is nothing here to move onto a blocking thread and
/// nothing that has ever needed the caller-side `PREPARE_BACKSTOP` it still
/// runs under.
#[cfg(target_os = "linux")]
pub async fn prepare(
    cfg: &super::SandboxCfg,
    seam: &str,
    program: &Path,
    hints: &super::GrantHints,
    root: &Path,
    _env: &[(&str, OsString)],
) -> Result<Prepared, String> {
    let probe = cached_probe().clone()?;
    if !root.exists() {
        return Err(format!(
            "the project root {} does not exist, so it cannot be granted",
            root.display()
        ));
    }
    // Screened out of the grant list above; recorded here, where there is a
    // seam and a root to record against.
    for extra in &cfg.extra_grant_dirs {
        if let Some(why) = super::extra_grant_refusal_live(extra) {
            super::record_grant_refused(seam, root, extra, why);
        }
    }
    let grants = grant_list(cfg, program, hints, root, &|p| p.exists());
    let prepared = Prepared {
        abi_raw: probe.raw,
        abi_label: probe.label,
        abi_at_ceiling: probe.at_ceiling,
        // ABI 4 (Linux 6.7) is where TCP scoping arrives. Below it the honest
        // answer is "no network confinement", stated in the posture rather than
        // implied by the switch being off.
        net_denied: !cfg.allow_network && probe.raw >= 4,
        allow_network: cfg.allow_network,
        env_overrides: env_overrides(root),
        seam: seam.to_string(),
        root: root.to_path_buf(),
        subject: super::program_subject(program),
        grants,
    };
    record_grants(&prepared);
    Ok(prepared)
}

/// Record the grant list once per distinct list per seam.
///
/// The Windows engine records a grant because it *changed the machine* (an
/// inheritable ACE on a directory is durable). Nothing here is durable — the
/// rules live and die with the child — but the row answers the same question,
/// which is the one a user actually asks when a tool fails: *what could that
/// child reach?* Deduped on the list itself, so a repeated spawn is silent and
/// a widened boundary (a new `extra_grant_dirs` row, a different toolchain
/// directory inferred) speaks up.
#[cfg(target_os = "linux")]
fn record_grants(prepared: &Prepared) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static EMITTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

    let summary = grant_summary(&prepared.grants);
    let key = format!("{}|{}", prepared.seam, summary);
    if let Ok(mut guard) = EMITTED.lock() {
        let set = guard.get_or_insert_with(HashSet::new);
        if !set.insert(key) {
            return;
        }
    }
    super::record_event(
        &prepared.seam,
        &prepared.root,
        "grant",
        format!("{} sandbox grant(s) applied", prepared.grants.len()),
        format!(
            "{summary}\n\nEverything else is denied, including $HOME (and therefore ~/.ssh, \
             other projects, and toolchain state such as ~/.cargo — grant those deliberately \
             with sandbox.extra_grant_dirs if a tool needs them). Enforced by {}.",
            prepared.boundary_note()
        ),
        true,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grant list built against a synthetic filesystem where everything the
    /// caller names exists — the pure ladder, on any platform.
    fn ladder(cfg: &super::super::SandboxCfg, hints: &super::super::GrantHints) -> Vec<Grant> {
        grant_list(
            cfg,
            Path::new("/usr/bin/git"),
            hints,
            Path::new("/proj"),
            &|_| true,
        )
    }

    fn tier_of(grants: &[Grant], path: &str) -> Option<Tier> {
        grants
            .iter()
            .find(|g| g.path == Path::new(path))
            .map(|g| g.tier)
    }

    /// The ladder, as a test rather than a comment: one writable area, devices
    /// in their own tier, the OS read-only, and the program's own directory.
    #[test]
    fn the_grant_ladder_has_exactly_three_widths() {
        let cfg = super::super::SandboxCfg {
            enabled: true,
            allow_network: false,
            extra_grant_dirs: vec![PathBuf::from("/opt/tools")],
        };
        let hints = super::super::GrantHints {
            programs: vec![PathBuf::from("/home/me/.cargo/bin/cargo")],
            full_dirs: vec![PathBuf::from("/proj/.cimp/reports")],
            rows: Vec::new(),
        };
        let grants = ladder(&cfg, &hints);

        // (1) writable: the root and the seam's own scratch — and nothing else.
        assert_eq!(tier_of(&grants, "/proj"), Some(Tier::ReadWrite));
        assert_eq!(
            tier_of(&grants, "/proj/.cimp/reports"),
            Some(Tier::ReadWrite)
        );
        let writable: Vec<_> = grants
            .iter()
            .filter(|g| g.tier == Tier::ReadWrite)
            .map(|g| g.path.clone())
            .collect();
        assert_eq!(writable.len(), 2, "only the root and the scratch: {writable:?}");

        // (2) devices.
        assert_eq!(tier_of(&grants, "/dev"), Some(Tier::Devices));

        // (3) read+execute: the OS, the spawned program's dir, the inferred
        // tool's dir, and the user's own reviewed rows.
        for dir in ["/usr", "/bin", "/etc", "/proc"] {
            assert_eq!(tier_of(&grants, dir), Some(Tier::ReadExecute), "{dir}");
        }
        assert_eq!(tier_of(&grants, "/usr/bin"), Some(Tier::ReadExecute));
        assert_eq!(
            tier_of(&grants, "/home/me/.cargo/bin"),
            Some(Tier::ReadExecute),
            "the check seam's inferred tool directory must be granted"
        );
        assert_eq!(tier_of(&grants, "/opt/tools"), Some(Tier::ReadExecute));
    }

    /// **The settings rows are screened here too** (V33, 2026-08-18). The
    /// Windows engine refuses a credential/system/root grant row before it
    /// stamps an ACE; a Landlock rule is not durable the way an ACE is, but it
    /// is the same disclosure while the child runs, and the two engines must
    /// not disagree about which rows are honorable. The refused row is dropped
    /// from the list — the others still apply, because one bad settings row
    /// must not brick the sandbox.
    #[test]
    fn a_refused_settings_row_is_dropped_and_the_rest_still_granted() {
        let cfg = super::super::SandboxCfg {
            enabled: true,
            allow_network: false,
            extra_grant_dirs: vec![
                PathBuf::from("/opt/tools"),
                // A credential store named by a settings file — refused
                // whatever named it (`super::extra_grant_refusal`). Not under
                // `$HOME`, so this fires on the table rule alone and does not
                // depend on the runner's own profile path.
                PathBuf::from("/srv/state/.ssh"),
                // A rootless row would resolve against cImp's own cwd.
                PathBuf::from("relative/tools"),
            ],
        };
        let grants = ladder(&cfg, &super::super::GrantHints::default());
        assert_eq!(
            tier_of(&grants, "/opt/tools"),
            Some(Tier::ReadExecute),
            "a healthy row is still granted"
        );
        for refused in ["/srv/state/.ssh", "relative/tools"] {
            assert_eq!(tier_of(&grants, refused), None, "{refused} must be dropped");
        }
    }

    /// **The read-denial bar, as an assertion.** `$HOME` is not a grant, and
    /// neither is the toolchain state inside it — that is the honest
    /// degradation the module header promises, and the thing a well-meaning
    /// "make cargo work" patch would quietly delete.
    #[test]
    fn the_users_home_is_never_granted() {
        let cfg = super::super::SandboxCfg {
            enabled: true,
            allow_network: false,
            extra_grant_dirs: Vec::new(),
        };
        let hints = super::super::GrantHints {
            programs: vec![PathBuf::from("/home/me/.cargo/bin/cargo")],
            ..Default::default()
        };
        let grants = ladder(&cfg, &hints);
        for denied in ["/home", "/home/me", "/home/me/.ssh", "/home/me/.cargo", "/tmp"] {
            assert!(
                tier_of(&grants, denied).is_none(),
                "{denied} must not be granted; the ladder produced {grants:?}"
            );
        }
        // …and the inference above grants the BIN directory only, not the state
        // directory beside it.
        assert_eq!(
            tier_of(&grants, "/home/me/.cargo/bin"),
            Some(Tier::ReadExecute)
        );
    }

    /// A Landlock rule needs an openable path, so a path that does not exist is
    /// dropped — and dropped from the RECORDED list too, which is what keeps
    /// the grant row an account of what was actually granted rather than of
    /// what was hoped for.
    #[test]
    fn paths_that_do_not_exist_are_dropped() {
        let cfg = super::super::SandboxCfg {
            enabled: true,
            allow_network: false,
            extra_grant_dirs: vec![PathBuf::from("/opt/nope")],
        };
        let grants = grant_list(
            &cfg,
            Path::new("/usr/bin/git"),
            &super::super::GrantHints::default(),
            Path::new("/proj"),
            &|p| p == Path::new("/proj") || p == Path::new("/usr"),
        );
        let paths: Vec<_> = grants.iter().map(|g| g.path.clone()).collect();
        assert_eq!(
            paths,
            vec![PathBuf::from("/proj"), PathBuf::from("/usr")],
            "only the paths the filesystem confirmed"
        );
    }

    /// One path named twice keeps the WIDEST width. The shape that produces it
    /// in the field: a user adds the project root to `extra_grant_dirs` (a
    /// read-only tier) and the child then cannot write its own tree.
    #[test]
    fn a_duplicate_path_keeps_its_widest_grant() {
        let cfg = super::super::SandboxCfg {
            enabled: true,
            allow_network: false,
            extra_grant_dirs: vec![PathBuf::from("/proj"), PathBuf::from("/dev")],
        };
        let grants = ladder(&cfg, &super::super::GrantHints::default());
        assert_eq!(tier_of(&grants, "/proj"), Some(Tier::ReadWrite));
        assert_eq!(tier_of(&grants, "/dev"), Some(Tier::Devices));
        let roots = grants.iter().filter(|g| g.path == Path::new("/proj")).count();
        assert_eq!(roots, 1, "a path must appear once: {grants:?}");
    }

    /// The Phase B grant TABLE is honored on Linux too — a seam that adds a row
    /// must not silently get a narrower boundary here than on Windows.
    #[test]
    fn reviewed_grant_rows_are_honored() {
        let hints = super::super::GrantHints {
            rows: vec![
                super::super::GrantRow {
                    path: PathBuf::from("/home/me/.claude.json"),
                    access: super::super::GrantAccess::Full,
                    is_file: true,
                    reason: "the harness rewrites its own session file",
                    required: false,
                },
                super::super::GrantRow {
                    path: PathBuf::from("/home/me/.config/git"),
                    access: super::super::GrantAccess::ReadExecute,
                    is_file: false,
                    reason: "git configuration",
                    required: false,
                },
            ],
            ..Default::default()
        };
        let grants = ladder(&super::super::SandboxCfg::disabled(), &hints);
        assert_eq!(
            tier_of(&grants, "/home/me/.claude.json"),
            Some(Tier::ReadWrite)
        );
        assert_eq!(
            tier_of(&grants, "/home/me/.config/git"),
            Some(Tier::ReadExecute)
        );
    }

    /// The tables themselves: absolute, unique, and `/tmp`-free.
    #[test]
    fn the_system_tables_are_well_formed() {
        let mut seen: Vec<&str> = Vec::new();
        for dir in SYSTEM_DIRS.iter().chain(DEVICE_DIRS.iter()) {
            assert!(dir.starts_with('/'), "{dir} is not an absolute Unix path");
            assert!(!dir.ends_with('/'), "{dir} has a trailing slash");
            assert!(!seen.contains(dir), "{dir} is listed twice");
            seen.push(dir);
        }
        // The two lists are disjoint: `/dev` is in exactly one tier, and if it
        // ever appears in both, the dedup would silently pick one.
        for dir in DEVICE_DIRS {
            assert!(
                !SYSTEM_DIRS.contains(dir),
                "{dir} is in both tiers — pick one"
            );
        }
        // Deliberate absences (module header). A patch that adds either is a
        // patch that has to explain itself here first.
        for absent in ["/tmp", "/home", "/root", "/var/tmp"] {
            assert!(
                !SYSTEM_DIRS.contains(&absent) && !DEVICE_DIRS.contains(&absent),
                "{absent} must not be granted by default"
            );
        }
    }

    /// The redirection points every scratch and home name at the one writable
    /// place, and does not invent a Windows-only name for a Linux child.
    #[test]
    fn the_environment_redirect_points_at_the_root() {
        let overrides = env_overrides(Path::new("/proj"));
        let names: Vec<&str> = overrides.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["TMPDIR", "TEMP", "TMP", "HOME"]);
        for (_, value) in &overrides {
            assert_eq!(value, &OsString::from("/proj"));
        }
        assert!(
            !names.contains(&"USERPROFILE"),
            "USERPROFILE is Windows' name for HOME; a Linux child reads nothing from it"
        );
    }

    /// **The composition order, on the Linux path.** The sandbox's redirections
    /// are last and must beat both the C2 base and the seam's own forced
    /// variables — a child whose `TMPDIR` still points outside the root writes
    /// where it cannot write.
    #[test]
    fn the_sandbox_redirections_win_over_the_seam_and_the_base() {
        let prepared = Prepared {
            abi_raw: 4,
            abi_label: "V4".to_string(),
            abi_at_ceiling: false,
            net_denied: true,
            allow_network: false,
            grants: Vec::new(),
            env_overrides: env_overrides(Path::new("/proj")),
            seam: super::super::SEAM_RUN_COMMAND.to_string(),
            root: PathBuf::from("/proj"),
            subject: "git".to_string(),
        };
        let base = vec![
            ("PATH", OsString::from("/usr/bin")),
            ("HOME", OsString::from("/home/me")),
            ("TMPDIR", OsString::from("/tmp")),
        ];
        let composed = prepared.compose_env(
            &base,
            [
                ("CI".to_string(), "1".to_string()),
                // …a seam that sets its own scratch is overruled too.
                ("TMPDIR".to_string(), "/seam/tmp".to_string()),
            ],
        );
        let get = |name: &str| {
            composed
                .iter()
                .find(|(k, _)| k == &OsString::from(name))
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("HOME"), Some(OsString::from("/proj")));
        assert_eq!(get("TMPDIR"), Some(OsString::from("/proj")));
        assert_eq!(get("PATH"), Some(OsString::from("/usr/bin")));
        assert_eq!(get("CI"), Some(OsString::from("1")));
        // Every name exactly once — a duplicate leaves which one wins to the
        // child's libc.
        let mut names: Vec<String> = composed
            .iter()
            .map(|(k, _)| k.to_string_lossy().to_ascii_lowercase())
            .collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "a name was emitted twice: {composed:?}");
    }

    /// The grant row's text names every path, its width and its reason — the
    /// row is the only place a user can answer "what could that child reach?".
    #[test]
    fn the_grant_summary_names_path_width_and_reason() {
        let grants = vec![
            Grant {
                path: PathBuf::from("/proj"),
                tier: Tier::ReadWrite,
                why: "the project root",
            },
            Grant {
                path: PathBuf::from("/usr"),
                tier: Tier::ReadExecute,
                why: "an OS directory",
            },
        ];
        let summary = grant_summary(&grants);
        assert!(summary.contains("/proj (read+write, the project root)"), "{summary}");
        assert!(summary.contains("/usr (read+execute, an OS directory)"), "{summary}");
        assert_eq!(summary.lines().count(), 2);
    }

    /// **The network claim, in three states — and none of them says "confined"
    /// without meaning it.** Landlock scopes TCP only, so even the strongest
    /// state has to name UDP as the hole; below ABI 4 there is no scoping at
    /// all and the sentence must say so rather than letting `network=off` imply
    /// otherwise. Platform-neutral so this honesty is reviewed everywhere.
    #[test]
    fn the_network_note_never_overstates_the_confinement() {
        let denied = network_note(false, 4);
        assert!(denied.contains("TCP"), "{denied}");
        assert!(
            denied.contains("UDP") && denied.contains("NOT restricted"),
            "the TCP-only hole must be named: {denied}"
        );
        let too_old = network_note(false, 3);
        assert!(
            too_old.contains("NOT confined") && too_old.contains("ABI 4"),
            "a pre-4 kernel must say the network is not confined: {too_old}"
        );
        let granted = network_note(true, 9);
        assert!(granted.contains("not restricted"), "{granted}");
        // The three states are three different sentences — collapsing any two
        // is how a user reads "off" as "confined".
        assert_ne!(denied, too_old);
        assert_ne!(denied, granted);
        assert_ne!(too_old, granted);
    }

    /// The posture sentence is rendered from FACTS THE CALLER SUPPLIES, so the
    /// grant row (which passes the boundary's own values) and the generic
    /// posture line (which passes a fresh probe) cannot describe two different
    /// kernels. A kernel ahead of this binding must say which ABI is actually
    /// being enforced, not which one the kernel could support.
    #[test]
    fn the_posture_sentence_names_what_is_enforced_not_what_is_available() {
        let current = render_posture("V4", 4, false, false);
        assert!(current.starts_with("landlock ABI V4,"), "{current}");
        assert!(!current.contains("kernel reports"), "{current}");

        let ahead = render_posture("V9", 12, true, false);
        assert!(ahead.starts_with("landlock ABI V9 "), "{ahead}");
        assert!(
            ahead.contains("kernel reports 12"),
            "a kernel ahead of this build must be stated: {ahead}"
        );
        // …and the enforced ABI leads, because that is the one that is true.
        assert!(
            ahead.find("V9").unwrap() < ahead.find("12").unwrap(),
            "{ahead}"
        );
    }

    /// A `Prepared` describes ITS OWN boundary — the row must not re-ask the
    /// kernel, or a row could describe a probe the ruleset was never built from.
    #[test]
    fn a_boundary_note_is_rendered_from_the_boundarys_own_facts() {
        let prepared = Prepared {
            abi_raw: 3,
            abi_label: "V3".to_string(),
            abi_at_ceiling: false,
            net_denied: false,
            allow_network: false,
            grants: Vec::new(),
            env_overrides: Vec::new(),
            seam: super::super::SEAM_RUN_COMMAND.to_string(),
            root: PathBuf::from("/proj"),
            subject: "git".to_string(),
        };
        let note = prepared.boundary_note();
        assert!(note.contains("V3"), "{note}");
        // ABI 3 has no network scoping, and `net_denied` is false for exactly
        // that reason — the row must say so rather than implying confinement.
        assert!(note.contains("NOT confined"), "{note}");
    }

    /// **The skip policy, in both directions, on every platform.**
    ///
    /// The panic branch is the one that will never fire on a healthy CI runner,
    /// which is exactly why it is tested here rather than trusted: the day it
    /// does fire is the day someone needs its message to be right. Runs on this
    /// Windows dev box too, where no Landlock exists at all — the policy is
    /// text, and text is reviewable anywhere.
    #[test]
    fn a_broken_landlock_promise_fails_and_an_unpromised_kernel_only_skips() {
        // Promise kept? Not this function's business — it is asked only when
        // the kernel came up short. Unpromised ⇒ a skip, and the note has to
        // say how to make it strict, or nobody ever will.
        let skip = skip_or_fail_verdict(false, "some_live_test", "ENOSYS")
            .expect("an unpromised kernel must skip, not fail");
        assert!(skip.starts_with("SKIPPED `some_live_test`"), "{skip}");
        assert!(skip.contains("ENOSYS"), "{skip}");
        assert!(skip.contains(EXPECT_LANDLOCK_VAR), "{skip}");

        // Promised ⇒ a failure, and the message must carry the three things
        // whoever reads that red run needs: which test verified nothing, what
        // the kernel actually said, and where the promise is made so it can be
        // withdrawn deliberately.
        let broken = skip_or_fail_verdict(true, "some_live_test", "ENOSYS")
            .expect_err("a broken promise must fail, not skip");
        assert!(broken.contains("some_live_test"), "{broken}");
        assert!(broken.contains("ENOSYS"), "{broken}");
        assert!(broken.contains(EXPECT_LANDLOCK_VAR), "{broken}");
        assert!(broken.contains("tests.yml"), "{broken}");
        assert!(
            broken.contains("verified NOTHING"),
            "the message must say what was lost, not merely that a flag was set: {broken}"
        );
        // The two verdicts must not read alike — a skip that looks like a
        // failure (or vice versa) is how the wrong one gets ignored.
        assert!(!broken.starts_with("SKIPPED"), "{broken}");
    }

    /// The variable named in code is the variable the workflow **actively**
    /// sets, in the **Linux** job. Spelled in two files, so a rename or a
    /// deletion in one is a silent un-promising in the other — this is the
    /// assertion that notices.
    ///
    /// # Why this is line-structured rather than a `contains`
    ///
    /// The first version of this test was `workflow.contains("CIMP_EXPECT_
    /// LANDLOCK: '1'")`, and it PASSED against a workflow whose line had been
    /// commented out — `# CIMP_EXPECT_LANDLOCK: '1'` contains that substring
    /// too. Verified by actually commenting the line and re-running, which is
    /// the only way that class of hole is ever found. A tripwire that a
    /// commented-out promise satisfies is the exact failure it was written to
    /// prevent, one level up.
    #[test]
    fn the_promise_variable_is_actively_set_by_the_linux_job() {
        assert_eq!(EXPECT_LANDLOCK_VAR, "CIMP_EXPECT_LANDLOCK");
        let workflow = include_str!("../../../.github/workflows/tests.yml");
        // Scope to the Linux job: the promise protects the LINUX live tests, so
        // the same line sitting in the Windows job's `env:` would be worthless
        // and must not satisfy this.
        let linux_job = workflow
            .split("\n  test-linux:")
            .nth(1)
            .expect("tests.yml must still define a `test-linux` job");
        // `lines()` is CRLF-safe (it strips the trailing \r), and `trim` covers
        // the rest — the Windows runner checks this file out with CRLF.
        let actively_set = linux_job.lines().any(|line| {
            let t = line.trim();
            !t.starts_with('#') && t.starts_with(&format!("{EXPECT_LANDLOCK_VAR}:"))
        });
        assert!(
            actively_set,
            "the `test-linux` job must ACTIVELY set {EXPECT_LANDLOCK_VAR} (not commented out), \
             or the Landlock live tests silently go back to passing when they skip on a runner \
             that lost Landlock"
        );
    }

    // ── the live half (Linux only) ─────────────────────────────────────────
    //
    // These need a kernel with Landlock. On a machine (or container) without
    // it they skip rather than passing outright — but *how* they skip is the
    // point, see [`skip_or_fail`]: a sandbox test that silently reports success
    // on a kernel that cannot sandbox anything is the vacuous-canary shape this
    // project has been burned by before.

    /// Prepare a real boundary, or answer `None` after [`skip_or_fail`] has
    /// decided whether "no Landlock here" is a skip or a failure. `test` is the
    /// caller's own name so the message names the test that verified nothing.
    #[cfg(target_os = "linux")]
    fn live_prepare(test: &str, root: &Path, program: &Path) -> Option<Prepared> {
        match cached_probe() {
            Err(e) => {
                skip_or_fail(test, e);
                None
            }
            Ok(_) => {
                let cfg = super::super::SandboxCfg {
                    enabled: true,
                    allow_network: false,
                    extra_grant_dirs: Vec::new(),
                };
                let rt = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                // `expect`, not `ok()`: the probe already said this kernel can
                // sandbox, so a failure here is a real defect and must fail the
                // test rather than turning it into a silent skip.
                Some(
                    rt.block_on(prepare(
                        &cfg,
                        super::super::SEAM_RUN_COMMAND,
                        program,
                        &super::super::GrantHints::default(),
                        root,
                        &[],
                    ))
                    .expect("a Landlock-capable kernel must prepare a boundary"),
                )
            }
        }
    }

    /// **The boundary, live.** One confined shell reads a file inside the root
    /// (must succeed) and one reads a file outside it (must be denied) — the
    /// two halves in one test, because "the denial fired" is only evidence of a
    /// boundary if the permitted read worked in the same configuration.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_confined_child_reads_inside_the_root_and_is_denied_outside_it() {
        let base = std::env::temp_dir().join(format!("cimp-landlock-{}", std::process::id()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("inside.txt"), b"INSIDE-OK").unwrap();
        std::fs::write(outside.join("secret.txt"), b"SECRET").unwrap();

        let shell = Path::new("/bin/sh");
        let Some(prepared) = live_prepare(
            "a_confined_child_reads_inside_the_root_and_is_denied_outside_it",
            &root,
            shell,
        ) else {
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let base_env = super::super::child_env::minimal_env(&|k| std::env::var_os(k));
        let no_seam_env: [(String, String); 0] = [];

        let run = |script: String| -> (Option<i32>, String, String) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let mut cmd = tokio::process::Command::new(shell);
                cmd.arg("-c")
                    .arg(&script)
                    .current_dir(&root)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                prepared
                    .apply(&mut cmd, &base_env, no_seam_env.iter().cloned())
                    .expect("the ruleset must build on a Landlock kernel");
                let out = cmd.output().await.expect("the confined child must spawn");
                (
                    out.status.code(),
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                )
            })
        };

        let (code, stdout, stderr) = run(format!("cat {}", root.join("inside.txt").display()));
        assert_eq!(code, Some(0), "reading inside the root failed: {stderr}");
        assert!(stdout.contains("INSIDE-OK"), "stdout was {stdout:?}");

        let (code, stdout, stderr) = run(format!("cat {}", outside.join("secret.txt").display()));
        assert_ne!(code, Some(0), "reading OUTSIDE the root succeeded: {stdout}");
        assert!(
            !stdout.contains("SECRET"),
            "the child read a file outside the boundary"
        );
        assert!(
            super::super::denial_signature(code, &stderr, false).is_some(),
            "the denial must be classifiable by the lane's own classifier; stderr was {stderr:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A `Prepared` whose probe answer is unusable must REFUSE, never fall back
    /// to an unconfined spawn. The one outcome Phase D forbids, pinned.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_unusable_abi_refuses_the_spawn() {
        let prepared = Prepared {
            abi_raw: 0,
            abi_label: "Unsupported".to_string(),
            abi_at_ceiling: false,
            net_denied: false,
            allow_network: false,
            grants: Vec::new(),
            env_overrides: Vec::new(),
            seam: super::super::SEAM_RUN_COMMAND.to_string(),
            root: std::env::temp_dir(),
            subject: "sh".to_string(),
        };
        let mut cmd = tokio::process::Command::new("/bin/sh");
        let err = prepared
            .confine(&mut cmd, Vec::new())
            .expect_err("an unusable ABI must refuse, not confine-and-hope");
        assert!(err.contains("was not run"), "{err}");
    }

    /// The posture line must name the ABI *and* the network truth — a row that
    /// said only "network=off" would promise a confinement Landlock does not
    /// give (UDP is never scoped).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_posture_note_states_the_network_truth() {
        let note = posture_note(false);
        assert!(note.to_ascii_lowercase().contains("landlock"), "{note}");
        // The `Err` arm is a skip on a kernel without Landlock — but a SILENT
        // one would leave this test green while asserting only that the word
        // "landlock" appears in an "unavailable" string, which is not the claim
        // it is here to make.
        match cached_probe() {
            Err(e) => skip_or_fail("the_posture_note_states_the_network_truth", e),
            Ok(_) => assert!(
                note.contains("UDP") || note.contains("below ABI 4"),
                "the note must state which network confinement this kernel gives: {note}"
            ),
        }
    }
}

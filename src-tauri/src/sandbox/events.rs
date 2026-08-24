//! V33 — the activity rows the sandbox layer writes.
//!
//! **R17 (V42): moved out of `sandbox/mod.rs` verbatim**, alongside
//! [`super::runtime`], for the reason stated there. Not one line of logic
//! changed in the move and `sandbox/mod.rs` re-exports every name, so every
//! `crate::sandbox::record_*` path still resolves.
//!
//! Every row here lands in the `sandbox` activity lane (its own retention lane,
//! per #51's pick-a-lane-on-purpose rule) and most of them are recorded **once
//! per session** — see [`once_per_session`] for the dedup discipline and for
//! why the statics behind it are per-site rather than shared.

use std::path::Path;

use super::{SandboxCfg, SkipReason};


/// Run ONE call site's own once-per-session dedup, answering whether `key` is
/// new (and therefore whether the caller should record its row).
///
/// # Why this exists
///
/// Nine sites across the crate opened with the same preamble — a function-local
/// `static EMITTED: Mutex<Option<HashSet<String>>>`, a `lock()`, a
/// `get_or_insert_with`, an `insert` and an early `return` — and by V42 they had
/// drifted: some went through a `first_time` insert wrapper (folded in here),
/// some called `set.insert` on the guard directly,
/// one keys on a value its own doc line does not mention. R17 wrote the
/// *mechanism* once. The KEY stays at the call site, because the key IS the
/// policy: what counts as "the same fact" differs per row and belongs next to
/// the row that answers it.
///
/// # The static stays per site, deliberately
///
/// `slot` is the caller's own `static`, never a shared one. A single
/// process-wide set would merge every site's key namespace, so one row's key
/// scheme could silence another row — and no test could see it happen, because
/// a row that is never recorded looks exactly like a row that was correctly
/// deduped.
///
/// # A poisoned lock records
///
/// `Err` reads as "first time", which is what the hand-written preamble did
/// (`if let Ok(..)` simply fell through to the record): a duplicate row is a
/// smaller harm than a lost one, and this lane is how sandbox degradation gets
/// reported at all.
pub(crate) fn once_per_session(
    slot: &std::sync::Mutex<Option<std::collections::HashSet<String>>>,
    key: String,
) -> bool {
    match slot.lock() {
        Ok(mut guard) => guard
            .get_or_insert_with(std::collections::HashSet::new)
            .insert(key),
        Err(_) => true,
    }
}

/// Record one runtime need the boundary did **not** meet — once per
/// (seam, runtime, what) per session, for [`record_grant_refused`]'s reason:
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
    // R17 note — this key is DRIFTED from its four siblings and is left
    // exactly as it was. They key on `subject_key(subject)`; this function
    // has no `subject` parameter at all and keys on `what` instead, uncased.
    // So two programs of the same runtime missing the same pointer produce
    // ONE row rather than one each, and a `what` differing only in case
    // produces two. Changing that would change which rows a user sees, which
    // is not what a motion commit is for.
    if !once_per_session(&EMITTED, format!("{seam}|{runtime}|{what}")) {
        return;
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
    if !once_per_session(&EMITTED, format!("{seam}|{declared}|{seen}|{}", subject_key(subject))) {
        return;
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
pub(super) fn runtime_mismatch_body(declared: &str, seen: &str) -> String {
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
    if !once_per_session(&EMITTED, format!("{seam}|{}", subject_key(subject))) {
        return;
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
pub(super) fn declared_unsandboxed_body(subject: &str) -> String {
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
    if !once_per_session(&EMITTED, format!("{seam}|{why}|{}", subject_key(subject))) {
        return;
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
pub(super) fn sandbox_required_refusal_body(subject: &str, why: &str) -> String {
    format!(
        "{subject} was NOT run: its plugin manifest declares `sandbox: required`, and the OS \
         boundary could not be provided here — {why}. Running it anyway would have delivered \
         findings from a tool the manifest says must never run unprotected, which is a worse \
         outcome than this tool being missing from the report."
    )
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
    if !once_per_session(&EMITTED, key) {
        return;
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
pub(super) fn grant_refused_body(source: GrantSource, path: &Path, why: &str) -> String {
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
    if !once_per_session(&EMITTED, key) {
        return;
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
pub(super) const DENIAL_STDERR_TAIL_CHARS: usize = 500;

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
pub(super) const NAME_RESOLUTION_IS_A_BOUNDARY_SIGNAL: bool = cfg!(windows);

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
pub(super) fn posture(cfg: &SandboxCfg) -> String {
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
pub(super) fn subject_key(subject: &str) -> String {
    subject.to_ascii_lowercase()
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
    // Per subject per SEAM. Both halves earn their place:
    //
    // * the SEAM, because `run_check` and `run_command` can both spawn the
    //   same program (`cmd.exe`) and "checks are sandboxed" is not the same
    //   fact as "commands are sandboxed";
    // * the SUBJECT, which is a program name for `run_command`/audit but the
    //   CHECK NAME for `run_check` — so each configured check confirms once
    //   per session instead of the first one speaking for all of them.
    if !once_per_session(&EMITTED, format!("{seam}|{}", subject_key(subject))) {
        return;
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
pub(super) fn silent_exit_detail(
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
pub(super) fn refused_detail(subject: &str, args: &[String], err: &str, cfg: &SandboxCfg) -> String {
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
pub(super) fn summarize_invocation(subject: &str, args: &[String]) -> String {
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
pub(super) fn stderr_tail(stderr: &str) -> String {
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

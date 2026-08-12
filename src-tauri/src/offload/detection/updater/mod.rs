//! V32 Phase C3 (locked decision 13) — the **detection auto-updater**.
//!
//! # Why this exists
//!
//! Signature rules decay without updates — they only match phrasings someone
//! has already written down — and tying freshness to manual maintenance runs
//! makes staleness the default. So the rule bundle is kept current on a daily
//! check, from a channel the project curates ([`manifest`]).
//!
//! The **classifier weights are not on this channel** and never were, in the
//! end: locked decision 7 ships them through the models-v1 release-asset
//! pipeline with `CHECKSUMS.txt`, at maintenance-run cadence, exactly like the
//! TTS and STT blobs. A `classifier` component was built here and removed on
//! 2026-08-08 — a released Meta checkpoint has no update stream to poll, and
//! two delivery mechanisms for one artifact is one too many.
//!
//! # The shape of one run
//!
//! ```text
//!   scheduler tick (due? per component)
//!        │
//!        ├─ fetch the manifest ─────────────────► parse boundary (manifest.rs)
//!        │                                        schema, names, sizes,
//!        │                                        digests, asset origin
//!        ├─ newer than installed? applicable to this app version?
//!        │        │
//!        │        ├─ mode = check-only ─► record "available" + Advisor card. STOP.
//!        │        └─ mode = auto ────────┐
//!        │                               ▼
//!        ├─ download each file into MEMORY, verify SHA-256, only then write to
//!        │  staging/ (nothing untrusted reaches disk before its digest is
//!        │  checked, and nothing reaches a parser before that either)
//!        │
//!        ├─ validate ───────────────────────────► the validate.rs gauntlet
//!        │        └─ fail ─► reject, wipe staging, keep old data, card + row
//!        │
//!        ├─ activate: archive the current files under previous/, move the
//!        │            staged files in, hot-reload
//!        │        └─ reload unhealthy ─► restore the archive, card + row
//!        │
//!        └─ record state (version, outcome) + activity row
//! ```
//!
//! # Invariants this module is responsible for
//!
//! - **`rules.d/local/` is never touched.** Structural, not conditional: the
//!   activation path only ever enumerates the top level of `rules.d`
//!   ([`store::managed_rule_files`]). Nothing here opens `local/`.
//! - **Checksum before content.** A downloaded byte is hashed before it is
//!   written to disk and long before it is compiled. A mismatch aborts the
//!   component's run with the staging directory wiped.
//! - **Old data stays live on any failure.** There is no path from "the new
//!   bundle is bad" to "no bundle": the live set changes in exactly one place,
//!   after the gauntlet passed, and is restored if the hot-reload disagrees.
//! - **Inert when off.** With the Phase G detection feature resolving off
//!   ([`updates_enabled`], which the L1 master also decides), or with
//!   both modes `off`, the scheduler tick returns before touching the network,
//!   the disk, or anything but those switches — and the three Settings buttons
//!   refuse for the same reason, through the same [`updates_enabled`] call.
//!
//!   **Inert is not the same as "detection is off" (#48, M-21).** That resolution
//!   is app-scoped and does not see the `offload-worker` row, so the updater can
//!   be inert while the worker is screening with the bundle on disk. The
//!   behaviour is deliberate; what had to be fixed was every sentence that
//!   explained it by claiming a layer was off. See [`worker_only_detection`].
//!
//!   **One deliberate exception, and it is not about updating**:
//!   [`recover_on_launch`] finishes a swap a crash interrupted, whatever the
//!   switches say (#48, M-12). Gating THAT on the updater's own settings is how
//!   a user who turned detection off after a crash stranded a short `rules.d`
//!   permanently — the repair is about the completeness of the data on disk,
//!   not about whether new data is wanted. It is still silent and writes
//!   nothing on a healthy install: an existence check on the journal file
//!   returns before the lock, the state file or the rules directory is touched.
//! - **A refusal and an outage are different events.** [`Outcome::Rejected`]
//!   means a document reached us and a check said no; [`Outcome::Unavailable`]
//!   means the channel never answered. Collapsing them made every install
//!   report a permanent bundle rejection for a release that simply did not
//!   exist yet (#46), which is how a security-relevant card stops being read.
//!
//! # Every signal has its consumer
//!
//! Every outcome writes an `injection_flag` Tool Activity row (screen
//! `updater`, `ok` reflecting the outcome), and the three that need a decision
//! reach the Advisor: `detection.update_available.v1` (check-only found
//! something newer), `detection.update_failed.v1` (a bundle was refused) and
//! `detection.update_stalled.v1` ([`STALLED_AFTER_CHECKS`] consecutive checks
//! left the component no fresher, for ANY reason — the "this has stopped
//! getting fresher" signal, and the only one whose dismissal ages, so a
//! component cannot be frozen silently by dismissing the other two).
//! Versions, last-check time and outcome live in
//! Settings → Injection protection → Injection detection, next to Check now,
//! Apply, Revert and Open rules folder.
//!
//! # Testability
//!
//! Every path is driven through a [`Layout`] (four directories) and a
//! [`manifest::Fetcher`], so the whole pipeline — checksum mismatch, broken
//! bundle, false-positive smoke failure, successful swap, revert — runs against
//! a temp directory and an in-memory map with **no network and no writes
//! outside that directory**.

pub mod manifest;
pub mod store;
pub mod validate;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use tracing::{info, warn};

use manifest::{Component, Fetcher, Manifest};
use store::{ComponentState, State};

use crate::activity::{ActivityEntry, ActivityKind, ActivityRecord};
use crate::offload::outbound::Screen;
use crate::settings::Settings;

// ── Modes and scheduling ───────────────────────────────────────────────────

/// What the updater is allowed to do for one component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Mode {
    /// Never check. Fully inert: no network, no disk.
    Off,
    /// Check and report; never download or activate.
    Check,
    /// Check, validate and activate.
    Auto,
}

impl Mode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Check => "check",
            Mode::Auto => "auto",
        }
    }

    /// Parse a settings string. An unrecognized value becomes `check` — the
    /// middle setting, deliberately: a typo must neither silently disable the
    /// updater (staleness by accident) nor silently grant it activation rights.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Mode::Off,
            "auto" => Mode::Auto,
            "check" | "check-only" => Mode::Check,
            other => {
                warn!(
                    target: "offload",
                    mode = %other,
                    "detection updater: unrecognized mode; treating it as check-only"
                );
                Mode::Check
            }
        }
    }
}

/// How often the scheduler wakes up to ask whether anything is due.
///
/// Deliberately much shorter than the check interval, and fixed: the loop's
/// cadence is not the policy — [`is_due`] is. A tick that re-reads the current
/// settings means an interval change (or a mode change away from `off`) takes
/// effect within 15 minutes instead of at the end of the old interval, and a
/// tick with both components off costs one comparison.
pub const POLL_TICK: Duration = Duration::from_secs(15 * 60);

/// How long after launch the first tick happens — the debounce decision 13 asks
/// for. Launch is the busiest moment in the process (graph index build, model
/// loads, PTY spawns); a network fetch and a YARA compile have no business
/// competing with it, and detection data two minutes staler than it could be
/// costs nothing.
pub const LAUNCH_DELAY: Duration = Duration::from_secs(120);

/// Floor on the configurable interval: a mistyped `0` must not turn the updater
/// into a request loop against a release asset.
pub const MIN_INTERVAL_HOURS: u32 = 1;

/// Whether a component is due for a check.
///
/// Pure, so the scheduler's policy is unit-testable without timers:
/// - `Off` is never due — that is what "fully inert" means;
/// - never checked ⇒ due (this is also the launch check, after the debounce);
/// - a `last_check_ms` in the FUTURE ⇒ due. A clock that moved backwards (or a
///   state file copied from another machine) would otherwise park the component
///   until real time caught up, which for a 24-hour interval can be forever.
pub fn is_due(mode: Mode, now_ms: u64, last_check_ms: u64, interval_hours: u32) -> bool {
    if mode == Mode::Off {
        return false;
    }
    if last_check_ms == 0 || last_check_ms > now_ms {
        return true;
    }
    let interval_ms = interval_hours.max(MIN_INTERVAL_HOURS) as u64 * 60 * 60 * 1000;
    now_ms.saturating_sub(last_check_ms) >= interval_ms
}

/// The two modes plus the interval, snapshotted from `Settings` the same way
/// [`super::Config`] snapshots the layer toggles: read once where a `Settings`
/// is in hand, carried through the run, never re-read mid-flight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Schedule {
    pub rules: Mode,
    pub interval_hours: u32,
}

impl Schedule {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            rules: Mode::parse(&s.offload.detection_update_rules_mode),
            interval_hours: s.offload.detection_update_interval_hours,
        }
    }

    pub fn mode(self, c: Component) -> Mode {
        match c {
            Component::Rules => self.rules,
        }
    }

    /// Every component off ⇒ the updater does nothing at all.
    pub fn is_inert(self) -> bool {
        self.rules == Mode::Off
    }
}

/// The manifest URL in effect: the settings override when set, else the pinned
/// default. Trimmed, because a stray space would silently 404 forever.
pub fn manifest_url(s: &Settings) -> String {
    let over = s.offload.detection_update_manifest_url.trim();
    if over.is_empty() {
        manifest::DEFAULT_MANIFEST_URL.to_string()
    } else {
        over.to_string()
    }
}

/// Whether the updater may do anything at all — the ONE gate the scheduler and
/// all three Settings buttons resolve through.
///
/// The feature this data exists to feed — `Feature::Detection` — resolved at
/// the app scope. #46 gated the scheduler on the L1 master alone, which
/// left the supported state "protection ON, detection OFF" polling GitHub daily
/// and hot-swapping bundles for a surface that does nothing with them (#48).
/// The resolver folds L1 in — a master `off` resolves every feature `false` —
/// so one call at the right level covers both levels, and a raw field read
/// covers neither (decision 16 / #44).
///
/// **App scope, not per tab.** There is one bundle on disk for the whole
/// process; a per-tab detection override changes which tab SCANS with it, not
/// whether it is worth keeping current.
///
/// **⚠ #48 F-35 — that argument describes `Scope::AppWide`, and this site does
/// not resolve there.** The paragraph above used to end *"`Scope::App` resolves
/// to L1 ∧ L2, which is exactly that question"*, and **that has been false since
/// N-1**: the old `Scope::App` also honoured an L3 `On` stated by any configured
/// AI tab, so a single tab-scope `On` over an app-wide `Off` really does start
/// the updater. Locked decision 36 split the variant in two and this site kept
/// the behaviour it had, under the name that now describes it —
/// [`Scope::UnknownCaller`](crate::settings::injection::Scope::UnknownCaller).
///
/// The name is deliberately the visibly odd one: there is no unknown caller
/// here, so it reads as a question nobody has answered, which is exactly what it
/// is. Whether the updater should follow the app-wide baseline
/// (`Scope::AppWide`, so one hardened tab stops starting it) or the
/// armed-anywhere predicate (which M-21 explicitly rejected, `worker_only_detection`
/// below) is a **behaviour** decision with a live-verify box attached, raised as
/// **F-38** rather than folded into a rename that changes nothing. Until it is
/// taken, this stays exactly as it has behaved since N-1.
///
/// Read per call, never cached: a flip takes effect on the next tick or the
/// next click, so this is not spawn-baked and owes no `spawn_inject_sig` entry.
///
/// The manual buttons resolve through it too (#48). They were left ungated as
/// "explicit user intent", but the intent a user expresses by clicking Check
/// now is not consent to a feature they switched off — and the Settings panel
/// promised in so many words that with protection off nothing is polled or
/// swapped, which a button that did both made false.
pub fn updates_enabled(s: &Settings) -> bool {
    crate::settings::injection::effective(
        crate::settings::injection::Feature::Detection,
        crate::settings::injection::Scope::UnknownCaller,
        s,
    )
}

/// #48 (M-21) — **injection detection is running in the offload worker while the
/// updater is inert.** The one state in which *"injection detection is off"* is a
/// false statement about this install.
///
/// # Why this state exists, and why the resolution above is still right
///
/// [`updates_enabled`] resolves at
/// [`Scope::UnknownCaller`](crate::settings::injection::Scope::UnknownCaller),
/// which folds in L1 and L2 and — since N-1 — an L3 `On` stated by any configured
/// AI tab, but **deliberately not** the `offload-worker` row: `any_tab_override_on`
/// is *"tabs only, deliberately"*, because that elevation exists for a call that
/// arrived with no tab identity, and the worker is never that caller (it always
/// resolves through `Scope::OffloadWorker`). That argument is sound for what it
/// was written for and this function does not disturb it.
///
/// **#48 F-35 — this asymmetry is a CONTRACT, not an accident, and the split
/// preserved it on purpose.** These two functions answer two different
/// questions, and the frontend renders a distinct sentence for the worker-only
/// case gated on the published `worker_only_detection`, which is only ever
/// `true` while `updates_enabled` is `false`. Folding the offload-worker row
/// into `updates_enabled` — by repointing it at
/// [`armed_anywhere`](crate::settings::injection::armed_anywhere), which does
/// fold it in — would make `worker_only_detection` permanently `false` and kill
/// that branch while it still looks reachable in the Svelte source. If F-38 ever
/// moves `updates_enabled`, the frontend branch moves in the same change or it
/// is deleted; it may not be orphaned.
///
/// What went wrong was not the resolution but the **claim made about it**. A
/// worker-only override leaves the whole updater surface off — the scheduler
/// returns, the three buttons refuse — and every sentence explaining that said
/// "detection is off", while the worker was screening every fetched page it
/// touched with the bundle already on disk. Same class as M-5 and F-19: a value
/// that is *presented as authoritative* about a state it does not describe.
///
/// # What it is for
///
/// Naming the layer, nothing more. It changes no verdict — no caller may use it
/// to admit an update — and it never widens what runs: `updates_enabled` is
/// unchanged, so the updater stays app-scoped and one worker override still does
/// not start it. Its consumers are the refusal `ipc::commands::updates_allowed`
/// hands the user and the `worker_only_detection` field of [`UpdaterStatus`],
/// which the Settings surface renders instead of re-deriving the conjunction in
/// TypeScript.
///
/// Note it can only be true with the L1 master ON — [`decide`](crate::settings::injection::decide)
/// short-circuits every feature to `false` when protection is off — so the layer
/// it reports is genuinely armed rather than merely configured.
pub fn worker_only_detection(s: &Settings) -> bool {
    use crate::settings::injection::{effective, Feature, Scope};
    !updates_enabled(s) && effective(Feature::Detection, Scope::OffloadWorker, s)
}

// ── Layout ─────────────────────────────────────────────────────────────────

/// The four directories one run touches. Passed as a value rather than resolved
/// from `current_exe()` at each use so the whole pipeline is drivable against a
/// temp tree — and so a reader can see, in one struct, everything the updater
/// is able to write to.
#[derive(Debug, Clone)]
pub struct Layout {
    /// `<exe-dir>/detection-updates` — state file, staging, retained versions.
    pub state_root: PathBuf,
    /// `<exe-dir>/detection/rules.d` — the live rule bundle (whose `local/`
    /// subdirectory is never enumerated).
    pub rules_dest: PathBuf,
    /// `<exe-dir>/detection/smoke` — the validation corpus.
    pub smoke_dir: PathBuf,
}

impl Layout {
    /// The real layout. `None` when the exe path has no usable parent, in which
    /// case the updater stays inert rather than guessing at directories — the
    /// same discipline `signature::rules_dir` follows.
    pub fn resolve() -> Option<Self> {
        Some(Self {
            state_root: store::state_dir()?,
            rules_dest: store::destination(Component::Rules)?,
            smoke_dir: validate::smoke_dir()?,
        })
    }

    pub fn dest(&self, c: Component) -> &Path {
        match c {
            Component::Rules => &self.rules_dest,
        }
    }
}

/// How a component is made live after its files are in place.
///
/// A function rather than a direct call to `signature::reload()` because the
/// activation path must be able to reload a *specific* directory: production
/// reloads the process-wide rule set, the tests reload a temp directory without
/// disturbing the global one every other test reads.
/// `Send + Sync` because [`run`] holds one across the manifest `await` and the
/// scheduler's task must be spawnable — a plain `&dyn Fn` would make the whole
/// future non-`Send`.
pub type Reloader<'a> = &'a (dyn Fn(Component, &Path) -> Result<String, String> + Send + Sync);

/// The production reloader: recompile the live rules / rebuild the live `ort`
/// session, and report an error when the result is not healthy.
pub fn live_reload(c: Component, dir: &Path) -> Result<String, String> {
    match c {
        Component::Rules => {
            let s = super::signature::reload();
            health_from_rules(&s, dir)
        }
    }
}

/// Turn a rules [`Status`](super::signature::Status) into "healthy, and here is
/// the summary" or "unhealthy, and here is why". One definition, shared by the
/// live reloader and the tests' directory-scoped one, so both judge health the
/// same way.
///
/// # The seam with D-2's fix (#48)
///
/// [`signature::reload`](super::signature::reload) now KEEPS the previously
/// compiled rules when a directory compiles to nothing, rather than disarming
/// the layer. That must not make a bad bundle look healthy — and it cannot,
/// structurally: the `Status` it returns describes what the DIRECTORY compiled
/// to, never what is in the live slot, so a bundle that produced no rule set
/// still arrives here as `files_loaded: 0, rules: 0` and still fails. Keeping
/// old rules changes what is screening while the rollback runs; it changes
/// nothing about the verdict on the bundle.
///
/// The predicate itself is [`Status::healthy`](super::signature::Status::healthy)
/// — read, not restated (#48, N-3). `files_loaded == 0 || rules == 0` staying a
/// hard failure here is the never-degrade-to-nothing gate, so it must have
/// exactly one definition and every surface must bind that one.
/// The prefix [`super::signature::read_sources`] gives a file it read from the
/// user-owned overlay. One definition, because the whole U-4 fix keys on it —
/// and since #48/M-13 so does the collision rename, which lives beside the
/// reader, so the definition lives there too and this is the alias.
const LOCAL_PREFIX: &str = super::signature::LOCAL_PREFIX;

/// V32 Phase C3, #48 finding U-4 — which `rules.d/local/` files were **already
/// failing to compile before** an activation.
///
/// # The bug this exists to close
///
/// Validation compiles the staged bundle alone; a staging directory has no
/// `local/`. The post-activation health check compiles staged **plus `local/`**
/// and fails on `files_failed > 0`. So one malformed or identifier-colliding
/// `local/mine.yar` read as an unhealthy *bundle*: a perfectly good update was
/// rolled back, blamed on the publisher, and re-attempted — full download,
/// validate, swap, roll back — every 24 h, forever. The update channel was
/// frozen by a file the updater is contractually forbidden to touch.
///
/// The veto was incoherent on its own terms: at startup the app already
/// tolerates that same broken file (warn, keep the rest live), so the only
/// place it was fatal was the one place the user could not act on it.
///
/// # What is forgiven, and what is not
///
/// **Every `local/` failure, whether or not it predates the swap** (#48, M-13).
/// A failure in a bundle file is never forgiven at all — the prefix test
/// excludes it.
///
/// The original fix forgave only failures *present before the swap*, on the
/// reasoning that a `local/` file which compiled before and fails after is a
/// collision the bundle introduced and therefore the publisher's fault. That
/// reproduces U-4's exact symptom in the case the README tells users to expect.
/// The README's advice is "put your own rules in `rules.d/local/`"; a user who
/// takes it and happens to name a rule the way a future shipped rule is named
/// gets: bundle downloaded, validated, swapped, health check fails on their
/// file, rolled back, blamed on the publisher — every 24 h, forever. The update
/// channel is frozen by a file the updater is contractually forbidden to touch,
/// and the user is never told which file did it or that anything is wrong.
///
/// Rolling back was also incoherent with the ordering the rest of the layer
/// already commits to. [`super::signature::read_sources`] reads the shipped
/// bundle FIRST precisely so that on an identifier collision it is the *local*
/// file that loses — "losing a shipped rule to a stranger's typo would silently
/// weaken the layer". Having decided that the shipped rule wins the collision,
/// vetoing the shipped bundle over it says the opposite.
///
/// So the collision resolves the way `read_sources` already says it does: the
/// bundle goes live, the one colliding user file is skipped, and the skip is
/// **reported** — the returned sentence names it in the activity row and
/// Settings, `detection.local_rules_broken.v1` cards it with the file name and
/// the folder, and renaming one rule fixes it. That is U-4's other half doing
/// the job it was built for.
///
/// The baseline is still taken, and still matters: it is what lets the message
/// distinguish "this was already broken" from "this bundle collided with it",
/// which is the difference between a note and an apology.
///
/// And the never-degrade-to-nothing gate is untouched: `files_loaded == 0 ||
/// rules == 0` (i.e. `!Status::armed`) stays a hard failure whatever the
/// baseline says. Forgiveness only ever converts *degraded* into *degraded and
/// reported*; it can never convert *disarmed* into healthy.
#[derive(Debug, Clone, Default)]
pub struct LocalBaseline {
    /// `local/…`-prefixed names that already failed, as
    /// [`super::signature::Status::failed`] spells them.
    already_failing: BTreeSet<String>,
}

impl LocalBaseline {
    /// Compile `dest` as it stands right now and keep the `local/` failures.
    ///
    /// Uses [`super::signature::compile_report`], the pure reporter, so taking
    /// a baseline never disturbs the live rule set — this runs immediately
    /// before an activation that is about to swap that set.
    pub fn snapshot(dest: &Path) -> Self {
        let (_, status) = super::signature::compile_report(Some(dest));
        Self::from_failed(&status.failed)
    }

    /// The pure constructor, so the tests can state a baseline directly.
    pub fn from_failed(failed: &[String]) -> Self {
        Self {
            already_failing: failed
                .iter()
                .filter(|f| f.starts_with(LOCAL_PREFIX))
                .cloned()
                .collect(),
        }
    }

    /// Re-judge a post-activation health failure against this baseline.
    ///
    /// `Ok` means "the bundle is fine; these `local/` files were already broken
    /// and still are". `Err` passes the original verdict through unchanged —
    /// deliberately the original string, not a rewritten one, so the card the
    /// user reads is the one the reloader wrote.
    ///
    /// The directory is recompiled to answer this. That is one extra YARA
    /// compile, on the failure path of an operation that runs at most once a
    /// day, and it buys the alternative: threading a baseline through
    /// [`Reloader`], `activate`, `roll_back`, `reload_note`, `recover_interrupted`
    /// and `revert_inner`, four of which have no use for one. The compile is
    /// `compile_report`, the same pure function [`snapshot`](Self::snapshot)
    /// uses, so both halves of the comparison are produced by one code path.
    pub fn forgive(&self, dir: &Path, why: String) -> Result<String, String> {
        let (_, status) = super::signature::compile_report(Some(dir));
        if status.healthy {
            // The reloader and this disagree — trust the stricter answer and
            // keep the failure rather than inventing a pass.
            return Err(why);
        }
        if !status.armed {
            // The never-degrade-to-nothing gate. Not forgivable, ever.
            return Err(why);
        }
        // Only a BUNDLE file's failure vetoes. A `local/` file is the user's,
        // and it can neither be fixed by rolling back nor be blamed on the
        // publisher without freezing the channel (#48, M-13).
        let unforgiven: Vec<&String> = status
            .failed
            .iter()
            .filter(|f| !f.starts_with(LOCAL_PREFIX))
            .collect();
        if !unforgiven.is_empty() {
            return Err(why);
        }
        let (pre_existing, introduced): (Vec<&String>, Vec<&String>) = status
            .failed
            .iter()
            .partition(|f| self.already_failing.contains(*f));
        warn!(
            target: "offload",
            already_failing = %join_names(&pre_existing),
            newly_failing = %join_names(&introduced),
            dir = %dir.display(),
            "detection updater: the new bundle is live; these user rules in rules.d/local/ are \
             being skipped (detection.local_rules_broken.v1 names them to the user)"
        );
        let mut note = format!(
            "{} file(s), {} rule(s) live{}",
            status.files_loaded,
            status.rules,
            status.rename_note()
        );
        if !pre_existing.is_empty() {
            note.push_str(&format!(
                "; {} pre-existing broken file(s) in `rules.d/local/` ({}) were skipped, as they \
                 already were before this update",
                pre_existing.len(),
                join_names(&pre_existing)
            ));
        }
        if !introduced.is_empty() {
            note.push_str(&format!(
                "; {} file(s) in `rules.d/local/` ({}) stopped compiling with this bundle and are \
                 being skipped. An identifier a shipped rule has taken is normally handled by \
                 loading YOUR rule under a `{}` name instead (#48, M-13), so reaching this means \
                 the rename did not apply — the file has another compile error, or every renamed \
                 form of the identifier is taken as well. The update was NOT rolled back, because \
                 rolling it back would freeze every future update behind one file of yours",
                introduced.len(),
                join_names(&introduced),
                super::signature::CUSTOM_PREFIX
            ));
        }
        Ok(note)
    }
}

/// `", "`-join borrowed names — the one formatting helper the forgiveness note
/// needs, so the three lists in it are spelled identically.
fn join_names(names: &[&String]) -> String {
    names
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn health_from_rules(s: &super::signature::Status, dir: &Path) -> Result<String, String> {
    if !s.healthy {
        return Err(format!(
            "{} file(s) loaded, {} rule(s), {} rejected ({}) from {}",
            s.files_loaded,
            s.rules,
            s.files_failed,
            s.failed.join(", "),
            dir.display()
        ));
    }
    // #48/M-13: `rename_note` is empty unless a user rule is live under a
    // renamed identifier, so the ordinary sentence is unchanged — and when it
    // is not empty, the fact rides the ONE string every caller propagates (the
    // activation detail, the activity row, the Settings "Last check" line)
    // rather than needing a new channel of its own.
    Ok(format!(
        "{} file(s), {} rule(s) live{}",
        s.files_loaded,
        s.rules,
        s.rename_note()
    ))
}

// ── Cached state ───────────────────────────────────────────────────────────
//
// The state file is read once per root and kept in memory. The Settings poller
// and the Advisor's signal assembly both read it every couple of seconds;
// re-parsing a JSON file on each of those would be disk churn for a value only
// this module ever writes.

fn cache() -> &'static Mutex<HashMap<PathBuf, State>> {
    static C: OnceLock<Mutex<HashMap<PathBuf, State>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The process-wide **run lock**: at most one check/apply/revert at a time.
///
/// `staging/<component>/` is a single fixed path that [`run_component`] wipes
/// on the way in and on the way out, and `previous/<component>/<version>/` is
/// wiped by both [`activate`] and [`revert_inner`]. Two overlapping runs — a
/// scheduler tick and a Settings click is the realistic pair — would therefore
/// have one deleting what the other had just written. Async rather than a
/// `std::sync::Mutex` because [`run`] holds it across the download `await`.
fn run_lock() -> &'static tokio::sync::Mutex<()> {
    static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// The state under `root`, hydrating from disk on first use.
pub fn state_at(root: &Path) -> State {
    let mut g = cache().lock().unwrap_or_else(PoisonError::into_inner);
    g.entry(root.to_path_buf())
        .or_insert_with(|| store::load_state(root))
        .clone()
}

/// The state for the real layout — what Settings and the Advisor read.
pub fn state() -> State {
    match store::state_dir() {
        Some(root) => state_at(&root),
        None => State::default(),
    }
}

/// Mutate the state under `root` and persist it. A failed write is logged and
/// the in-memory copy still updates: losing the *record* of an update must not
/// make the update itself look like it never happened.
fn update_state_at(root: &Path, f: impl FnOnce(&mut State)) {
    let mut next = state_at(root);
    f(&mut next);
    if let Err(e) = store::save_state(root, &next) {
        warn!(target: "offload", error = %e, "detection updater: could not persist state");
    }
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(root.to_path_buf(), next);
}

// ── What Settings renders ──────────────────────────────────────────────────

/// One component's updater readout.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ComponentStatus {
    pub component: &'static str,
    /// The mode as its settings string (`off`/`check`/`auto`), so the Settings
    /// select can compare it to the value it writes without a mapping table.
    pub mode: &'static str,
    /// Empty before the first successful update: the shipped bundle carries no
    /// manifest version, and inventing one would make the first real update
    /// look like a downgrade.
    pub installed_version: String,
    pub previous_version: String,
    /// True exactly when Revert has something to restore.
    pub can_revert: bool,
    pub available_version: String,
    pub available_notes: String,
    pub last_check_ms: u64,
    pub last_outcome: String,
    pub last_ok: bool,
    /// [`Outcome::as_str`] for that check. Settings branches on `unavailable`
    /// to say "could not reach the update channel" instead of "an update was
    /// refused" — the distinction #46 is about, which `last_ok` cannot carry.
    pub last_outcome_kind: String,
    /// Consecutive unreachable checks, so Settings can say how long this has
    /// been going on rather than repeating one 404 forever.
    pub unreachable_streak: u32,
    pub last_failure: String,
    /// Files a rollback could not put back, so the live set is short of them
    /// (#48, M-11). Empty is the healthy steady state; non-empty means reduced
    /// coverage that no other field on this struct can express — `last_ok` is
    /// about the last CHECK, and the rule counts are about what compiled, not
    /// about what should have been there.
    pub unrestored_files: Vec<String>,
}

/// The whole updater surface, folded into `DetectionStatus` so the Settings
/// poller that already asks for rule counts gets this for free.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpdaterStatus {
    pub components: Vec<ComponentStatus>,
    /// The manifest URL actually in use, so "nothing ever updates" is
    /// diagnosable without opening the settings file.
    pub manifest_url: String,
    pub interval_hours: u32,
    /// What "Open rules folder" opens — shown as text too, so the path is
    /// visible even when the button cannot open a file manager.
    pub rules_dir: String,
    pub state_dir: String,
    /// #48 (M-21): whether the updater may do anything at all —
    /// [`updates_enabled`], published so a surface that greys a control reads the
    /// SAME predicate the two IPC commands enforce instead of a second opinion
    /// assembled from the resolved-scope matrix.
    pub updates_enabled: bool,
    /// #48 (M-21): [`worker_only_detection`] — detection is armed for the offload
    /// worker while this updater is inert.
    ///
    /// It exists so no surface has to say "detection is off" about a layer that is
    /// running. Only ever true when `updates_enabled` is false, so a reader can
    /// treat the pair as one three-valued answer: on / off / off here but on in
    /// the worker.
    pub worker_only_detection: bool,
}

/// Build the status from the cached state plus the live settings.
pub fn status(settings: &Settings) -> UpdaterStatus {
    let st = state();
    let sched = Schedule::from_settings(settings);
    UpdaterStatus {
        components: Component::ALL
            .iter()
            .map(|c| {
                let cs = st.get(*c);
                ComponentStatus {
                    component: c.as_str(),
                    mode: sched.mode(*c).as_str(),
                    installed_version: cs.installed_version.clone(),
                    previous_version: cs.previous_version.clone(),
                    can_revert: !cs.previous_version.is_empty(),
                    available_version: cs.available_version.clone(),
                    available_notes: cs.available_notes.clone(),
                    last_check_ms: cs.last_check_ms,
                    last_outcome: cs.last_outcome.clone(),
                    last_ok: cs.last_ok,
                    last_outcome_kind: cs.last_outcome_kind.clone(),
                    unreachable_streak: cs.unreachable_streak,
                    last_failure: cs.last_failure.clone(),
                    unrestored_files: cs.unrestored_files.clone(),
                }
            })
            .collect(),
        manifest_url: manifest_url(settings),
        interval_hours: sched.interval_hours.max(MIN_INTERVAL_HOURS),
        rules_dir: super::signature::rules_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default(),
        state_dir: store::state_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default(),
        // #48 (M-21): both from the predicates themselves, in one snapshot, so the
        // readout cannot disagree with the buttons' enforcement.
        updates_enabled: updates_enabled(settings),
        worker_only_detection: worker_only_detection(settings),
    }
}

// ── Outcomes and their activity rows ───────────────────────────────────────

/// What happened to one component on one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Checked; already current.
    UpToDate,
    /// A newer bundle exists and was not applied (check-only mode, or blocked
    /// by `min_app_version`).
    Available,
    /// Downloaded, validated and activated.
    Applied,
    /// Rejected before activation, or rolled back after it. Old data still live.
    ///
    /// **This means a document reached us and a check refused it**: a manifest
    /// that violates a parse invariant (unknown schema, an artifact URL outside
    /// the curated directory), a bundle whose checksum or size disagrees with
    /// the manifest, a bundle the gauntlet failed, a set that would not reload.
    /// Every one of those is a decision someone should look at, so this is the
    /// only outcome that cards immediately and writes an `ok:false` row.
    Rejected,
    /// The update channel could not be reached, so nothing was checked (#46).
    ///
    /// A 404 on a release that does not exist yet, DNS failure, an offline
    /// laptop, a corporate proxy serving a login page — none of these are a
    /// bundle being refused, and painting them as one made a rejection card
    /// mean nothing. Deliberately quiet: a neutral activity row, a distinct
    /// line in Settings, and **no** card until [`STALLED_AFTER_CHECKS`]
    /// consecutive checks say the same thing, which is the point at which "this
    /// component has stopped getting fresher" becomes true and worth saying.
    Unavailable,
    /// The previous bundle was restored.
    Reverted,
    /// A **revert** could not be carried out: nothing was retained, or the
    /// files would not move back (#48).
    ///
    /// Split out of [`Outcome::Rejected`] because a revert fetches nothing:
    /// no document reached us, so nothing was refused and nothing about the
    /// channel was learned. Recording it as a refusal made a benign Revert
    /// click on a component with no retained version raise a security card
    /// claiming a bundle refusal, write the *previous* version into the offer
    /// slot (advertising a downgrade as an upgrade), and zero the unreachable
    /// streak of a machine that had reached nothing.
    ///
    /// It is still an unhealthy outcome — the user asked for something and did
    /// not get it, so the activity row is `ok:false` — but it records nothing
    /// about the DATA and raises no card: the user is standing in front of the
    /// button and the detail is on screen the moment it returns.
    RevertFailed,
}

impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::UpToDate => "up-to-date",
            Outcome::Available => "available",
            Outcome::Applied => "applied",
            Outcome::Rejected => "rejected",
            Outcome::Unavailable => "unavailable",
            Outcome::Reverted => "reverted",
            Outcome::RevertFailed => "revert-failed",
        }
    }

    /// The unhealthy outcomes: a bundle refused, and a revert that could not be
    /// carried out. "Available" is check-only mode working exactly as
    /// configured and must not paint the feed red — and neither must
    /// `Unavailable`, which reports the network, not the bundle.
    pub const fn ok(self) -> bool {
        !matches!(self, Outcome::Rejected | Outcome::RevertFailed)
    }
}

/// Consecutive checks that left a component no fresher before the Advisor says
/// anything ([`store::ComponentState::stale_streak`]).
///
/// Seven, i.e. exactly one week at the default 24 h interval. The number is
/// chosen to be un-producible by any ordinary transient: a weekend offline, a
/// multi-hour GitHub incident, a laptop on a long flight and a hotel captive
/// portal all clear well inside it, and each of them ends the run on the first
/// check that comes back current. What survives seven consecutive failures is a
/// component that has genuinely stopped getting fresher — a dead URL, a proxy
/// that blocks it, or a channel that answers every day and refuses every bundle
/// it serves — and *that* is the staleness decision 13 forbids leaving silent.
/// Lower would re-create the noise #46 is about; much higher would let a
/// component freeze for a month before anyone heard.
pub const STALLED_AFTER_CHECKS: u32 = 7;

/// Write one updater row into the Tool Activity feed.
///
/// Composed here rather than through
/// [`outbound::record_flag`](crate::offload::outbound::record_flag) because
/// that function encodes "a flag row's `ok` follows [`Screen::is_denial`]",
/// which is true of every other screen and false of this one: an updater row's
/// `ok` is the outcome. Bending `record_flag` to carry an override would make
/// six call sites carry a field only this one uses.
fn record_row(c: Component, outcome: Outcome, version: &str, detail: &str) {
    let ts = crate::activity::now_ms();
    let request = serde_json::json!({
        "screen": Screen::Updater.as_str(),
        "component": c.as_str(),
        "outcome": outcome.as_str(),
        "version": version,
    });
    let root = std::env::current_dir()
        .map(|d| crate::activity::root_key(&d))
        .unwrap_or_default();
    crate::activity::record_bg(ActivityRecord {
        entry: ActivityEntry::new(
            ActivityKind::InjectionFlag,
            ts,
            root,
            Screen::Updater.as_str().to_string(),
            c.as_str().to_string(),
            // The at-a-glance column: what happened, to which version.
            if version.is_empty() {
                outcome.as_str().to_string()
            } else {
                format!("{} {version}", outcome.as_str())
            },
            0,
            0,
            outcome.ok(),
            // The C3 updater is cImp's own scheduled work — no tab.
            crate::activity::Attribution::Headless,
            None,
        ),
        request: serde_json::to_string_pretty(&request).unwrap_or_default(),
        response: detail.to_string(),
    });
}

// ── Advisor signals ────────────────────────────────────────────────────────

/// A component with a newer version the user has not taken.
#[derive(Debug, Clone, PartialEq)]
pub struct AvailableUpdate {
    pub component: String,
    pub installed: String,
    pub available: String,
    pub notes: String,
}

/// A component whose last update attempt was **refused** — a document arrived
/// and a check said no.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedUpdate {
    pub component: String,
    /// The bundle version, when the refusal happened at bundle level. Empty for
    /// a manifest-level refusal — display only.
    pub version: String,
    /// The dismissal key. Never empty (see [`failure_signature`]): keying the
    /// card on `version` alone let one dismissal of an unversioned failure
    /// silence every later refusal, containment violations included (#46).
    pub signature: String,
    pub reason: String,
}

/// A component that has stopped getting fresher, whatever the reason.
///
/// Distinct from [`FailedUpdate`] on purpose, and deliberately **not** about
/// the cause: it does not matter to this signal whether the channel is
/// unreachable, whether it answers and refuses every bundle, or whether the
/// component is simply never offered anything it can take. What matters is that
/// nothing has landed for [`STALLED_AFTER_CHECKS`] checks in a row, which is
/// the freshness decision 13 forbids leaving silent — and the one condition a
/// dismissal of the refusal card cannot hide, because this signature ages.
#[derive(Debug, Clone, PartialEq)]
pub struct StalledUpdate {
    pub component: String,
    /// Consecutive checks that left the component no fresher, ≥
    /// [`STALLED_AFTER_CHECKS`].
    pub streak: u32,
    /// The most recent outcome, verbatim — the thing that makes this
    /// diagnosable rather than merely worrying.
    pub reason: String,
}

/// A component whose live directory is **short of files a rollback could not
/// put back** (#48, M-11).
///
/// Its own signal, not a variant of [`FailedUpdate`], because the two say
/// opposite things about the present. A refusal card's whole reassurance is
/// "nothing is degraded right now, the previous data is still live"; this one
/// exists precisely because that sentence is false. And it is not
/// `detection.signature_down.v1` either: the layer is armed and matching, it is
/// simply matching with fewer rules than it has on disk — a state neither
/// existing card can see, since the files are absent rather than broken.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RulesIncomplete {
    pub component: String,
    /// The names the live directory is missing.
    pub files: Vec<String>,
    /// Where they should be, so the card names a folder the user can open.
    pub dir: String,
}

/// The four Advisor inputs, read from the cached state — no disk, no clock, so
/// they are safe on the advice poll's cadence.
pub fn advisor_signals() -> (Vec<AvailableUpdate>, Vec<FailedUpdate>, Vec<StalledUpdate>) {
    signals_from(&state())
}

/// The consumer for [`store::ComponentState::unrestored_files`] — separate from
/// [`advisor_signals`] only so that tuple does not grow a fourth element every
/// call site has to re-destructure.
pub fn rules_incomplete() -> Vec<RulesIncomplete> {
    incomplete_from(&state())
}

/// The pure half, so the rule that consumes this can be tested from a state
/// value.
pub fn incomplete_from(st: &State) -> Vec<RulesIncomplete> {
    let mut out = Vec::new();
    for c in Component::ALL {
        let cs = st.get(c);
        if cs.unrestored_files.is_empty() {
            continue;
        }
        out.push(RulesIncomplete {
            component: c.as_str().to_string(),
            files: cs.unrestored_files.clone(),
            dir: store::destination(c)
                .map(|d| d.display().to_string())
                .unwrap_or_default(),
        });
    }
    out
}

/// **The state of the user's own rules in `rules.d/local/`**, when it is
/// something they need to know — the consumer for U-4's other half (#48), and
/// since M-13 for the collision rename as well.
///
/// Two conditions, one signal, deliberately:
///
/// - `failed` — a file that does not compile. Once a broken `local/` rule
///   stopped vetoing the update channel it stopped being loud: before, it was
///   loud for the wrong reason (a daily update, applied and rolled back,
///   blaming the publisher); after, its only trace was a `warn!` in a log
///   nobody has open. The user's own file is silently not protecting them.
/// - `renamed` — a rule that IS live, under a different identifier than the one
///   their file spells, because a shipped rule took the name (M-13). Nothing is
///   degraded, but the identifier they will see in a hit, in an activity row
///   and in their own grep is not the one they wrote.
///
/// They share one signal because they share one folder and one audience, and
/// because this module has already decided that *"two cards about one folder is
/// how a user learns to dismiss both"*. The card and the Settings row read the
/// two lists separately, so neither is described in the other's words.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BrokenLocalRules {
    /// Where the rules live, so the card names a folder the user can open.
    pub dir: String,
    /// The rejected file names, `local/`-prefixed as `read_sources` spells them.
    pub failed: Vec<String>,
    /// Rules loaded under a renamed identifier (#48, M-13). Empty is the
    /// ordinary case.
    pub renamed: Vec<super::signature::RenamedRule>,
    /// What IS live — the card must not read as "detection is off".
    pub files_loaded: usize,
    pub rules: usize,
}

/// Whether the user's own rule files need their attention: any of them failing
/// to compile, or any of them live under a renamed identifier.
///
/// `None` in every healthy or irrelevant case: the layer is switched off (the
/// switch is resolved through [`super::Config::armed_anywhere`], the same call
/// [`super::signature::advisor_signal`] uses, so the L1 master and the per-layer
/// toggle compose exactly once and never as a second opinion), the whole set
/// compiles with no collisions, or the failure is in a BUNDLE file — that is the
/// updater's problem and already has three cards; this one is about the files
/// only the user can change.
///
/// **"Switched off" means armed in NO scope** (#48, F-35, locked decision 36),
/// not "off app-wide". It resolved at `Scope::App` until the split, and a user
/// who narrowed detection to the offload worker — L2 `off`,
/// `worker.detection = on`, the supported state M-21 documents — got `None` here
/// while the worker kept screening with those very files. Their own file failing
/// to compile has no other user-facing trace: `signature::reload` keeps the old
/// set live and warns to a log nobody has open. This card is where they find
/// out.
///
/// This does **not** move [`updates_enabled`], which is still app-scoped: naming
/// a broken file of the user's is reporting, starting a network updater is a
/// capability, and only the first one widened.
///
/// It also stays quiet when the layer is disarmed, because
/// `detection.signature_down.v1` is already saying something louder and more
/// urgent about the same directory, and two cards about one folder is how a
/// user learns to dismiss both.
pub fn broken_local_rules(s: &Settings) -> Option<BrokenLocalRules> {
    if !super::Config::armed_anywhere(s).signature {
        return None;
    }
    from_status(super::signature::status())
}

/// The pure half, so a test can drive the predicate from the `Status` a real
/// collision produced instead of the process-wide slot it must not disturb.
pub fn from_status(st: super::signature::Status) -> Option<BrokenLocalRules> {
    if !st.armed {
        return None;
    }
    let failed: Vec<String> = st
        .failed
        .iter()
        .filter(|f| f.starts_with(LOCAL_PREFIX))
        .cloned()
        .collect();
    // Renames are `local/`-only by construction (`rename_colliding_local_rules`
    // rewrites nothing else), so there is no second prefix filter to keep in
    // step with the one above.
    if failed.is_empty() && st.renamed.is_empty() {
        return None;
    }
    Some(BrokenLocalRules {
        dir: st.dir,
        failed,
        renamed: st.renamed,
        files_loaded: st.files_loaded,
        rules: st.rules,
    })
}

/// The pure half, so the rules that consume these can be tested from a state
/// value.
pub fn signals_from(st: &State) -> (Vec<AvailableUpdate>, Vec<FailedUpdate>, Vec<StalledUpdate>) {
    let mut available = Vec::new();
    let mut failed = Vec::new();
    let mut stalled = Vec::new();
    for c in Component::ALL {
        let cs = st.get(c);
        // An offer the last check REFUSED is not an offer the user is declining
        // — it is the refusal, already carded by `detection.update_failed.v1`.
        // Offering it again would double-card one event and, worse, blame the
        // user's own settings for it: the available card's rationale says "this
        // component is set to check-only, or the bundle needs a newer cImp",
        // which is false when the mode is `auto` (#48). The version stays in
        // `available_version` regardless — Settings keeps saying which bundle
        // was refused, and Apply stays live so a retry is one click away.
        let offer_was_refused = cs.available_version == cs.last_failure_version;
        if !cs.available_version.is_empty() && !offer_was_refused {
            available.push(AvailableUpdate {
                component: c.as_str().to_string(),
                installed: cs.installed_version.clone(),
                available: cs.available_version.clone(),
                notes: cs.available_notes.clone(),
            });
        }
        if !cs.last_failure.is_empty() {
            failed.push(FailedUpdate {
                component: c.as_str().to_string(),
                version: cs.last_failure_version.clone(),
                // A state file written before `last_failure_signature` existed
                // still has to key on something non-empty, so fall back to the
                // same derivation `finish` would have used.
                signature: if cs.last_failure_signature.is_empty() {
                    failure_signature(&cs.last_failure_version, &cs.last_failure)
                } else {
                    cs.last_failure_signature.clone()
                },
                reason: cs.last_failure.clone(),
            });
        }
        // The freshness canary, keyed on the outcome-agnostic counter (#48).
        // `unreachable_streak` cannot see a channel that answers every day and
        // refuses every bundle: the streak never leaves 0, the failure card's
        // signature never ages, and one dismissal freezes the component with
        // no signal at all — exactly the state decision 13 exists to prevent.
        //
        // Suppressed while a takeable offer stands: the user already has a card
        // naming the version and the button, and a second card saying "nothing
        // is landing" would be the same event twice. `offer_was_refused` is why
        // this is not simply `available_version.is_empty()` — a refused bundle
        // sits in that slot and cards nothing.
        //
        // The threshold is applied HERE, not in the Advisor: it is the updater's
        // policy about its own data, and the rule that renders it should not be
        // able to disagree with the counter that feeds it.
        let offer_stands = !cs.available_version.is_empty() && !offer_was_refused;
        if cs.stale_streak >= STALLED_AFTER_CHECKS && !offer_stands {
            stalled.push(StalledUpdate {
                component: c.as_str().to_string(),
                streak: cs.stale_streak,
                reason: cs.last_outcome.clone(),
            });
        }
    }
    (available, failed, stalled)
}

/// The dismissal key for a refusal.
///
/// The bundle version when there is one, so a dismissal holds for that bundle
/// and re-fires on the next — the spec's claim, which was false while
/// manifest-level failures all signed themselves `component:` (#46). With no
/// version to key on, the REASON is the thing that distinguishes one refusal
/// from another, so it is hashed into the key: dismissing "schema 2 is not
/// supported" leaves a later "artifact points outside the curated directory"
/// free to fire.
///
/// Derived rather than passed in, so no call site can reintroduce an empty
/// signature by forgetting an argument.
pub fn failure_signature(version: &str, reason: &str) -> String {
    if !version.trim().is_empty() {
        return version.to_string();
    }
    let digest = manifest::sha256_hex(reason.trim().as_bytes());
    format!("reason:{}", &digest[..16])
}

// ── One component's run ────────────────────────────────────────────────────

/// The result of checking (and possibly applying) one component.
#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub component: Component,
    pub outcome: Outcome,
    pub version: String,
    pub detail: String,
}

/// Check one component against `man`, applying it when `mode` allows.
///
/// Separated from the manifest fetch so one download serves both components,
/// and so tests drive the whole pipeline from a parsed manifest.
pub async fn run_component(
    c: Component,
    man: &Manifest,
    mode: Mode,
    fetcher: &dyn Fetcher,
    layout: &Layout,
    reload: Reloader<'_>,
) -> RunResult {
    let now = crate::activity::now_ms();
    let root = layout.state_root.clone();
    let Some(entry) = man.components.get(&c) else {
        return finish(
            c,
            &root,
            now,
            Outcome::UpToDate,
            String::new(),
            format!(
                "the manifest lists no `{}` component; nothing to do",
                c.as_str()
            ),
        );
    };
    let installed = state_at(&root).get(c).installed_version.clone();
    if !manifest::is_newer(&entry.version, &installed) {
        return finish(
            c,
            &root,
            now,
            Outcome::UpToDate,
            entry.version.clone(),
            format!(
                "installed version `{}` is current (the manifest offers `{}`)",
                if installed.is_empty() {
                    "(shipped)"
                } else {
                    installed.as_str()
                },
                entry.version
            ),
        );
    }
    if !manifest::app_version_satisfies(entry.min_app_version.as_deref()) {
        return finish(
            c,
            &root,
            now,
            Outcome::Available,
            entry.version.clone(),
            format!(
                "version `{}` needs cImp {} or newer (this build is {}) — update the app to take it",
                entry.version,
                entry.min_app_version.as_deref().unwrap_or("?"),
                env!("CARGO_PKG_VERSION")
            ),
        );
    }
    if mode != Mode::Auto {
        return finish(
            c,
            &root,
            now,
            Outcome::Available,
            entry.version.clone(),
            format!(
                "version `{}` is available; this component is set to check-only, so nothing was \
                 downloaded and nothing on disk changed",
                entry.version
            ),
        );
    }

    // ── auto: download, verify, validate, activate ──────────────────────
    let staging = store::staging_dir(&root, c);
    store::wipe_dir(&staging);
    let applied = apply_component(c, entry, fetcher, layout, &staging, reload).await;
    // Whatever happened, the staging directory does not outlive the run — a
    // rejected bundle must leave nothing behind that a later reader could
    // mistake for validated content.
    store::wipe_dir(&staging);
    match applied {
        Ok(detail) => finish(c, &root, now, Outcome::Applied, entry.version.clone(), detail),
        // The version rides along for the row label in both cases; `finish`
        // decides what, if anything, is recorded about the DATA.
        Err(ApplyFailure::Rejected(detail)) => finish(
            c,
            &root,
            now,
            Outcome::Rejected,
            entry.version.clone(),
            detail,
        ),
        Err(ApplyFailure::Unreachable(detail)) => finish(
            c,
            &root,
            now,
            Outcome::Unavailable,
            entry.version.clone(),
            detail,
        ),
    }
}

/// Why an apply did not happen — the #46 outcome split, applied to the
/// **artifact** fetch this time (#48, M-9).
///
/// #46 split "the channel never answered" from "a document reached us and a
/// check said no" at the manifest fetch, and stopped there. Everything after it
/// funnelled through one `Result<String, String>` that
/// [`run_component`] recorded as [`Outcome::Rejected`], so an artifact 404, a
/// timeout, a proxy login page, a dropped connection mid-download — none of
/// which is a bundle being refused, and none of which is a decision anyone
/// made — raised the security card that means *someone published something we
/// would not take*, wrote an `ok:false` row, and **reset `unreachable_streak`**,
/// which is the counter whose whole job is to notice that the channel has gone
/// quiet.
///
/// That is not a corner case here. The deploy note publishes the manifest and
/// the artifacts as separate steps, so "manifest reachable, artifact not yet"
/// is the ordinary state of a half-published channel — the likely steady state
/// on the day `detection-v1` first goes up, and a daily red card for a bundle
/// that is perfectly fine.
///
/// Exactly one thing maps to `Unreachable`: a [`Fetcher::get`] transport error
/// on an artifact URL. A response that ARRIVED and disagrees with the manifest
/// — wrong size, wrong digest — is a refusal, because a document reached us and
/// a check said no. Stated as a two-variant enum rather than a string prefix so
/// a future call site has to answer the question.
enum ApplyFailure {
    /// A bundle reached us and a check refused it.
    Rejected(String),
    /// The artifact could not be fetched at all. Nothing was refused.
    Unreachable(String),
}

impl From<String> for ApplyFailure {
    /// Every `?` inside [`apply_component`] that is not the artifact fetch
    /// itself is a refusal. Deliberately the default, so forgetting to classify
    /// fails toward the louder card rather than toward silence.
    fn from(s: String) -> Self {
        ApplyFailure::Rejected(s)
    }
}

/// Download + verify + validate + activate. Every early return leaves the live
/// data exactly as it was.
async fn apply_component(
    c: Component,
    entry: &manifest::ComponentEntry,
    fetcher: &dyn Fetcher,
    layout: &Layout,
    staging: &Path,
    reload: Reloader<'_>,
) -> Result<String, ApplyFailure> {
    std::fs::create_dir_all(staging).map_err(|e| format!("create {}: {e}", staging.display()))?;

    // Fetch each artifact into memory, verify its size and digest, and only
    // then write it. Nothing untrusted touches disk before its checksum is
    // confirmed, and nothing reaches a parser before that either.
    for f in &entry.files {
        let bytes = match fetcher.get(&f.url, f.size).await {
            Ok(b) => b,
            // Transport, on an artifact. The manifest answered and this did
            // not; nothing was refused (#48, M-9).
            Err(e) if e.kind == manifest::FetchErrorKind::Transport => {
                return Err(ApplyFailure::Unreachable(format!(
                    "`{}` from version `{}` could not be downloaded ({e}). Nothing was written and \
                     the current detection data is still live.",
                    f.name, entry.version
                )))
            }
            // A body arrived and is bigger than the manifest says it is. The
            // symmetric case — a body SHORT of its declared size — is caught by
            // the length check below and refused, so this one must be refused
            // too, or the same disagreement would be an outage in one direction
            // and a refusal in the other.
            Err(e) => {
                return Err(ApplyFailure::Rejected(format!(
                    "`{}` is larger than the {} bytes the manifest declares ({e}) — rejected \
                     before the content was written or parsed",
                    f.name, f.size
                )))
            }
        };
        if bytes.len() as u64 != f.size {
            return Err(ApplyFailure::Rejected(format!(
                "`{}` is {} bytes but the manifest declares {} — rejected before the content was \
                 written or parsed",
                f.name,
                bytes.len(),
                f.size
            )));
        }
        let got = manifest::sha256_hex(&bytes);
        if got != f.sha256 {
            return Err(ApplyFailure::Rejected(format!(
                "checksum mismatch on `{}` (expected {}, got {}) — rejected before the content \
                 was written or parsed",
                f.name, f.sha256, got
            )));
        }
        std::fs::write(staging.join(&f.name), &bytes)
            .map_err(|e| format!("stage `{}`: {e}", f.name))?;
    }
    let names: Vec<String> = entry.files.iter().map(|f| f.name.clone()).collect();
    validate::staged_files_present(staging, c, &names)?;

    // Validation and activation are blocking work (a YARA compile bounded by
    // `validate::COMPILE_BUDGET`, an ONNX session, a handful of file moves) and
    // run inline rather than on the blocking pool. Two reasons: the reloader is
    // a borrowed closure that cannot be moved into a `spawn_blocking` task, and
    // this runs at most once a day on a background task with nothing waiting on
    // it — the seconds-scale ceiling is the whole point of having a ceiling.
    match c {
        Component::Rules => {
            let _ = &names;
            // Past the fetch, every failure is a document being refused.
            validate_and_activate_rules(staging, layout, &entry.version, reload)
                .map_err(ApplyFailure::Rejected)
        }
    }
}

/// The rules half.
/// Fraction of the live rule count a candidate bundle may not fall below.
///
/// Curation churn moves the count by a few rules; halving it is not curation.
const COVERAGE_FLOOR: usize = 2;

/// Refuse a bundle that would sharply shrink the live rule set.
///
/// **This is a curation guard, not an anti-tamper control**, and the distinction
/// is worth being honest about (#48, N-10 / the H-6 decision). The gauntlet's
/// positive control is the shipped `smoke/hostile/` corpus — public, on every
/// user's disk — so a bundle of three rules that match exactly those documents
/// and nothing else compiles clean, scans fast, hits every hostile control,
/// misses every benign one, and activates green. `validate.rs`'s own header
/// claims to stop a bundle that "would quietly disable the layer", and that
/// bundle walks straight through.
///
/// What this does NOT defend against is a hostile publisher: anyone who can
/// write the manifest can also write the rule count, and — since the channel's
/// trust root is `contents: write` on the repo that ships the binary — can also
/// ship a cImp release with detection removed outright. That is precisely why
/// bundle signing was declined (H-6): a key reachable by the compromise it
/// defends against is ceremony.
///
/// What it DOES catch is the likelier failure by far: a curator publishing a
/// half-built bundle. That is worth ten lines.
///
/// Compares against the **shipped** set only — `store::managed_rule_files` is
/// non-recursive, so a user's `local/` rules never inflate the baseline and a
/// user who writes twenty of their own cannot make every future bundle look
/// like a regression. An unreadable or empty live set yields no baseline and
/// the check passes: a first install has nothing to compare against.
fn coverage_floor(candidate_rules: usize, dest: &Path) -> Result<(), String> {
    let live: Vec<(String, String)> = store::managed_rule_files(dest)
        .into_iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().to_string();
            Some((name, std::fs::read_to_string(&p).ok()?))
        })
        .collect();
    if live.is_empty() {
        return Ok(());
    }
    let (compiled, _failed) = super::signature::compile_sources(&live);
    let Some(compiled) = compiled else {
        // The live set does not compile, so it is not a baseline worth
        // defending — the candidate can only be an improvement.
        return Ok(());
    };
    let live_rules = compiled.iter().count();
    if live_rules > 0 && candidate_rules * COVERAGE_FLOOR < live_rules {
        return Err(format!(
            "coverage floor: the candidate bundle carries {candidate_rules} rule(s) against the \
             {live_rules} currently live — a drop that large is a half-built bundle, not curation. \
             The smoke corpus cannot catch this on its own (a bundle matching only the shipped \
             hostile controls passes every other gate), so the count is checked directly."
        ));
    }
    Ok(())
}

fn validate_and_activate_rules(
    staging: &Path,
    layout: &Layout,
    version: &str,
    reload: Reloader<'_>,
) -> Result<String, String> {
    let sources = super::signature::read_sources(staging);
    let corpus = validate::load_corpus(&layout.smoke_dir);
    let report = validate::validate_rules(&sources, &corpus)?;
    coverage_floor(report.rules, layout.dest(Component::Rules))?;
    // #48, U-4: snapshot which `local/` files are ALREADY broken, before the
    // swap, so the post-activation health check judges the BUNDLE rather than
    // the directory. Taken here (not inside `activate`) because it must be read
    // from the destination while the OLD bundle is still in it.
    let baseline = LocalBaseline::snapshot(layout.dest(Component::Rules));
    let judged = |c: Component, dir: &Path| match reload(c, dir) {
        Ok(live) => Ok(live),
        Err(why) => baseline.forgive(dir, why),
    };
    // The live description, not the validation report's counts (#48, M-13).
    // They are close but not the same number — validation compiles the bundle
    // alone, the live set includes `rules.d/local/` — and only this one can say
    // that a user file was skipped, which after M-13 is a thing an APPLIED
    // update has to be able to report. Dropping it on the floor is how U-4's
    // forgiveness message stayed invisible for two fix rounds.
    let live = activate(Component::Rules, staging, layout, version, &judged)?;
    Ok(format!(
        "activated rules `{version}`: {live}. Validated against {} benign + {} hostile control \
         document(s) ({} bundle file(s), {} rule(s); compile {} ms, slowest scan {} ms). The \
         previous bundle is retained and can be reverted from Settings; `rules.d/local/` was not \
         touched.",
        report.benign_samples,
        report.hostile_samples,
        report.files,
        report.rules,
        report.compile_ms,
        report.slowest_scan_ms
    ))
}

/// Swap the staged files into the component's destination, archiving the
/// current ones, then hot-reload.
///
/// The ordering is what "atomic-as-possible" means here. A directory swap has
/// no all-or-nothing primitive across two multi-file directories, so instead:
/// the outgoing files are archived FIRST, the incoming ones moved in second,
/// and a failure at **any** point in either step is undone before returning.
/// The window in which the destination is short of files is the move loop
/// itself, and the only way out of it is "new set live and healthy" or "old set
/// back".
///
/// The hot-reload is part of the transaction, not a follow-up: a set that moved
/// perfectly but does not LOAD (an identifier collision with a `local/` rule, a
/// file quarantined by antivirus between validation and activation) is rolled
/// back exactly like a failed move.
///
/// # The two loops need opposite undos (#48, U-2)
///
/// As first built only the *second* loop rolled back; the archive loop
/// propagated its first error with a bare `?`, so the most ordinary Windows
/// failure there is — AV real-time scanning, or the user holding a rule file
/// open through the panel's own *Open rules folder* button, making both
/// `rename` and `copy` fail with a sharing violation — left `rules.d` holding a
/// subset of its files, with no reload, no rollback and no `previous_version`
/// recorded, so Revert stayed disabled. The signature layer then ran at reduced
/// coverage across every restart: exactly the silent degradation decision 13
/// forbids.
///
/// The undos are **not** the same, which is why there are two of them:
/// [`roll_back`] clears the destination before restoring, because after the
/// move loop started the destination holds staged files that must not survive;
/// [`restore_archived`] alone is what the archive loop needs, because at that
/// point the destination still holds every file the loop has not reached and
/// clearing it would destroy the only copy of them.
fn activate(
    c: Component,
    staging: &Path,
    layout: &Layout,
    version: &str,
    reload: Reloader<'_>,
) -> Result<String, String> {
    let dest = layout.dest(c);
    let root = &layout.state_root;
    std::fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    // ONE label for the outgoing version, used both for the archive directory
    // and for the `previous_version` recorded in state. They were briefly two
    // expressions and the empty case diverged (`unknown` on disk vs.
    // `(shipped)` in state), which made Revert look for a directory that had
    // never existed — the archive path and the recorded name must be derived
    // from the same string.
    let installed = state_at(root).get(c).installed_version.clone();
    let outgoing_version = if installed.is_empty() {
        SHIPPED_VERSION.to_string()
    } else {
        installed
    };
    let archive = store::previous_dir(root, c, &outgoing_version);
    let mut archived: Vec<(PathBuf, PathBuf)> = prepare_archive(c, &archive, dest);

    // Archive the current managed set. For rules this is the top level of
    // `rules.d` only, so `local/` is untouched by construction.
    journal(root, c, store::Phase::Archiving, &archive, dest);
    for p in store::managed_files(dest, c) {
        let name = p.file_name().unwrap_or_default().to_os_string();
        let to = archive.join(&name);
        if let Err(e) = store::move_file(&p, &to) {
            // Restore-only: the files this loop has NOT reached are still at
            // `dest` and are the only copy of themselves.
            let note = restore_only(c, root, dest, &archived, reload);
            return Err(format!(
                "archiving the current bundle failed ({e}); nothing was replaced and the previous \
                 version is still live{note}"
            ));
        }
        archived.push((to, p.clone()));
    }

    // Move the staged set in. On any failure, put the archive back.
    journal(root, c, store::Phase::Moving, &archive, dest);
    for p in store::managed_files(staging, c) {
        let name = p.file_name().unwrap_or_default().to_os_string();
        if let Err(e) = store::move_file(&p, &dest.join(&name)) {
            let note = roll_back(c, root, &archive, dest, &archived, reload);
            return Err(format!(
                "activating the staged bundle failed ({e}); the previous version was restored{note}"
            ));
        }
    }

    match reload(c, dest) {
        Ok(live) => {
            // Cleared BEFORE the state write, deliberately. A crash in the gap
            // leaves the new files live and the state still naming the old
            // version, so the next check simply applies the same bundle again —
            // idempotent. The other order would let a crash leave a journal
            // that undoes an update the state already claims.
            store::clear_journal(root);
            update_state_at(root, |s| {
                let cs = s.get_mut(c);
                cs.previous_version = outgoing_version.clone();
                cs.installed_version = version.to_string();
                // A full swap resolves any outstanding restore debt by
                // construction: `dest` now holds a complete validated set and
                // `archive` holds a complete outgoing one (see
                // `prepare_archive`). Nothing is missing, so nothing is owed.
                cs.unrestored_files.clear();
            });
            info!(
                target: "offload",
                component = c.as_str(),
                version,
                live = %live,
                "detection updater: bundle activated"
            );
            Ok(live)
        }
        Err(why) => {
            let note = roll_back(c, root, &archive, dest, &archived, reload);
            Err(format!(
                "the activated bundle did not load cleanly ({why}); the previous version was \
                 restored{note}"
            ))
        }
    }
}

/// Ready `archive` to receive the outgoing set, **keeping any file the
/// destination is missing** (#48, M-11's other half).
///
/// This used to be a bare [`store::wipe_dir`], and with M-11 fixed that becomes
/// a data-loss path rather than hygiene. A rollback that could not put
/// `core.yar` back leaves it in `previous/<outgoing>/` — the archive of the very
/// version being replaced — as the only copy in existence. The next check then
/// downloads a newer bundle, computes the same archive path from the same
/// unchanged `installed_version`, and wipes it.
///
/// The file belongs where it already is: this archive is the outgoing version's
/// archive, and a file the destination lacks is part of that version and
/// nothing else. So it stays, and the returned `archived` list starts with it —
/// which also means a rollback puts the COMPLETE old set back, repairing the
/// debt rather than perpetuating it.
///
/// Everything else in the archive is a stale copy of a file that is still live
/// at `dest` (the previous run's archive of the same version) and is removed,
/// so the archive never accumulates.
///
/// Returns the `(in-archive, restore-to)` pairs for the files kept, in the same
/// shape the archive loop appends to.
fn prepare_archive(c: Component, archive: &Path, dest: &Path) -> Vec<(PathBuf, PathBuf)> {
    let live: BTreeSet<std::ffi::OsString> = store::managed_files(dest, c)
        .iter()
        .map(|p| p.file_name().unwrap_or_default().to_os_string())
        .collect();
    let mut kept = Vec::new();
    for p in store::managed_files(archive, c) {
        let name = p.file_name().unwrap_or_default().to_os_string();
        if live.contains(&name) {
            let _ = std::fs::remove_file(&p);
        } else {
            kept.push((p.clone(), dest.join(&name)));
        }
    }
    if kept.is_empty() {
        // Nothing worth keeping: wipe the directory itself so non-rule
        // leftovers (a partial download, a file from a build that managed a
        // different extension set) do not accumulate either.
        store::wipe_dir(archive);
    } else {
        warn!(
            target: "offload",
            component = c.as_str(),
            archive = %archive.display(),
            files = kept.len(),
            "detection updater: the retained copy still holds file(s) the live set is missing \
             from an earlier failed restore; keeping them rather than wiping the last copy"
        );
    }
    kept
}

/// Record an in-flight swap so a crash between the two loops is recoverable
/// ([`store::Journal`]).
fn journal(root: &Path, c: Component, phase: store::Phase, archive: &Path, dest: &Path) {
    store::write_journal(
        root,
        &store::Journal {
            component: c.as_str().to_string(),
            phase,
            archive: archive.to_path_buf(),
            dest: dest.to_path_buf(),
        },
    );
}

/// Finish an interrupted swap, once, before this run touches anything.
///
/// The recovery decision 13's "old data stays live on any failure" needs to
/// survive a **kill**, not just an error return (#48, U-2). Without it a crash
/// between the two loops left `rules.d` short, and the next activation then
/// recomputed the archive path from the unchanged `installed_version` and
/// [`store::wipe_dir`]'d it — turning a recoverable interruption into the
/// permanent loss of the only surviving copy of the old bundle.
///
/// Called from [`run`] and [`revert`] under the run lock, so it can never race
/// the swap it is repairing. A journal whose recorded destination is not this
/// layout's is discarded untouched: the exe moved, and those paths are not
/// ours to write to.
fn recover_interrupted(layout: &Layout, reload: Reloader<'_>) {
    let root = &layout.state_root;
    let Some(j) = store::read_journal(root) else {
        return;
    };
    let Some(c) = Component::parse(&j.component) else {
        warn!(
            target: "offload",
            component = %j.component,
            "detection updater: an activation journal names a component this build does not know; \
             discarding it"
        );
        store::clear_journal(root);
        return;
    };
    let dest = layout.dest(c);
    if j.dest != dest {
        warn!(
            target: "offload",
            recorded = %j.dest.display(),
            current = %dest.display(),
            "detection updater: an activation journal points at a different destination than this \
             layout; discarding it rather than writing to a path we no longer own"
        );
        store::clear_journal(root);
        return;
    }
    let archived: Vec<(PathBuf, PathBuf)> = store::managed_files(&j.archive, c)
        .into_iter()
        .map(|p| {
            let name = p.file_name().unwrap_or_default().to_os_string();
            (p, dest.join(&name))
        })
        .collect();
    if archived.is_empty() {
        // Nothing was archived before the interruption, or the archive is
        // already back. There is nothing to undo and nothing to lose — and no
        // debt either, so the state field is cleared with the journal.
        // Nothing owed and nothing to say: the note is for a caller composing
        // an error message, and this path has none.
        let _ = settle_restore(root, c, &[]);
        return;
    }
    match j.phase {
        // The destination still holds every file the archive loop did not
        // reach; clearing it would destroy them.
        store::Phase::Archiving => {}
        // The destination holds however many staged files landed. They must go:
        // old-plus-some-new is a set no curation step ever validated.
        store::Phase::Moving => {
            for p in store::managed_files(dest, c) {
                let _ = std::fs::remove_file(&p);
            }
        }
        // **A rollback was itself in flight (#48, M-10).** The destination has
        // already been cleared of staged files and holds however much of the
        // archive got put back. Deleting `managed_files(dest)` here — which is
        // what `Moving` does, and what this state used to be misread as — would
        // destroy exactly those restored files, and the archive no longer holds
        // a second copy. Restore-only, and idempotent, so running it again over
        // a rollback that actually finished is a no-op.
        store::Phase::Restoring => {}
    }
    let unrestored = restore_archived(&archived);
    let debt = settle_restore(root, c, &unrestored);
    if debt.is_empty() {
        warn!(
            target: "offload",
            component = c.as_str(),
            phase = ?j.phase,
            files = archived.len(),
            "detection updater: an update was interrupted mid-swap; the previous version was \
             restored"
        );
    } else {
        // Deliberately NOT the reassuring sentence above: `settle_restore` has
        // already kept the journal, so this repeats on the next run — and the
        // Advisor card is what the user actually sees.
        warn!(
            target: "offload",
            component = c.as_str(),
            phase = ?j.phase,
            files = %unrestored.join(", "),
            "detection updater: an interrupted update could only be PARTLY undone; the live set \
             is short of these files and the retry is queued"
        );
    }
    if let Err(e) = reload(c, dest) {
        warn!(
            target: "offload",
            component = c.as_str(),
            error = %e,
            "detection updater: the recovered version did not reload cleanly"
        );
    }
}

/// Finish an interrupted swap against `layout`, taking the run lock — the entry
/// point for callers that are not already inside a [`run`].
///
/// Only [`store::acquire_run_lock`] is taken, not the process-local
/// [`run_lock`] mutex as well, and that is deliberate: the file lock excludes
/// **every** contender including this process (its staleness rule is age, never
/// pid — see [`store::acquire_run_lock`]), so it is sufficient on its own, and
/// taking the async mutex from a synchronous launch path would mean either
/// `blocking_lock` (which panics if a runtime is ever wrapped around startup)
/// or `try_lock` (which would silently skip the repair whenever anything else
/// happened to hold it). One lock, one story.
///
/// Declining is safe: a peer holding the lock is inside `run`, which does this
/// same recovery on the way in.
pub fn recover_now(layout: &Layout, reload: Reloader<'_>) {
    let file_lock = match store::acquire_run_lock(&layout.state_root, crate::activity::now_ms()) {
        Ok(l) => l,
        Err(e) => {
            warn!(
                target: "offload",
                error = %e,
                "detection updater: skipping crash recovery; another instance holds the run lock"
            );
            return;
        }
    };
    recover_interrupted(layout, reload);
    drop(file_lock);
}

/// **Crash recovery at launch, unconditionally (#48, M-12).**
///
/// Recovery used to reach the disk from exactly one place: [`run`], which
/// [`tick_once`] calls only when [`updates_enabled`] resolves true AND a
/// component is `check`/`auto` AND [`is_due`] says so. Every one of those is a
/// question about *fetching updates*, and none of them is a question about
/// *whether the rule set on disk is complete*.
///
/// So the failure was: a crash mid-swap leaves `rules.d` short; the user — quite
/// reasonably, having just seen the app die — switches detection off, or sets
/// the component to `off`, or simply is not due for another 23 hours. The
/// journal then sits there and `rules.d` stays short across every restart,
/// which is the silent permanent degradation decision 13 exists to forbid,
/// reached by a switch that has nothing to do with it. "Never degrade to no
/// rules" must not be conditional on an unrelated preference.
///
/// Called from [`detection::init`](super::init), before the first
/// [`signature::reload`](super::signature::reload), so the set that compiles at
/// startup is the repaired one. Takes no `Settings` **by construction** — there
/// is no switch it could consult and no way for a future edit to gate it on one
/// without changing this signature.
pub fn recover_on_launch() {
    let Some(layout) = Layout::resolve() else {
        return;
    };
    // An unlocked peek first, so the overwhelmingly common case — no journal —
    // costs one failed `read_to_string` and writes NOTHING. Taking the lock
    // straight away would create `detection-updates/` on every launch of every
    // install, including one with detection switched off, which would quietly
    // spend the module header's "inert when off" promise on a repair that is
    // almost never needed.
    //
    // Not a race: this only decides whether to bother. If a peer wrote the
    // journal a moment ago we take the lock and act; if a peer cleared it, the
    // authoritative re-read inside `recover_interrupted` (under the lock) finds
    // nothing and returns. The unsafe direction — acting on a stale read — is
    // the one the lock covers.
    // `has_journal`, not `read_journal`: the latter deletes what it cannot
    // parse, and `write_journal` is a plain `fs::write`, so an unlocked reader
    // can catch a peer's journal half-written and destroy the record of a swap
    // that is in flight.
    if !store::has_journal(&layout.state_root) {
        return;
    }
    recover_now(&layout, &live_reload);
}

/// The label recorded for the bundle that shipped with the app — the one
/// version that has no manifest entry. Displayed in Settings as the revert
/// target, so it has to read as something a user recognizes.
pub const SHIPPED_VERSION: &str = "(shipped)";

/// Put archived files back where they came from, and report the ones that
/// would not go (#48, M-11).
///
/// The half of a rollback that is safe at **any** point of a swap, because it
/// only ever writes files the archive already holds — and therefore idempotent,
/// which is what makes [`store::Phase::Restoring`] recoverable by simply
/// running it again.
///
/// **The return value is the whole of M-11.** This used to be `-> ()` with a
/// `warn!` per failure, and every caller then reported "the previous version
/// was restored" verbatim. Nothing downstream could contradict it: the
/// post-rollback health check compiles what IS on disk, and a file that is
/// absent contributes no compile error, no `files_failed`, and no missing
/// `rules` beyond the ones it carried — so `Status::healthy` came back true
/// about a rule set that had silently lost a file. The one outcome that
/// permanently reduces coverage had the most reassuring message in the module.
///
/// A failed restore leaves the file in the archive, which is why it is
/// recoverable at all: see [`settle_restore`] for what is done with this list.
#[must_use]
fn restore_archived(archived: &[(PathBuf, PathBuf)]) -> Vec<String> {
    let mut unrestored = Vec::new();
    for (from, to) in archived {
        if let Err(e) = store::move_file(from, to) {
            warn!(
                target: "offload",
                from = %from.display(),
                to = %to.display(),
                error = %e,
                "detection updater: could not restore a previous file"
            );
            unrestored.push(
                to.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
            );
        }
    }
    unrestored
}

/// Record what a restore attempt achieved, and leave the disk in a state the
/// next run can finish (#48, M-10 + M-11).
///
/// # Loud and degraded, not escalated
///
/// A restore that could not put every file back leaves the layer running on a
/// short rule set. The tempting "escalate" — refuse to run the updater, or
/// disarm the layer until a human intervenes — is exactly backwards: it would
/// trade a *partial* rule set for *no* rule set, which is the one thing
/// decision 13 forbids, over a condition whose overwhelmingly likely cause
/// (a sharing violation from AV real-time scanning, or a file held open through
/// the panel's own *Open rules folder* button) clears by itself within minutes.
///
/// So: loud, degraded, and repaired automatically.
///
/// - **Durable.** The `Restoring` journal is left on disk, so the missing files
///   stay in the archive and every later run — and, since M-12, every launch —
///   retries the move. The retry is [`restore_archived`] itself, which is
///   idempotent.
/// - **Visible.** The names land in
///   [`store::ComponentState::unrestored_files`], which Settings renders and
///   `detection.rules_incomplete.v1` cards. The rule set really is short, and
///   the user is the only one who can unlock a locked file.
/// - **Honest.** The returned sentence is appended to the caller's own message,
///   so no path can say "the previous version was restored" full stop while
///   this is non-empty.
///
/// Empty on the ordinary path: the journal is cleared, the state field is
/// cleared, and the returned note is the empty string, so every existing
/// message is unchanged.
#[must_use]
fn settle_restore(root: &Path, c: Component, unrestored: &[String]) -> String {
    update_state_at(root, |s| {
        s.get_mut(c).unrestored_files = unrestored.to_vec();
    });
    if unrestored.is_empty() {
        store::clear_journal(root);
        return String::new();
    }
    warn!(
        target: "offload",
        component = c.as_str(),
        files = %unrestored.join(", "),
        "detection updater: a rollback could not put every file back; the live set is short of \
         them and the journal is kept so the next run retries"
    );
    format!(
        " — but {} file(s) could not be put back ({}), so the live set is running SHORT of them; \
         they are still in the retained copy and every later check (and the next launch) retries \
         the restore",
        unrestored.len(),
        unrestored.join(", ")
    )
}

/// Remove whatever is at `dest` for this component, put the archive back, and
/// reload. The undo for a failure **after** the staged set started landing.
///
/// Returns the note the caller appends to its own error: a rollback that put
/// the files back but could not recompile them is a second, separate problem,
/// and it used to be visible only as a `warn!` in a log nobody was reading
/// (#48). Empty on the ordinary path, so the existing messages are unchanged.
///
/// # The journal moves to `Restoring` between the two halves (#48, M-10)
///
/// The destructive half runs while the journal still reads `Moving`, which is
/// the correct undo for a kill inside it: the destination holds staged files
/// and the archive holds the complete outgoing set, so "clear the destination,
/// restore the archive" is right whether or not the delete loop finished.
///
/// The moment the destination is clear, that stops being true — from here on
/// the destination holds RESTORED files, and `Moving`'s recovery would delete
/// them and then restore only whatever remained in the archive. That is M-10:
/// a crash mid-rollback destroyed the difference, permanently, and reported
/// "the previous version was restored". So the phase is advanced first, and
/// `Restoring`'s recovery never deletes anything.
#[must_use]
fn roll_back(
    c: Component,
    root: &Path,
    archive: &Path,
    dest: &Path,
    archived: &[(PathBuf, PathBuf)],
    reload: Reloader<'_>,
) -> String {
    for p in store::managed_files(dest, c) {
        let _ = std::fs::remove_file(&p);
    }
    journal(root, c, store::Phase::Restoring, archive, dest);
    let unrestored = restore_archived(archived);
    let debt = settle_restore(root, c, &unrestored);
    format!("{}{debt}", reload_note(c, dest, reload))
}

/// The archive loop's undo: restore only, never clear the destination (see
/// [`activate`]), with the same debt handling as [`roll_back`].
///
/// No phase change is needed on the way in — the journal already reads
/// `Archiving`, whose recovery is restore-only too, so a kill anywhere in here
/// recovers to exactly the place this is heading.
#[must_use]
fn restore_only(
    c: Component,
    root: &Path,
    dest: &Path,
    archived: &[(PathBuf, PathBuf)],
    reload: Reloader<'_>,
) -> String {
    let unrestored = restore_archived(archived);
    let debt = settle_restore(root, c, &unrestored);
    format!("{}{debt}", reload_note(c, dest, reload))
}

/// Reload after a rollback and turn a failure into a sentence the caller can
/// append. Warned as well as returned: the return reaches the Advisor card and
/// the activity row, the log reaches whoever is diagnosing the machine.
#[must_use]
fn reload_note(c: Component, dest: &Path, reload: Reloader<'_>) -> String {
    match reload(c, dest) {
        Ok(_) => String::new(),
        Err(e) => {
            warn!(
                target: "offload",
                component = c.as_str(),
                error = %e,
                "detection updater: the restored version did not reload cleanly"
            );
            format!(" — but the restored version did not reload cleanly either ({e})")
        }
    }
}

/// Record the outcome, write the row, and return it.
///
/// One funnel, so no path can produce an outcome without also producing its
/// consumers: the state record, the activity row, and (through the state) the
/// Advisor card.
fn finish(
    c: Component,
    root: &Path,
    now_ms: u64,
    outcome: Outcome,
    version: String,
    detail: String,
) -> RunResult {
    update_state_at(root, |s| {
        let cs: &mut ComponentState = s.get_mut(c);
        cs.last_check_ms = now_ms;
        cs.last_outcome = detail.clone();
        cs.last_ok = outcome.ok();
        cs.last_outcome_kind = outcome.as_str().to_string();
        // Did this outcome prove the channel answered? An exhaustive match and
        // not `!= Unavailable` (#48): the negated form silently counted the two
        // REVERT outcomes as proof of reachability, so a user on a permanently
        // blocked proxy could zero a six-check streak by clicking Revert, and
        // clicking it weekly would suppress the stall card indefinitely. A
        // revert reaches nothing. Stated as a match so a future outcome has to
        // answer the question rather than inherit an answer.
        match outcome {
            // The channel produced a document — being told "no" proves it
            // answered just as well as being told "here it is".
            Outcome::UpToDate | Outcome::Available | Outcome::Applied | Outcome::Rejected => {
                cs.unreachable_streak = 0;
            }
            Outcome::Unavailable => {
                cs.unreachable_streak = cs.unreachable_streak.saturating_add(1);
            }
            // Local file work. It touches no network in either direction, so it
            // is neither evidence of reachability nor of silence.
            Outcome::Reverted | Outcome::RevertFailed => {}
        }
        // Did this outcome leave the component FRESHER? The canary decision 13
        // asks for, and the one that survives a dismissed failure card (#48).
        match outcome {
            // The only two outcomes that prove the installed data IS the
            // currently published data.
            Outcome::Applied | Outcome::UpToDate => cs.stale_streak = 0,
            // Everything else is a check that came and went with the component
            // no fresher than before — refused, unreachable, or offered
            // something it did not take.
            Outcome::Available | Outcome::Rejected | Outcome::Unavailable => {
                cs.stale_streak = cs.stale_streak.saturating_add(1);
            }
            // A revert is not a check. It says nothing about what is published,
            // so it neither confirms freshness nor counts against it.
            Outcome::Reverted | Outcome::RevertFailed => {}
        }
        match outcome {
            Outcome::Available => {
                cs.available_version = version.clone();
            }
            // A successful apply, a clean check or a revert clears both the
            // pending offer and the failure record: the condition each card
            // reports is over, and a card outliving its condition is worse
            // than no card.
            Outcome::Applied | Outcome::UpToDate | Outcome::Reverted => {
                cs.available_version.clear();
                cs.available_notes.clear();
                cs.last_failure.clear();
                cs.last_failure_version.clear();
                cs.last_failure_signature.clear();
            }
            Outcome::Rejected => {
                cs.last_failure = detail.clone();
                cs.last_failure_version = version.clone();
                cs.last_failure_signature = failure_signature(&version, &detail);
                // The offer stands: the user may still want to retry, and the
                // Settings row should keep saying which version was refused.
                //
                // Only when there IS one. A manifest-level refusal carries no
                // version, and writing that empty string into the offer slot
                // silently withdrew a legitimate pending offer (#48). The slot
                // holds a version some manifest actually offered; a refusal
                // that never got as far as a version has nothing to say about
                // it either way.
                if !version.is_empty() {
                    cs.available_version = version.clone();
                }
            }
            // Nothing was checked, and a revert checked nothing either, so
            // nothing recorded about the DATA changes: a standing offer stays
            // offered and a standing refusal stays refused, because a check
            // that never happened resolves neither. The counters above are the
            // only things that move.
            Outcome::Unavailable | Outcome::RevertFailed => {}
        }
    });
    record_row(c, outcome, &version, &detail);
    if outcome == Outcome::Rejected {
        warn!(
            target: "offload",
            component = c.as_str(),
            version = %version,
            detail = %detail,
            "detection updater: bundle rejected; the previous data is still active"
        );
    } else if outcome == Outcome::Unavailable {
        // Logged, not carded (#46). `info` and not `warn`: on a machine that is
        // simply offline this is the expected result of every check, and a WARN
        // per component per day would train the log to be ignored.
        info!(
            target: "offload",
            component = c.as_str(),
            streak = state_at(root).get(c).unreachable_streak,
            detail = %detail,
            "detection updater: update channel unreachable; the current data stays live"
        );
    } else if outcome == Outcome::RevertFailed {
        // A user action that did not do what it said, so `warn` — but nothing
        // about the bundle or the channel is implied, which is the whole reason
        // this is not `Rejected` (#48).
        warn!(
            target: "offload",
            component = c.as_str(),
            detail = %detail,
            "detection updater: revert did not complete; the current data is unchanged"
        );
    } else {
        info!(
            target: "offload",
            component = c.as_str(),
            outcome = outcome.as_str(),
            version = %version,
            "detection updater: check complete"
        );
    }
    RunResult {
        component: c,
        outcome,
        version,
        detail,
    }
}

// ── The entry points ───────────────────────────────────────────────────────

/// Fetch the manifest and run every component in `components`.
///
/// `force_auto` is what Settings' "Apply" passes: it overrides the configured
/// mode for this one run, so an explicit click applies the update without the
/// user having to flip a setting and wait for a tick. It never overrides
/// `Off` — a component the user turned off stays off, including against a
/// button press meant for the other one.
pub async fn run(
    components: &[Component],
    sched: Schedule,
    manifest_url: &str,
    force_auto: bool,
    fetcher: &dyn Fetcher,
    layout: &Layout,
    reload: Reloader<'_>,
) -> Vec<RunResult> {
    // One run at a time, process-wide. A scheduler tick and a "Check now" click
    // otherwise share `staging/<component>/`, and the loser's `wipe_dir` would
    // delete the bundle the winner had just validated. Serialized rather than
    // skipped: a click that has to wait for a tick still does what the user
    // asked, whereas a click that silently no-ops does not.
    let _run_guard = run_lock().lock().await;
    // …and one run at a time across PROCESSES, which the mutex above cannot
    // reach (#48, M-14). Nothing is recorded on contention: the peer holding
    // the lock is doing this same work against the same directories, so a
    // state write here would be a second opinion about a run in flight.
    let _file_guard = match store::acquire_run_lock(&layout.state_root, crate::activity::now_ms()) {
        Ok(l) => l,
        Err(e) => {
            warn!(
                target: "offload",
                error = %e,
                "detection updater: skipping this run; another instance holds the run lock"
            );
            return Vec::new();
        }
    };
    // Finish any swap a crash interrupted before this run can wipe the archive
    // it would have been recovered from (#48, U-2).
    recover_interrupted(layout, reload);
    let root = layout.state_root.clone();
    // Validate the manifest URL BEFORE fetching it (#48, U-1). The parse
    // boundary in `manifest.rs` already refuses a plaintext or unusable
    // channel, but it only runs on the response — by which time the document
    // whose SHA-256s gate every artifact has already travelled in the clear.
    // `detection_update_manifest_url` is a user-editable setting and the only
    // other validation site, so this is where an unusable override stops.
    //
    // `Rejected` rather than `Unavailable`: nothing was unreachable, a check
    // refused to run, and the person who typed the override is exactly who the
    // card is for. The pinned default always passes, so this is silent on
    // every install that has not set one.
    if let Err(e) = manifest::AssetAnchor::parse(manifest_url) {
        return fail_all(components, sched, &root, Outcome::Rejected, &e);
    }
    let raw = match fetcher.get(manifest_url, manifest::MAX_MANIFEST_BYTES).await {
        Ok(b) => b,
        // Transport. Nothing was refused; the channel did not answer (#46).
        //
        // Both `FetchErrorKind`s land here, deliberately, and this is the one
        // place the artifact split (#48, M-9) does NOT apply. An artifact's
        // ceiling is a size the manifest itself declares, so exceeding it is a
        // document contradicting its own index; the manifest's ceiling is a
        // blanket sanity bound, and the thing most likely to exceed it is
        // precisely what #46 is about — a proxy login page or a GitHub 404,
        // neither of which is anybody publishing anything.
        Err(e) => {
            return fail_all(
                components,
                sched,
                &root,
                Outcome::Unavailable,
                &e.to_string(),
            )
        }
    };
    let text = match String::from_utf8(raw) {
        Ok(t) => t,
        Err(_) => {
            return fail_all(
                components,
                sched,
                &root,
                Outcome::Unavailable,
                "the response was not valid UTF-8, so it is not the manifest",
            )
        }
    };
    let man = match Manifest::parse(&text, manifest_url) {
        Ok(m) => m,
        Err(e) => {
            // The one place the two failure classes are told apart. A body that
            // is not even shaped like our index means nobody is publishing here
            // (a 404 page, a proxy login, a tag that does not exist yet); a body
            // that IS shaped like it and still fails is our document being
            // refused by a parse invariant — schema, containment, file names.
            let outcome = if manifest::looks_like_manifest(&text) {
                Outcome::Rejected
            } else {
                Outcome::Unavailable
            };
            return fail_all(components, sched, &root, outcome, &e);
        }
    };

    let mut out = Vec::new();
    for c in components {
        let configured = sched.mode(*c);
        if configured == Mode::Off {
            continue;
        }
        let effective = if force_auto { Mode::Auto } else { configured };
        let result = run_component(*c, &man, effective, fetcher, layout, reload).await;
        if result.outcome == Outcome::Available {
            // The curator's note belongs to the offer, so it is recorded only
            // on the path that creates one. Remote text: stored and displayed,
            // never interpreted.
            if let Some(notes) = man.components.get(c).and_then(|e| e.notes.clone()) {
                update_state_at(&root, |s| s.get_mut(*c).available_notes = notes);
            }
        }
        out.push(result);
    }
    out
}

/// The production wrapper: resolve the layout, use the HTTP fetcher and the
/// live reloader.
pub async fn run_live(
    components: &[Component],
    settings: &Settings,
    force_auto: bool,
) -> Vec<RunResult> {
    let Some(layout) = Layout::resolve() else {
        warn!(target: "offload", "detection updater: no usable layout; skipping");
        return Vec::new();
    };
    run(
        components,
        Schedule::from_settings(settings),
        &manifest_url(settings),
        force_auto,
        &manifest::HttpFetcher,
        &layout,
        &live_reload,
    )
    .await
}

/// A manifest-level failure is every enabled component's failure: none of them
/// could be checked, and a silent no-op would be indistinguishable from
/// "everything is current".
///
/// `outcome` is the caller's classification — [`Outcome::Unavailable`] when the
/// channel did not produce our index at all, [`Outcome::Rejected`] when it did
/// and a parse invariant refused it. The distinction is the whole of #46, so it
/// is an argument here rather than a guess inside.
fn fail_all(
    components: &[Component],
    sched: Schedule,
    root: &Path,
    outcome: Outcome,
    reason: &str,
) -> Vec<RunResult> {
    let now = crate::activity::now_ms();
    let detail = match outcome {
        // Deliberately does NOT open with "could not reach the update channel"
        // (#48): Settings renders this line under exactly that label, and the
        // stored detail repeating it made every unavailable check read
        // "Could not reach the update channel: could not reach the update
        // channel: …". The label belongs to the surface; the detail is the
        // reason plus what it cost, which is nothing.
        Outcome::Unavailable => format!(
            "{reason}. Nothing was checked and nothing changed; the current detection data is \
             still live."
        ),
        _ => format!("update check failed: {reason}"),
    };
    components
        .iter()
        .filter(|c| sched.mode(**c) != Mode::Off)
        .map(|c| finish(*c, root, now, outcome, String::new(), detail.clone()))
        .collect()
}

/// Restore a component's previous version — the Settings Revert button.
///
/// Symmetric with activation: the files being replaced are archived under the
/// version they represent, so a revert is itself revertible.
pub fn revert(c: Component, layout: &Layout, reload: Reloader<'_>) -> RunResult {
    let now = crate::activity::now_ms();
    let root = layout.state_root.clone();
    // A revert rewrites the same two directories a run does, so it needs the
    // same cross-process exclusion (#48, M-14). `RevertFailed`, not `Rejected`:
    // nothing was fetched and nothing about the DATA is being recorded — the
    // user pressed a button at the wrong moment and should press it again.
    let _file_guard = match store::acquire_run_lock(&root, now) {
        Ok(l) => l,
        Err(e) => {
            return finish(
                c,
                &root,
                now,
                Outcome::RevertFailed,
                String::new(),
                format!("revert failed: {e}; nothing was changed — try again in a moment"),
            )
        }
    };
    // Same reason as in `run`: a revert also rewrites an archive directory, so
    // an interrupted swap has to be finished first (#48, U-2).
    recover_interrupted(layout, reload);
    let st = state_at(&root);
    let cs = st.get(c);
    let previous_version = cs.previous_version.clone();
    let current_version = cs.installed_version.clone();
    if previous_version.is_empty() {
        // `RevertFailed`, not `Rejected` (#48): nothing was fetched, so nothing
        // was refused. As `Rejected` this raised a card claiming a bundle
        // refusal AND wrote `String::new()` into the offer slot, withdrawing a
        // legitimate pending offer — two lies for one benign click.
        return finish(
            c,
            &root,
            now,
            Outcome::RevertFailed,
            String::new(),
            format!("nothing to revert to for `{}`", c.as_str()),
        );
    }
    match revert_inner(c, layout, &previous_version, &current_version, reload) {
        Ok(detail) => finish(
            c,
            &root,
            now,
            Outcome::Reverted,
            previous_version,
            detail,
        ),
        // The version rides along for the activity row's "revert-failed
        // <version>" label only: `RevertFailed` records nothing about the data,
        // which is what keeps the PREVIOUS version out of the offer slot — as
        // `Rejected` it landed there and Settings advertised a downgrade as
        // "a newer bundle is available" (#48).
        Err(e) => finish(
            c,
            &root,
            now,
            Outcome::RevertFailed,
            previous_version,
            format!("revert failed: {e}"),
        ),
    }
}

/// The production wrapper for [`revert`].
///
/// **Must be called from a blocking context** (`spawn_blocking`, which is how
/// the `detection_revert` IPC command already invokes it) — it takes the same
/// [`run_lock`] a scheduler tick holds, and `blocking_lock` panics if called on
/// a runtime thread. The pure [`revert`] is the one the tests drive; it takes no
/// lock, because a test owns its own tree.
pub fn revert_live(c: Component) -> RunResult {
    let _run_guard = run_lock().blocking_lock();
    match Layout::resolve() {
        Some(layout) => revert(c, &layout, &live_reload),
        None => RunResult {
            component: c,
            outcome: Outcome::RevertFailed,
            version: String::new(),
            detail: "revert failed: no usable layout".to_string(),
        },
    }
}

fn revert_inner(
    c: Component,
    layout: &Layout,
    previous_version: &str,
    current_version: &str,
    reload: Reloader<'_>,
) -> Result<String, String> {
    let dest = layout.dest(c);
    let root = &layout.state_root;
    let archive = store::previous_dir(root, c, previous_version);
    // Where the currently-live set will be archived, so this revert is itself
    // revertible.
    let keep = store::previous_dir(root, c, current_version);
    // #48, U-4: a revert is judged by the same post-swap health check, so a
    // broken `rules.d/local/` file would veto it too — and the user pressing
    // Revert is *already* trying to get out of a bad state. Same baseline rule
    // as `validate_and_activate_rules`: pre-existing `local/` failures are
    // forgiven, anything the restore introduces is not. Inert for the
    // classifier, whose reloader never reports a `local/` failure.
    let baseline = LocalBaseline::snapshot(dest);
    let judged = |c: Component, dir: &Path| match reload(c, dir) {
        Ok(live) => Ok(live),
        Err(why) if c == Component::Rules => baseline.forgive(dir, why),
        Err(why) => Err(why),
    };
    let reload: Reloader<'_> = &judged;

    // **Revert must never wipe its own source (#48, U-2).**
    //
    // `store::sanitize_version` is lossy — every character outside
    // `[A-Za-z0-9._-]` becomes `_` and the result is trimmed — so two different
    // version strings can name one directory. The reachable case is not exotic:
    // on a fresh install `outgoing_version` is the literal `(shipped)`, which
    // sanitizes to `shipped`, so a manifest publishing a rules version of
    // `shipped` makes `keep` and `archive` THE SAME PATH. The `wipe_dir(&keep)`
    // below would then delete the very files being restored, the live directory
    // would be emptied into a directory that no longer exists, and a second
    // Revert — still enabled, because the state write never happened — would
    // destroy the surviving copy.
    //
    // Compared as PATHS, not as strings, because the collision is created by
    // the sanitizer and only the sanitized form can see it. Fail closed:
    // refusing a revert costs the user a click and a message, emptying
    // `rules.d` costs them the layer.
    if keep == archive {
        return Err(format!(
            "the retained `{previous_version}` and the installed `{current_version}` are archived \
             under the same directory (`{}`), so restoring one would destroy the other — refusing \
             rather than risking an empty rules directory. Re-publish the bundle under a version \
             that does not collide, or reinstall from \
             Settings → Injection protection → Injection detection (Check now)",
            archive.display()
        ));
    }

    let restoring = store::managed_files(&archive, c);
    if restoring.is_empty() {
        return Err(format!(
            "the retained `{previous_version}` version is empty or missing from {}",
            archive.display()
        ));
    }
    // Archive what is live now under ITS version, so this revert can be undone
    // — keeping anything the live set is missing, for the same reason
    // `activate` does (#48, M-11): those files are the current version's own
    // and this is the only copy of them.
    let mut archived: Vec<(PathBuf, PathBuf)> = prepare_archive(c, &keep, dest);
    // The same two-loop shape as `activate`, and therefore the same undos and
    // the same journal (#48, U-2): a bare `?` in either loop left the live
    // directory holding a subset with nothing put back, and this path is the
    // one a user triggers by hand.
    journal(root, c, store::Phase::Archiving, &keep, dest);
    for p in store::managed_files(dest, c) {
        let name = p.file_name().unwrap_or_default().to_os_string();
        let to = keep.join(&name);
        if let Err(e) = store::move_file(&p, &to) {
            let note = restore_only(c, root, dest, &archived, reload);
            return Err(format!(
                "archiving the current `{current_version}` files failed ({e}); nothing was \
                 restored and the current version is still live{note}"
            ));
        }
        archived.push((to, p.clone()));
    }
    journal(root, c, store::Phase::Moving, &keep, dest);
    for p in &restoring {
        let name = p.file_name().unwrap_or_default().to_os_string();
        if let Err(e) = store::move_file(p, &dest.join(&name)) {
            let note = roll_back(c, root, &keep, dest, &archived, reload);
            return Err(format!(
                "restoring `{previous_version}` failed ({e}); `{current_version}` was put \
                 back{note}"
            ));
        }
    }
    let live = match reload(c, dest) {
        Ok(live) => live,
        Err(why) => {
            let note = roll_back(c, root, &keep, dest, &archived, reload);
            return Err(format!(
                "the restored `{previous_version}` version did not load cleanly ({why}); \
                 `{current_version}` was put back{note}"
            ));
        }
    };
    store::clear_journal(root);
    update_state_at(root, |s| {
        let cs = s.get_mut(c);
        cs.installed_version = previous_version.to_string();
        cs.previous_version = current_version.to_string();
        // Same reasoning as `activate`'s success path: the live set has been
        // rewritten whole from a complete retained copy, so nothing is owed.
        cs.unrestored_files.clear();
    });
    Ok(format!(
        "reverted `{}` to `{previous_version}` ({live}); `{current_version}` is retained and can \
         be restored the same way",
        c.as_str()
    ))
}

// ── The scheduler ──────────────────────────────────────────────────────────

/// Spawn the background scheduler: a debounced launch check plus a periodic
/// due-ness poll.
///
/// Follows the app's existing background-task shape (a `tauri::async_runtime`
/// task around a `tokio::time::interval`, as `state::manager` and the loopback
/// heartbeats use) rather than introducing a scheduling framework. Settings are
/// re-read on every tick, so a switch, mode or interval change takes effect
/// within one [`POLL_TICK`] with no restart and no broadcast subscription to
/// keep in sync — which is why the task is still spawned unconditionally even
/// though [`tick_once`] may decline to do anything.
pub fn spawn_scheduler(settings: crate::settings::SettingsHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(LAUNCH_DELAY).await;
        let mut tick = tokio::time::interval(POLL_TICK);
        // `Delay`, not `Burst`: a machine waking from sleep must not fire every
        // missed tick at once against a release asset.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick_once(&settings).await;
            tick.tick().await;
        }
    });
}

/// One scheduler pass. Separated so the inertness property is visible in one
/// place: with [`updates_enabled`] false, or with both components `off`, this
/// returns before reading the state file and long before touching the network.
async fn tick_once(settings: &crate::settings::SettingsHandle) {
    let snap = settings.current();
    if !updates_enabled(&snap) {
        return;
    }
    let sched = Schedule::from_settings(&snap);
    if sched.is_inert() {
        return;
    }
    let st = state();
    let now = crate::activity::now_ms();
    let due: Vec<Component> = Component::ALL
        .iter()
        .copied()
        .filter(|c| {
            is_due(
                sched.mode(*c),
                now,
                st.get(*c).last_check_ms,
                sched.interval_hours,
            )
        })
        .collect();
    if due.is_empty() {
        return;
    }
    info!(
        target: "offload",
        components = %due.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(","),
        "detection updater: scheduled check"
    );
    run_live(&due, &snap, false).await;
}

#[cfg(test)]
mod tests;

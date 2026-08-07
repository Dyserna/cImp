//! V32 Phase C3 (locked decision 13) — the **detection auto-updater**.
//!
//! # Why this exists
//!
//! Signature rules and a classifier decay without updates, and tying freshness
//! to manual maintenance runs makes staleness the default. So the data both
//! detection layers read is kept current on a daily check, from a channel the
//! project curates ([`manifest`]).
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
//! Versions, last-check time and outcome live in Settings → Tools → Detection,
//! next to Check now, Apply, Revert and Open rules folder.
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

use std::collections::HashMap;
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
    pub classifier: Mode,
    pub interval_hours: u32,
}

impl Schedule {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            rules: Mode::parse(&s.offload.detection_update_rules_mode),
            classifier: Mode::parse(&s.offload.detection_update_classifier_mode),
            interval_hours: s.offload.detection_update_interval_hours,
        }
    }

    pub fn mode(self, c: Component) -> Mode {
        match c {
            Component::Rules => self.rules,
            Component::Classifier => self.classifier,
        }
    }

    /// Both components off ⇒ the updater does nothing at all.
    pub fn is_inert(self) -> bool {
        self.rules == Mode::Off && self.classifier == Mode::Off
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
/// whether it is worth keeping current. `Scope::App` resolves to L1 ∧ L2, which
/// is exactly that question.
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
        crate::settings::injection::Scope::App,
        s,
    )
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
    /// `<models>/promptguard2-22m` — the live classifier weights.
    pub classifier_dest: PathBuf,
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
            classifier_dest: store::destination(Component::Classifier)?,
            smoke_dir: validate::smoke_dir()?,
        })
    }

    pub fn dest(&self, c: Component) -> &Path {
        match c {
            Component::Rules => &self.rules_dest,
            Component::Classifier => &self.classifier_dest,
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
        Component::Classifier => {
            let s = super::classifier::rebuild();
            if s.present {
                Ok("weights loaded".to_string())
            } else {
                Err(s.error.unwrap_or_else(|| "weights did not load".into()))
            }
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
    Ok(format!("{} file(s), {} rule(s) live", s.files_loaded, s.rules))
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

/// The three Advisor inputs, read from the cached state — no disk, no clock, so
/// they are safe on the advice poll's cadence.
pub fn advisor_signals() -> (Vec<AvailableUpdate>, Vec<FailedUpdate>, Vec<StalledUpdate>) {
    signals_from(&state())
}

/// The pure half, so the rules that consume these can be tested from a state
/// value.
pub fn signals_from(
    st: &State,
) -> (Vec<AvailableUpdate>, Vec<FailedUpdate>, Vec<StalledUpdate>) {
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
        Err(detail) => finish(
            c,
            &root,
            now,
            Outcome::Rejected,
            entry.version.clone(),
            detail,
        ),
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
) -> Result<String, String> {
    std::fs::create_dir_all(staging).map_err(|e| format!("create {}: {e}", staging.display()))?;

    // Fetch each artifact into memory, verify its size and digest, and only
    // then write it. Nothing untrusted touches disk before its checksum is
    // confirmed, and nothing reaches a parser before that either.
    for f in &entry.files {
        let bytes = fetcher.get(&f.url, f.size).await?;
        if bytes.len() as u64 != f.size {
            return Err(format!(
                "`{}` is {} bytes but the manifest declares {} — rejected before the content was \
                 written or parsed",
                f.name,
                bytes.len(),
                f.size
            ));
        }
        let got = manifest::sha256_hex(&bytes);
        if got != f.sha256 {
            return Err(format!(
                "checksum mismatch on `{}` (expected {}, got {}) — rejected before the content \
                 was written or parsed",
                f.name, f.sha256, got
            ));
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
        Component::Rules => validate_and_activate_rules(staging, layout, &entry.version, reload),
        Component::Classifier => {
            validate_and_activate_classifier(staging, layout, &entry.version, &names, reload)
        }
    }
}

/// The rules half.
fn validate_and_activate_rules(
    staging: &Path,
    layout: &Layout,
    version: &str,
    reload: Reloader<'_>,
) -> Result<String, String> {
    let sources = super::signature::read_sources(staging);
    let corpus = validate::load_corpus(&layout.smoke_dir);
    let report = validate::validate_rules(&sources, &corpus)?;
    activate(Component::Rules, staging, layout, version, reload)?;
    Ok(format!(
        "activated rules `{version}`: {} file(s), {} rule(s), validated against {} benign + {} \
         hostile control document(s) (compile {} ms, slowest scan {} ms). The previous bundle is \
         retained and can be reverted from Settings; `rules.d/local/` was not touched.",
        report.files,
        report.rules,
        report.benign_samples,
        report.hostile_samples,
        report.compile_ms,
        report.slowest_scan_ms
    ))
}

/// The classifier half.
///
/// The staged weights score the shipped smoke corpus BEFORE anything is
/// swapped. With no usable corpus — or with staged weights the scorer cannot
/// load — the update is rejected rather than trusted: swapping a classifier is
/// exactly the change locked decision 13 said should *ask*, and installing an
/// unverified model is the opposite of asking.
fn validate_and_activate_classifier(
    staging: &Path,
    layout: &Layout,
    version: &str,
    names: &[String],
    reload: Reloader<'_>,
) -> Result<String, String> {
    let corpus = validate::load_corpus(&layout.smoke_dir);
    if !corpus.is_usable() {
        return Err(format!(
            "the smoke corpus is missing or empty ({} benign, {} hostile documents) — staged \
             weights cannot be verified, so they are rejected rather than trusted",
            corpus.benign.len(),
            corpus.hostile.len()
        ));
    }
    let injection = super::classifier::score_many_with(staging, &corpus.hostile)?;
    let benign = super::classifier::score_many_with(staging, &corpus.benign)?;
    validate::classifier_smoke_verdict(&injection, &benign)?;
    let _ = names;
    activate(Component::Classifier, staging, layout, version, reload)?;
    Ok(format!(
        "activated classifier weights `{version}`, verified against {} injection + {} benign \
         control document(s). The previous weights are retained and can be reverted from Settings.",
        injection.len(),
        benign.len()
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
) -> Result<(), String> {
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
    store::wipe_dir(&archive);

    // Archive the current managed set. For rules this is the top level of
    // `rules.d` only, so `local/` is untouched by construction.
    journal(root, c, store::Phase::Archiving, &archive, dest);
    let mut archived: Vec<(PathBuf, PathBuf)> = Vec::new();
    for p in store::managed_files(dest, c) {
        let name = p.file_name().unwrap_or_default().to_os_string();
        let to = archive.join(&name);
        if let Err(e) = store::move_file(&p, &to) {
            // Restore-only: the files this loop has NOT reached are still at
            // `dest` and are the only copy of themselves.
            restore_archived(&archived);
            let note = reload_note(c, dest, reload);
            store::clear_journal(root);
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
            let note = roll_back(c, dest, &archived, reload);
            store::clear_journal(root);
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
            });
            info!(
                target: "offload",
                component = c.as_str(),
                version,
                live = %live,
                "detection updater: bundle activated"
            );
            Ok(())
        }
        Err(why) => {
            let note = roll_back(c, dest, &archived, reload);
            store::clear_journal(root);
            Err(format!(
                "the activated bundle did not load cleanly ({why}); the previous version was \
                 restored{note}"
            ))
        }
    }
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
        // Nothing was archived before the interruption (or the archive is
        // already back). There is nothing to undo and nothing to lose.
        store::clear_journal(root);
        return;
    }
    match j.phase {
        // The destination still holds every file the archive loop did not
        // reach; clearing it would destroy them.
        store::Phase::Archiving => restore_archived(&archived),
        // The destination holds however many staged files landed. They must go:
        // old-plus-some-new is a set no curation step ever validated.
        store::Phase::Moving => {
            for p in store::managed_files(dest, c) {
                let _ = std::fs::remove_file(&p);
            }
            restore_archived(&archived);
        }
    }
    store::clear_journal(root);
    warn!(
        target: "offload",
        component = c.as_str(),
        phase = ?j.phase,
        files = archived.len(),
        "detection updater: an update was interrupted mid-swap; the previous version was restored"
    );
    if let Err(e) = reload(c, dest) {
        warn!(
            target: "offload",
            component = c.as_str(),
            error = %e,
            "detection updater: the recovered version did not reload cleanly"
        );
    }
}

/// The label recorded for the bundle that shipped with the app — the one
/// version that has no manifest entry. Displayed in Settings as the revert
/// target, so it has to read as something a user recognizes.
pub const SHIPPED_VERSION: &str = "(shipped)";

/// Put archived files back where they came from.
///
/// The half of a rollback that is safe at **any** point of a swap, because it
/// only ever writes files the archive already holds. [`roll_back`] adds the
/// destructive half on top of it, and the archive loop's undo deliberately does
/// not (see [`activate`]).
fn restore_archived(archived: &[(PathBuf, PathBuf)]) {
    for (from, to) in archived {
        if let Err(e) = store::move_file(from, to) {
            warn!(
                target: "offload",
                from = %from.display(),
                to = %to.display(),
                error = %e,
                "detection updater: could not restore a previous file"
            );
        }
    }
}

/// Remove whatever is at `dest` for this component, put the archive back, and
/// reload. The undo for a failure **after** the staged set started landing.
///
/// Returns the note the caller appends to its own error: a rollback that put
/// the files back but could not recompile them is a second, separate problem,
/// and it used to be visible only as a `warn!` in a log nobody was reading
/// (#48). Empty on the ordinary path, so the existing messages are unchanged.
#[must_use]
fn roll_back(
    c: Component,
    dest: &Path,
    archived: &[(PathBuf, PathBuf)],
    reload: Reloader<'_>,
) -> String {
    for p in store::managed_files(dest, c) {
        let _ = std::fs::remove_file(&p);
    }
    restore_archived(archived);
    reload_note(c, dest, reload)
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
        Err(e) => return fail_all(components, sched, &root, Outcome::Unavailable, &e),
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
    // Same reason as in `run`: a revert also wipes an archive directory, so an
    // interrupted swap has to be finished first (#48, U-2).
    recover_interrupted(layout, reload);
    let root = layout.state_root.clone();
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
             that does not collide, or reinstall from Settings → Tools → Detection (Check now)",
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
    // Archive what is live now under ITS version, so this revert can be undone.
    store::wipe_dir(&keep);
    // The same two-loop shape as `activate`, and therefore the same two undos
    // and the same journal (#48, U-2): a bare `?` in either loop left the live
    // directory holding a subset with nothing put back, and this path is the
    // one a user triggers by hand.
    journal(root, c, store::Phase::Archiving, &keep, dest);
    let mut archived: Vec<(PathBuf, PathBuf)> = Vec::new();
    for p in store::managed_files(dest, c) {
        let name = p.file_name().unwrap_or_default().to_os_string();
        let to = keep.join(&name);
        if let Err(e) = store::move_file(&p, &to) {
            restore_archived(&archived);
            let note = reload_note(c, dest, reload);
            store::clear_journal(root);
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
            let note = roll_back(c, dest, &archived, reload);
            store::clear_journal(root);
            return Err(format!(
                "restoring `{previous_version}` failed ({e}); `{current_version}` was put \
                 back{note}"
            ));
        }
    }
    let live = match reload(c, dest) {
        Ok(live) => live,
        Err(why) => {
            let note = roll_back(c, dest, &archived, reload);
            store::clear_journal(root);
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

//! **When** an update check may run: the three [`Mode`]s, the poll tick and
//! launch delay the scheduler obeys, the interval floor, and the two settings
//! questions every caller asks first (is the channel on at all, and is
//! detection worker-only).

use super::*;

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
/// the behaviour it had.
///
/// **#48 F-38 / locked decision 41 — the resolution is unchanged and now states
/// its own question.** The site used to spell
/// [`Scope::UnknownCaller`](crate::settings::injection::Scope::UnknownCaller)
/// inline, which is *behaviourally* right and *nominally* wrong: there is no
/// unknown caller here. The question this site asks is **"is the one shared
/// bundle on disk worth keeping current, i.e. is detection armed for anybody
/// other than the offload worker?"**, and
/// [`armed_outside_the_worker`](crate::settings::injection::armed_outside_the_worker)
/// is that question by name — the same resolution, delegated, byte for byte.
/// This is preventive, not cosmetic: `Scope::App` produced two defects (M-21,
/// F-35) for exactly one reason, a name standing in for a question it did not
/// ask, and a future reader must not be able to borrow the wrong one here.
///
/// **Still not a behaviour decision.** Whether the updater should instead follow
/// the app-wide baseline (`Scope::AppWide`, so one hardened tab stops starting
/// it) or the armed-anywhere predicate (which M-21 explicitly rejected,
/// `worker_only_detection` below) remains open and carries a live-verify box.
/// Decision 41 declined both. This stays exactly as it has behaved since N-1.
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
    crate::settings::injection::armed_outside_the_worker(
        crate::settings::injection::Feature::Detection,
        s,
    )
}

/// #48 (M-21) — **injection detection is running in the offload worker while the
/// updater is inert.** The one state in which *"injection detection is off"* is a
/// false statement about this install.
///
/// # Why this state exists, and why the resolution above is still right
///
/// [`updates_enabled`] resolves through
/// [`armed_outside_the_worker`](crate::settings::injection::armed_outside_the_worker)
/// — i.e. at [`Scope::UnknownCaller`](crate::settings::injection::Scope::UnknownCaller),
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
/// not start it. Its consumers are the refusal `service::offload::updates_allowed`
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

//! The terminal verdicts of one component run ([`Outcome`]) and the Tool
//! Activity rows they mint. The stall threshold lives here because it is a
//! property of the outcome history, not of the run that produced it.

use super::*;

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
pub(super) fn record_row(c: Component, outcome: Outcome, version: &str, detail: &str) {
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
            None,
            None,
        ),
        request: serde_json::to_string_pretty(&request).unwrap_or_default(),
        response: detail.to_string(),
    });
}

//! What the advisor surfaces read: available / failed / stalled updates, rule
//! sets that arrived incomplete, and `rules.d/local/` files that will not
//! compile. Every one is derived from a [`State`] snapshot, so they are pure
//! functions of what the last run recorded.

use super::*;

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

//! The harness-administration use cases: the roster the windows learn from, the
//! *Harness health* panel's payload and its one action, the "Mark verified"
//! stamp, the model-visible instruction slots, and one harness's usage reading.
//!
//! ## What the A1 harness run found
//!
//! Six commands, one handle, and no events at all. Five of the six took
//! `State<'_, AppState>` — or nothing — to reach the settings snapshot and the
//! registry, both of which are ordinary in-process things; the sixth
//! ([`run_checks`]) is fire-and-forget over a process-global. So the whole
//! cluster was WebView-only for no reason beyond where it was written.
//!
//! What is worth stating, because it looks like an inconsistency and is not:
//! **three different reads of "the settings" appear here on purpose.**
//!
//! * [`HarnessService::versions`] layers a FRESH read of the physical global
//!   `harness_versions` and `harness` map over the live snapshot, because the
//!   auto-verify worker and the version tap write both out of band — a
//!   hand-recorded spike outcome has to disable a toggle without an app
//!   restart, which is the reason this command exists at all.
//! * [`HarnessService::mark_verified`] writes the physical global and THEN
//!   mirrors the result into the live settings, so an open Settings window sees
//!   the stamp without a restart.
//! * [`HarnessService::instructions`] reads only the live snapshot, because all
//!   it needs from it is which harness a tab runs.
//!
//! Collapsing those into one "read the settings" would silently break the first
//! two. They are not duplication; they are three different freshness contracts.
//!
//! ## What did NOT change
//!
//! [`run_checks`] still resolves its argument through the probe's own token
//! table rather than a `match` here — the panel renders `HarnessHealth::harness`,
//! which IS that table's output, so round-tripping through it is what keeps the
//! button pointed at the harness whose header it sits under. [`usage`] still
//! errors on an unregistered harness id rather than answering an empty reading:
//! a widget polling for a harness that does not exist is a bug, and answering it
//! with "nothing to show" would hide it forever. And
//! [`HarnessService::versions`] still serves the health read-model from the SAME
//! fresh-versions settings as the gates, so the panel's headers, its gate badges
//! and its last-verified dates are one consistent reading rather than three.

use std::collections::BTreeMap;

use crate::error::{AppError, AppResult};
use crate::settings::SettingsHandle;

/// The Settings window's harness payload: the raw spike/version record plus the
/// **computed** gate verdicts (V35 Phase E).
///
/// `capability_gates` is a list of self-describing records rather than one
/// bespoke boolean per feature, and that is the whole point: before Phase E the
/// window received `e1_status` and re-implemented the fail-closed reading of it
/// in TypeScript (`harnessStatusBlocks`), so a change to the rule had to be made
/// twice or the toggle and the installed hook would disagree. Adding a second
/// bespoke flag here would have recreated exactly that. Phase G's *Harness
/// health* panel renders this same list.
#[derive(serde::Serialize)]
pub struct HarnessStatus {
    /// Fresh read of the physical global `harness_versions`.
    pub versions: crate::settings::HarnessVersions,
    /// Every gated capability's verdict, keyed by capability id — the same
    /// query `tabs/config.rs` asks before installing the hook.
    pub capability_gates: Vec<crate::harness::contract::Gate>,
    /// V35 Phase G: the whole *Harness health* read-model — every registry row
    /// with its tier, contract sentence, degradation, coverage marks, TCB
    /// controls, gate verdict and last check result, grouped by harness and
    /// ordered riskiest-tier-first.
    ///
    /// Served from THIS payload rather than a sibling command: it is the same
    /// fresh `harness_versions` read and the same `contract::gates` call the
    /// payload already makes, the command is called on Settings open (and while
    /// a run is in flight) rather than on any hot path, and a second command
    /// would mean two round trips that could disagree about the versions they
    /// were computed against.
    pub harness_health: Vec<crate::harness::health::HarnessHealth>,
    /// A verify run is happening right now, so *Run checks now* is a no-op and
    /// the panel should keep polling.
    pub verify_in_flight: bool,
    /// V40 Phase F (locked decision 27): the gated capability ids, keyed by the
    /// neutral CONTROL each one gates (`harness::contract::GATED_CONTROLS`).
    ///
    /// The window used to hold one of these ids — a harness-namespaced hook
    /// name — as a TypeScript constant so it could join on it. It looks the id
    /// up here now, so a gate whose capability belongs to a harness reaches the
    /// frontend as data rather than as a second spelling.
    pub gated_controls: BTreeMap<&'static str, &'static str>,
}

/// The answer [`usage`] gives. See that function for the three source states and
/// for why the turn shape sits BESIDE them, not inside them.
#[derive(serde::Serialize)]
pub struct HarnessUsage {
    /// What this harness's quota source *can* report — `None` when it has none.
    pub source: Option<UsageSourceInfo>,
    /// The billing categories this harness reports a RECORDED turn's tokens
    /// under, in declared order. Empty when it records no turns.
    pub token_kinds: Vec<DeclaredLabel>,
    /// The lanes a recorded turn can be attributed to, in declared order — what
    /// the Usage donut labels its rings with. Empty when it records no turns.
    pub origins: Vec<DeclaredOrigin>,
    /// What the quota source reports right now.
    pub reading: Option<crate::harness::plugin::UsageReading>,
}

/// The declared shape of a QUOTA source: which windows it can report.
///
/// Sent alongside the reading rather than mirrored in the frontend, so a harness
/// with three quota windows (or one, or none) needs no UI change — locked
/// decision 19. V40 Phase G moved `token_kinds` / `origins` OUT of here: they
/// describe a stored turn, not a quota reading, and a harness can have either
/// without the other.
#[derive(serde::Serialize)]
pub struct UsageSourceInfo {
    pub windows: Vec<DeclaredWindow>,
}

/// One declared quota window, without a reading.
#[derive(serde::Serialize)]
pub struct DeclaredWindow {
    pub id: &'static str,
    pub label: &'static str,
    pub short: &'static str,
    pub description: &'static str,
}

/// A declared id with the label a UI renders for it (token categories).
#[derive(serde::Serialize)]
pub struct DeclaredLabel {
    pub id: &'static str,
    pub label: &'static str,
}

/// One declared turn lane. Carries `subagent` because that is what tells a UI
/// which lane gets the fan-out treatment (the outlined bar, the "A" badge)
/// without recognising the word `"agent"` — the literal locked decision 19
/// exists to delete.
#[derive(serde::Serialize)]
pub struct DeclaredOrigin {
    pub id: &'static str,
    pub label: &'static str,
    pub subagent: bool,
}

/// The harness-administration use cases, over one borrowed handle — same shape
/// and rationale as [`crate::service::tabs::TabService`].
pub struct HarnessService<'a> {
    settings: &'a SettingsHandle,
}

impl<'a> HarnessService<'a> {
    pub fn new(settings: &'a SettingsHandle) -> Self {
        Self { settings }
    }

    /// V16 Feature 1: the harness version + contract-verification state, read
    /// from the physical global `settings.json` (fresh — background writers
    /// bypass the live settings snapshot).
    ///
    /// V35 Phase E: the gates are computed against the live settings with the
    /// FRESH `harness_versions` layered in, so a hand-recorded spike outcome
    /// disables the toggle without an app restart — the reason this exists.
    pub fn versions(&self) -> AppResult<HarnessStatus> {
        let versions = crate::settings::read_global_harness_versions();
        let mut settings = self.settings.current();
        settings.harness_versions = versions.clone();
        // V40 Phase B: the versions, the auto-verify records and the recorded
        // spike outcomes all live in `harness` now, and all three are written
        // out of band — so the panel has to be computed against a FRESH read of
        // that map for exactly the reason it already was for `harness_versions`.
        settings.harness = crate::settings::read_global_harness_map();
        Ok(HarnessStatus {
            capability_gates: crate::harness::contract::gates(&settings),
            // V35 Phase G: computed against the SAME fresh-versions settings as
            // the gates, so the panel's headers, its gate badges and its
            // last-verified dates are one consistent reading rather than three.
            harness_health: crate::harness::health::health(&settings),
            verify_in_flight: crate::harness::verify::in_flight(),
            versions,
            gated_controls: crate::harness::contract::GATED_CONTROLS
                .iter()
                .copied()
                .collect(),
        })
    }

    /// V16 Feature 1: the Advisor card's "Mark verified" action — stamp the
    /// currently-seen version of `harness` as the last-verified one (the user
    /// just re-ran the MAINTENANCE.md contract checks). Also mirrors the change
    /// into the live settings so the open Settings window sees it without a
    /// restart.
    ///
    /// **V40 Phase B: it takes a harness.** It used to write
    /// `claude_last_verified` with no argument at all, so the OpenCode row of
    /// the health panel had no action that could ever clear it. `None` is the
    /// DEFAULT harness — the documented wire-compatibility default (locked
    /// decision 22).
    pub fn mark_verified(&self, harness: Option<String>) -> AppResult<()> {
        let id = match harness.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) => crate::harness::HarnessId::from_id(name).ok_or_else(|| {
                AppError::Ipc(format!("harness_mark_verified: {name:?} is not a harness"))
            })?,
            None => crate::harness::DEFAULT_HARNESS,
        };
        let after = crate::settings::mutate_global_harness(id, |row| {
            row.last_verified = row.last_seen.clone();
        })?;
        let key = id.token().to_string();
        self.settings.mutate(move |cur| {
            cur.harness.insert(key.clone(), after.clone());
        });
        Ok(())
    }

    /// **The model-visible text one tab's harness receives**, keyed by slot (V40
    /// Phase E, locked decision 24).
    ///
    /// The compose overlay is the first consumer: it appends one instruction
    /// line after the `[image] <path>` lines it types into the tab, and that
    /// line used to be a literal in `compose/attachments.ts` — a string the
    /// model reads that nothing in the backend inventory could see, and that no
    /// harness could influence.
    ///
    /// `tab` is a tab id; a tab that runs no registered harness (or an unknown
    /// id) gets the NEUTRAL rendering, which is a real answer rather than a
    /// failure — the same posture `instructions::all_for` takes.
    pub fn instructions(&self, tab: Option<String>) -> AppResult<BTreeMap<String, String>> {
        let settings = self.settings.current();
        let harness = tab
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .and_then(|t| crate::tabs::tab_harness_by_id(&settings, t));
        Ok(crate::harness::instructions::all_for(harness)
            .iter()
            .map(|i| (i.slot.id().to_string(), i.text.to_string()))
            .collect())
    }
}

/// V35 Phase G: the *Harness health* panel's one action — run this harness's L1
/// canaries and L2 probes now.
///
/// Returns whether a run STARTED. `false` means one was already in flight (a
/// second click, or an automatic run triggered by a version change) and this
/// request was dropped rather than queued; the panel shows the in-flight state
/// either way and re-reads [`HarnessService::versions`] when it clears.
///
/// Fire-and-forget by construction: the work spawns a blocking OS thread that
/// drives child processes for up to 90 s, so this returns as soon as the thread
/// is up. The result arrives through the payload above.
///
/// Free rather than a [`HarnessService`] method: it reads no settings, so a
/// service would be a handle it never touches.
pub fn run_checks(harness: &str) -> AppResult<bool> {
    // Resolved through the probe's own token table rather than a `match` here:
    // the panel renders `HarnessHealth::harness`, which IS that table's output,
    // so round-tripping through it is what keeps the button pointed at the
    // harness whose header it sits under.
    let h = crate::harness::probe::harness_from_name(harness.trim()).ok_or_else(|| {
        AppError::Ipc(format!(
            "harness_run_checks: {harness:?} is not a harness that can be run"
        ))
    })?;
    Ok(crate::harness::verify::run_now(h))
}

/// **One harness's usage reading** for the bottom-bar tracker (V40 Phase D,
/// locked decision 19). Local read, never the network.
///
/// Three distinguishable states, which is the whole point:
///
/// * `source: None` — **this harness has no usage source at all.** A UI must
///   render that as absence; rendering it as a harness at 0 % would be a number
///   nobody reported (global principle 5). It says nothing about whether the
///   harness RECORDS turns — see `token_kinds`/`origins` below.
/// * `source: Some(..), reading: None` — it has one, and nothing has been
///   reported yet (no tab of that harness has pushed, or the last push aged
///   out).
/// * `source: Some(..), reading: Some(..)` — the declared windows that have a
///   reading, in declared order, plus the live context block.
///
/// Beside those three, and **independent of them**, the answer carries the
/// declared shape of a RECORDED turn: `token_kinds` and `origins`. V40 Phase G
/// split the two questions, because they had different answers all along — a
/// harness can report no quota and no context window (`source: None`) and still
/// write real per-turn token rows with a parent/child lane split. Nesting the
/// declaration under `source` meant the Usage donut could not label such a
/// session's lanes at all. Both lists are EMPTY for a harness that declares no
/// turn shape.
///
/// An unregistered harness id is an error, not an empty reading: a widget
/// polling for a harness that does not exist is a bug, and answering it with
/// "nothing to show" would hide it forever.
///
/// Free rather than a [`HarnessService`] method: it reads no settings.
pub fn usage(harness: &str) -> AppResult<HarnessUsage> {
    let id = crate::harness::HarnessId::from_id(harness).ok_or_else(|| {
        AppError::Ipc(format!(
            "unknown harness `{harness}` — registered: {}",
            crate::harness::registry::harness_ids().join(", ")
        ))
    })?;
    let plugin = id.plugin();
    let source = plugin.and_then(|p| p.usage_source());
    let shape = plugin.and_then(|p| p.turn_usage_shape());
    Ok(HarnessUsage {
        source: source.map(|s| UsageSourceInfo {
            windows: s
                .windows()
                .iter()
                .map(|w| DeclaredWindow {
                    id: w.id,
                    label: w.label,
                    short: w.short,
                    description: w.description,
                })
                .collect(),
        }),
        token_kinds: shape
            .map(|s| {
                s.token_kinds
                    .iter()
                    .map(|k| DeclaredLabel {
                        id: k.id,
                        label: k.label,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        origins: shape
            .map(|s| {
                s.origins
                    .iter()
                    .map(|o| DeclaredOrigin {
                        id: o.id,
                        label: o.label,
                        subagent: o.subagent,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        reading: source.and_then(|s| s.read()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **"No quota source" and "records no turns" are two answers, and this use
    /// case gives both** (V40 Phase G, locked decision 19).
    ///
    /// The regression this pins is the one the phase exists to remove: the
    /// declared token categories and turn lanes used to hang off `source`, so a
    /// harness that reports no quota was also declared to record no turns — and
    /// the Usage donut had no labels for its sessions' lanes. Live-verify 14
    /// reads the FIRST half of this (a harness answering *no usage source*, not
    /// a widget at 0 %), so both halves are asserted together.
    ///
    /// Names no product: the two harnesses are picked out of the registry by
    /// what they DECLARE, which is what locked decision 10(a) asks of core.
    #[test]
    fn usage_reports_a_turn_shape_independently_of_a_quota_source() {
        let mut quota_only = 0usize;
        let mut turns_without_quota = 0usize;
        for id in crate::harness::registry::all() {
            let answer = usage(id.token()).expect("a registered harness answers");
            let plugin = id.plugin();
            let has_source = plugin.and_then(|p| p.usage_source()).is_some();
            let has_shape = plugin.and_then(|p| p.turn_usage_shape()).is_some();
            assert_eq!(
                answer.source.is_some(),
                has_source,
                "{id}: the `source` half must mirror the declaration exactly"
            );
            assert_eq!(
                !answer.origins.is_empty(),
                has_shape,
                "{id}: the lanes must arrive whenever a turn shape is declared"
            );
            assert_eq!(
                !answer.token_kinds.is_empty(),
                has_shape,
                "{id}: the categories must arrive whenever a turn shape is declared"
            );
            if has_source {
                quota_only += 1;
                // The quota half carries WINDOWS and nothing else now.
                assert!(!answer.source.as_ref().unwrap().windows.is_empty());
            }
            if has_shape && !has_source {
                turns_without_quota += 1;
                // Exactly the case that was unrepresentable: no quota widget at
                // all, and still a labelled lane split for its stored rows.
                assert!(answer.reading.is_none(), "{id}: no source can produce no reading");
                assert!(
                    answer.origins.iter().any(|o| o.subagent),
                    "{id}: it rolls a child session's spend up, so it declares the lane"
                );
            }
        }
        assert!(quota_only > 0, "no harness declares a quota source at all");
        assert!(
            turns_without_quota > 0,
            "no harness records turns without reporting quota — if that becomes true, this \r
             use case's independence has no live example and the two fields can silently \r
             re-couple"
        );
    }

    /// An unregistered id is an error, not an empty reading — see [`usage`].
    #[test]
    fn an_unregistered_harness_is_an_error_not_an_empty_reading() {
        assert!(usage("not-a-harness").is_err());
        assert!(run_checks("not-a-harness").is_err());
    }
}

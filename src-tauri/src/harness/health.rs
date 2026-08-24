//! V35 Phase G — the **Harness health** read-model: one computed answer to
//! "what is broken right now", built entirely in Rust so the Settings panel
//! renders it rather than re-deriving it.
//!
//! This is the third of the matrix draft's four consumers (§ 3, consumer 3) and
//! the last one the registry was seeded for. Phase E gave the frontend the
//! *gate* verdicts; this gives it everything else a row carries — tier,
//! contract sentence, degradation, coverage marks, the TCB column — joined
//! against the two things that are not in the registry at all: the Phase F
//! auto-verify record on disk, and the last run made in this process.
//!
//! # Why the whole view is computed here
//!
//! Milestone locked decision (Phase G brief 2), and the same reasoning that
//! deleted `harnessStatusBlocks` in Phase E: every rule the panel would need to
//! apply — which tier is riskiest, whether a row's coverage is real or only a
//! waiver, whether a recorded run says anything about a given capability — is a
//! rule about the registry. A copy of it in TypeScript is a second place for it
//! to be wrong, and the frontend has no way to fail a test when it drifts.
//! What crosses the wire is display data plus display *order*; the panel groups
//! and paints.
//!
//! # The one honest gap: a stored record names only failures
//!
//! [`crate::settings::AutoVerify`] persists `status` + the FAILING
//! capabilities, by design (Phase F) — it is what the Advisor needs, and
//! storing an answer per capability would have made it a growing settings
//! field. So for a row the record does not name, the disk says exactly one
//! thing: *this run did not report a failure for it*. That is strictly weaker
//! than "it passed" — a [`crate::harness::probe::Outcome::Unknown`] (CLI absent,
//! no transcript to read) looks identical from here.
//!
//! Rather than reshape the stored record or quietly promote silence to a pass,
//! the view has its own outcome token for it ([`OUTCOME_NO_FAILURE`]) and says
//! so in the row. The full four-value answer IS available — but only from
//! [`crate::harness::verify::last_run`], the in-process memory of a run made
//! since launch, which is what the panel's *Run checks now* button fills in.
//! In-memory on purpose: a per-capability result table is exactly the stored
//! state the Phase G brief rules out, and the schema bump it would cost is the
//! milestone's own deploy trap.

use crate::harness::chp::{self, StalePlugin};
use crate::harness::contract::{self, Capability, Degradation, Gate, Harness, Seam};
use crate::harness::probe::{harness_name, tier_name};
use crate::harness::verify::{self, RunSummary};
use crate::settings::{AutoVerify, Settings};

/// The panels the view shows, in display order, with the name a user
/// recognizes. Deliberately not derived from the registry's distinct
/// `Capability::harness` values: a harness with zero rows must still get a
/// header saying so.
///
/// **V39 Phase B added the third, and it is not a product.** [`Harness::ANY`]
/// marks a row whose contract is stated about a *tab* rather than about a
/// vendor — `delegation.worker` is the first — so it has no version, no
/// installed CLI and no auto-verify record, and every one of those fields
/// renders empty for it by construction. It gets a panel anyway because it
/// carries a **gate**: a capability the user can be blocked by and cannot see
/// is exactly the thing this panel exists to end.
/// The neutral panel, and the reason it is a SECOND SOURCE rather than a
/// pseudo-descriptor with empty binaries (amendment 0-e).
///
/// A descriptor with no binary, no plugin and no directory would satisfy every
/// registry lookup by lying; the neutral rows are not a harness, they are the
/// absence of one. Two explicit sources, joined here, is the honest shape — and
/// `every_registry_entry_is_fully_wired` asserts both halves so neither can be
/// dropped.
const NEUTRAL_PANEL: (Harness, &str) = (Harness::ANY, "Cross-harness");

/// The panels the view shows, in display order.
///
/// The registry's descriptors — so a harness with zero capability rows still
/// gets a header saying so — **plus** [`NEUTRAL_PANEL`]. Plain iteration over
/// the registry would silently drop the `Harness::ANY` rows and hide a gate the
/// user can be blocked by, which is exactly what this panel exists to end.
pub(in crate::harness) fn panel_labels() -> Vec<(Harness, &'static str)> {
    let mut out: Vec<(Harness, &'static str)> = crate::harness::registry::HARNESSES
        .iter()
        .map(|d| (d.harness(), d.label))
        .collect();
    out.push(NEUTRAL_PANEL);
    out
}

// ── outcome vocabulary ──────────────────────────────────────────────────────

/// The stored record ran against this version and did **not** name this
/// capability among its failures — and that is all it says. Not a pass: the
/// record keeps failures only, so an `unknown` (an absent CLI, a session with
/// no tool call to read) is indistinguishable from a `pass` in it. Its own
/// token so the panel can render it as the weaker statement it is instead of
/// painting a green tick the evidence does not support.
pub const OUTCOME_NO_FAILURE: &str = "no_failure";

// ── the wire types ──────────────────────────────────────────────────────────

/// What cImp does when a capability is known-broken, flattened for the wire.
///
/// A tagged view rather than serde's enum representation because two of the
/// four variants carry a payload the panel must show verbatim (the message a
/// user would see, the name of the fallback that takes over) and the other two
/// carry none — an externally-tagged enum would make the frontend branch on the
/// shape before it can render a sentence.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DegradationView {
    /// Machine token: `silent` | `visible_off` | `fail_closed` | `fallback`.
    pub kind: &'static str,
    /// The same fact as a sentence, written once here rather than four times in
    /// a Svelte `{#if}` chain.
    pub label: &'static str,
    /// [`Degradation::VisibleOff`]'s message, verbatim — what the user is told
    /// when this row breaks.
    pub user_message: Option<&'static str>,
    /// [`Degradation::Fallback`]'s target capability id. A join key, so the
    /// panel can point at the row that takes over.
    pub fallback_to: Option<&'static str>,
}

impl DegradationView {
    fn of(d: Degradation) -> Self {
        match d {
            Degradation::Silent => DegradationView {
                kind: "silent",
                label: "Breaks SILENTLY — the feature produces nothing and says nothing.",
                user_message: None,
                fallback_to: None,
            },
            Degradation::VisibleOff { user_message } => DegradationView {
                kind: "visible_off",
                label: "Turns itself off and says so in the UI.",
                user_message: Some(user_message),
                fallback_to: None,
            },
            Degradation::FailClosed => DegradationView {
                kind: "fail_closed",
                label: "Fails CLOSED — the dependent feature refuses to install or run.",
                user_message: None,
                fallback_to: None,
            },
            Degradation::Fallback { to } => DegradationView {
                kind: "fallback",
                label: "A named fallback covers it.",
                user_message: None,
                fallback_to: Some(to),
            },
        }
    }
}

/// What actually checks this row: the L1 fixture canary, the L2 live probe, and
/// the accepted-residual waiver — carried as the ids/prose themselves rather
/// than as booleans, because a waiver is only worth showing if the panel can
/// show what it *says*.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Coverage {
    /// The L1 embedded-fixture canary id (which IS the capability id).
    pub canary: Option<&'static str>,
    /// The L2 live-probe id (likewise).
    pub probe: Option<&'static str>,
    /// The accepted-residual note: why nothing mechanical covers this row yet.
    pub waiver: Option<&'static str>,
    /// **The weakest state on the board**: degrades silently, and the only
    /// thing standing behind it is prose. Computed here so the panel cannot
    /// accidentally render a waiver-only `Silent` row as though it were
    /// canaried — which is the exact confusion
    /// `contract::tests::every_silent_degradation_has_a_canary_or_a_probe_or_a_waiver`
    /// permits by design (it accepts a waiver as coverage so the enforcement
    /// test can run from day one; this flag is what keeps that leniency from
    /// reaching the user as a false reassurance).
    pub unproven: bool,
}

/// The last thing any check said about one capability.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VerifyView {
    /// [`crate::harness::probe::Outcome::label`] (`pass` / `fail` / `unknown` /
    /// `transition`) when it came from a run this process made, or
    /// [`OUTCOME_NO_FAILURE`] when it was inferred from a stored record.
    pub outcome: &'static str,
    /// Which layer saw it: `harness.canary.l1` or `harness.probe.l2`. Empty for
    /// [`OUTCOME_NO_FAILURE`] — no single layer said it.
    pub evidence: String,
    /// The sentence: the assertion message, the probe's observation, or the
    /// explanation of what a silence in the record does and does not mean.
    pub detail: String,
    /// Wall-clock ms of the run that produced it, for the "how old is this"
    /// half of the panel.
    pub at_ms: u64,
    /// The harness version it was checked against — the answer to "verified,
    /// but against WHAT".
    pub version: String,
    /// `true` = a full answer from a run made since launch (all four outcomes
    /// available). `false` = read out of the stored failures-only record, where
    /// only `fail` is directly attested.
    pub from_run: bool,
}

/// One registry row, everything the panel shows about it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CapabilityHealth {
    /// The join key, displayed verbatim — it is the vocabulary the Advisor
    /// cards speak, so a user must be able to match a card to a row by eye.
    pub id: &'static str,
    pub harness: &'static str,
    /// `A`..`D` — the seam, which predicts how it breaks.
    pub tier: &'static str,
    /// The human sentence: what upstream must keep doing.
    pub contract: &'static str,
    /// What the user loses when this row is broken — [`Capability::user_effect`]
    /// verbatim. The user view of the panel shows this for a failing row; the
    /// contract sentence is maintainer detail.
    pub user_effect: &'static str,
    pub degradation: DegradationView,
    pub coverage: Coverage,
    /// The TCB column (matrix decision 10): security controls that *execute
    /// inside* this capability. Marked distinctly in the panel — these rows are
    /// not data pipes, and changing one changes the trusted computing base.
    pub controls: &'static [&'static str],
    /// The modules that break if this drifts.
    pub wired_in: &'static [&'static str],
    /// The Phase E gate verdict, when this capability has one at all. `None` =
    /// ungated (most rows *degrade* rather than gate a feature off), which is a
    /// different statement from "gated and currently fine".
    pub gate: Option<Gate>,
    /// `None` = no check has ever spoken about this row.
    pub last_verify: Option<VerifyView>,
}

/// The tally of a run made in this process — the visible consequence of
/// clicking *Run checks now*, and the only place OpenCode's checks are reported
/// at all (nothing on disk records them).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RunView {
    pub at_ms: u64,
    pub version: String,
    pub pass: usize,
    pub fail: usize,
    pub unknown: usize,
    pub transition: usize,
    /// The overall time budget was spent before the L2 probes started, so they
    /// did not run. Recorded, never scored.
    pub capped: bool,
}

/// One harness's header plus its rows, riskiest tier first.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HarnessHealth {
    /// `claude` | `opencode` — the same token [`CapabilityHealth::harness`]
    /// carries, and what *Run checks now* passes back over IPC.
    pub harness: &'static str,
    pub label: &'static str,
    /// The version cImp last observed this CLI running.
    pub last_seen: String,
    /// The version whose contracts were last verified. `None` for a harness
    /// with no verified column at all (OpenCode) — deliberately not `Some("")`,
    /// which would render as "verified against nothing" rather than "this
    /// harness does not track it".
    pub last_verified: Option<String>,
    /// The persisted Phase F record, when this harness has one.
    pub auto_verify: Option<AutoVerify>,
    /// The last run made since launch, when there is one.
    pub last_run: Option<RunView>,
    /// **V35 Phase I:** this harness's tabs whose spawn-baked artifact is out of
    /// step with the running binary — the consumer of the CHP version field.
    ///
    /// Empty is the normal state and renders as nothing. A non-empty list is the
    /// V32 deploy trap ("needs a FRESH TAB") caught and named instead of met as
    /// a capability that quietly misbehaves. Nothing is refused on the strength
    /// of it; it is a report.
    pub stale_plugins: Vec<StalePlugin>,
    pub capabilities: Vec<CapabilityHealth>,
}

// ── building it ─────────────────────────────────────────────────────────────

/// Tier rank for display: **D first**. The panel leads with the riskiest seam
/// because that is the question it exists to answer — a Tier-D scrape breaking
/// silently on a cosmetic upstream change is what the user needs to see, not a
/// Tier-A row that has never broken anything.
fn risk_rank(t: Seam) -> u8 {
    match t {
        Seam::D => 0,
        Seam::C => 1,
        Seam::B => 2,
        Seam::A => 3,
    }
}

/// Whether any automatic check DRIVES this row. Exactly the rows
/// [`verify::verify`] produces an answer for: `canary: Some(..)` is the
/// embedded L1 set and `probe: Some(..)` the implemented L2 set (both pinned
/// set-equal to the registry by `canary::tests::canaries_and_the_matrix_agree`
/// and `contract::tests::probes_and_the_matrix_agree`).
///
/// Load-bearing for the inference below: silence in a stored record only means
/// "did not fail" for a row a run would actually have visited. For a row
/// nothing drives, the same silence means nothing at all.
fn driven(cap: &Capability) -> bool {
    cap.canary.is_some() || cap.probe.is_some()
}

/// The last word on one capability, preferring a full in-process answer over
/// the stored record's failures-only view. See the module docs for why the
/// third arm is not a pass.
fn last_verify(
    cap: &Capability,
    run: Option<&RunSummary>,
    record: Option<&AutoVerify>,
) -> Option<VerifyView> {
    if let Some(run) = run {
        if let Some(a) = run.answers.iter().find(|a| a.id == cap.id) {
            return Some(VerifyView {
                outcome: a.outcome.label(),
                evidence: a.evidence.to_string(),
                detail: a.outcome.detail().to_string(),
                at_ms: run.at_ms,
                version: run.version.clone(),
                from_run: true,
            });
        }
    }
    let record = record?;
    // A failure is the one thing the record attests directly — and it is
    // matched by id even for a row nothing currently drives, because a
    // hand-edited or newer-cImp record naming a capability must never be
    // dropped on the floor for disagreeing with this build's coverage.
    if let Some(f) = record.failures.iter().find(|f| f.capability == cap.id) {
        return Some(VerifyView {
            outcome: "fail",
            evidence: f.evidence.clone(),
            detail: f.detail.clone(),
            at_ms: record.at_ms,
            version: record.version.clone(),
            from_run: false,
        });
    }
    if !driven(cap) {
        return None;
    }
    Some(VerifyView {
        outcome: OUTCOME_NO_FAILURE,
        evidence: String::new(),
        detail: "The last automatic run did not report a failure for this capability. The stored \
                 record keeps failures only, so this is weaker than a recorded pass — a check that \
                 could not run at all looks the same. Use Run checks now for a per-capability \
                 answer."
            .to_string(),
        at_ms: record.at_ms,
        version: record.version.clone(),
        from_run: false,
    })
}

fn run_view(run: &RunSummary) -> RunView {
    let count = |l: &str| run.answers.iter().filter(|a| a.outcome.label() == l).count();
    RunView {
        at_ms: run.at_ms,
        version: run.version.clone(),
        pass: count("pass"),
        fail: count("fail"),
        unknown: count("unknown"),
        transition: count("transition"),
        capped: run.capped,
    }
}

/// **The** Harness health query — everything the Settings panel renders,
/// grouped by harness and ordered riskiest-tier-first inside each group.
///
/// `settings` must already carry a FRESH `harness` map and `harness_versions`
/// (the caller layers the physical-global read in, exactly as
/// `harness_versions_get` does for the gates): the versions and the
/// auto-verify record are written out-of-band by the tap and the verify worker,
/// so a snapshot from app start would show a panel that is stale in precisely
/// the situation the panel exists for.
pub fn health(settings: &Settings) -> Vec<HarnessHealth> {
    let gates = contract::gates(settings);
    panel_labels()
        .into_iter()
        .map(|(harness, label)| {
            let run = verify::last_run(harness);
            // V40 Phase B: every harness has a row, so this is a map read and
            // not a plugin call any more. Before it, `auto_verify_record` was a
            // trait method whose only job was to know that Claude's record
            // lived in a field called `claude_auto_verify` and that no other
            // harness had one at all.
            let row = settings.harness_settings(harness);
            let record = row.auto_verify.as_ref();
            let mut rows: Vec<&'static Capability> =
                contract::capabilities().filter(|c| c.harness == harness).collect();
            rows.sort_by_key(|c| risk_rank(c.tier));
            let capabilities = rows
                .into_iter()
                .map(|c| CapabilityHealth {
                    id: c.id,
                    harness: harness_name(c.harness),
                    tier: tier_name(c.tier),
                    contract: c.contract,
                    user_effect: c.user_effect,
                    degradation: DegradationView::of(c.degradation),
                    coverage: Coverage {
                        canary: c.canary,
                        probe: c.probe,
                        waiver: c.waiver,
                        unproven: c.degradation == Degradation::Silent
                            && c.canary.is_none()
                            && c.probe.is_none(),
                    },
                    controls: c.controls,
                    wired_in: c.wired_in,
                    gate: gates.iter().find(|g| g.id == c.id).cloned(),
                    last_verify: last_verify(c, run.as_ref(), record),
                })
                .collect();
            let last_seen = row.last_seen.trim().to_string();
            HarnessHealth {
                harness: harness_name(harness),
                label,
                // V35 Phase I. The version arm of the staleness check compares a
                // hello's declared harness version against the one cImp has
                // actually observed, which is this same `last_seen` — passed in
                // rather than read inside `chp`, so the header's own version
                // line and its stale-plugin list are one reading of one value.
                stale_plugins: chp::stale_for(harness_name(harness), &last_seen),
                last_seen,
                last_verified: Some(row.last_verified.clone()),
                auto_verify: record.cloned(),
                last_run: run.as_ref().map(run_view),
                capabilities,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::AutoVerifyFailure;
    use std::collections::BTreeSet;

    fn settings_with(record: Option<AutoVerify>) -> Settings {
        let mut s = Settings::default();
        s.harness_row("claude").last_seen = "2.2.0".to_string();
        s.harness_row("claude").last_verified = "2.1.0".to_string();
        s.harness_row("opencode").last_seen = "1.19.0".to_string();
        s.harness_row("claude").auto_verify = record;
        s
    }

    fn find<'a>(h: &'a [HarnessHealth], id: &str) -> &'a CapabilityHealth {
        h.iter()
            .flat_map(|p| p.capabilities.iter())
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no health row for `{id}`"))
    }

    /// Every registry row reaches the panel, exactly once, under the harness it
    /// declares. A row that fell out of the view is a dependency the user can no
    /// longer see is broken — the same "stopped being counted" failure the probe
    /// avoids by enumerating its permanent `unknown`s.
    #[test]
    fn every_capability_appears_exactly_once() {
        let health = health(&settings_with(None));
        let shown: Vec<&str> = health
            .iter()
            .flat_map(|p| p.capabilities.iter().map(|c| c.id))
            .collect();
        let unique: BTreeSet<&str> = shown.iter().copied().collect();
        assert_eq!(shown.len(), unique.len(), "a capability is rendered twice");
        let registry: BTreeSet<&str> = contract::capabilities().map(|c| c.id).collect();
        assert_eq!(
            unique, registry,
            "the Harness health panel and the registry disagree about which capabilities exist"
        );
        for p in &health {
            for c in &p.capabilities {
                assert_eq!(c.harness, p.harness, "`{}` is under the wrong header", c.id);
            }
        }
    }

    /// Riskiest first, per harness. The order is the panel's whole argument —
    /// Tier D breaks silently on a cosmetic upstream change and Tier A has never
    /// broken cImp at all, so a registry-order list would bury the answer.
    #[test]
    fn rows_are_ordered_riskiest_tier_first() {
        for p in health(&settings_with(None)) {
            let ranks: Vec<&str> = p.capabilities.iter().map(|c| c.tier).collect();
            let mut sorted = ranks.clone();
            sorted.sort_by_key(|t| match *t {
                "D" => 0u8,
                "C" => 1,
                "B" => 2,
                _ => 3,
            });
            assert_eq!(ranks, sorted, "{} rows are not tier-ordered", p.harness);
        }
    }

    /// A `Silent` row standing on prose alone must be distinguishable from a
    /// canaried one. `perm.tui_scrape` is the archetype (Tier D, waiver only);
    /// `claude.transcript.usage` is the counterexample (canary AND probe).
    #[test]
    fn a_waiver_only_silent_row_is_marked_unproven() {
        let health = health(&settings_with(None));
        let scrape = find(&health, "perm.tui_scrape");
        assert!(scrape.coverage.unproven);
        assert!(scrape.coverage.canary.is_none() && scrape.coverage.probe.is_none());
        assert!(scrape.coverage.waiver.is_some(), "and it must show WHY");
        assert_eq!(scrape.degradation.kind, "silent");

        let usage = find(&health, "claude.transcript.usage");
        assert!(!usage.coverage.unproven);
        assert_eq!(usage.coverage.canary, Some("claude.transcript.usage"));
        assert_eq!(usage.coverage.probe, Some("claude.transcript.usage"));

        // A row that fails CLOSED is never "unproven" in this sense: it cannot
        // break silently, whatever its coverage.
        let deny = find(&health, contract::CAP_PRETOOLUSE_DENY);
        assert_eq!(deny.degradation.kind, "fail_closed");
        assert!(!deny.coverage.unproven);
    }

    /// The two degradation variants that carry a payload must carry it to the
    /// panel — a `VisibleOff` whose message did not survive would render as a
    /// promise of an explanation the user never gets, and a `Fallback` without
    /// its target cannot point at the row that takes over.
    #[test]
    fn degradation_payloads_survive_the_view() {
        let health = health(&settings_with(None));
        let noauth = find(&health, "opencode.route.noauth");
        assert_eq!(noauth.degradation.kind, "visible_off");
        assert!(noauth
            .degradation
            .user_message
            .is_some_and(|m| m.contains("authentication")));

        let notif = find(&health, "claude.hook.notification");
        assert_eq!(notif.degradation.kind, "fallback");
        assert_eq!(notif.degradation.fallback_to, Some("perm.tui_scrape"));
        // The fallback target is a join key, not prose: it must resolve.
        assert!(contract::get(notif.degradation.fallback_to.unwrap()).is_some());
    }

    /// The TCB column reaches the panel, and only the rows that own the
    /// controls carry them (matrix decision 10 — these are security controls
    /// that EXECUTE inside the capability, not rows that merely depend on one).
    ///
    /// **V35 Phase I widened this from one row to three.** A control id names a
    /// *place* enforcement executes, and the taint beacon and the pre-mutation
    /// checkpoint each run in two of them — inside the OpenCode plugin, and on
    /// Claude's own `PreToolUse` path (a shim binary until 2026-08-17, a
    /// `type: "http"` entry plus its handler since). The Claude sites have rows,
    /// so they carry their own control ids rather than the plugin row standing
    /// for both harnesses; the ids did not change when the site moved, which is
    /// what keeps this assertion meaningful across such a migration.
    #[test]
    fn the_tcb_column_reaches_the_panel() {
        let health = health(&settings_with(None));
        let plugin = find(&health, "opencode.plugin.load_all");
        assert!(plugin.controls.contains(&contract::CONTROL_TOOL_GATE));
        assert!(find(&health, "claude.hook.taint_beacon")
            .controls
            .contains(&contract::CONTROL_TAINT_BEACON_CLAUDE));
        assert!(find(&health, "claude.hook.checkpoint_beacon")
            .controls
            .contains(&contract::CONTROL_CHECKPOINT_PRE_MUTATION_CLAUDE));
        let mut marked: Vec<&str> = health
            .iter()
            .flat_map(|p| p.capabilities.iter())
            .filter(|c| !c.controls.is_empty())
            .map(|c| c.id)
            .collect();
        marked.sort_unstable();
        assert_eq!(
            marked,
            vec![
                "claude.hook.checkpoint_beacon",
                "claude.hook.taint_beacon",
                "opencode.plugin.load_all",
            ]
        );
    }

    /// **V35 Phase I:** a stale spawn-baked artifact reaches the harness header
    /// it belongs to, and only that one.
    ///
    /// This is the whole consumer side of the `chp` field (milestone locked
    /// decision 4 / design D5). Without this assertion the version would be a
    /// number on the wire that nothing reads — a computed-then-discarded signal,
    /// which is what the milestone exists to stop.
    ///
    /// Uses a tab id no other test touches, because the peer registry is
    /// process-global and the suite runs concurrently.
    #[test]
    fn a_stale_plugin_reaches_its_harness_header() {
        let tab = "opencode-health-stale-test";
        chp::note_for_test(chp::Peer {
            agent: "opencode".to_string(),
            tab: tab.to_string(),
            chp: chp::PRE_CHP,
            ..Default::default()
        });
        let health = health(&settings_with(None));
        let oc = health.iter().find(|p| p.harness == "opencode").unwrap();
        let row = oc
            .stale_plugins
            .iter()
            .find(|s| s.tab == tab)
            .expect("the stale peer must reach the OpenCode header");
        assert_eq!(row.kind, chp::STALE_OLD_PLUGIN);
        assert_eq!(row.seen_chp, chp::PRE_CHP);
        assert_eq!(row.expected, chp::CHP_VERSION);
        assert!(!row.note.trim().is_empty(), "a report with no sentence");

        // …and it does NOT leak into the other harness's header.
        let claude = health.iter().find(|p| p.harness == "claude").unwrap();
        assert!(claude.stale_plugins.iter().all(|s| s.tab != tab));
    }

    /// The Phase E gate verdict is attached to the row it is about, and to no
    /// other. `None` (ungated) and `Some(not blocked)` are different statements
    /// and the panel must be able to tell them apart.
    #[test]
    fn gate_verdicts_land_on_their_own_rows() {
        let mut s = settings_with(None);
        s.harness_versions.e1_status = "fail".to_string();
        let health = health(&s);
        let deny = find(&health, contract::CAP_PRETOOLUSE_DENY);
        let gate = deny.gate.as_ref().expect("the gated row must carry a gate");
        assert!(gate.blocked);
        assert!(!gate.reason.trim().is_empty());

        let gated: Vec<&str> = health
            .iter()
            .flat_map(|p| p.capabilities.iter())
            .filter(|c| c.gate.is_some())
            .map(|c| c.id)
            .collect();
        assert_eq!(gated, contract::GATED.to_vec());

        // The same row with a healthy status keeps its gate — present and not
        // blocked — rather than losing it.
        s.harness_versions.e1_status = "pass".to_string();
        let ok = super::health(&s);
        let deny = find(&ok, contract::CAP_PRETOOLUSE_DENY);
        assert!(!deny.gate.as_ref().unwrap().blocked);
    }

    /// A stored failure becomes that row's last word, with its evidence and its
    /// sentence intact — this is the join a user follows from an Advisor card to
    /// the panel row it names.
    #[test]
    fn a_stored_failure_becomes_its_rows_last_verify() {
        let record = AutoVerify {
            version: "2.2.0".to_string(),
            at_ms: 4242,
            status: AutoVerify::FAIL.to_string(),
            failures: vec![AutoVerifyFailure {
                capability: "claude.statusline.stdin".to_string(),
                evidence: crate::harness::verify::EVIDENCE_CANARY.to_string(),
                detail: "context_window.used_percentage gone".to_string(),
            }],
        };
        let health = health(&settings_with(Some(record)));
        let broken = find(&health, "claude.statusline.stdin")
            .last_verify
            .clone()
            .expect("the failing row must carry the failure");
        assert_eq!(broken.outcome, "fail");
        assert_eq!(broken.evidence, crate::harness::verify::EVIDENCE_CANARY);
        assert_eq!(broken.detail, "context_window.used_percentage gone");
        assert_eq!(broken.at_ms, 4242);
        assert_eq!(broken.version, "2.2.0");
        assert!(!broken.from_run);

        // The header carries the record itself, so the panel can date the run
        // without re-deriving it from the rows.
        let claude = health.iter().find(|p| p.harness == "claude").unwrap();
        assert_eq!(claude.auto_verify.as_ref().unwrap().at_ms, 4242);
        assert_eq!(claude.last_verified.as_deref(), Some("2.1.0"));
        // **V40 Phase B: OpenCode HAS a verified column now** — every harness
        // does, because the record is a `harness[<id>]` row rather than two
        // Claude-named fields. It is EMPTY here, which is the honest answer
        // ("never verified"), and it is a different state from the `None` this
        // used to assert: `None` meant "this harness cannot record one at all",
        // which was a fact about the schema, not about OpenCode.
        let oc = health.iter().find(|p| p.harness == "opencode").unwrap();
        assert_eq!(oc.last_verified.as_deref(), Some(""));
        assert!(oc.auto_verify.is_none(), "no run has been recorded for it");
        assert_eq!(oc.last_seen, "1.19.0");
    }

    /// The honest-silence rule, both halves. A DRIVEN row the record does not
    /// name reads as [`OUTCOME_NO_FAILURE`] — never `pass`, because the record
    /// keeps failures only and an `unknown` is indistinguishable from a pass in
    /// it. A row nothing drives gets no inferred answer at all: the same silence
    /// says nothing whatsoever about it.
    #[test]
    fn silence_in_the_record_is_not_promoted_to_a_pass() {
        let record = AutoVerify {
            version: "2.2.0".to_string(),
            at_ms: 99,
            status: AutoVerify::PASS.to_string(),
            failures: Vec::new(),
        };
        let health = health(&settings_with(Some(record)));

        let driven = find(&health, "claude.transcript.usage")
            .last_verify
            .clone()
            .expect("a driven row inherits the run's silence");
        assert_eq!(driven.outcome, OUTCOME_NO_FAILURE);
        assert_ne!(driven.outcome, "pass");
        assert!(driven.evidence.is_empty(), "no single layer said it");
        assert!(driven.detail.contains("weaker than a recorded pass"));
        assert!(!driven.from_run);
        assert_eq!(driven.version, "2.2.0");

        // Nothing drives `claude.hook.precompact` (no canary, no probe — it is
        // in `probe::DECLARED_UNPROBED`), so the record cannot speak for it.
        assert!(find(&health, "claude.hook.precompact").last_verify.is_none());

        // And the OTHER harness is untouched by a Claude record.
        assert!(find(&health, "opencode.sse.events").last_verify.is_none());
    }

    /// With no record and no run, every row says "never checked" rather than
    /// anything reassuring.
    #[test]
    fn a_fresh_install_claims_nothing() {
        for p in health(&Settings::default()) {
            for c in &p.capabilities {
                assert!(c.last_verify.is_none(), "`{}` invented an answer", c.id);
            }
            assert!(p.auto_verify.is_none());
        }
    }

    /// The panel's field names are the contract with the Svelte mirror. Same
    /// `include_str!` tripwire as `contract::tests::
    /// the_gated_capability_ids_reach_the_frontend`: a rename here fails the
    /// Rust build instead of silently rendering `undefined`.
    ///
    /// **V42 Phase E RE-POINTED this scan, deliberately rather than retiring
    /// it.** The phase generated the `settings/schema.rs` tree into
    /// `src/lib/settings/generated/settings.ts` and deleted the hand-written
    /// mirror of THOSE types — but the health payload is `harness::health`'s,
    /// not `schema.rs`', and it is still hand-mirrored in `types.ts`. Only
    /// `auto_verify` moved (it is a `HarnessSettings` field, so it is
    /// generated now), so the scan reads BOTH files: retiring it would have
    /// dropped a live tripwire over eleven still-hand-written names on the
    /// strength of a codegen that does not cover them.
    ///
    /// **V42 review (dropped-at-cap) NARROWED the generated half.** The
    /// re-point was `concat!(types.ts, generated/settings.ts)` and a plain
    /// `.contains`, which put ~1,900 lines of generated PROSE behind every
    /// needle: `auto_verify` is satisfied by the sentence
    /// "`claude_auto_verify` had no OpenCode twin at all" in a doc comment
    /// thirty lines above the field, and would keep being satisfied by it
    /// after the field was renamed. A tripwire whose needle can be met by
    /// commentary is a tripwire that fires when nobody trips it and stays
    /// quiet when somebody does. The generated half is now sliced to the one
    /// declaration that carries the field, and the needle is the DECLARATION
    /// form (`name: `), not the bare name.
    #[test]
    fn the_health_field_names_reach_the_frontend() {
        /// The hand-written mirror — `harness::health`'s own payload types,
        /// which no generator covers.
        const HAND_WRITTEN: &str = include_str!("../../../src/lib/settings/types.ts");
        /// The generated bindings, read only through [`generated_block`].
        const GENERATED: &str =
            include_str!("../../../src/lib/settings/generated/settings.ts");

        /// One `export type <name> = …;` block out of the generated bindings,
        /// signature to terminator — so a needle cannot be answered by the
        /// prose around a different type.
        fn generated_block<'a>(name: &str) -> &'a str {
            let decl = format!("export type {name} = ");
            let at = GENERATED
                .find(&decl)
                .unwrap_or_else(|| panic!("`{decl}` is not in the generated bindings"));
            let body = &GENERATED[at + decl.len()..];
            let end = body
                .find(";\n")
                .unwrap_or_else(|| panic!("`{decl}` is never terminated"));
            &body[..end]
        }

        /// Whether `block` DECLARES `field`, rather than merely mentioning it.
        /// ts-rs emits one field per line inside a block, so a declaration is
        /// a trimmed line opening `field: `.
        fn declares_field(block: &str, field: &str) -> bool {
            block
                .lines()
                .any(|l| l.trim_start().starts_with(&format!("{field}: ")))
        }

        for field in [
            "harness_health",
            "user_effect",
            "verify_in_flight",
            "last_verify",
            "unproven",
            "from_run",
            "fallback_to",
            "user_message",
            "wired_in",
            "last_run",
            // V35 Phase I — the CHP staleness report and its three display
            // fields. `note` is the sentence the panel renders verbatim; a
            // frontend that re-derived it from `seen_chp`/`expected` would be a
            // second place for the rule to be wrong.
            "stale_plugins",
            "seen_chp",
            "expected",
        ] {
            assert!(
                HAND_WRITTEN.contains(field),
                "`{field}` is served by the Harness health payload but is not spelled in \
                 src/lib/settings/types.ts — the panel reads this exact name"
            );
        }

        // `auto_verify` is the one name V42 Phase E moved into the generated
        // bindings: it is a `HarnessSettings` field now. Checked against THAT
        // declaration only.
        let harness_settings = generated_block("HarnessSettings");
        assert!(
            declares_field(harness_settings, "auto_verify"),
            "`auto_verify` is served by the Harness health payload but `HarnessSettings` \
             does not declare it — the panel reads this exact name"
        );
        // The control for the narrowing, permanent because the prose it rules
        // out is really in the file. `claude_auto_verify` appears in TWO doc
        // comments (`HarnessSettings`' own and `HarnessVersions`', both
        // recounting V40 Phase B), neither inside the declaration — so the
        // retired whole-file `.contains("auto_verify")` was satisfied by
        // commentary alone and would have stayed green through a rename of the
        // field it was watching.
        assert!(
            GENERATED
                .split("export type HarnessSettings = ")
                .next()
                .expect("the text before the declaration")
                .contains("auto_verify"),
            "the control's premise is gone — the generated prose OUTSIDE the declaration \
             used to satisfy the old whole-file needle on its own"
        );
        assert!(
            !declares_field(harness_settings, "claude_auto_verify"),
            "a doc-comment mention read as a field declaration; the narrowing is not \
             narrowing anything"
        );

        assert!(
            HAND_WRITTEN.contains(&format!("'{OUTCOME_NO_FAILURE}'")),
            "the `{OUTCOME_NO_FAILURE}` outcome token must be spelled in types.ts — the panel \
             renders it differently from a real pass on purpose"
        );
    }
}

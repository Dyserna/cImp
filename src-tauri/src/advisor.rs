//! V14 Phase D2 — budget-tuning advisor.
//!
//! The Usage section's Phase C/D data collects exactly what the V10/V11
//! context-injection knobs need for tuning, but the sliders leave the tuning
//! entirely to the user. This module proposes measured changes —
//! **propose-and-confirm, never silent self-modification** (a setting that
//! changes itself would erode the trust the honest-accounting posture
//! builds across the rest of the Code Intelligence tab).
//!
//! [`evaluate`] is a deterministic, PURE function of [`Signals`] — no I/O,
//! no settings writes, no clock reads. The `graph_usage_advice` IPC handler
//! (`ipc/commands.rs`) does all the work of assembling `Signals` from the
//! `GraphService` (D2.1's aggregates) and the live `Settings`; the Apply
//! button on a returned [`Proposal`] writes through the ordinary
//! `settings_update` IPC path, same as every other setting in the app —
//! there is no bespoke "apply" IPC here.
//!
//! Rules are versioned in code (`rule_id` is a `"...vN"` string constant,
//! never reused for a changed rule) and listed in the Usage section's
//! Advisor card tooltip — inspectable, not magic.

use crate::settings::{DismissedRule, GraphSettings};

// ── Cold-start floor ────────────────────────────────────────────────────
//
// Below these, a rule simply doesn't propose — the card shows "collecting"
// (see `ipc::commands::graph_usage_advice`'s `collecting` flag) rather than
// a possibly-noisy proposal from a handful of data points.

/// Global floor shared by every rule: fewer sessions than this and NOTHING
/// proposes, regardless of how extreme an individual rate looks — a single
/// busy session shouldn't be enough to retune a project-wide budget.
pub const MIN_SESSIONS: u64 = 5;
/// Rule 1 / rule 3's floor: distinct injected-file instances observed
/// (across every session currently tracked — see
/// `GraphService::injection_follow_rate`).
pub const MIN_INJECTIONS: u64 = 200;
/// Rule 2's floor: read-advisor reminders observed. Much lower than
/// `MIN_INJECTIONS` — reminders are naturally scarcer than injections (the
/// advisor only fires once per file per session), so gating it at 200 would
/// mean the rule almost never gets enough data to speak.
pub const MIN_REMINDS: u64 = 20;
/// Rule 3's second floor: retrieval turns observed (its `budget_maxed_rate`
/// signal).
pub const MIN_TURNS: u64 = 50;

// ── "High rate" thresholds ──────────────────────────────────────────────

/// Rule 1 / rule 3: an injected-but-never-touched rate at or above this is
/// "high" (70%+ of injected files going unused).
const UNUSED_HIGH: f64 = 0.7;
/// Rule 2: a reminder-then-reread rate at or above this is "high".
const REREAD_HIGH: f64 = 0.5;
/// Rule 3: a turn-budget-maxed rate at or above this is "high".
const BUDGET_MAXED_HIGH: f64 = 0.5;

/// V14 code-review fix (FIX 8): ceiling on `context_min_score` proposals.
/// `build_context`'s additive relevance score is roughly a ~3-20 scale (see
/// its own scoring doc comment); a `context_min_score` anywhere near the top
/// of that range already rejects nearly every candidate file, so repeated
/// applies of rule 1 (each `saturating_add(1)`, otherwise unbounded) could
/// climb the floor past any real score and silently turn off injection
/// entirely. Once the LIVE value has reached this ceiling, rule 1 stops
/// proposing further raises — it's already as aggressive as makes sense.
const MIN_SCORE_CEILING: u32 = 12;

/// Rule 1's versioned id. Never reuse a rule id for a changed threshold or
/// proposed-value formula — bump the version suffix instead, so an old
/// dismissal (keyed by `rule_id` + signature) doesn't silently suppress a
/// materially different rule.
pub const RULE_MIN_SCORE: &str = "advisor.raise_context_min_score.v1";
pub const RULE_ADVISOR_LINES: &str = "advisor.raise_read_advisor_min_lines.v1";
pub const RULE_TURN_BUDGET: &str = "advisor.lower_context_turn_budget_chars.v1";

/// The aggregated signals [`evaluate`] reasons over. Each optional rate
/// field degrades to `None` when its source feature has never produced data
/// (context injection never fired, or the read advisor never reminded
/// anyone) — a rule whose signal is `None` simply doesn't fire, it never
/// treats the absence as "0% / healthy". `dismissed` and `graph` are read
/// straight from the live `Settings` — see `ipc::commands::graph_usage_advice`
/// for how this is assembled.
#[derive(Clone, Debug, Default)]
pub struct Signals {
    /// D2.1: fraction of injected files never subsequently read/edited this
    /// session (V11-C `injected` ⋈ V10 `mem_event`), plus its sample count
    /// (distinct injected-file instances).
    pub injection_follow_rate: Option<f64>,
    pub injection_follow_samples: u64,
    /// D2.1: fraction of read-advisor reminders followed by a real re-read
    /// of the same file in the same session (V11-E), plus its sample count
    /// (reminders observed).
    pub advisor_reread_rate: Option<f64>,
    pub advisor_reread_samples: u64,
    /// D2.1: fraction of retrieval turns that filled ≥90% of
    /// `context_turn_budget_chars`, plus its sample count (turns observed).
    pub budget_maxed_rate: Option<f64>,
    pub budget_maxed_samples: u64,
    /// Distinct sessions this project's memory knows about — the global
    /// cold-start floor's session half.
    pub session_count: u64,
    /// The live graph settings — proposals read CURRENT values from here and
    /// compute PROPOSED values from them; never hardcoded.
    pub graph: GraphSettings,
    /// The user's dismissed-proposal list (`Settings::advisor_dismissed`).
    pub dismissed: Vec<DismissedRule>,
}

/// One budget-tuning proposal: a setting, its current and proposed values
/// (both pre-formatted for display), the measured rationale, the rule that
/// produced it, and the coarse signature of the rate that triggered it.
/// `setting` is a dotted path into `Settings` (e.g.
/// `"graph.context_min_score"`) that the frontend's Apply button uses to
/// locate the field to mutate before round-tripping through
/// `settingsUpdate` — see `proposal_setting_names_match_real_fields` below
/// for the test that keeps this honest.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Proposal {
    pub setting: String,
    pub current: String,
    pub proposed: String,
    pub rationale: String,
    pub rule_id: &'static str,
    /// Coarse (10%-bucketed) signature of the rate that triggered this
    /// proposal — round-tripped through `advisor_dismiss` so a dismissal is
    /// keyed to "this rate, roughly": measurement noise within the same
    /// bucket stays suppressed, but a materially changed rate (a different
    /// bucket) re-fires even for the same `rule_id`.
    pub signature: String,
}

/// Bucket a `[0, 1]` rate to the nearest 10% and render it as a compact
/// string (`"0"`..`"10"`). A raw float would make an exact-equality
/// dismissal comparison fragile (the same underlying condition rarely
/// produces bit-identical rates twice); bucketing absorbs that noise while
/// still re-firing on a real shift.
fn bucket10(rate: f64) -> String {
    let b = (rate.clamp(0.0, 1.0) * 10.0).round() as u32;
    b.to_string()
}

/// Whether `(rule_id, signature)` is in the dismissed list.
fn is_dismissed(dismissed: &[DismissedRule], rule_id: &str, signature: &str) -> bool {
    dismissed.iter().any(|d| d.rule_id == rule_id && d.signature == signature)
}

/// Evaluate the static V1 rule list over `sig`, returning the proposals that
/// clear their sample floor, their rate threshold, AND aren't already
/// dismissed at their current (bucketed) rate. Pure — no I/O, no clock.
pub fn evaluate(sig: &Signals) -> Vec<Proposal> {
    let mut out = Vec::new();
    // Global cold-start floor: nothing proposes below it, no matter how
    // extreme an individual rate looks.
    if sig.session_count < MIN_SESSIONS {
        return out;
    }

    // Rule 1 — injected files rarely touched again ⇒ raise context_min_score.
    if let Some(follow) = sig.injection_follow_rate {
        if sig.injection_follow_samples >= MIN_INJECTIONS {
            let unused = 1.0 - follow;
            if unused >= UNUSED_HIGH && sig.graph.context_min_score < MIN_SCORE_CEILING {
                let signature = bucket10(unused);
                if !is_dismissed(&sig.dismissed, RULE_MIN_SCORE, &signature) {
                    let proposed = sig.graph.context_min_score.saturating_add(1);
                    out.push(Proposal {
                        setting: "graph.context_min_score".to_string(),
                        current: sig.graph.context_min_score.to_string(),
                        proposed: proposed.to_string(),
                        rationale: format!(
                            "{:.0}% of files injected in full this project were never read or \
                             edited again this session (n={} injections) — raising the \
                             relevance floor should stop marginal files from being force-fed.",
                            unused * 100.0,
                            sig.injection_follow_samples
                        ),
                        rule_id: RULE_MIN_SCORE,
                        signature,
                    });
                }
            }
        }
    }

    // Rule 2 — reminders followed by a full re-read anyway ⇒ raise
    // read_advisor_min_lines (the reminders fire on files the agent
    // genuinely needs whole).
    if let Some(rate) = sig.advisor_reread_rate {
        if sig.advisor_reread_samples >= MIN_REMINDS && rate >= REREAD_HIGH {
            let signature = bucket10(rate);
            if !is_dismissed(&sig.dismissed, RULE_ADVISOR_LINES, &signature) {
                let proposed = sig.graph.read_advisor_min_lines.saturating_add(100);
                out.push(Proposal {
                    setting: "graph.read_advisor_min_lines".to_string(),
                    current: sig.graph.read_advisor_min_lines.to_string(),
                    proposed: proposed.to_string(),
                    rationale: format!(
                        "{:.0}% of read-advisor reminders were followed by a full re-read of \
                         the same file anyway (n={} reminders) — those files are evidently \
                         needed whole; raising the line floor lets them pass instead of being \
                         reminded.",
                        rate * 100.0,
                        sig.advisor_reread_samples
                    ),
                    rule_id: RULE_ADVISOR_LINES,
                    signature,
                });
            }
        }
    }

    // Rule 3 — injected-but-unread rate high WHILE the turn budget is maxed
    // ⇒ lower context_turn_budget_chars (the budget is spending on files
    // that aren't helping).
    if let (Some(follow), Some(maxed)) = (sig.injection_follow_rate, sig.budget_maxed_rate) {
        if sig.injection_follow_samples >= MIN_INJECTIONS && sig.budget_maxed_samples >= MIN_TURNS
        {
            let unused = 1.0 - follow;
            if unused >= UNUSED_HIGH && maxed >= BUDGET_MAXED_HIGH {
                let signature = bucket10(unused);
                let proposed =
                    (((sig.graph.context_turn_budget_chars as f64) * 0.8) as u32).max(1_000);
                // V14 code-review fix (FIX 8): the `.max(1_000)` floor means
                // this formula can propose a value ≥ `current` when
                // `current` is already ≤ 1250 (e.g. current=1000 ⇒
                // proposed=1000, or current=1100 ⇒ proposed=1100.max(1000)
                // still not a REDUCTION) — a rule whose entire premise is
                // "lower the budget" must never propose raising (or
                // no-op'ing) it. Only emit when it's a real reduction.
                if proposed < sig.graph.context_turn_budget_chars
                    && !is_dismissed(&sig.dismissed, RULE_TURN_BUDGET, &signature)
                {
                    out.push(Proposal {
                        setting: "graph.context_turn_budget_chars".to_string(),
                        current: sig.graph.context_turn_budget_chars.to_string(),
                        proposed: proposed.to_string(),
                        rationale: format!(
                            "{:.0}% of injected files go unread AND {:.0}% of turns fill the \
                             turn budget (n={} turns) — the budget is spending on files that \
                             aren't helping; lowering it should cut waste without losing \
                             anything actually used.",
                            unused * 100.0,
                            maxed * 100.0,
                            sig.budget_maxed_samples
                        ),
                        rule_id: RULE_TURN_BUDGET,
                        signature,
                    });
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A signals blob with every sample floor cleared and every rate at the
    /// most extreme end (100%/0%), so all three rules should fire.
    fn extreme_signals() -> Signals {
        Signals {
            injection_follow_rate: Some(0.0), // 100% unused
            injection_follow_samples: MIN_INJECTIONS,
            advisor_reread_rate: Some(1.0), // 100% reread
            advisor_reread_samples: MIN_REMINDS,
            budget_maxed_rate: Some(1.0), // 100% maxed
            budget_maxed_samples: MIN_TURNS,
            session_count: MIN_SESSIONS,
            graph: GraphSettings::default(),
            dismissed: Vec::new(),
        }
    }

    #[test]
    fn all_three_rules_fire_when_every_signal_is_extreme_and_sampled_enough() {
        let proposals = evaluate(&extreme_signals());
        let ids: Vec<&str> = proposals.iter().map(|p| p.rule_id).collect();
        assert_eq!(ids, vec![RULE_MIN_SCORE, RULE_ADVISOR_LINES, RULE_TURN_BUDGET]);
    }

    #[test]
    fn below_global_session_floor_nothing_proposes_even_with_extreme_rates() {
        let mut sig = extreme_signals();
        sig.session_count = MIN_SESSIONS - 1;
        assert!(evaluate(&sig).is_empty());
    }

    #[test]
    fn rule1_respects_its_own_sample_floor() {
        let mut sig = extreme_signals();
        sig.injection_follow_samples = MIN_INJECTIONS - 1;
        let ids: Vec<&str> = evaluate(&sig).iter().map(|p| p.rule_id).collect();
        assert!(!ids.contains(&RULE_MIN_SCORE));
        // Rule 3 shares the same sample floor and signal, so it's gated too.
        assert!(!ids.contains(&RULE_TURN_BUDGET));
        // Rule 2 is independent — still fires.
        assert!(ids.contains(&RULE_ADVISOR_LINES));
    }

    #[test]
    fn rule1_respects_its_own_rate_threshold() {
        let mut sig = extreme_signals();
        sig.injection_follow_rate = Some(0.5); // 50% unused, below UNUSED_HIGH (70%)
        let ids: Vec<&str> = evaluate(&sig).iter().map(|p| p.rule_id).collect();
        assert!(!ids.contains(&RULE_MIN_SCORE));
        assert!(!ids.contains(&RULE_TURN_BUDGET));
    }

    #[test]
    fn rule2_respects_its_own_sample_floor_and_threshold() {
        let mut sig = extreme_signals();
        sig.advisor_reread_samples = MIN_REMINDS - 1;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_ADVISOR_LINES));

        let mut sig = extreme_signals();
        sig.advisor_reread_rate = Some(0.1); // below REREAD_HIGH
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_ADVISOR_LINES));
    }

    #[test]
    fn rule3_needs_both_unused_and_maxed_high() {
        let mut sig = extreme_signals();
        sig.budget_maxed_rate = Some(0.1); // budget not maxed
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_TURN_BUDGET));

        let mut sig = extreme_signals();
        sig.budget_maxed_samples = MIN_TURNS - 1;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_TURN_BUDGET));
    }

    #[test]
    fn missing_signals_mean_the_dependent_rules_never_fire() {
        let mut sig = extreme_signals();
        sig.injection_follow_rate = None;
        let ids: Vec<&str> = evaluate(&sig).iter().map(|p| p.rule_id).collect();
        assert!(!ids.contains(&RULE_MIN_SCORE));
        assert!(!ids.contains(&RULE_TURN_BUDGET)); // also depends on injection_follow_rate
        assert!(ids.contains(&RULE_ADVISOR_LINES)); // independent signal, unaffected

        let mut sig = extreme_signals();
        sig.advisor_reread_rate = None;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_ADVISOR_LINES));

        let mut sig = extreme_signals();
        sig.budget_maxed_rate = None;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_TURN_BUDGET));
    }

    #[test]
    fn dismissal_suppresses_the_same_bucket_but_a_changed_bucket_refires() {
        let mut sig = extreme_signals(); // unused = 1.0 -> bucket "10"
        sig.dismissed = vec![DismissedRule {
            rule_id: RULE_MIN_SCORE.to_string(),
            signature: "10".to_string(),
        }];
        let ids: Vec<&str> = evaluate(&sig).iter().map(|p| p.rule_id).collect();
        assert!(!ids.contains(&RULE_MIN_SCORE), "same-bucket dismissal must suppress");
        // Rule 3 shares the signal but is a DIFFERENT rule_id — its own
        // dismissal list is empty, so it still fires.
        assert!(ids.contains(&RULE_TURN_BUDGET));

        // A materially changed rate (still "high", but a different 10%
        // bucket) re-fires despite the dismissal.
        let mut sig2 = extreme_signals();
        sig2.injection_follow_rate = Some(0.25); // unused = 0.75 -> bucket "8"
        sig2.dismissed = vec![DismissedRule {
            rule_id: RULE_MIN_SCORE.to_string(),
            signature: "10".to_string(),
        }];
        assert!(evaluate(&sig2).iter().any(|p| p.rule_id == RULE_MIN_SCORE));
    }

    #[test]
    fn proposal_setting_names_match_real_graphsettings_fields() {
        // The thing that actually breaks silently if a field gets renamed:
        // the frontend's Apply button maps `Proposal::setting` to a
        // GraphSettings field by string. Verify each of the three settings
        // this module can propose is a real, assignable u32 field.
        let proposals = evaluate(&extreme_signals());
        assert_eq!(proposals.len(), 3);
        let mut g = GraphSettings::default();
        let before = (
            g.context_min_score,
            g.read_advisor_min_lines,
            g.context_turn_budget_chars,
        );
        for p in &proposals {
            let val: u32 = p.proposed.parse().expect("proposed value must be a u32 string");
            match p.setting.as_str() {
                "graph.context_min_score" => g.context_min_score = val,
                "graph.read_advisor_min_lines" => g.read_advisor_min_lines = val,
                "graph.context_turn_budget_chars" => g.context_turn_budget_chars = val,
                other => panic!("unrecognized proposal setting: {other}"),
            }
        }
        assert_ne!(
            (g.context_min_score, g.read_advisor_min_lines, g.context_turn_budget_chars),
            before,
            "the round-trip must actually change the settings"
        );
    }

    // ── V14 code-review FIX 8: bad-proposal guards ──────────────────────

    #[test]
    fn rule3_never_proposes_a_raise_or_no_op_for_a_small_budget() {
        // The `.max(1_000)` floor in the proposed-value formula would
        // otherwise RAISE the budget for any `current` below the floor
        // (e.g. current=900 -> proposed=max(720, 1000)=1000, a raise) —
        // contradicting a rule whose entire premise is "lower the budget".
        let mut sig = extreme_signals();
        sig.graph.context_turn_budget_chars = 900;
        assert!(
            !evaluate(&sig).iter().any(|p| p.rule_id == RULE_TURN_BUDGET),
            "must not propose when the formula would raise (or leave unchanged) the budget"
        );

        // Exactly at the floor: proposed == current (900 -> no, use 1000
        // here), also not a real reduction, so still must not propose.
        let mut sig_eq = extreme_signals();
        sig_eq.graph.context_turn_budget_chars = 1000;
        assert!(!evaluate(&sig_eq).iter().any(|p| p.rule_id == RULE_TURN_BUDGET));

        // A comfortably large current still gets a real reduction proposed.
        let sig_large = extreme_signals(); // default context_turn_budget_chars = 6_000
        assert!(evaluate(&sig_large).iter().any(|p| p.rule_id == RULE_TURN_BUDGET));
    }

    #[test]
    fn rule1_stops_proposing_once_min_score_hits_the_ceiling() {
        // At the ceiling, rule 1 must not propose a further raise — an
        // unbounded `saturating_add(1)` applied repeatedly could otherwise
        // climb `context_min_score` past any real score and silently kill
        // injection entirely.
        let mut sig = extreme_signals();
        sig.graph.context_min_score = MIN_SCORE_CEILING;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_MIN_SCORE));

        // Comfortably above the ceiling too (in case of a stale/hand-edited
        // value higher than the ceiling itself).
        let mut sig_above = extreme_signals();
        sig_above.graph.context_min_score = MIN_SCORE_CEILING + 5;
        assert!(!evaluate(&sig_above).iter().any(|p| p.rule_id == RULE_MIN_SCORE));

        // Just below the ceiling: one more raise is still reasonable.
        let mut sig_below = extreme_signals();
        sig_below.graph.context_min_score = MIN_SCORE_CEILING - 1;
        assert!(evaluate(&sig_below).iter().any(|p| p.rule_id == RULE_MIN_SCORE));
    }

    #[test]
    fn bucket10_rounds_to_the_nearest_ten_percent() {
        assert_eq!(bucket10(0.0), "0");
        assert_eq!(bucket10(0.04), "0");
        assert_eq!(bucket10(0.06), "1");
        assert_eq!(bucket10(0.75), "8"); // rounds up from 7.5
        assert_eq!(bucket10(1.0), "10");
    }
}

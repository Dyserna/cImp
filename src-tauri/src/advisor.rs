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

// ── V16 Feature 1/2/3/4 — drift canary rule class ───────────────────────
//
// Same versioned-id + dismiss-memory discipline as the tuning rules, but a
// different purpose: these detect the SYMPTOMS of a broken harness contract
// (a Claude Code / OpenCode auto-update changing hook semantics under us).
// Most are warn-only (`warn_only: true`, no Apply); the two that propose a
// settings write both propose DISABLING `read_advisor` — an advisor whose
// deny reason isn't reaching the model (or is being routed around via the
// shell) is strictly worse than no advisor. Drift rules carry their OWN
// sample floors — the global `MIN_SESSIONS` floor gates only the tuning
// rules (a version bump is a fact, not a statistic).

pub const RULE_DRIFT_VERSION: &str = "drift.harness_version.v1";
pub const RULE_DRIFT_READ_REASON: &str = "drift.read_reason.v1";
pub const RULE_DRIFT_HOOK_SILENT: &str = "drift.read_hook_silent.v1";
pub const RULE_DRIFT_INJECTION_UNSEEN: &str = "drift.injection_unseen.v1";
pub const RULE_DRIFT_USAGE_FIELDS: &str = "drift.usage_fields_gone.v1";
pub const RULE_DRIFT_PAYLOAD: &str = "drift.payload.v1";
pub const RULE_DRIFT_READ_BYPASS: &str = "drift.read_bypass.v1";

/// `drift.read_reason.v1`: reminders observed before the ~100%-reread check
/// can speak. Lower than the tuning rule's `MIN_REMINDS` (20) — this is a
/// breakage detector, and waiting longer just burns more bare refusals.
pub const DRIFT_MIN_REMINDS: u64 = 15;
/// `drift.read_reason.v1`: a remind→full-reread rate at or above this is no
/// longer "the files are needed whole" (the tuning rule's ≥50% diagnosis) —
/// it's "the deny *reason* isn't reaching the model at all".
const READ_REASON_HIGH: f64 = 0.9;
/// `drift.read_hook_silent.v1`: sessions observed before "zero reminds" is
/// evidence of a dead hook rather than a quiet project.
pub const DRIFT_SILENT_MIN_SESSIONS: u64 = 3;
/// `drift.read_hook_silent.v1`: re-reads of large files that SHOULD have
/// drawn a reminder before silence is suspicious.
pub const DRIFT_SILENT_MIN_REREADS: u64 = 10;
/// `drift.injection_unseen.v1`: injected-file floor (distinct from the
/// tuning rules' 200 — near-zero follow is detectable much earlier).
pub const DRIFT_UNSEEN_MIN_INJECTIONS: u64 = 30;
/// `drift.injection_unseen.v1`: session floor.
pub const DRIFT_UNSEEN_MIN_SESSIONS: u64 = 5;
/// `drift.injection_unseen.v1`: a follow rate at or below this is "the
/// block likely never reaches the model" (vs. the tuning rule's "the floor
/// is too low" at ≤30% follow).
const INJECTION_UNSEEN_LOW: f64 = 0.02;
/// `drift.read_bypass.v1`: reminders observed before the bypass share can
/// speak (V16 open item: placeholder — tune on real bypass rates).
pub const DRIFT_MIN_BYPASS_REMINDS: u64 = 10;
/// `drift.read_bypass.v1`: share of reminders answered with a shell read
/// (`cat`/`Get-Content` via Bash) at or above this proposes disabling the
/// advisor (V16 open item: placeholder threshold).
const BYPASS_HIGH: f64 = 0.4;
/// `drift.usage_fields_gone.v1`: Claude sessions without token fields
/// before the rule speaks (one could be a fluke/crashed session).
pub const DRIFT_MIN_TOKENLESS: u64 = 2;

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

    // ── V16 drift signals ───────────────────────────────────────────────
    /// Feature 1: latest Claude Code version seen in a transcript (empty
    /// until a Claude tab has run) and the version the hook contracts were
    /// last verified against (`HarnessVersions` in global settings).
    pub claude_last_seen: String,
    pub claude_last_verified: String,
    /// Feature 2 (`drift.read_hook_silent.v1`): total read-advisor remind
    /// events recorded for this root's sessions (mem_event `remind` rows —
    /// written server-side, so a dead hook means exactly zero).
    pub remind_count: u64,
    /// Feature 2 (`drift.read_hook_silent.v1`): (session, file) pairs with
    /// ≥2 observed reads of a file large enough that `should_read` would
    /// have reminded (≥ `read_advisor_min_lines` lines at index time) —
    /// approximation labeled est.; hash-unchanged isn't reconstructible
    /// retroactively.
    pub large_reread_pairs: u64,
    /// Feature 2 (`drift.usage_fields_gone.v1`): Claude-agent sessions in
    /// the window, and how many of them recorded NO token-bearing
    /// `usage_stat` rows (transcript schema change ⇒ `parse_usage_line`
    /// stops matching ⇒ token totals all zero).
    pub claude_sessions: u64,
    pub claude_tokenless_sessions: u64,
    /// Feature 3 (`drift.payload.v1`): distinct "shim: missing-fields"
    /// summaries from `contract_drift` Activity events this run. Empty =
    /// no payload drift observed.
    pub contract_drift: Vec<String>,
    /// Feature 4 (`drift.read_bypass.v1`): share of reminders answered with
    /// a shell read of the same file within the bypass window, plus the
    /// remind count backing it. `None` when the advisor never reminded.
    pub bypass_rate: Option<f64>,
    pub bypass_samples: u64,
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
    /// bucket) re-fires even for the same `rule_id`. V16 drift rules key it
    /// to the observed harness VERSION instead where that's the natural
    /// re-fire boundary (a dismissed version notice must re-fire on the
    /// NEXT update, not the same one).
    pub signature: String,
    /// V16: true for drift canaries with nothing safe to auto-apply — the
    /// card renders no Apply button (`setting` is empty).
    pub warn_only: bool,
    /// V16: bespoke card action instead of the settings-write Apply.
    /// Currently only `"mark_verified"` (Feature 1's tripwire →
    /// `harness_mark_verified` IPC).
    pub action: Option<&'static str>,
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

/// Evaluate the static rule list over `sig`, returning the proposals that
/// clear their sample floor, their rate threshold, AND aren't already
/// dismissed at their current signature. Pure — no I/O, no clock. Drift
/// canaries (V16) run first with their own floors; the V14 tuning rules
/// stay behind the global `MIN_SESSIONS` cold-start floor. When a drift
/// rule proposes DISABLING the read advisor, the tuning rule that would
/// tweak it (`RULE_ADVISOR_LINES`) is suppressed — "turn it off, it's
/// broken" and "raise its floor" must never appear side by side.
pub fn evaluate(sig: &Signals) -> Vec<Proposal> {
    let mut out = drift_rules(sig);
    let advisor_disable_proposed = out
        .iter()
        .any(|p| p.rule_id == RULE_DRIFT_READ_REASON || p.rule_id == RULE_DRIFT_READ_BYPASS);

    // Global cold-start floor: no TUNING rule proposes below it, no matter
    // how extreme an individual rate looks.
    if sig.session_count < MIN_SESSIONS {
        return out;
    }
    out.extend(tuning_rules(sig, advisor_disable_proposed));
    out
}

/// Signature for the version-keyed drift rules: the SEEN Claude version, or
/// `"unknown"` before any Claude tab has run. A dismissal therefore holds
/// until the next harness update re-fires the rule.
fn version_signature(seen: &str) -> String {
    if seen.is_empty() {
        "unknown".to_string()
    } else {
        seen.to_string()
    }
}

/// The V16 drift canary rules (Features 1–4). Each carries its own sample
/// floor; none consult the global `MIN_SESSIONS` (a harness version change
/// or a malformed hook payload is a fact, not a statistic).
fn drift_rules(sig: &Signals) -> Vec<Proposal> {
    let mut out = Vec::new();

    // Feature 1 — harness version tripwire. Signature = the SEEN version,
    // so a dismissal suppresses this exact version but re-fires on the next
    // update. Fires on a never-verified install too (that's what drives the
    // initial Phase-0 verification pass).
    if !sig.claude_last_seen.is_empty() && sig.claude_last_seen != sig.claude_last_verified {
        let signature = sig.claude_last_seen.clone();
        if !is_dismissed(&sig.dismissed, RULE_DRIFT_VERSION, &signature) {
            let current = if sig.claude_last_verified.is_empty() {
                "(never verified)".to_string()
            } else {
                sig.claude_last_verified.clone()
            };
            out.push(Proposal {
                setting: String::new(),
                current,
                proposed: sig.claude_last_seen.clone(),
                rationale: format!(
                    "Claude Code is now {} but the hook contracts were last verified against \
                     {} — a harness auto-update can change hook semantics with no error \
                     anywhere (hooks fail open). Re-run the checks in MAINTENANCE.md → \
                     \"harness contracts\", then Mark verified.",
                    sig.claude_last_seen,
                    if sig.claude_last_verified.is_empty() {
                        "nothing"
                    } else {
                        sig.claude_last_verified.as_str()
                    }
                ),
                rule_id: RULE_DRIFT_VERSION,
                signature,
                warn_only: true,
                action: Some("mark_verified"),
            });
        }
    }

    // Feature 2 — drift.read_reason.v1: a ~100% remind→full-reread rate is
    // a different disease than the tuning rule's ≥50% ("files needed
    // whole"): the deny REASON isn't reaching the model, so every remind is
    // a bare refusal — the exact failure mode the V11 spec said must cancel
    // the feature.
    if sig.graph.read_advisor {
        if let Some(rate) = sig.advisor_reread_rate {
            if sig.advisor_reread_samples >= DRIFT_MIN_REMINDS && rate >= READ_REASON_HIGH {
                let signature = bucket10(rate);
                if !is_dismissed(&sig.dismissed, RULE_DRIFT_READ_REASON, &signature) {
                    out.push(Proposal {
                        setting: "graph.read_advisor".to_string(),
                        current: "true".to_string(),
                        proposed: "false".to_string(),
                        rationale: format!(
                            "{:.0}% of read-advisor reminders were immediately followed by a \
                             full Read of the same file (n={} reminders) — at ~100% the deny \
                             reason is likely not reaching the model at all (bare refusals), \
                             so every remind costs a turn and displaces nothing. Disable the \
                             advisor and re-verify the E1 contract per MAINTENANCE.md.",
                            rate * 100.0,
                            sig.advisor_reread_samples
                        ),
                        rule_id: RULE_DRIFT_READ_REASON,
                        signature,
                        warn_only: false,
                        action: None,
                    });
                }
            }
        }
    }

    // Feature 2 — drift.read_hook_silent.v1: the project keeps re-reading
    // large files (exactly what the advisor exists to catch) yet zero
    // reminders were ever recorded — the hook isn't firing (overlay
    // ignored, matcher renamed, shim broken). Warn-only. Signature = the
    // seen Claude version: fixing the hook clears it; a dismissal holds
    // until the next harness update.
    if sig.graph.read_advisor
        && sig.session_count >= DRIFT_SILENT_MIN_SESSIONS
        && sig.large_reread_pairs >= DRIFT_SILENT_MIN_REREADS
        && sig.remind_count == 0
    {
        let signature = version_signature(&sig.claude_last_seen);
        if !is_dismissed(&sig.dismissed, RULE_DRIFT_HOOK_SILENT, &signature) {
            out.push(Proposal {
                setting: String::new(),
                current: format!("{} large re-reads (est.)", sig.large_reread_pairs),
                proposed: "0 reminders".to_string(),
                rationale: format!(
                    "The read advisor is on and this project re-read {} large files across \
                     {} sessions (est.) — the exact condition it reminds on — yet not one \
                     remind reached the loopback. The PreToolUse hook is likely not firing \
                     (settings overlay ignored, matcher renamed, or shim broken). Check the \
                     hook wiring per MAINTENANCE.md → \"harness contracts\".",
                    sig.large_reread_pairs, sig.session_count
                ),
                rule_id: RULE_DRIFT_HOOK_SILENT,
                signature,
                warn_only: true,
                action: None,
            });
        }
    }

    // Feature 2 — drift.injection_unseen.v1: injection keeps writing chars
    // but essentially NOTHING injected is ever followed — distinct from the
    // raise-min-score tuning rule by magnitude (near-zero follow means the
    // block likely never reaches the model at all). Warn-only.
    if sig.graph.context_injection {
        if let Some(follow) = sig.injection_follow_rate {
            if sig.injection_follow_samples >= DRIFT_UNSEEN_MIN_INJECTIONS
                && sig.session_count >= DRIFT_UNSEEN_MIN_SESSIONS
                && follow <= INJECTION_UNSEEN_LOW
            {
                let signature = bucket10(follow);
                if !is_dismissed(&sig.dismissed, RULE_DRIFT_INJECTION_UNSEEN, &signature) {
                    out.push(Proposal {
                        setting: String::new(),
                        current: format!("{:.0}% follow rate", follow * 100.0),
                        proposed: "injected context reaching the model".to_string(),
                        rationale: format!(
                            "Context injection is on and growing, but only {:.1}% of {} \
                             injected files were ever read or edited afterwards across {} \
                             sessions — near-zero follow suggests the injected block never \
                             reaches the model at all (hook output dropped by a harness \
                             change), not that relevance is mistuned. Check the \
                             UserPromptSubmit contract per MAINTENANCE.md.",
                            follow * 100.0,
                            sig.injection_follow_samples,
                            sig.session_count
                        ),
                        rule_id: RULE_DRIFT_INJECTION_UNSEEN,
                        signature,
                        warn_only: true,
                        action: None,
                    });
                }
            }
        }
    }

    // Feature 2 — drift.usage_fields_gone.v1: Claude sessions are active
    // but every one of them stopped carrying token fields — the transcript
    // usage schema changed under the tap. Warn-only. Signature = the seen
    // Claude version (same re-fire boundary as the tripwire).
    if sig.claude_sessions >= DRIFT_MIN_TOKENLESS
        && sig.claude_tokenless_sessions == sig.claude_sessions
    {
        let signature = version_signature(&sig.claude_last_seen);
        if !is_dismissed(&sig.dismissed, RULE_DRIFT_USAGE_FIELDS, &signature) {
            out.push(Proposal {
                setting: String::new(),
                current: format!("{} Claude sessions without token fields", sig.claude_sessions),
                proposed: "usage_stat rows with token counts".to_string(),
                rationale: format!(
                    "All {} recent Claude sessions recorded zero token-bearing usage rows — \
                     the transcript's `message.usage` shape has likely changed and the Usage \
                     section is now blind (chars-only estimates). The token-efficiency \
                     counters underneath it are unaffected but the cost view can't price \
                     these sessions.",
                    sig.claude_sessions
                ),
                rule_id: RULE_DRIFT_USAGE_FIELDS,
                signature,
                warn_only: true,
                action: None,
            });
        }
    }

    // Feature 3 — drift.payload.v1: a shim reported a payload missing
    // required fields. One event is enough — the shims rate-limit
    // themselves to one report per shim per session, and a malformed
    // payload is a contract fact.
    if !sig.contract_drift.is_empty() {
        let mut shims = sig.contract_drift.clone();
        shims.sort();
        shims.dedup();
        let signature = shims.join("+");
        if !is_dismissed(&sig.dismissed, RULE_DRIFT_PAYLOAD, &signature) {
            out.push(Proposal {
                setting: String::new(),
                current: shims.join(", "),
                proposed: "hook payloads with all required fields".to_string(),
                rationale: format!(
                    "Hook shims reported payloads missing required fields this run: {}. The \
                     shims keep failing open (nothing breaks), but the harness's hook payload \
                     shape has drifted — verify the contracts per MAINTENANCE.md before \
                     trusting the features built on them.",
                    shims.join("; ")
                ),
                rule_id: RULE_DRIFT_PAYLOAD,
                signature,
                warn_only: true,
                action: None,
            });
        }
    }

    // Feature 4 — drift.read_bypass.v1: the agent routes around the advisor
    // with shell reads — same tokens spent, PLUS the remind overhead, MINUS
    // memory's read tracking. Strictly worse than no advisor: propose
    // disabling it.
    if sig.graph.read_advisor {
        if let Some(rate) = sig.bypass_rate {
            if sig.bypass_samples >= DRIFT_MIN_BYPASS_REMINDS && rate >= BYPASS_HIGH {
                let signature = bucket10(rate);
                if !is_dismissed(&sig.dismissed, RULE_DRIFT_READ_BYPASS, &signature) {
                    out.push(Proposal {
                        setting: "graph.read_advisor".to_string(),
                        current: "true".to_string(),
                        proposed: "false".to_string(),
                        rationale: format!(
                            "{:.0}% of read-advisor reminders were answered with a shell read \
                             of the same file (est., n={} reminders) — the agent is routing \
                             around the advisor, which costs the same tokens plus the remind \
                             overhead and loses memory's read tracking. Better off disabled.",
                            rate * 100.0,
                            sig.bypass_samples
                        ),
                        rule_id: RULE_DRIFT_READ_BYPASS,
                        signature,
                        warn_only: false,
                        action: None,
                    });
                }
            }
        }
    }

    out
}

/// The V14 budget-tuning rules (behind the global cold-start floor —
/// enforced by the caller). `advisor_disable_proposed` suppresses
/// `RULE_ADVISOR_LINES` when a drift rule already proposed turning the
/// advisor off.
fn tuning_rules(sig: &Signals, advisor_disable_proposed: bool) -> Vec<Proposal> {
    let mut out = Vec::new();

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
                        warn_only: false,
                        action: None,
                    });
                }
            }
        }
    }

    // Rule 2 — reminders followed by a full re-read anyway ⇒ raise
    // read_advisor_min_lines (the reminders fire on files the agent
    // genuinely needs whole). Suppressed when a V16 drift rule already
    // proposed disabling the advisor — at ~100% reread the diagnosis is
    // "broken contract", and proposing a floor tweak beside "turn it off"
    // would be incoherent.
    if let Some(rate) = sig.advisor_reread_rate {
        if !advisor_disable_proposed && sig.advisor_reread_samples >= MIN_REMINDS && rate >= REREAD_HIGH {
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
                    warn_only: false,
                    action: None,
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
                        warn_only: false,
                        action: None,
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
            ..Signals::default()
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

    // ── V16 drift canary rules ──────────────────────────────────────────

    #[test]
    fn version_tripwire_fires_below_the_global_session_floor() {
        // A version bump is a fact, not a statistic — zero sessions must
        // not gate it.
        let sig = Signals {
            claude_last_seen: "2.2.0".to_string(),
            claude_last_verified: "2.1.14".to_string(),
            session_count: 0,
            ..Signals::default()
        };
        let props = evaluate(&sig);
        assert_eq!(props.len(), 1);
        let p = &props[0];
        assert_eq!(p.rule_id, RULE_DRIFT_VERSION);
        assert!(p.warn_only);
        assert_eq!(p.action, Some("mark_verified"));
        assert_eq!(p.signature, "2.2.0");
    }

    #[test]
    fn version_tripwire_fires_on_a_never_verified_install_and_not_when_matched() {
        let sig = Signals {
            claude_last_seen: "2.2.0".to_string(),
            ..Signals::default()
        };
        assert!(evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_VERSION));

        let sig_ok = Signals {
            claude_last_seen: "2.2.0".to_string(),
            claude_last_verified: "2.2.0".to_string(),
            ..Signals::default()
        };
        assert!(!evaluate(&sig_ok).iter().any(|p| p.rule_id == RULE_DRIFT_VERSION));

        // Never-seen (no Claude tab yet): nothing to trip on.
        assert!(!evaluate(&Signals::default()).iter().any(|p| p.rule_id == RULE_DRIFT_VERSION));
    }

    #[test]
    fn version_tripwire_dismissal_is_keyed_to_the_seen_version() {
        let mut sig = Signals {
            claude_last_seen: "2.2.0".to_string(),
            claude_last_verified: "2.1.14".to_string(),
            dismissed: vec![DismissedRule {
                rule_id: RULE_DRIFT_VERSION.to_string(),
                signature: "2.2.0".to_string(),
            }],
            ..Signals::default()
        };
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_VERSION));

        // The NEXT version change re-fires despite the old dismissal.
        sig.claude_last_seen = "2.3.0".to_string();
        assert!(evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_VERSION));
    }

    /// Signals for the read-reason drift: advisor ON, ~100% reread at the
    /// drift floor (15), below the tuning rule's floor (20) — only the
    /// drift rule can speak.
    fn read_reason_signals() -> Signals {
        let mut graph = GraphSettings::default();
        graph.read_advisor = true;
        Signals {
            advisor_reread_rate: Some(0.95),
            advisor_reread_samples: DRIFT_MIN_REMINDS,
            session_count: MIN_SESSIONS,
            graph,
            ..Signals::default()
        }
    }

    #[test]
    fn read_reason_drift_proposes_disabling_the_advisor() {
        let props = evaluate(&read_reason_signals());
        let p = props.iter().find(|p| p.rule_id == RULE_DRIFT_READ_REASON).expect("fires");
        assert_eq!(p.setting, "graph.read_advisor");
        assert_eq!(p.proposed, "false");
        assert!(!p.warn_only);
    }

    #[test]
    fn read_reason_drift_needs_the_advisor_on_and_its_own_floors() {
        let mut sig = read_reason_signals();
        sig.graph.read_advisor = false;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_READ_REASON));

        let mut sig = read_reason_signals();
        sig.advisor_reread_samples = DRIFT_MIN_REMINDS - 1;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_READ_REASON));

        let mut sig = read_reason_signals();
        sig.advisor_reread_rate = Some(0.8); // high for tuning, below drift's 0.9
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_READ_REASON));
    }

    #[test]
    fn read_reason_drift_takes_precedence_over_the_min_lines_tuning_rule() {
        // Both rules' floors and thresholds satisfied at once (samples ≥ 20,
        // rate 1.0): only the drift diagnosis may surface.
        let mut sig = read_reason_signals();
        sig.advisor_reread_rate = Some(1.0);
        sig.advisor_reread_samples = MIN_REMINDS; // 20 ≥ both floors
        let ids: Vec<&str> = evaluate(&sig).iter().map(|p| p.rule_id).collect();
        assert!(ids.contains(&RULE_DRIFT_READ_REASON));
        assert!(!ids.contains(&RULE_ADVISOR_LINES), "tuning rule must be suppressed");
    }

    #[test]
    fn hook_silent_drift_needs_rereads_sessions_and_exactly_zero_reminds() {
        let mut graph = GraphSettings::default();
        graph.read_advisor = true;
        let base = Signals {
            session_count: DRIFT_SILENT_MIN_SESSIONS,
            large_reread_pairs: DRIFT_SILENT_MIN_REREADS,
            remind_count: 0,
            claude_last_seen: "2.2.0".to_string(),
            graph,
            ..Signals::default()
        };
        let props = evaluate(&base);
        let p = props.iter().find(|p| p.rule_id == RULE_DRIFT_HOOK_SILENT).expect("fires");
        assert!(p.warn_only);
        assert!(p.setting.is_empty());
        assert_eq!(p.signature, "2.2.0"); // re-fires per harness version

        let mut sig = base.clone();
        sig.remind_count = 1; // one remind reached the loopback ⇒ hook alive
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_HOOK_SILENT));

        let mut sig = base.clone();
        sig.large_reread_pairs = DRIFT_SILENT_MIN_REREADS - 1;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_HOOK_SILENT));

        let mut sig = base.clone();
        sig.session_count = DRIFT_SILENT_MIN_SESSIONS - 1;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_HOOK_SILENT));

        let mut sig = base;
        sig.graph.read_advisor = false;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_HOOK_SILENT));
    }

    #[test]
    fn injection_unseen_drift_fires_only_at_near_zero_follow() {
        let mut graph = GraphSettings::default();
        graph.context_injection = true;
        let base = Signals {
            injection_follow_rate: Some(0.0),
            injection_follow_samples: DRIFT_UNSEEN_MIN_INJECTIONS,
            session_count: DRIFT_UNSEEN_MIN_SESSIONS,
            graph,
            ..Signals::default()
        };
        let props = evaluate(&base);
        let p = props.iter().find(|p| p.rule_id == RULE_DRIFT_INJECTION_UNSEEN).expect("fires");
        assert!(p.warn_only);

        // 10% follow is unhealthy but NOT "never reaches the model" — the
        // tuning rule's territory, not the drift rule's.
        let mut sig = base.clone();
        sig.injection_follow_rate = Some(0.10);
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_INJECTION_UNSEEN));

        let mut sig = base;
        sig.graph.context_injection = false;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_INJECTION_UNSEEN));
    }

    #[test]
    fn usage_fields_gone_fires_only_when_every_claude_session_is_tokenless() {
        let base = Signals {
            claude_sessions: 3,
            claude_tokenless_sessions: 3,
            claude_last_seen: "2.2.0".to_string(),
            ..Signals::default()
        };
        assert!(evaluate(&base).iter().any(|p| p.rule_id == RULE_DRIFT_USAGE_FIELDS));

        // One healthy session ⇒ the schema didn't change, that session is
        // just odd.
        let mut sig = base.clone();
        sig.claude_tokenless_sessions = 2;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_USAGE_FIELDS));

        // Below the floor a single tokenless session could be a fluke.
        let mut sig = base;
        sig.claude_sessions = DRIFT_MIN_TOKENLESS - 1;
        sig.claude_tokenless_sessions = DRIFT_MIN_TOKENLESS - 1;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_USAGE_FIELDS));
    }

    #[test]
    fn payload_drift_fires_on_any_contract_drift_event() {
        let sig = Signals {
            contract_drift: vec!["read_hook: session_id".to_string()],
            ..Signals::default()
        };
        let props = evaluate(&sig);
        let p = props.iter().find(|p| p.rule_id == RULE_DRIFT_PAYLOAD).expect("fires");
        assert!(p.warn_only);
        assert!(p.rationale.contains("read_hook: session_id"));

        // Signature is the deduped, sorted shim list — a second identical
        // report doesn't change it (dismissal holds), a NEW shim does.
        let sig2 = Signals {
            contract_drift: vec![
                "read_hook: session_id".to_string(),
                "compact_hook: cwd".to_string(),
                "read_hook: session_id".to_string(),
            ],
            ..Signals::default()
        };
        let p2 = evaluate(&sig2);
        let p2 = p2.iter().find(|p| p.rule_id == RULE_DRIFT_PAYLOAD).unwrap();
        assert_eq!(p2.signature, "compact_hook: cwd+read_hook: session_id");
    }

    #[test]
    fn read_bypass_drift_proposes_disabling_at_the_threshold() {
        let mut graph = GraphSettings::default();
        graph.read_advisor = true;
        let base = Signals {
            bypass_rate: Some(0.5),
            bypass_samples: DRIFT_MIN_BYPASS_REMINDS,
            graph,
            ..Signals::default()
        };
        let p = evaluate(&base);
        let p = p.iter().find(|p| p.rule_id == RULE_DRIFT_READ_BYPASS).expect("fires");
        assert_eq!(p.setting, "graph.read_advisor");
        assert_eq!(p.proposed, "false");

        let mut sig = base.clone();
        sig.bypass_rate = Some(0.3); // below BYPASS_HIGH
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_READ_BYPASS));

        let mut sig = base;
        sig.bypass_samples = DRIFT_MIN_BYPASS_REMINDS - 1;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_DRIFT_READ_BYPASS));
    }

    #[test]
    fn drift_disable_proposals_carry_a_real_settings_write() {
        // The frontend Apply switch needs `graph.read_advisor` to be a real
        // assignable bool field — same guard as
        // `proposal_setting_names_match_real_graphsettings_fields`.
        let mut sig = read_reason_signals();
        sig.advisor_reread_rate = Some(1.0);
        let props = evaluate(&sig);
        let p = props.iter().find(|p| p.rule_id == RULE_DRIFT_READ_REASON).unwrap();
        assert_eq!(p.setting, "graph.read_advisor");
        let val: bool = p.proposed.parse().expect("proposed must be a bool string");
        let mut g = GraphSettings::default();
        g.read_advisor = true;
        g.read_advisor = val;
        assert!(!g.read_advisor);
    }
}

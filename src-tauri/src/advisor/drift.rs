//! The V16 drift canaries — is the harness contract still holding?
//!
//! Two halves, both split out of the old 744-line `drift_rules` by V42 R7:
//! the per-harness loop (version tripwire, failed auto-verify, usage-field
//! and sub-agent drift) and the project-scoped hook rules (read reason, hook
//! silence, unseen injection, payload drift, read bypass). Every one of them
//! carries its own sample floor and none consults the global
//! [`MIN_SESSIONS`] — a moved contract is a fact, not a statistic.
//!
//! **Card order.** [`read_bypass`] is a separate entry point ONLY because the
//! detection block ([`super::detection`]) was appended into the middle of the
//! old function, ahead of it. The panel renders the proposal vector in order,
//! so folding it back into [`rules`] would reshuffle a user's cards; see
//! [`super::evaluate`].

use super::*;

/// `drift.read_reason.v1`: reminders observed before the ~100%-reread check
/// can speak. Lower than the tuning rule's `MIN_REMINDS` (20) — this is a
/// breakage detector, and waiting longer just burns more bare refusals.
pub(super) const DRIFT_MIN_REMINDS: u64 = 15;
/// `drift.read_reason.v1`: a remind→full-reread rate at or above this is no
/// longer "the files are needed whole" (the tuning rule's ≥50% diagnosis) —
/// it's "the deny *reason* isn't reaching the model at all".
pub(super) const READ_REASON_HIGH: f64 = 0.9;
/// `drift.read_hook_silent.v1`: sessions observed before "zero reminds" is
/// evidence of a dead hook rather than a quiet project.
pub(super) const DRIFT_SILENT_MIN_SESSIONS: u64 = 3;
/// `drift.read_hook_silent.v1`: re-reads of large files that SHOULD have
/// drawn a reminder before silence is suspicious.
pub(super) const DRIFT_SILENT_MIN_REREADS: u64 = 10;
/// `drift.injection_unseen.v1`: injected-file floor (distinct from the
/// tuning rules' 200 — near-zero follow is detectable much earlier).
pub(super) const DRIFT_UNSEEN_MIN_INJECTIONS: u64 = 30;
/// `drift.injection_unseen.v1`: session floor.
pub(super) const DRIFT_UNSEEN_MIN_SESSIONS: u64 = 5;
/// `drift.injection_unseen.v1`: a follow rate at or below this is "the
/// block likely never reaches the model" (vs. the tuning rule's "the floor
/// is too low" at ≤30% follow).
pub(super) const INJECTION_UNSEEN_LOW: f64 = 0.02;
/// `drift.read_bypass.v1`: reminders observed before the bypass share can
/// speak (V16 open item: placeholder — tune on real bypass rates).
pub(super) const DRIFT_MIN_BYPASS_REMINDS: u64 = 10;
/// `drift.read_bypass.v1`: share of reminders answered with a shell read
/// (`cat`/`Get-Content` via Bash) at or above this proposes disabling the
/// advisor (V16 open item: placeholder threshold).
pub(super) const BYPASS_HIGH: f64 = 0.4;
/// `drift.usage_fields_gone.v1`: Claude sessions without token fields
/// before the rule speaks (one could be a fluke/crashed session).
pub(super) const DRIFT_MIN_TOKENLESS: u64 = 2;

/// Compose the ONE consolidated drift notice (V35 Phase E).
///
/// The V16 detector has already fired by the time this is called — its
/// threshold, its sample floor and its warn-only/Apply choice are the caller's
/// and are unchanged. This owns only the envelope: the notice id, the
/// three-part signature (see [`RULE_DRIFT_CAPABILITY`]) and the fix pointer.
///
/// The body names the capability, the evidence, and the registry row's
/// `wired_in` paths — matrix draft § 3.2. `wired_in` is the useful half: it is
/// the list of modules that break when this contract moves, which is the
/// question a user reading a drift card actually has and the one thing the
/// prose rationale never carried.
/// `warn_only` is DERIVED from `setting` rather than passed: the two were
/// always the same fact (`Proposal::warn_only`'s own doc — "a drift canary with
/// nothing safe to auto-apply renders no Apply button, `setting` is empty"),
/// and a notice that named a setting while claiming to be warn-only would be a
/// card with an Apply button the frontend refuses to draw.
fn capability_notice(
    cap: &'static crate::harness::contract::Capability,
    evidence: &'static str,
    inner_signature: &str,
    setting: &str,
    current: String,
    proposed: &str,
    rationale: String,
) -> Proposal {
    // V40 Phase A: the label is the descriptor's. Core rendering a harness's
    // NAME by matching on its identity is the shape locked decision 10(a)
    // forbids — and it is how a third harness would have read as "OpenCode".
    let harness = cap.harness.label();
    Proposal {
        setting: setting.to_string(),
        current,
        proposed: proposed.to_string(),
        rationale: format!(
            "{rationale}\n\nCapability `{}` — {harness}, seam tier {:?}, evidence `{evidence}`. \
             What breaks if this contract has moved: {}.",
            cap.id,
            cap.tier,
            cap.wired_in.join(", ")
        ),
        rule_id: RULE_DRIFT_CAPABILITY,
        signature: format!("{}:{evidence}:{inner_signature}", cap.id),
        warn_only: setting.is_empty(),
        action: None,
        capability: Some(cap.id),
        // The capability's own harness — so a notice about an OpenCode row
        // carries OpenCode, whichever rule raised it.
        harness: cap.harness.id(),
    }
}

/// The single registry row a drift rule is evidence about, or `None` when the
/// matrix names none.
///
/// Every consolidated rule below resolves to exactly one row today; the
/// multi-row case (`drift.payload.v1`, named by four) is handled by its own
/// per-shim attribution rather than through here. A rule the matrix stopped
/// naming would silently stop raising a notice, so
/// `contract::tests::every_declared_drift_rule_resolves_back_to_its_rows` and
/// [`tests::every_consolidated_drift_rule_has_a_capability`] are what keep this
/// from returning `None` in production.
fn sole_capability(rule: &'static str) -> Option<&'static crate::harness::contract::Capability> {
    let rows = crate::harness::contract::capabilities_for_rule(rule);
    match rows.len() {
        1 => Some(rows[0]),
        _ => None,
    }
}

/// A rule's neutral rationale, plus the fix pointer the capability's own harness
/// supplies (V40 Phase C, locked decision 23).
///
/// The prose that used to name `PreToolUse`, `UserPromptSubmit`,
/// `message.usage` and `subagents/*.jsonl` from inside these rules is
/// `Capability::drift_hint()` now. A capability whose harness declares no hint
/// renders the neutral sentence and stops — which is the fail-quiet direction,
/// because a pointer at a mechanism the harness does not have is worse than no
/// pointer at all.
fn with_hint(cap: &'static crate::harness::contract::Capability, body: String) -> String {
    match cap.drift_hint() {
        Some(hint) => format!("{body} {hint}"),
        None => body,
    }
}

/// The version `harness` was last seen at, for a rule whose EVIDENCE is
/// project-scoped rather than per-harness (the two hook-silence rules): they
/// fire from one project's counters, and the capability row says whose
/// mechanism is implicated, so the re-fire boundary is that harness's version.
fn seen_for(sig: &Signals, harness: crate::harness::HarnessId) -> String {
    sig.harness
        .get(&harness)
        .map(|d| d.last_seen.clone())
        .unwrap_or_default()
}

/// Signature for the version-keyed drift rules: **the harness and** the version
/// it was last seen at, or `"unknown"` before any of its tabs has run. A
/// dismissal therefore holds until that harness's next update re-fires the rule.
///
/// **V40 Phase C, locked decision 23 — the harness is part of the key now.** It
/// used to be the bare version string, which was sound only while exactly one
/// harness could raise these rules. With every rule evaluating per harness, two
/// harnesses on the same version string would share one dismissal: silencing
/// Claude's version notice would silence OpenCode's, which is the precise
/// failure the signature exists to prevent one rule over. The cost is one-time
/// and visible: a dismissal recorded before this change no longer matches, so a
/// dismissed version notice re-fires once.
///
/// The dismissal signature a version-keyed notice is stored under:
/// `"<harness token>:<version>"`, with `"unknown"` for a harness that has never
/// been observed.
///
/// **A PERSISTED wire form**, not an internal join key: it lands in
/// `Settings::advisor_dismissed[].signature`, so changing the separator, the
/// order or the empty-version sentinel resurrects every dismissed version
/// notice on the next launch. V40 Phase C changed it (it was the bare version,
/// app-wide) and that re-fire is a recorded, intended one-off — the next change
/// would not be. Pinned by a literal in
/// `tests::the_version_notice_signature_is_the_stored_wire_form` (V40 review
/// finding W-2, parity lens), which is the only thing standing between a tidy
/// refactor here and every user's dismissals coming back.
fn version_signature(harness: crate::harness::HarnessId, seen: &str) -> String {
    let seen = if seen.is_empty() { "unknown" } else { seen };
    format!("{}:{seen}", harness.token())
}

/// The V16 drift canary rules (Features 1–4). Each carries its own sample
/// floor; none consult the global `MIN_SESSIONS` (a harness version change
/// or a malformed hook payload is a fact, not a statistic).
///
/// Everything except `drift.read_bypass.v1`, which [`read_bypass`] raises
/// after the detection block so the card order is unchanged.
pub(super) fn rules(sig: &Signals) -> Vec<Proposal> {
    let mut out = Vec::new();

    // ── the per-harness half (V40 Phase C, locked decision 23) ─────────────
    //
    // Everything in this loop used to read `sig.claude_*` and therefore spoke
    // for exactly one product. `drift.version.v1` is the sharpest case: its
    // condition was `claude_last_seen != claude_last_verified`, so OpenCode had
    // no version-drift path at all and could auto-update straight through a
    // contract change with nothing anywhere saying so.
    for (harness, d) in &sig.harness {
        // V35 Phase F — one notice per capability that auto-verify found BROKEN
        // on the currently-installed build. Raised before the tripwire because
        // it is what replaces it: a fact-trigger (no sample floor — a failed
        // canary is a fact, not a statistic), naming the capability, the layer
        // that saw it and the modules that break, instead of "the version moved,
        // go check by hand".
        for failure in crate::harness::verify::notifiable_failures(
            d.auto_verify.as_ref(),
            &d.last_seen,
            &d.last_verified,
        ) {
            // A capability the registry no longer carries: skip rather than
            // invent a card with no `wired_in` pointer. Unreachable while the
            // record is written by this build (the ids come from the registry),
            // and the honest answer for a hand-edited or newer-build record.
            let Some(cap) = crate::harness::contract::get(&failure.capability) else {
                continue;
            };
            let p = capability_notice(
                cap,
                crate::harness::verify::evidence_const(&failure.evidence),
                // Keyed by the version verified against: a dismissal holds for
                // this build and re-fires when the next update reproduces the
                // failure, the same re-fire boundary the tripwire it replaces
                // had — and, since Phase C, keyed by the harness too, so two
                // harnesses on one version string cannot share a dismissal.
                &version_signature(*harness, &d.last_seen),
                "",
                failure.detail.clone(),
                "the recorded contract holding again",
                format!(
                    "{} updated to {} and the automatic contract check FAILED for this                      capability, so the version was NOT auto-verified: {}

This ran by                      itself when the update was observed — no session had to degrade first.                      Fix the reader (or re-record the shape) and the next check passes                      silently; if you have verified this build by hand, use Mark verified on                      the harness card.",
                    harness.label(),
                    d.last_seen,
                    failure.detail
                ),
            );
            if !is_dismissed(&sig.dismissed, p.rule_id, &p.signature) {
                out.push(p);
            }
        }

        // Feature 1 — drift.version.v1: the harness moved and the contracts
        // have not been re-verified against it.
        //
        // V35 Phase F demoted it to the **cannot-verify fallback**. The routine
        // case — an auto-update that broke nothing — no longer reaches here at
        // all: auto-verify advances `last_verified` on its own, so the versions
        // match and the condition is false. What remains are the cases nothing
        // else can speak for: auto-verify has not run yet (a fresh install, a
        // version observed while the check was in flight), it errored, or it
        // passed and the advance did not land. When it ran and FOUND failures,
        // the loop above is already naming them and a second card would be the
        // noise this phase exists to remove.
        if !d.last_seen.is_empty()
            && d.last_seen != d.last_verified
            && !crate::harness::verify::tripwire_superseded(d.auto_verify.as_ref(), &d.last_seen)
        {
            let signature = version_signature(*harness, &d.last_seen);
            if !is_dismissed(&sig.dismissed, RULE_DRIFT_VERSION, &signature) {
                let current = if d.last_verified.is_empty() {
                    "(never verified)".to_string()
                } else {
                    d.last_verified.clone()
                };
                out.push(Proposal {
                    setting: String::new(),
                    current,
                    proposed: d.last_seen.clone(),
                    rationale: format!(
                        "{} is now {} but the hook contracts were last verified against {} — a \
                         harness auto-update can change hook semantics with no error anywhere \
                         (hooks fail open). Re-run the checks in MAINTENANCE.md → \"harness \
                         contracts\", then Mark verified.",
                        harness.label(),
                        d.last_seen,
                        if d.last_verified.is_empty() {
                            "nothing"
                        } else {
                            d.last_verified.as_str()
                        }
                    ),
                    rule_id: RULE_DRIFT_VERSION,
                    signature,
                    warn_only: true,
                    action: Some("mark_verified"),
                    // Not capability-scoped, and not consolidated: see the const.
                    capability: None,
                    harness: harness.id(),
                });
            }
        }

        // Feature 2 — drift.usage_fields_gone.v1: this harness's sessions are
        // active but every one of them stopped carrying token fields — the
        // usage payload changed under the tap. Warn-only. Signature = the seen
        // version (same re-fire boundary as the tripwire).
        if d.sessions >= DRIFT_MIN_TOKENLESS && d.tokenless_sessions == d.sessions {
            if let Some(cap) =
                crate::harness::contract::capability_for_rule(RULE_DRIFT_USAGE_FIELDS, *harness)
            {
                let inner = version_signature(*harness, &d.last_seen);
                let p = capability_notice(
                    cap,
                    RULE_DRIFT_USAGE_FIELDS,
                    &inner,
                    "",
                    format!("{} {} sessions without token fields", d.sessions, harness.label()),
                    "usage_stat rows with token counts",
                    with_hint(
                        cap,
                        format!(
                            "All {} recent {} sessions recorded zero token-bearing usage rows — \
                             the usage payload has likely changed and the Usage section is now \
                             blind (chars-only estimates). The token-efficiency counters \
                             underneath it are unaffected but the cost view can't price these \
                             sessions.",
                            d.sessions,
                            harness.label()
                        ),
                    ),
                );
                if !is_dismissed(&sig.dismissed, p.rule_id, &p.signature) {
                    out.push(p);
                }
            }
        }

        // V17.1 — drift.subagent_transcripts.v1: this harness's tap reported
        // that sub-agent traffic is visible in none of the places it knows (or
        // the launcher tool was renamed). One event is enough — the tap
        // rate-limits itself to one report per session, and a moved contract is
        // a fact. Signature = the seen version, so a dismissal holds until the
        // next harness update (same boundary as the version tripwire — this
        // drift IS a harness-update symptom).
        if !d.subagent_drift.is_empty() {
            let mut what = d.subagent_drift.clone();
            what.sort();
            what.dedup();
            if let Some(cap) =
                crate::harness::contract::capability_for_rule(RULE_DRIFT_SUBAGENT, *harness)
            {
                let inner = version_signature(*harness, &d.last_seen);
                let p = capability_notice(
                    cap,
                    RULE_DRIFT_SUBAGENT,
                    &inner,
                    "",
                    what.join(", "),
                    "sub-agent transcripts tailed (usage + agents-active tracked)",
                    with_hint(
                        cap,
                        format!(
                            "The {} tap reported sub-agent contract drift this run: {}. Until \
                             the tail is re-pointed, sub-agent token spend may be missing from \
                             the Usage section and/or the agents-active avatar hold may be dead.",
                            harness.label(),
                            what.join("; ")
                        ),
                    ),
                );
                if !is_dismissed(&sig.dismissed, p.rule_id, &p.signature) {
                    out.push(p);
                }
            }
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
                if let Some(cap) = sole_capability(RULE_DRIFT_READ_REASON) {
                    let inner = bucket10(rate);
                    let p = capability_notice(
                        cap,
                        RULE_DRIFT_READ_REASON,
                        &inner,
                        "graph.read_advisor",
                        "true".to_string(),
                        "false",
                        format!(
                            "{:.0}% of read-advisor reminders were immediately followed by a \
                             full Read of the same file (n={} reminders) — at ~100% the deny \
                             reason is likely not reaching the model at all (bare refusals), \
                             so every remind costs a turn and displaces nothing. Disable the \
                             advisor and re-verify the E1 contract per MAINTENANCE.md.",
                            rate * 100.0,
                            sig.advisor_reread_samples
                        ),
                    );
                    if !is_dismissed(&sig.dismissed, p.rule_id, &p.signature) {
                        out.push(p);
                    }
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
        if let Some(cap) = sole_capability(RULE_DRIFT_HOOK_SILENT) {
            let inner = version_signature(cap.harness, &seen_for(sig, cap.harness));
            let p = capability_notice(
                cap,
                RULE_DRIFT_HOOK_SILENT,
                &inner,
                "",
                format!("{} large re-reads (est.)", sig.large_reread_pairs),
                "0 reminders",
                with_hint(
                    cap,
                    format!(
                        "The read advisor is on and this project re-read {} large files across \
                         {} sessions (est.) — the exact condition it reminds on — yet not one \
                         remind reached the loopback. The pre-tool hook is likely not firing \
                         (settings overlay ignored, matcher renamed, or artifact stale). Check \
                         the hook wiring per MAINTENANCE.md → \"harness contracts\".",
                        sig.large_reread_pairs, sig.session_count
                    ),
                ),
            );
            if !is_dismissed(&sig.dismissed, p.rule_id, &p.signature) {
                out.push(p);
            }
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
                if let Some(cap) = sole_capability(RULE_DRIFT_INJECTION_UNSEEN) {
                    let inner = bucket10(follow);
                    let p = capability_notice(
                        cap,
                        RULE_DRIFT_INJECTION_UNSEEN,
                        &inner,
                        "",
                        format!("{:.0}% follow rate", follow * 100.0),
                        "injected context reaching the model",
                        with_hint(
                            cap,
                            format!(
                                "Context injection is on and growing, but only {:.1}% of {} \
                                 injected files were ever read or edited afterwards across {} \
                                 sessions — near-zero follow suggests the injected block never \
                                 reaches the model at all (hook output dropped by a harness \
                                 change), not that relevance is mistuned.",
                                follow * 100.0,
                                sig.injection_follow_samples,
                                sig.session_count
                            ),
                        ),
                    );
                    if !is_dismissed(&sig.dismissed, p.rule_id, &p.signature) {
                        out.push(p);
                    }
                }
            }
        }
    }

    // Feature 3 — payload drift: a shim reported a payload missing required
    // fields. One event is enough — the shims rate-limit themselves to one
    // report per shim per session, and a malformed payload is a contract fact.
    //
    // V35 Phase E: this is the one rule FOUR registry rows name, so it is also
    // the one that could have lost precision in consolidation. It does not.
    // Each report is attributed to the capability whose shim sent it —
    // `loopback::contract_drift_row` writes the target as
    // `"<shim>: <missing fields>"`, and `contract::capability_for_payload_shim`
    // resolves the leading token through the `wired_in` column — so a
    // `read_hook` payload drift now names `claude.hook.pretooluse_deny` and its
    // consumers instead of listing four shims in one undifferentiated card,
    // and dismissing it no longer silences the other three.
    //
    // Reports from a shim the matrix does not cover (`taint_beacon`,
    // `checkpoint_beacon`, a forged name folded into the loopback's
    // `(unrecognized shim)` bucket) keep the un-consolidated `drift.payload.v1`
    // channel below rather than being pinned on a capability that did not
    // report them.
    if !sig.contract_drift.is_empty() {
        let mut reports = sig.contract_drift.clone();
        reports.sort();
        reports.dedup();

        // Keyed by capability id (not by the row) so the notice order is
        // stable and independent of registry declaration order.
        type Reports = (&'static crate::harness::contract::Capability, Vec<String>);
        let mut by_capability: std::collections::BTreeMap<&'static str, Reports> =
            std::collections::BTreeMap::new();
        let mut unattributed: Vec<String> = Vec::new();
        for report in reports {
            let shim = report.split(':').next().unwrap_or("").trim();
            match crate::harness::contract::capability_for_payload_shim(shim) {
                Some(cap) => by_capability
                    .entry(cap.id)
                    .or_insert_with(|| (cap, Vec::new()))
                    .1
                    .push(report),
                None => unattributed.push(report),
            }
        }

        for (_, (cap, what)) in by_capability {
            // Inner signature = THIS capability's reports (shim + the missing
            // field list, as recorded), so a dismissal holds for the drift the
            // user actually looked at and re-fires when a different field goes
            // missing — the same precision the pre-Phase-E signature carried
            // across all shims at once.
            let inner = what.join("+");
            let p = capability_notice(
                cap,
                RULE_DRIFT_PAYLOAD,
                &inner,
                "",
                what.join(", "),
                "hook payloads with all required fields",
                format!(
                    "This capability's hook shim reported payloads missing required fields this \
                     run: {}. The shim keeps failing open (nothing breaks), but the harness's \
                     hook payload shape has drifted — verify the contract per MAINTENANCE.md \
                     before trusting the features built on it.",
                    what.join("; ")
                ),
            );
            if !is_dismissed(&sig.dismissed, p.rule_id, &p.signature) {
                out.push(p);
            }
        }

        if !unattributed.is_empty() {
            let signature = unattributed.join("+");
            if !is_dismissed(&sig.dismissed, RULE_DRIFT_PAYLOAD, &signature) {
                out.push(Proposal {
                    setting: String::new(),
                    current: unattributed.join(", "),
                    proposed: "hook payloads with all required fields".to_string(),
                    rationale: format!(
                        "Hook shims reported payloads missing required fields this run: {}. The \
                         shims keep failing open (nothing breaks), but the harness's hook \
                         payload shape has drifted — verify the contracts per MAINTENANCE.md \
                         before trusting the features built on them.\n\nThese reports are NOT \
                         attributed to a capability: no row in the harness capability matrix \
                         names the shim that sent them, so the matrix cannot say which \
                         consumers break. That is itself worth knowing — either the shim \
                         belongs in the matrix, or the name is not one cImp ships.",
                        unattributed.join("; ")
                    ),
                    rule_id: RULE_DRIFT_PAYLOAD,
                    signature,
                    warn_only: true,
                    action: None,
                    capability: None,
                    harness: None,
                });
            }
        }
    }
    out
}

/// `drift.read_bypass.v1` on its own (at most one proposal), because the
/// V32 detection block sits between it and the rules above and the panel
/// renders the vector in order. A `Vec` rather than an `Option` so the body
/// is the one that was in `drift_rules`, unchanged.
pub(super) fn read_bypass(sig: &Signals) -> Vec<Proposal> {
    let mut out = Vec::new();

    // Feature 4 — drift.read_bypass.v1: the agent routes around the advisor
    // with shell reads — same tokens spent, PLUS the remind overhead, MINUS
    // memory's read tracking. Strictly worse than no advisor: propose
    // disabling it.
    if sig.graph.read_advisor {
        if let Some(rate) = sig.bypass_rate {
            if sig.bypass_samples >= DRIFT_MIN_BYPASS_REMINDS && rate >= BYPASS_HIGH {
                if let Some(cap) = sole_capability(RULE_DRIFT_READ_BYPASS) {
                    let inner = bucket10(rate);
                    let p = capability_notice(
                        cap,
                        RULE_DRIFT_READ_BYPASS,
                        &inner,
                        "graph.read_advisor",
                        "true".to_string(),
                        "false",
                        format!(
                            "{:.0}% of read-advisor reminders were answered with a shell read \
                             of the same file (est., n={} reminders) — the agent is routing \
                             around the advisor, which costs the same tokens plus the remind \
                             overhead and loses memory's read tracking. With V17 shell \
                             interception live, whole-file reads (`cat`, `Get-Content`) are \
                             already caught, so a persistently high rate points at RESIDUAL \
                             escape routes (`sed -n`, `head`, `tail`, redirections) the strict \
                             parser can't intercept — better off disabled.",
                            rate * 100.0,
                            sig.bypass_samples
                        ),
                    );
                    if !is_dismissed(&sig.dismissed, p.rule_id, &p.signature) {
                        out.push(p);
                    }
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`HarnessDriftSignals`] with one entry: the DEFAULT harness's.
    ///
    /// V40 Phase C. Every drift test below was written when `Signals` carried
    /// six `claude_*` scalars, and each one is still about ONE harness's drift —
    /// what changed is that the rules now loop, so the fixture has to say which
    /// harness it is describing. `evaluate_for_two` covers the loop itself.
    fn one_harness(d: DriftSignals) -> HarnessDriftSignals {
        [(crate::harness::DEFAULT_HARNESS, d)].into_iter().collect()
    }

    /// The DEFAULT harness's drift row, created empty if absent — the mutable
    /// twin of [`one_harness`] for the tests that build a `Signals` and then
    /// adjust one field.
    fn drift_mut(sig: &mut Signals) -> &mut DriftSignals {
        sig.harness
            .entry(crate::harness::DEFAULT_HARNESS)
            .or_default()
    }

    // ── V16 drift canary rules ──────────────────────────────────────────

    /// Whether `p` is the consolidated `drift.capability.v1` notice raised on
    /// `evidence` (V35 Phase E).
    ///
    /// The V16 detectors kept their ids — as the EVIDENCE half of the notice
    /// signature — so the tests below still name the exact rule they exercise;
    /// they just no longer find it in `rule_id`. The middle signature field is
    /// unambiguous because neither a capability id nor a rule id contains a
    /// colon, while the third field (the detector's own re-fire key) may.
    fn is_drift(p: &Proposal, evidence: &str) -> bool {
        p.rule_id == RULE_DRIFT_CAPABILITY && p.signature.split(':').nth(1) == Some(evidence)
    }

    /// The consolidated signature a detector's notice carries:
    /// `<capability>:<evidence>:<the detector's own key>`.
    fn drift_signature(capability: &str, evidence: &str, inner: &str) -> String {
        format!("{capability}:{evidence}:{inner}")
    }

    /// The version half of a drift signature, for the DEFAULT harness.
    ///
    /// V40 Phase C: the version-keyed rules key on `(harness, version)` now, so
    /// a test that spells the bare version string is asserting the OLD format.
    /// One helper rather than a literal per site, so the two harnesses' notices
    /// cannot come to share a dismissal without this turning red.
    fn version_key(seen: &str) -> String {
        version_signature(crate::harness::DEFAULT_HARNESS, seen)
    }

    /// **The dismissal signature's wire form, as a literal** (V40 review
    /// finding W-2, parity lens).
    ///
    /// `version_signature` builds a string that is STORED in
    /// `Settings::advisor_dismissed`, and every test that touched it went
    /// through `version_key`, which calls the function under test — so the
    /// separator, the field order and the empty-version sentinel were pinned by
    /// nothing at all. Changing any of them resurrects every dismissed version
    /// notice on the next launch, for every user, silently.
    #[test]
    fn the_version_notice_signature_is_the_stored_wire_form() {
        let default = crate::harness::DEFAULT_HARNESS;
        // The shipped default harness, written out. A rename of the token is a
        // migration, not an edit.
        assert_eq!(version_signature(default, "2.1.232"), "claude:2.1.232");
        // An unobserved harness gets the sentinel, not an empty tail that would
        // collide with a real signature ending in `:`.
        assert_eq!(version_signature(default, ""), "claude:unknown");
        // …and the SHAPE holds for every registered harness: token, one colon,
        // version. Two harnesses on one version must never share a dismissal.
        let mut seen = std::collections::BTreeSet::new();
        for h in crate::harness::registry::all() {
            let sig = version_signature(h, "9.9.9");
            assert_eq!(sig, format!("{}:9.9.9", h.token()));
            assert_eq!(sig.matches(':').count(), 1, "{sig}");
            assert!(seen.insert(sig.clone()), "two harnesses share `{sig}`");
        }
    }

    /// Every consolidated detector resolves to a capability the notice can be
    /// about. A rule the matrix stopped naming would keep computing and raise
    /// nothing — the exact "signal with no consumer" failure V35 exists to
    /// prevent, and the one that would be invisible from inside these tests
    /// (they would simply see no proposal and could not tell "healthy" from
    /// "unwired"). Named explicitly rather than derived from the registry, so
    /// that dropping a `drift_rule` link fails here with the rule's name.
    #[test]
    fn every_consolidated_drift_rule_has_a_capability() {
        for (rule, expect) in [
            (RULE_DRIFT_READ_REASON, "claude.hook.pretooluse_deny"),
            (RULE_DRIFT_HOOK_SILENT, "claude.hook.pretooluse_deny"),
            (RULE_DRIFT_READ_BYPASS, "claude.hook.pretooluse_deny"),
            (RULE_DRIFT_INJECTION_UNSEEN, "claude.hook.user_prompt_submit"),
            (RULE_DRIFT_USAGE_FIELDS, "claude.transcript.usage"),
            (RULE_DRIFT_SUBAGENT, "claude.transcript.subagents"),
        ] {
            let cap = sole_capability(rule)
                .unwrap_or_else(|| panic!("`{rule}` names no single capability — no notice would \
                                           ever be raised for it"));
            assert_eq!(cap.id, expect, "`{rule}` moved to a different capability");
        }
        // `drift.payload.v1` is deliberately NOT sole-resolvable: four rows
        // name it, and each report is attributed by shim instead.
        assert!(sole_capability(RULE_DRIFT_PAYLOAD).is_none());
        // The tripwire is not capability-scoped at all (Phase F reworks it).
        assert!(sole_capability(RULE_DRIFT_VERSION).is_none());
    }

    /// Every consolidated notice names its capability, its evidence and the
    /// modules that break — matrix draft § 3.2. The fix pointer is the half
    /// the prose rationale never carried, and a card that says "the contract
    /// moved" without saying what breaks is the investigation this milestone
    /// exists to replace with a diff.
    #[test]
    fn a_capability_notice_carries_its_join_key_and_its_fix_pointer() {
        let props = evaluate(&read_reason_signals());
        let p = props
            .iter()
            .find(|p| is_drift(p, RULE_DRIFT_READ_REASON))
            .expect("fires");
        assert_eq!(p.rule_id, RULE_DRIFT_CAPABILITY);
        assert_eq!(p.capability, Some("claude.hook.pretooluse_deny"));
        assert!(p.signature.starts_with("claude.hook.pretooluse_deny:drift.read_reason.v1:"));
        assert!(p.rationale.contains("claude.hook.pretooluse_deny"));
        assert!(p.rationale.contains(RULE_DRIFT_READ_REASON));
        // The `wired_in` column, verbatim — this is what makes the card
        // actionable rather than merely alarming.
        // V35 Phase J: the read advisor's shim binary is gone; its payload
        // mechanics and its route are what a reader must now open.
        // V35 Phase K: both live under `harness/claude/` — the paths come from
        // the registry's `wired_in`, so this assertion moved when the files did.
        assert!(
            p.rationale.contains("src-tauri/src/harness/claude/hook.rs"),
            "{}",
            p.rationale
        );
        assert!(p
            .rationale
            .contains("src-tauri/src/harness/claude/overlay.rs"));
    }

    /// A dismissal holds per (capability, evidence) pair and NOWHERE else.
    ///
    /// Both halves matter. Dismissing the transcript-usage notice must not
    /// silence the sub-agent one even though they now share a `rule_id` — that
    /// is the risk consolidation introduced. And the third signature field
    /// (each detector's original key) must still be honoured, or every drift
    /// dismissal would silently have become permanent.
    #[test]
    fn a_capability_dismissal_does_not_silence_a_sibling_capability() {
        let sig = Signals {
            harness: one_harness(DriftSignals {
                sessions: 3,
                tokenless_sessions: 3,
                subagent_drift: vec!["subagents/*.jsonl vanished".to_string()],
                last_seen: "2.2.0".to_string(),
                ..DriftSignals::default()
            }),
            ..Signals::default()
        };
        assert!(evaluate(&sig).iter().any(|p| is_drift(p, RULE_DRIFT_USAGE_FIELDS)));
        assert!(evaluate(&sig).iter().any(|p| is_drift(p, RULE_DRIFT_SUBAGENT)));

        let mut dismissed = sig.clone();
        dismissed.dismissed = vec![DismissedRule {
            rule_id: RULE_DRIFT_CAPABILITY.to_string(),
            signature: drift_signature(
                "claude.transcript.usage",
                RULE_DRIFT_USAGE_FIELDS,
                &version_key("2.2.0"),
            ),
        }];
        let props = evaluate(&dismissed);
        assert!(
            !props.iter().any(|p| is_drift(p, RULE_DRIFT_USAGE_FIELDS)),
            "the dismissed (capability, evidence) pair must be silent"
        );
        assert!(
            props.iter().any(|p| is_drift(p, RULE_DRIFT_SUBAGENT)),
            "a sibling capability sharing the notice id must still speak"
        );

        // Same capability, next harness version ⇒ re-fires (the third
        // signature field is the detector's own re-fire boundary).
        let mut next = dismissed;
        drift_mut(&mut next).last_seen = "2.3.0".to_string();
        assert!(evaluate(&next).iter().any(|p| is_drift(p, RULE_DRIFT_USAGE_FIELDS)));
    }

    /// Applying the one drift notice that carries an Apply must not silence
    /// the warn-only notices that now share its `rule_id`.
    ///
    /// `AppliedRule` is keyed by `rule_id` alone (a Settings wire type), so
    /// consolidation put every capability's warn-only card behind one
    /// cooldown record. The exemption in `evaluate` is what keeps a
    /// transcript-usage break from going quiet because the user acted on an
    /// unrelated read-advisor card.
    #[test]
    fn applying_one_capability_notice_does_not_mute_the_others() {
        let mut sig = read_reason_signals();
        drift_mut(&mut sig).sessions = 3;
        drift_mut(&mut sig).tokenless_sessions = 3;
        drift_mut(&mut sig).last_seen = "2.2.0".to_string();
        sig.applied = vec![AppliedRule {
            rule_id: RULE_DRIFT_CAPABILITY.to_string(),
            root: String::new(),
            session_count: sig.session_count,
        }];
        let props = evaluate(&sig);
        assert!(
            !props.iter().any(|p| is_drift(p, RULE_DRIFT_READ_REASON)),
            "the applied (Apply-bearing) notice stays in cooldown"
        );
        assert!(
            props.iter().any(|p| is_drift(p, RULE_DRIFT_USAGE_FIELDS)),
            "a warn-only notice has no Apply and must not inherit the cooldown"
        );
    }

    /// **Every registered harness gets the version tripwire** (V40 Phase C,
    /// locked decision 23).
    ///
    /// Before this phase the condition was `claude_last_seen !=
    /// claude_last_verified` and OpenCode had no version-drift path at all: it
    /// could auto-update straight through a contract change and nothing
    /// anywhere would say so. This is the test that would have caught that, and
    /// it fails the moment a rule goes back to reading one harness's row.
    #[test]
    fn the_version_tripwire_fires_for_every_registered_harness() {
        for h in crate::harness::registry::all() {
            let sig = Signals {
                harness: [(
                    h,
                    DriftSignals {
                        last_seen: "9.9.9".to_string(),
                        last_verified: "9.9.8".to_string(),
                        ..DriftSignals::default()
                    },
                )]
                .into_iter()
                .collect(),
                ..Signals::default()
            };
            let props = evaluate(&sig);
            let p = props
                .iter()
                .find(|p| p.rule_id == RULE_DRIFT_VERSION)
                .unwrap_or_else(|| panic!("{h}: no version tripwire"));
            assert_eq!(p.harness, h.id(), "{h}: the notice must name its harness");
            assert!(
                p.rationale.contains(h.label()),
                "{h}: the card must name the harness a reader is being asked to verify"
            );
            assert_eq!(p.action, Some("mark_verified"));
        }
    }

    /// Two harnesses drifting at once produce **two** notices, and dismissing
    /// one leaves the other standing.
    ///
    /// The signature is `(harness, version)` for exactly this: two harnesses on
    /// the same version string would otherwise share a dismissal, and silencing
    /// one would silence the other.
    #[test]
    fn two_harnesses_drifting_at_one_version_do_not_share_a_dismissal() {
        let ids: Vec<_> = crate::harness::registry::all().collect();
        assert!(ids.len() >= 2, "this test needs two registered harnesses");
        let row = || DriftSignals {
            last_seen: "7.0.0".to_string(),
            last_verified: "6.9.0".to_string(),
            ..DriftSignals::default()
        };
        let mut sig = Signals {
            harness: ids.iter().map(|h| (*h, row())).collect(),
            ..Signals::default()
        };
        let all = evaluate(&sig);
        let versions: Vec<&Proposal> = all
            .iter()
            .filter(|p| p.rule_id == RULE_DRIFT_VERSION)
            .collect();
        assert_eq!(versions.len(), ids.len(), "one notice per drifting harness");

        sig.dismissed = vec![DismissedRule {
            rule_id: RULE_DRIFT_VERSION.to_string(),
            signature: version_signature(ids[0], "7.0.0"),
        }];
        let after: Vec<String> = evaluate(&sig)
            .into_iter()
            .filter(|p| p.rule_id == RULE_DRIFT_VERSION)
            .filter_map(|p| p.harness.map(str::to_string))
            .collect();
        assert_eq!(
            after,
            ids[1..].iter().filter_map(|h| h.id()).map(str::to_string).collect::<Vec<_>>(),
            "dismissing one harness's version notice silenced another's"
        );
    }

    #[test]
    fn version_tripwire_fires_below_the_global_session_floor() {
        // A version bump is a fact, not a statistic — zero sessions must
        // not gate it.
        let sig = Signals {
            harness: one_harness(DriftSignals {
                last_seen: "2.2.0".to_string(),
                last_verified: "2.1.14".to_string(),
                ..DriftSignals::default()
            }),
            session_count: 0,
            ..Signals::default()
        };
        let props = evaluate(&sig);
        assert_eq!(props.len(), 1);
        let p = &props[0];
        assert_eq!(p.rule_id, RULE_DRIFT_VERSION);
        assert!(p.warn_only);
        assert_eq!(p.action, Some("mark_verified"));
        assert_eq!(p.signature, version_key("2.2.0"));
    }

    #[test]
    fn version_tripwire_fires_on_a_never_verified_install_and_not_when_matched() {
        let sig = Signals {
            harness: one_harness(DriftSignals {
                last_seen: "2.2.0".to_string(),
                ..DriftSignals::default()
            }),
            ..Signals::default()
        };
        assert!(evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_VERSION));

        let sig_ok = Signals {
            harness: one_harness(DriftSignals {
                last_seen: "2.2.0".to_string(),
                last_verified: "2.2.0".to_string(),
                ..DriftSignals::default()
            }),
            ..Signals::default()
        };
        assert!(!evaluate(&sig_ok)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_VERSION));

        // Never-seen (no Claude tab yet): nothing to trip on.
        assert!(!evaluate(&Signals::default())
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_VERSION));
    }

    #[test]
    fn version_tripwire_dismissal_is_keyed_to_the_seen_version() {
        let mut sig = Signals {
            harness: one_harness(DriftSignals {
                last_seen: "2.2.0".to_string(),
                last_verified: "2.1.14".to_string(),
                ..DriftSignals::default()
            }),
            dismissed: vec![DismissedRule {
                rule_id: RULE_DRIFT_VERSION.to_string(),
                signature: version_key("2.2.0"),
            }],
            ..Signals::default()
        };
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_VERSION));

        // The NEXT version change re-fires despite the old dismissal.
        drift_mut(&mut sig).last_seen = "2.3.0".to_string();
        assert!(evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_VERSION));
    }

    // ── V35 Phase F — the tripwire as cannot-verify fallback ────────────
    //
    // The three tests above keep passing unchanged, and that is the point:
    // with `claude_auto_verify == None` (auto-verify has never completed on
    // this machine) the tripwire behaves exactly as V16 built it. They now pin
    // the FALLBACK case. The four below pin the cases Phase F added.

    /// The auto-verify record for the currently-seen build, with one failing
    /// capability.
    fn auto_verify_failed(version: &str, capability: &str) -> crate::settings::AutoVerify {
        crate::settings::AutoVerify {
            version: version.to_string(),
            at_ms: 42,
            status: crate::settings::AutoVerify::FAIL.to_string(),
            failures: vec![crate::settings::AutoVerifyFailure {
                capability: capability.to_string(),
                evidence: crate::harness::verify::EVIDENCE_CANARY.to_string(),
                detail: "context_window.used_percentage gone".to_string(),
            }],
        }
    }

    /// A failed auto-verify replaces the tripwire with a notice that NAMES the
    /// capability — the exit criterion of Phase F's other half.
    ///
    /// Two cards for one event would be exactly the noise this phase removes,
    /// and the one that survives has to be the specific one: "the version
    /// moved, go re-run ten minutes of recipes" is the card that trained the
    /// reflex, "`claude.statusline.stdin` broke, see harness/claude/statusline.rs" is the
    /// one worth reading.
    #[test]
    fn a_failed_auto_verify_replaces_the_tripwire_with_a_named_capability() {
        let sig = Signals {
            harness: one_harness(DriftSignals {
                last_seen: "2.2.0".to_string(),
                last_verified: "2.1.14".to_string(),
                auto_verify: Some(auto_verify_failed("2.2.0", "claude.statusline.stdin")),
                ..DriftSignals::default()
            }),
            ..Signals::default()
        };
        let props = evaluate(&sig);
        assert!(
            !props.iter().any(|p| p.rule_id == RULE_DRIFT_VERSION),
            "the tripwire is superseded — the capability notice below says the same thing, \
             precisely"
        );
        let p = props
            .iter()
            .find(|p| p.rule_id == RULE_DRIFT_CAPABILITY)
            .expect("the failing capability speaks");
        assert_eq!(p.capability, Some("claude.statusline.stdin"));
        assert!(p.warn_only, "there is nothing safe to auto-apply");
        assert_eq!(p.action, None, "Mark verified stays on the tripwire card");
        assert_eq!(
            p.signature,
            format!(
                "claude.statusline.stdin:{}:{}",
                crate::harness::verify::EVIDENCE_CANARY,
                version_key("2.2.0")
            ),
            "signature = <capability>:<evidence>:<version>, so a dismissal holds for this build \
             and re-fires on the next one"
        );
        // The card must be actionable: the assertion that failed, and the file
        // that breaks.
        assert!(p.rationale.contains("context_window.used_percentage gone"));
        assert!(p.rationale.contains("src-tauri/src/harness/claude/statusline.rs"));
        assert!(p.rationale.contains("2.2.0"));
    }

    /// A record that PASSED but left the versions apart keeps the tripwire.
    ///
    /// This is the "the advance did not land" case (a failed settings write).
    /// Suppressing here would be the worst outcome available: a harness moved,
    /// nothing verified it, and no card anywhere.
    #[test]
    fn a_passing_record_that_did_not_advance_keeps_the_fallback() {
        let sig = Signals {
            harness: one_harness(DriftSignals {
                last_seen: "2.2.0".to_string(),
                last_verified: "2.1.14".to_string(),
                auto_verify: Some(crate::settings::AutoVerify {
                version: "2.2.0".to_string(),
                at_ms: 42,
                status: crate::settings::AutoVerify::PASS.to_string(),
                failures: Vec::new(),
                }),
                ..DriftSignals::default()
            }),
            ..Signals::default()
        };
        let props = evaluate(&sig);
        assert!(props.iter().any(|p| p.rule_id == RULE_DRIFT_VERSION));
        assert!(!props.iter().any(|p| p.rule_id == RULE_DRIFT_CAPABILITY));
    }

    /// A record for the PREVIOUS build says nothing about this one: the
    /// tripwire speaks (nothing has verified the new version yet) and the old
    /// failure does not.
    #[test]
    fn a_stale_record_neither_suppresses_nor_speaks() {
        let sig = Signals {
            harness: one_harness(DriftSignals {
                last_seen: "2.3.0".to_string(),
                last_verified: "2.1.14".to_string(),
                auto_verify: Some(auto_verify_failed("2.2.0", "claude.statusline.stdin")),
                ..DriftSignals::default()
            }),
            ..Signals::default()
        };
        let props = evaluate(&sig);
        assert!(props.iter().any(|p| p.rule_id == RULE_DRIFT_VERSION));
        assert!(!props.iter().any(|p| p.rule_id == RULE_DRIFT_CAPABILITY));
    }

    /// The routine auto-update — canaries green, version advanced by the
    /// background run — produces **no card at all**. This is the milestone's
    /// Phase F exit criterion, asserted over the whole proposal list rather
    /// than over one rule id.
    #[test]
    fn a_verified_update_produces_no_advisor_card() {
        let sig = Signals {
            harness: one_harness(DriftSignals {
                last_seen: "2.2.0".to_string(),
                last_verified: "2.2.0".to_string(),
                auto_verify: Some(crate::settings::AutoVerify {
                version: "2.2.0".to_string(),
                at_ms: 42,
                status: crate::settings::AutoVerify::PASS.to_string(),
                failures: Vec::new(),
                }),
                ..DriftSignals::default()
            }),
            ..Signals::default()
        };
        assert!(
            evaluate(&sig).is_empty(),
            "a routine CLI auto-update that broke nothing must be silent: {:?}",
            evaluate(&sig)
                .iter()
                .map(|p| p.rule_id)
                .collect::<Vec<_>>()
        );
    }

    /// Signals for the read-reason drift: advisor ON, ~100% reread at the
    /// drift floor (15), below the tuning rule's floor (20) — only the
    /// drift rule can speak.
    fn read_reason_signals() -> Signals {
        let graph = GraphSettings {
            read_advisor: true,
            ..GraphSettings::default()
        };
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
        let p = props
            .iter()
            .find(|p| is_drift(p, RULE_DRIFT_READ_REASON))
            .expect("fires");
        assert_eq!(p.setting, "graph.read_advisor");
        assert_eq!(p.proposed, "false");
        assert!(!p.warn_only);
    }

    #[test]
    fn read_reason_drift_needs_the_advisor_on_and_its_own_floors() {
        let mut sig = read_reason_signals();
        sig.graph.read_advisor = false;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_READ_REASON)));

        let mut sig = read_reason_signals();
        sig.advisor_reread_samples = DRIFT_MIN_REMINDS - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_READ_REASON)));

        let mut sig = read_reason_signals();
        sig.advisor_reread_rate = Some(0.8); // high for tuning, below drift's 0.9
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_READ_REASON)));
    }

    #[test]
    fn read_reason_drift_takes_precedence_over_the_min_lines_tuning_rule() {
        // Both rules' floors and thresholds satisfied at once (samples ≥ 20,
        // rate 1.0): only the drift diagnosis may surface.
        let mut sig = read_reason_signals();
        sig.advisor_reread_rate = Some(1.0);
        sig.advisor_reread_samples = MIN_REMINDS; // 20 ≥ both floors
        let props = evaluate(&sig);
        assert!(props.iter().any(|p| is_drift(p, RULE_DRIFT_READ_REASON)));
        assert!(
            !props.iter().any(|p| p.rule_id == RULE_ADVISOR_LINES),
            "tuning rule must be suppressed"
        );
    }

    #[test]
    fn hook_silent_drift_needs_rereads_sessions_and_exactly_zero_reminds() {
        let graph = GraphSettings {
            read_advisor: true,
            ..GraphSettings::default()
        };
        let base = Signals {
            harness: one_harness(DriftSignals {
                last_seen: "2.2.0".to_string(),
                ..DriftSignals::default()
            }),
            session_count: DRIFT_SILENT_MIN_SESSIONS,
            large_reread_pairs: DRIFT_SILENT_MIN_REREADS,
            remind_count: 0,
            graph,
            ..Signals::default()
        };
        let props = evaluate(&base);
        let p = props
            .iter()
            .find(|p| is_drift(p, RULE_DRIFT_HOOK_SILENT))
            .expect("fires");
        assert!(p.warn_only);
        assert!(p.setting.is_empty());
        // Still re-fires per harness version — the detector's own key is the
        // third signature field, unchanged by V35 Phase E's consolidation.
        assert_eq!(
            p.signature,
            drift_signature(
                "claude.hook.pretooluse_deny",
                RULE_DRIFT_HOOK_SILENT,
                &version_key("2.2.0")
            )
        );

        let mut sig = base.clone();
        sig.remind_count = 1; // one remind reached the loopback ⇒ hook alive
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_HOOK_SILENT)));

        let mut sig = base.clone();
        sig.large_reread_pairs = DRIFT_SILENT_MIN_REREADS - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_HOOK_SILENT)));

        let mut sig = base.clone();
        sig.session_count = DRIFT_SILENT_MIN_SESSIONS - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_HOOK_SILENT)));

        let mut sig = base;
        sig.graph.read_advisor = false;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_HOOK_SILENT)));
    }

    #[test]
    fn injection_unseen_drift_fires_only_at_near_zero_follow() {
        let graph = GraphSettings {
            context_injection: true,
            ..GraphSettings::default()
        };
        let base = Signals {
            injection_follow_rate: Some(0.0),
            injection_follow_samples: DRIFT_UNSEEN_MIN_INJECTIONS,
            session_count: DRIFT_UNSEEN_MIN_SESSIONS,
            graph,
            ..Signals::default()
        };
        let props = evaluate(&base);
        let p = props
            .iter()
            .find(|p| is_drift(p, RULE_DRIFT_INJECTION_UNSEEN))
            .expect("fires");
        assert!(p.warn_only);

        // 10% follow is unhealthy but NOT "never reaches the model" — the
        // tuning rule's territory, not the drift rule's.
        let mut sig = base.clone();
        sig.injection_follow_rate = Some(0.10);
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_INJECTION_UNSEEN)));

        let mut sig = base;
        sig.graph.context_injection = false;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_INJECTION_UNSEEN)));
    }

    #[test]
    fn usage_fields_gone_fires_only_when_every_claude_session_is_tokenless() {
        let base = Signals {
            harness: one_harness(DriftSignals {
                sessions: 3,
                tokenless_sessions: 3,
                last_seen: "2.2.0".to_string(),
                ..DriftSignals::default()
            }),
            ..Signals::default()
        };
        assert!(evaluate(&base)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_USAGE_FIELDS)));

        // One healthy session ⇒ the schema didn't change, that session is
        // just odd.
        let mut sig = base.clone();
        drift_mut(&mut sig).tokenless_sessions = 2;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_USAGE_FIELDS)));

        // Below the floor a single tokenless session could be a fluke.
        let mut sig = base;
        drift_mut(&mut sig).sessions = DRIFT_MIN_TOKENLESS - 1;
        drift_mut(&mut sig).tokenless_sessions = DRIFT_MIN_TOKENLESS - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_USAGE_FIELDS)));
    }

    #[test]
    fn payload_drift_fires_on_any_contract_drift_event() {
        let sig = Signals {
            contract_drift: vec!["read_hook: session_id".to_string()],
            ..Signals::default()
        };
        let props = evaluate(&sig);
        let p = props
            .iter()
            .find(|p| is_drift(p, RULE_DRIFT_PAYLOAD))
            .expect("fires");
        assert!(p.warn_only);
        assert!(p.rationale.contains("read_hook: session_id"));
        // V35 Phase E: the report is attributed to the capability whose shim
        // sent it, resolved through the registry's `wired_in` column.
        assert_eq!(p.capability, Some("claude.hook.pretooluse_deny"));
        assert_eq!(
            p.signature,
            drift_signature(
                "claude.hook.pretooluse_deny",
                RULE_DRIFT_PAYLOAD,
                "read_hook: session_id"
            )
        );

        // Reports are deduped and sorted — a second identical report doesn't
        // change the signature (a dismissal holds), a new missing FIELD does.
        // Two different shims are now two different capabilities and therefore
        // two notices: before Phase E they shared one card and one dismissal,
        // so silencing a `read_hook` drift silenced the `compact_hook` one too.
        let sig2 = Signals {
            contract_drift: vec![
                "read_hook: session_id".to_string(),
                "compact_hook: cwd".to_string(),
                "read_hook: session_id".to_string(),
            ],
            ..Signals::default()
        };
        let props2 = evaluate(&sig2);
        let payload: Vec<&Proposal> = props2
            .iter()
            .filter(|p| is_drift(p, RULE_DRIFT_PAYLOAD))
            .collect();
        assert_eq!(payload.len(), 2, "one notice per affected capability");
        assert_eq!(
            payload[0].signature,
            drift_signature("claude.hook.precompact", RULE_DRIFT_PAYLOAD, "compact_hook: cwd")
        );
        assert_eq!(
            payload[1].signature,
            drift_signature(
                "claude.hook.pretooluse_deny",
                RULE_DRIFT_PAYLOAD,
                "read_hook: session_id"
            )
        );
        // Dismissing one leaves the other speaking.
        let mut sig3 = sig2;
        sig3.dismissed = vec![DismissedRule {
            rule_id: RULE_DRIFT_CAPABILITY.to_string(),
            signature: payload[0].signature.clone(),
        }];
        let props3 = evaluate(&sig3);
        assert_eq!(
            props3
                .iter()
                .filter(|p| is_drift(p, RULE_DRIFT_PAYLOAD))
                .count(),
            1
        );
    }

    /// A payload report from a shim the matrix does not cover keeps the
    /// un-consolidated `drift.payload.v1` channel rather than being pinned on
    /// a capability that did not report it.
    ///
    /// **V35 Phase I changed which shims land here, and that is the point.**
    /// When this test was written, `taint_beacon` and `checkpoint_beacon` were
    /// the live occupants of the unattributed channel: they really do post
    /// through `/activity/contract_drift` and really had no registry row. Phase I
    /// gave them rows (`claude.hook.taint_beacon` / `claude.hook.checkpoint_beacon`),
    /// so both are now attributed like every other shim — which is Phase E's
    /// accepted residual closed, and "one notice source" holding for the whole
    /// matrix.
    ///
    /// The channel itself stays, and its remaining occupant is a shim name
    /// **nobody declared** — a forged one, or a future shim someone added without
    /// a registry row. Dropping those reports to make the notice-source count
    /// come out at one would be discarding a signal about a shim nobody has
    /// declared, which is the failure V35 exists to remove rather than an
    /// instance of the tidiness it is after.
    #[test]
    fn an_unmatrixed_shim_keeps_the_unattributed_payload_channel() {
        let sig = Signals {
            contract_drift: vec![
                "mystery_shim: tool_name".to_string(),
                "read_hook: session_id".to_string(),
            ],
            ..Signals::default()
        };
        let props = evaluate(&sig);
        let attributed = props
            .iter()
            .find(|p| is_drift(p, RULE_DRIFT_PAYLOAD))
            .expect("the read_hook report is attributed");
        assert_eq!(attributed.capability, Some("claude.hook.pretooluse_deny"));
        assert!(!attributed.rationale.contains("mystery_shim"));

        let residual = props
            .iter()
            .find(|p| p.rule_id == RULE_DRIFT_PAYLOAD)
            .expect("the undeclared shim's report still surfaces");
        assert_eq!(residual.capability, None);
        assert_eq!(residual.signature, "mystery_shim: tool_name");
        assert!(residual.warn_only);
        assert!(!residual.rationale.contains("read_hook"));
    }

    /// **V35 Phase I:** the two beacon shims now resolve to their own
    /// capabilities instead of the unattributed channel — the concrete closure
    /// of Phase E's accepted residual, asserted through the Advisor rather than
    /// only through the registry, because the Advisor card is where the user
    /// meets it.
    #[test]
    fn the_beacon_shims_are_attributed_to_their_own_capabilities() {
        for (shim, expect) in [
            ("taint_beacon", "claude.hook.taint_beacon"),
            ("checkpoint_beacon", "claude.hook.checkpoint_beacon"),
        ] {
            let sig = Signals {
                contract_drift: vec![format!("{shim}: tool_name")],
                ..Signals::default()
            };
            let props = evaluate(&sig);
            let p = props
                .iter()
                .find(|p| is_drift(p, RULE_DRIFT_PAYLOAD))
                .unwrap_or_else(|| panic!("`{shim}`'s report must reach a capability card"));
            assert_eq!(p.capability, Some(expect));
            assert!(p.rationale.contains(shim));
            assert!(
                !props.iter().any(|p| p.rule_id == RULE_DRIFT_PAYLOAD),
                "`{shim}` must no longer land in the un-consolidated channel"
            );
        }
    }

    #[test]
    fn subagent_drift_fires_on_any_subagent_drift_event() {
        let summary = "subagents/*.jsonl present but no Task/Agent launch tool_use recognized";
        let sig = Signals {
            harness: one_harness(DriftSignals {
                subagent_drift: vec![summary.to_string()],
                last_seen: "2.2.0".to_string(),
                ..DriftSignals::default()
            }),
            ..Signals::default()
        };
        let props = evaluate(&sig);
        let p = props
            .iter()
            .find(|p| is_drift(p, RULE_DRIFT_SUBAGENT))
            .expect("fires");
        assert!(p.warn_only);
        assert!(p.rationale.contains(summary));
        // Version-keyed signature: a dismissal holds until the next harness
        // update re-fires the rule (same boundary as the version tripwire),
        // now carried as the third field of the consolidated signature.
        assert_eq!(
            p.signature,
            drift_signature(
                "claude.transcript.subagents",
                RULE_DRIFT_SUBAGENT,
                &version_key("2.2.0")
            )
        );

        // Dismissed for this version ⇒ quiet.
        let mut sig = sig;
        sig.dismissed = vec![DismissedRule {
            rule_id: RULE_DRIFT_CAPABILITY.to_string(),
            signature: p.signature.clone(),
        }];
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_SUBAGENT)));

        // No events ⇒ silent.
        assert!(!evaluate(&Signals::default())
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_SUBAGENT)));
    }

    #[test]
    fn read_bypass_drift_proposes_disabling_at_the_threshold() {
        let graph = GraphSettings {
            read_advisor: true,
            ..GraphSettings::default()
        };
        let base = Signals {
            bypass_rate: Some(0.5),
            bypass_samples: DRIFT_MIN_BYPASS_REMINDS,
            graph,
            ..Signals::default()
        };
        let p = evaluate(&base);
        let p = p
            .iter()
            .find(|p| is_drift(p, RULE_DRIFT_READ_BYPASS))
            .expect("fires");
        assert_eq!(p.setting, "graph.read_advisor");
        assert_eq!(p.proposed, "false");

        let mut sig = base.clone();
        sig.bypass_rate = Some(0.3); // below BYPASS_HIGH
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_READ_BYPASS)));

        let mut sig = base;
        sig.bypass_samples = DRIFT_MIN_BYPASS_REMINDS - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_READ_BYPASS)));
    }

    #[test]
    fn drift_disable_proposals_carry_a_real_settings_write() {
        // The frontend Apply switch needs `graph.read_advisor` to be a real
        // assignable bool field — same guard as
        // `proposal_setting_names_match_real_graphsettings_fields`.
        let mut sig = read_reason_signals();
        sig.advisor_reread_rate = Some(1.0);
        let props = evaluate(&sig);
        let p = props
            .iter()
            .find(|p| is_drift(p, RULE_DRIFT_READ_REASON))
            .unwrap();
        assert_eq!(p.setting, "graph.read_advisor");
        let val: bool = p.proposed.parse().expect("proposed must be a bool string");
        let mut g = GraphSettings {
            read_advisor: true,
            ..GraphSettings::default()
        };
        g.read_advisor = val;
        assert!(!g.read_advisor);
    }

    #[test]
    fn apply_cooldown_covers_drift_disable_proposals_too() {
        // Applying "disable read_advisor" flips the setting, which already
        // gates the rule off — but if the user re-enables it within the
        // cooldown, the drift rule must not fire again off the same stale rate.
        let mut sig = read_reason_signals();
        sig.applied = vec![AppliedRule {
            // V35 Phase E: the record the frontend writes is the notice id the
            // card carried, which for a drift notice is now the consolidated
            // one.
            rule_id: RULE_DRIFT_CAPABILITY.to_string(),
            root: String::new(),
            session_count: sig.session_count,
        }];
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| is_drift(p, RULE_DRIFT_READ_REASON)));
    }
}

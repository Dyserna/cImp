//! The Usage section's use cases: the token X-ray, one session's drill-in, and
//! the budget-tuning advisor's proposals, dismissals and apply cooldowns.
//!
//! ## What the A1 usage run found
//!
//! The largest body in the graph domain and the one with the most collaborators
//! outside it. [`UsageService::advice`] assembles ~25 signals from four
//! different places — the code graph's Datalog passes, the process-wide activity
//! ring, the detection updater's in-memory caches, and the physical global
//! `settings.json` — and hands them to [`crate::advisor::evaluate`]. None of
//! that needed a WebView; it needed an `AppHandle` only to reach
//! `State<'_, AppState>`, which is to say it needed nothing.
//!
//! Two things here are genuinely cross-domain, and each gets the treatment
//! [`ChecksLangStats`](crate::service::checks::ChecksLangStats) established:
//!
//! * **The offload run counts** on the Usage snapshot's Effectiveness panel —
//!   "N tasks served locally" — come from [`crate::offload::OffloadService`],
//!   which `GraphService` has no dependency on and should not grow one.
//!   [`OffloadRunMetrics`] is that reach, one method wide. It is an
//!   `Arc<dyn …>` rather than a `&dyn …` because the call happens INSIDE the
//!   blocking-pool closure, which must be `'static` — and it happens there on
//!   purpose: hoisting it to the caller would move work back onto a runtime
//!   worker that the whole shape of this module exists to keep it off.
//! * **The harness drift signals** are read per registered harness from the
//!   physical global file rather than the live settings snapshot, because the
//!   auto-verify worker writes them out of band and a record a second old has to
//!   be visible to the very next 2 s poll. That is not a coupling to move; it is
//!   a rule, and it stays spelled out at [`harness_drift_signals`].
//!
//! ## Why so much of this runs on the blocking pool
//!
//! [`UsageService::snapshot`] and [`UsageService::advice`] are multi-query Cozo
//! passes measured in *seconds* against a large store, and the Overview polls
//! both on a timer. Left on a tokio worker each one parked that worker for the
//! whole pass and every other IPC queued behind it — which is what made
//! switching tabs feel sluggish while the dashboard was open. The hop is
//! [`crate::service::on_blocking_pool`], and it is load-bearing, not tidiness.
//!
//! ## What did NOT change
//!
//! The `collecting` flag still yields to a non-empty proposal list (a drift
//! canary carries its own floor and can fire below the tuning floor — a version
//! bump is a fact, not a statistic). The apply-cooldown filter still compares
//! roots with [`crate::activity::root_key_eq`] rather than `==`, and the writer
//! still derives its root string the same way, so the two forms compare equal.
//! And `remind_count` is still `advisor_reread_samples` reused rather than a
//! second identical Datalog scan.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::AppResult;
use crate::graph::GraphService;
use crate::service::{on_blocking_pool, project_root};
use crate::settings::{Settings, SettingsHandle};

/// The offload pool's per-backend dashboard rows, as the Usage snapshot's
/// Effectiveness panel needs them. See the module docs for why this is a trait
/// and why it is owned rather than borrowed.
pub trait OffloadRunMetrics: Send + Sync + 'static {
    fn backend_dashboards(&self) -> Vec<crate::offload::metrics::BackendDashboard>;
}

impl OffloadRunMetrics for crate::offload::OffloadService {
    fn backend_dashboards(&self) -> Vec<crate::offload::metrics::BackendDashboard> {
        self.server_metrics()
    }
}

/// V14 Phase D2: the advisor's answer. Wraps `advisor::evaluate`'s
/// `Vec<Proposal>` with a `collecting` flag — NOT part of the milestone's
/// literal `Vec<Proposal>` pseudocode, added because the Advisor card (D2.4)
/// needs to distinguish "no data yet" from "checked, all healthy", and a bare
/// `Vec<Proposal>` can't carry that distinction on its own.
#[derive(serde::Serialize)]
pub struct AdvisorSnapshot {
    pub proposals: Vec<crate::advisor::Proposal>,
    pub collecting: bool,
}

/// The advisor's rule reference (V40 Phase F, locked decision 23).
#[derive(serde::Serialize)]
pub struct AdvisorRules {
    /// One row per rule, in the order the reference lists them.
    pub rules: Vec<crate::advisor::RuleReference>,
    /// The one sentence that is about the panel rather than about a rule.
    pub footer: &'static str,
}

/// The Usage/Advisor use cases, over one borrowed handle — same shape and
/// rationale as [`crate::service::graph::CodeIntelService`], which is also why
/// the settings handle is a per-call parameter here rather than a field: only
/// two of these four methods touch it, and the two that do not are called from
/// commands that never had an `AppState` to hand one from.
pub struct UsageService<'a> {
    index: &'a Arc<GraphService>,
}

impl<'a> UsageService<'a> {
    pub fn new(index: &'a Arc<GraphService>) -> Self {
        Self { index }
    }

    /// V14 Phase D: the Usage section's full payload for `root` — the current
    /// session's per-turn series + top-tools ranking, every known session's
    /// totals row, and the effectiveness counters.
    ///
    /// Two fields `GraphService` cannot fill are filled here: the offload
    /// local-task count (completed runs on `local` backends — a run still in
    /// flight is not a task served) and the advertised tool-surface size, which
    /// is measured post-`lean_tools`-filter from live settings and so depends on
    /// settings rather than on the index.
    pub async fn snapshot(
        &self,
        root: Option<String>,
        offload: Arc<dyn OffloadRunMetrics>,
    ) -> AppResult<crate::graph::UsageSnapshot> {
        let root = project_root(root)?;
        let graph = self.index.clone();
        on_blocking_pool(move || {
            let mut snap = graph.usage_snapshot(&root);
            snap.offload_local_tasks = offload
                .backend_dashboards()
                .into_iter()
                .filter(|b| b.kind == "local")
                .flat_map(|b| b.metrics.runs)
                .filter(|r| r.outcome != "running")
                .count() as u64;
            snap.surface = crate::graph::surface_stats();
            snap
        })
        .await
    }

    /// V24 Phase B: full drill-in detail for ONE session under `root`. Unlike
    /// [`Self::snapshot`] (which only surfaces the current session at full
    /// detail) this works for any session id, so the Usage card can render a
    /// clicked historical session. An unknown session id returns an empty
    /// detail — no error, no panic.
    pub async fn session_detail(
        &self,
        root: Option<String>,
        session_id: String,
    ) -> AppResult<crate::graph::SessionUsageDetail> {
        let root = project_root(root)?;
        let graph = self.index.clone();
        on_blocking_pool(move || graph.session_usage_detail(&root, &session_id)).await
    }

    /// V14 Phase D2: the budget-tuning advisor's current proposals for `root`.
    /// Assembled fresh on every call from the D2.1 signal getters — cheap
    /// (bounded Datalog queries + a small in-memory scan), no caching needed.
    pub async fn advice(
        &self,
        settings: &SettingsHandle,
        root: Option<String>,
    ) -> AppResult<AdvisorSnapshot> {
        let root = project_root(root)?;
        let settings = settings.current();
        let graph = self.index.clone();
        // A dozen bounded Datalog queries against the same single-connection
        // store, on the Overview's poll cadence — off the runtime workers, same
        // reasoning as `snapshot`. A plain sync fn rather than an inline closure
        // so it stays readable and its `root`/`settings` stay owned.
        on_blocking_pool(move || advisor_snapshot_blocking(&graph, root, settings)).await
    }

    /// Record that the user APPLIED an advisor proposal, starting the rule's
    /// Apply cooldown (`advisor::APPLY_COOLDOWN_SESSIONS` sessions of quiet so
    /// fresh post-change data can accumulate before the rule re-evaluates — the
    /// rates are cumulative, and an immediate re-proposal would be judging the
    /// OLD value's data).
    ///
    /// Captures the root's session count server-side at call time; one record
    /// per (rule, root), re-applying replaces it. Called by the Advisor card's
    /// Apply right after the `settings_update` that writes the proposed value —
    /// the settings write itself stays the ordinary path (never silent
    /// self-modification).
    pub fn mark_applied(
        &self,
        settings: &SettingsHandle,
        root: Option<String>,
        rule_id: String,
    ) -> AppResult<()> {
        let root = project_root(root)?;
        let session_count = self.index.advisor_session_count(&root);
        let root_str = root.to_string_lossy().to_string();
        settings.mutate(move |cur| {
            cur.advisor_applied
                .retain(|a| !(a.rule_id == rule_id && crate::activity::root_key_eq(&a.root, &root_str)));
            cur.advisor_applied.push(crate::settings::AppliedRule {
                rule_id,
                root: root_str,
                session_count,
            });
        });
        Ok(())
    }
}

/// V14 Phase D2: dismiss one advisor proposal (`rule_id` + its coarse rate
/// `signature`, both echoed from the `Proposal` the user clicked Dismiss on).
/// Persisted in `Settings.advisor_dismissed`; a materially changed rate (a
/// different signature bucket) re-fires the proposal even for the same
/// `rule_id`. Idempotent — dismissing the same pair twice is a no-op.
///
/// Free rather than a [`UsageService`] method: it reads no index, so a service
/// would be a handle it never touches.
pub fn dismiss(settings: &SettingsHandle, rule_id: String, signature: String) -> AppResult<()> {
    settings.mutate(move |cur| {
        let already = cur
            .advisor_dismissed
            .iter()
            .any(|d| d.rule_id == rule_id && d.signature == signature);
        if !already {
            cur.advisor_dismissed
                .push(crate::settings::DismissedRule { rule_id, signature });
        }
    });
    Ok(())
}

/// The advisor's rule reference, as the Code Intelligence panel renders it. The
/// panel used to hold this table as a hard-coded tooltip — a restatement of
/// thresholds `advisor.rs` owns, with one harness's mechanisms named in it for
/// rules that fire per registered harness.
///
/// `'static` data; the window fetches it once when the panel first opens.
pub fn rules() -> AdvisorRules {
    AdvisorRules {
        rules: crate::advisor::RULE_REFERENCE.to_vec(),
        footer: crate::advisor::RULE_REFERENCE_FOOTER,
    }
}

/// Count calls to any `graph::LEAN_HIDDEN` tool in `activity` within the
/// trailing `window_ms` ending at `now_ms` — the `hideable_tool_calls` signal
/// feeding `surface.lean.v1`. Zero ⇒ the lean-surface rule may fire.
/// `now_ms.saturating_sub(window_ms)` is the inclusive cutoff, so entries older
/// than the window (including ancient residue in the count-capped ring) don't
/// count. Free function so the window semantics stay unit-testable apart from
/// the signal assembly.
fn count_hideable_tool_calls(
    activity: &[crate::activity::ActivityEntry],
    now_ms: u64,
    window_ms: u64,
) -> u64 {
    let cutoff = now_ms.saturating_sub(window_ms);
    activity
        .iter()
        .filter(|e| e.ts_ms >= cutoff && crate::graph::LEAN_HIDDEN.contains(&e.tool.as_str()))
        .count() as u64
}

/// Every registered harness's [`crate::advisor::DriftSignals`], for one advisor
/// poll (V40 Phase C, locked decision 23).
///
/// The version half — `last_seen`, `last_verified`, `auto_verify` — is genuinely
/// per harness: it comes out of `Settings::harness[<id>]`, which Phase B made a
/// map, so a second harness gets a real `drift.version.v1` path for the first
/// time.
///
/// **The SESSION half is per harness too since V40 Phase D** (locked decision
/// 20). It used to be filled for the default harness only, with zeros for every
/// other, because the queries behind it had one agent literal inside them —
/// `drift.usage_fields_gone.v1` therefore never tripped its sample floor for a
/// second harness, and a rule that cannot fire looks exactly like a rule that
/// found nothing. `sessions` / `tokenless_sessions` now come from
/// `GraphIndex::tokenless_sessions(agent)` run once per registered harness, and
/// `subagent_drift` from the Activity rows each plugin declares it files
/// (`drift_report_tools`). A harness whose reader files no drift reports gets an
/// empty list — the truth, rather than a zero-fill that looks like one.
fn harness_drift_signals(
    sessions: &BTreeMap<crate::harness::HarnessId, (u64, u64)>,
    subagent_drift: &BTreeMap<crate::harness::HarnessId, Vec<String>>,
) -> crate::advisor::HarnessDriftSignals {
    let map = crate::settings::read_global_harness_map();
    crate::harness::registry::all()
        .map(|id| {
            let row = map
                .get(id.token())
                .cloned()
                .unwrap_or_else(|| crate::settings::read_global_harness_settings(id));
            let (sessions, tokenless_sessions) = sessions.get(&id).copied().unwrap_or((0, 0));
            (
                id,
                crate::advisor::DriftSignals {
                    last_seen: row.last_seen,
                    last_verified: row.last_verified,
                    auto_verify: row.auto_verify,
                    sessions,
                    tokenless_sessions,
                    subagent_drift: subagent_drift.get(&id).cloned().unwrap_or_default(),
                },
            )
        })
        .collect()
}

/// The blocking body of [`UsageService::advice`] — every signal read plus the
/// `advisor::evaluate` call. `root` and `settings` are owned because the closure
/// that carries them must be `'static`.
fn advisor_snapshot_blocking(
    graph: &GraphService,
    root: PathBuf,
    settings: Settings,
) -> AdvisorSnapshot {
    let (injection_follow_rate, injection_follow_samples) = match graph.injection_follow_rate(&root)
    {
        Some((r, n)) => (Some(r), n),
        None => (None, 0),
    };
    let (budget_maxed_rate, budget_maxed_samples) = match graph.budget_maxed_rate(&root) {
        Some((r, n)) => (Some(r), n),
        None => (None, 0),
    };
    let (advisor_reread_rate, advisor_reread_samples) = match graph.advisor_reread_rate(&root) {
        Some((r, n)) => (Some(r), n),
        None => (None, 0),
    };
    let session_count = graph.advisor_session_count(&root);

    // V16 drift signals. `harness_versions` is read from the physical global
    // file (not the live merged settings) so background writes — the tap
    // noting a version mid-run — are visible without a restart (mtime-cached,
    // so the 2s poll doesn't re-parse the file every tick).
    let hv = crate::settings::read_global_harness_versions();
    // `remind_count` (drift.read_hook_silent.v1) is the same total-remind-rows
    // count `advisor_reread_rate` just scanned for — reuse its sample count
    // instead of a second identical Datalog scan.
    let remind_count = advisor_reread_samples;
    let (large_reread_pairs, sessions_by_harness) = graph.drift_db_signals(&root);
    // One clone of the activity ring serves both the bypass-rate signal and
    // the contract-drift filter.
    let activity = crate::activity::snapshot();
    let (bypass_rate, bypass_samples) = match graph.bypass_rate(&activity) {
        Some((r, n)) => (Some(r), n),
        None => (None, 0),
    };
    let since = crate::activity::process_start_ms();
    let contract_drift: Vec<String> = activity
        .iter()
        .filter(|e| e.source == "harness" && e.tool == "contract_drift" && e.ts_ms >= since)
        .map(|e| e.target.clone())
        .collect();
    // V17.1: sub-agent transcript-contract drift reports filed by a harness's
    // own reader — same channel discipline as the `contract_drift` events
    // above. V40 Phase D: attributed to the harness that files them, which each
    // plugin declares (`drift_report_tools`), because the rule that reads them
    // runs per harness. A harness whose reader files none has an empty list —
    // which is the truth, not a zero-fill.
    let subagent_drift_by_harness: BTreeMap<crate::harness::HarnessId, Vec<String>> =
        crate::harness::registry::all()
            .map(|h| {
                let tools = h.plugin().map(|p| p.drift_report_tools()).unwrap_or(&[]);
                let rows = activity
                    .iter()
                    .filter(|e| {
                        e.source == "harness" && e.ts_ms >= since && tools.contains(&e.tool.as_str())
                    })
                    .map(|e| e.target.clone())
                    .collect();
                (h, rows)
            })
            .collect();

    // V17 Phase E signals: RECENT calls to any lean-hidden tool in the Activity
    // ring (zero ⇒ the lean-surface rule may fire) and the measured advertised
    // surface size for its rationale. Unlike the drift filters above, this uses
    // a trailing recency window rather than process-start `since`: the ring is
    // count-capped (GRAPH_CAP/OFFLOAD_CAP), so an all-time scan would let one
    // cold-tail call weeks ago suppress the suggestion forever, while
    // process-start would flip a tool to "unused" minutes after every restart.
    let hideable_tool_calls = count_hideable_tool_calls(
        &activity,
        crate::activity::now_ms(),
        crate::advisor::HIDEABLE_RECENCY_WINDOW_MS,
    );
    let surface_chars = crate::graph::surface_stats().mcp_chars as u64;

    // V17 Phase F1/F2 signals. Redundant re-read pairs per session over the
    // last 10 sessions, sized by the current advisor line floor. `e1_pass` is
    // STRICTLY the "pass" status (trimmed/lowercased) — NOT "the
    // `claude.hook.pretooluse_deny` gate is not blocking"
    // (`harness::contract::gate`, which passes `"unverified"` as well), so an
    // "unverified" E1 (the default) never auto-graduates a hook we've never
    // proven works. V35 Phase E retired the gate helper this used to be
    // contrasted with and left this check untouched.
    let (redundant_reads_per_session, redundant_read_sessions) =
        match graph.redundant_read_candidates(&root, settings.graph.read_advisor_min_lines, 10) {
            Some((pairs, sessions)) if sessions > 0 => {
                (Some(pairs as f64 / sessions as f64), sessions)
            }
            _ => (None, 0),
        };
    let e1_pass = hv.e1_status.trim().eq_ignore_ascii_case("pass");

    // V32 Phase C3: the detection updater's three canaries (a newer bundle
    // offered, a bundle refused, a channel that has been unreachable for a week
    // — #46 split the last one out of the second). Read from its in-memory
    // state cache — no disk and no clock, so this is safe on the advice poll's
    // cadence — and unlike every other signal here they are not per-root: the
    // detection data is process-wide, so the same card shows in whichever
    // project the user happens to have open.
    let (detection_updates, detection_update_failures, detection_update_stalled) =
        crate::offload::detection::updater::advisor_signals();
    // #48/D-2: the fourth detection canary, and the only one about the data on
    // disk rather than the channel — the signature layer switched on with
    // nothing to match against. Reads the cached compile report (no disk, no
    // clock) and resolves the layer's own switch through the injection
    // hierarchy, so a layer the user turned off says nothing.
    let detection_signature_down = crate::offload::detection::signature::advisor_signal(&settings);
    // #48/U-4: the fifth — a rule file the USER wrote that does not compile.
    // Its own signal rather than a widening of the one above, because the two
    // are different states (skipped file vs. disarmed layer) with different
    // fixes; the updater suppresses this one while that one is up.
    let detection_local_rules_broken =
        crate::offload::detection::updater::broken_local_rules(&settings);
    // #48/M-11: the sixth — the live rule directory is SHORT of files a
    // rollback could not put back. Deliberately not gated on the detection
    // switch: the files are missing from disk whether or not the layer is
    // currently screening with them, and a user who switches detection back on
    // must not silently get a short set.
    let detection_rules_incomplete = crate::offload::detection::updater::rules_incomplete();

    // Apply-cooldown records are stored per (rule, root) — hand `evaluate`
    // only THIS root's, so an Apply in one project never mutes another
    // (whose own session count may be far lower). Both this filter and the
    // writer (`UsageService::mark_applied`) derive the string from the same
    // root resolution, so the forms compare equal.
    let root_str = root.to_string_lossy().to_string();
    let applied: Vec<crate::settings::AppliedRule> = settings
        .advisor_applied
        .iter()
        .filter(|a| crate::activity::root_key_eq(&a.root, &root_str))
        .cloned()
        .collect();

    let sig = crate::advisor::Signals {
        injection_follow_rate,
        injection_follow_samples,
        advisor_reread_rate,
        advisor_reread_samples,
        budget_maxed_rate,
        budget_maxed_samples,
        session_count,
        graph: settings.graph.clone(),
        dismissed: settings.advisor_dismissed.clone(),
        applied,
        // V40 Phase C, locked decision 23: ONE ROW PER REGISTERED HARNESS,
        // read from the same fresh physical-global snapshot (the auto-verify
        // worker writes the version half out of band, so a record a second old
        // must be visible to the very next 2 s advisor poll without a restart).
        //
        // Phase B moved the storage into `harness[<id>]` and left a note here
        // saying the reader still took the DEFAULT harness's row because every
        // V16 rule was written around Claude's payload shapes. The rules are
        // per-harness now, so this is the whole map.
        harness: harness_drift_signals(&sessions_by_harness, &subagent_drift_by_harness),
        remind_count,
        large_reread_pairs,
        contract_drift,
        bypass_rate,
        bypass_samples,
        hideable_tool_calls,
        surface_chars,
        redundant_reads_per_session,
        redundant_read_sessions,
        e1_pass,
        detection_updates,
        detection_update_failures,
        detection_update_stalled,
        detection_signature_down,
        detection_local_rules_broken,
        detection_rules_incomplete,
    };
    let proposals = crate::advisor::evaluate(&sig);
    // "Collecting" = nothing has cleared the cold-start floor yet: not
    // enough sessions, OR neither of the two independent sample counts
    // (injections / reminders) has cleared its own rule's floor. Distinct
    // from "cleared the floor, rates are just healthy" (empty proposals,
    // `collecting = false`). V16: drift canaries carry their OWN floors and
    // can fire below the tuning floor (a version bump is a fact, not a
    // statistic) — a non-empty proposal list must therefore always render,
    // so `collecting` yields to it.
    let collecting = proposals.is_empty()
        && (session_count < crate::advisor::MIN_SESSIONS
            || (injection_follow_samples < crate::advisor::MIN_INJECTIONS
                && advisor_reread_samples < crate::advisor::MIN_REMINDS));
    AdvisorSnapshot {
        proposals,
        collecting,
    }
}

#[cfg(test)]
mod tests {
    use crate::testutil::ScratchDir;
    use super::*;
    use crate::activity::{ActivityEntry, ActivityKind};
    use crate::advisor::HIDEABLE_RECENCY_WINDOW_MS;
    use crate::settings::Settings;

    fn hidden_call(ts_ms: u64) -> ActivityEntry {
        // `graph_cycles` is one of graph::LEAN_HIDDEN.
        ActivityEntry::new(
            ActivityKind::Graph,
            ts_ms,
            "root".to_string(),
            // An opaque source tag: this test is about the RECENCY window, and
            // `ActivityEntry::source` is a persisted free string (locked
            // decision 29). Asking the registry keeps it a real one without
            // hard-coding which harness happens to be first.
            crate::harness::DEFAULT_HARNESS.token().to_string(),
            "graph_cycles".to_string(),
            "target".to_string(),
            0,
            0,
            true,
            crate::activity::Attribution::Unattributed,
            None,
            None,
            None,
        )
    }

    #[test]
    fn hideable_call_inside_window_counts() {
        let now = 1_000_000_000_000;
        // One day inside the trailing window.
        let recent = now - (HIDEABLE_RECENCY_WINDOW_MS - 24 * 60 * 60 * 1000);
        let activity = vec![hidden_call(recent)];
        assert_eq!(
            count_hideable_tool_calls(&activity, now, HIDEABLE_RECENCY_WINDOW_MS),
            1
        );
    }

    #[test]
    fn hideable_call_outside_window_is_ignored() {
        let now = 1_000_000_000_000;
        // One day OLDER than the window edge — a cold-tail call from long ago
        // must not suppress the lean suggestion.
        let ancient = now - (HIDEABLE_RECENCY_WINDOW_MS + 24 * 60 * 60 * 1000);
        let activity = vec![hidden_call(ancient)];
        assert_eq!(
            count_hideable_tool_calls(&activity, now, HIDEABLE_RECENCY_WINDOW_MS),
            0
        );
    }

    #[test]
    fn non_hidden_tool_never_counts() {
        let now = 1_000_000_000_000;
        // A workhorse tool inside the window still doesn't count.
        let mut e = hidden_call(now - 1000);
        e.tool = "graph_find_symbol".to_string();
        assert_eq!(
            count_hideable_tool_calls(&[e], now, HIDEABLE_RECENCY_WINDOW_MS),
            0
        );
    }

    fn handle(scratch: &ScratchDir) -> SettingsHandle {
        let defaults = Settings::default();
        SettingsHandle::new(defaults.clone(), defaults, scratch.0.clone())
    }

    /// **Previously "user clicks in the app".** The Dismiss button's whole
    /// contract: it is idempotent for one (rule, signature) pair, and a
    /// materially changed rate — a different signature bucket — is a NEW record
    /// rather than a match, which is what re-fires a proposal the user dismissed
    /// when the numbers were different.
    #[test]
    fn dismissing_is_idempotent_per_signature_and_a_new_bucket_is_a_new_record() {
        let scratch = ScratchDir::new("usagesvc");
        let settings = handle(&scratch);

        dismiss(&settings, "surface.lean.v1".into(), "hi".into()).expect("dismiss");
        dismiss(&settings, "surface.lean.v1".into(), "hi".into()).expect("dismiss again");
        assert_eq!(
            settings.current().advisor_dismissed.len(),
            1,
            "the same pair twice is a no-op"
        );

        dismiss(&settings, "surface.lean.v1".into(), "lo".into()).expect("dismiss");
        assert_eq!(
            settings.current().advisor_dismissed.len(),
            2,
            "a different rate bucket is a different dismissal"
        );
    }
}

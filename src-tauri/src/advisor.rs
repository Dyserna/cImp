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

use crate::settings::{AppliedRule, DismissedRule, GraphSettings};

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

/// Apply cooldown: after the user APPLIES a proposal, that rule stays quiet
/// until this many further sessions have been observed for the root. The
/// advisor's rates are cumulative over the tracked sessions, so a
/// re-evaluation right after Apply is dominated by data collected under the
/// OLD value — re-proposing off it would look like "the raise didn't work"
/// when nothing new has been measured yet. Distinct from a dismissal: it
/// expires on its own, and it re-fires at ANY rate bucket afterwards (the
/// whole point is to come back with fresh post-change numbers).
pub const APPLY_COOLDOWN_SESSIONS: u64 = 3;

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

/// V17 Phase E: propose hiding the cold-tail graph tools once enough sessions
/// have gone by with zero calls to any of them. Standard propose-with-Apply
/// (writes `graph.lean_tools = true`). Signature is the fixed `"zero-usage"`:
/// a single call to any hidden tool moves the signal off zero and silences it,
/// so there's no rate to bucket.
pub const RULE_SURFACE_LEAN: &str = "surface.lean.v1";
/// `surface.lean.v1`'s session floor — higher than `MIN_SESSIONS` (5) because
/// "nobody used these tools" is only convincing after a fair run of sessions.
pub const SURFACE_LEAN_MIN_SESSIONS: u64 = 10;
/// Trailing window over which a call to a `graph::LEAN_HIDDEN` tool counts as
/// recent usage for `surface.lean.v1` (see `hideable_tool_calls`). The activity
/// ring is COUNT-capped, not time-capped, so scanning it all-time lets a single
/// cold-tail call weeks ago suppress the lean suggestion forever; process-start
/// (the drift signals' `since`) is the opposite extreme — it would call a tool
/// "unused" minutes after every restart, flapping the advice. A month of
/// evidence is the middle ground: long enough to be meaningful, short enough to
/// forget stale one-off calls. Cited literally in the rule's rationale, so keep
/// the two in step.
pub const HIDEABLE_RECENCY_WINDOW_DAYS: u64 = 30;
pub const HIDEABLE_RECENCY_WINDOW_MS: u64 = HIDEABLE_RECENCY_WINDOW_DAYS * 24 * 60 * 60 * 1000;

// ── V17 Phase F — graduation rules (`adopt.*`) ──────────────────────────
//
// These propose ENABLING a token-saving feature once its precondition data
// says it would help AND (for the advisor) the harness contract it depends on
// is *proven* (`e1_pass`, not merely unverified). Same versioned-id + dismiss
// discipline as the tuning rules; behind the same global `MIN_SESSIONS` floor.

/// Propose turning the read advisor ON when the project keeps redundantly
/// re-reading large files with no edit in between and the E1 deny-reason
/// contract is verified. Signature = rounded redundant-reads-per-session.
pub const RULE_ADOPT_ADVISOR: &str = "adopt.read_advisor.v1";
/// Propose upgrading the advisor from advise-only to substitute mode when
/// reminders almost never lead to a full re-read (the outline is enough) and
/// shell bypass is low. Signature = the bucketed reread rate.
pub const RULE_ADOPT_SUBSTITUTE: &str = "adopt.read_advisor_substitute.v1";

/// `adopt.read_advisor.v1`: redundant same-file re-reads per session at or
/// above this proposes enabling the advisor.
const ADOPT_REDUNDANT_HIGH: f64 = 3.0;
/// `adopt.read_advisor.v1`: distinct sessions of evidence required — a full
/// window's worth (the caller passes `last_sessions = 10`).
const ADOPT_MIN_SESSIONS: u64 = 10;
/// `adopt.read_advisor_substitute.v1`: a remind→full-reread rate at or below
/// this means the outline is evidently enough — safe to substitute the body.
/// Deliberately disjoint from `REREAD_HIGH` (0.5), so this rule and
/// `RULE_ADVISOR_LINES` can never fire on the same rate (pinned by a test).
const SUBSTITUTE_REREAD_LOW: f64 = 0.2;
/// `adopt.read_advisor_substitute.v1`: reminders observed before the low
/// reread rate is trustworthy (same 20 as the tuning rule's `MIN_REMINDS`).
const SUBSTITUTE_MIN_SAMPLES: u64 = 20;

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
pub const RULE_DRIFT_SUBAGENT: &str = "drift.subagent_transcripts.v1";

// ── V32 Phase C3 — detection-updater canaries ───────────────────────────
//
// Locked decision 13's "every signal has its consumer": the updater records
// what it found and what it refused, and these two rules are what turn those
// records into something the user actually sees. Both are warn-only — the fix
// is a button in Settings → Tools → Detection, not a settings write the Apply
// path could make — and both carry their own trigger (a fact, not a statistic),
// so neither sits behind the global `MIN_SESSIONS` floor.

/// A newer detection bundle exists and was not applied: the component is in
/// `check-only` mode, or its `min_app_version` is ahead of this build.
/// Signature = `component:version`,
/// so a dismissal holds for THAT bundle and re-fires on the next one.
pub const RULE_DETECTION_UPDATE_AVAILABLE: &str = "detection.update_available.v1";
/// The last update attempt was **refused** — bad checksum, a bundle that would
/// not compile, a failed smoke control, an artifact pointing outside the
/// curated directory. The old data is still live (the updater never degrades to
/// no-detection), but a component that keeps refusing every bundle is silently
/// freezing, which is exactly what a card is for.
///
/// Signature = `component:<the updater's failure signature>` — the bundle
/// version when the refusal had one, else a digest of the reason. It used to be
/// `component:version` with an empty version on every manifest-level failure,
/// so one dismissal silenced every later refusal including a containment
/// violation (#46).
///
/// This rule fires ONLY for refusals. A channel that cannot be reached is
/// [`RULE_DETECTION_UPDATE_STALLED`]'s business, and only after a week of it.
pub const RULE_DETECTION_UPDATE_FAILED: &str = "detection.update_failed.v1";
/// A component has not got any fresher for
/// [`updater::STALLED_AFTER_CHECKS`](crate::offload::detection::updater::STALLED_AFTER_CHECKS)
/// consecutive checks — a week at the default interval — **whatever the
/// reason**.
///
/// The honest version of what #46's rejection cards were trying to say. One
/// failed check is weather and says nothing; a week of them means this
/// component has stopped getting fresher, which is the failure decision 13
/// exists to prevent.
///
/// Outcome-agnostic since #48, and that is the point: the two rules above both
/// stop firing once dismissed for their condition, so a channel that is
/// reachable and refuses every bundle it serves could otherwise freeze a
/// component permanently with no signal at all. This signature buckets the
/// streak by the threshold, so a dismissal holds for roughly another week and
/// then re-raises, and any check that comes back current resets the streak and
/// starts the count over.
pub const RULE_DETECTION_UPDATE_STALLED: &str = "detection.update_stalled.v1";
/// The signature layer is switched ON and has **nothing to match with** — the
/// rules directory is unreadable, or every file in it was rejected, so
/// `files_loaded == 0 || rules == 0` (#48, D-2).
///
/// The fourth rule, and the one that was missing. The other three all speak
/// about the update CHANNEL; none of them reads
/// [`signature::status`](crate::offload::detection::signature::status), so a
/// disarmed layer had no consumer anywhere: the reduced-protection badge is
/// derived from settings toggles and rendered full protection, no activity row
/// is written on reload, and `detection.update_stalled.v1` says in so many
/// words *"Nothing is degraded — the data you have is still live and still
/// scanning."* The only signal was `files_loaded: 0` in a Settings panel nobody
/// had open, while every screened page came back clean.
///
/// Decision 13: *"A failed validation surfaces an Advisor card and keeps the
/// old data — never silently degrades to no-detection."* The updater's path
/// honoured that; the plain reload path did not, and this rule is what makes
/// the sentence true on both.
///
/// Warn-only, and signed by the directory plus the file count, so a dismissal
/// holds for the state the user looked at and re-raises when it changes.
pub const RULE_DETECTION_SIGNATURE_DOWN: &str = "detection.signature_down.v1";

/// `detection.local_rules_broken.v1`: the user's own rules in `rules.d/local/`
/// need their attention — one does not compile and is being skipped, or one is
/// live under a **renamed** identifier because a shipped rule took the name it
/// declares (#48, M-13).
///
/// The consumer for the second half of #48's U-4. The first half stopped a
/// broken `local/` file from vetoing every bundle update forever; that fix has
/// to come with a way for the user to find out the file is broken, or it just
/// trades a loud wrong signal for a quiet absent one — a `warn!` in a log
/// nobody opens, and rules the user believes are protecting them that are not.
/// M-13 rides the same card for the same reason: a rename is a silent success
/// otherwise, and a silent success is what every finding in this milestone
/// turned out to be.
///
/// The two are described in their own words inside the card, never folded into
/// one sentence: a renamed rule IS matching, and saying otherwise would be the
/// same class of lie the card exists to stop.
///
/// Warn-only and signed by the failing file names AND the renames, so a
/// dismissal holds for the state the user looked at and re-raises when either
/// set changes. Deliberately suppressed while `detection.signature_down.v1` is
/// up: the disarmed-layer card is louder and about the same folder.
pub const RULE_DETECTION_LOCAL_RULES_BROKEN: &str = "detection.local_rules_broken.v1";
/// `detection.rules_incomplete.v1`: a rollback could not put every file back,
/// so `rules.d` is **missing** shipped rule files that still exist in the
/// retained copy (#48, M-11).
///
/// The consumer for the one updater failure mode that permanently reduces
/// coverage while every other surface reports success. `restore_archived`
/// swallowed a per-file failure with a `warn!`; the caller said "the previous
/// version was restored"; and the post-rollback health check could not
/// disagree, because a file that is absent produces no compile error and no
/// `files_failed` — `signature::Status::healthy` was true about a set that had
/// silently lost a file.
///
/// Distinct from `detection.signature_down.v1` (the layer has NOTHING to match
/// with) and from `detection.local_rules_broken.v1` (a file the user wrote is
/// on disk and broken): here the layer is armed, every file present compiles,
/// and the problem is one nobody can see by looking at what is there.
///
/// Signed by the missing file names, so a dismissal holds for the set the user
/// looked at and re-raises if it grows. Not `warn_only`-quiet about the fix: the
/// updater retries the restore on every check and every launch, so the usual
/// resolution is "close whatever is holding the file open and restart".
pub const RULE_DETECTION_RULES_INCOMPLETE: &str = "detection.rules_incomplete.v1";

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
    /// The user's applied-proposal records (`Settings::advisor_applied`),
    /// ALREADY filtered to this root by the caller — the advisor never sees
    /// another project's cooldowns. Each holds its rule quiet until
    /// [`APPLY_COOLDOWN_SESSIONS`] sessions after the apply.
    pub applied: Vec<AppliedRule>,

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
    /// V17.1 (`drift.subagent_transcripts.v1`): summaries of
    /// `subagent_drift` Activity events this run — the Claude OOB tap
    /// reporting that the sub-agent transcript contract moved again
    /// (transcripts neither inline nor under `subagents/*.jsonl`, or the
    /// launcher tool renamed). Empty = healthy.
    pub subagent_drift: Vec<String>,
    /// Feature 4 (`drift.read_bypass.v1`): share of reminders answered with
    /// a shell read of the same file within the bypass window, plus the
    /// remind count backing it. `None` when the advisor never reminded.
    pub bypass_rate: Option<f64>,
    pub bypass_samples: u64,

    // ── V17 Phase E — lean tool surface (`surface.lean.v1`) ─────────────
    /// Calls to any `graph::LEAN_HIDDEN` tool observed in the Activity ring
    /// within the trailing `HIDEABLE_RECENCY_WINDOW_MS` (NOT all-time — the ring
    /// is count-capped, so an ancient one-off call must not suppress the rule
    /// forever). `0` is the fire condition; any recent call silences the rule.
    pub hideable_tool_calls: u64,
    /// The measured advertised MCP tool-surface size in chars
    /// (`graph::surface_stats().mcp_chars`) — cited in the rule's rationale so
    /// the number is honest and current. `evaluate` stays pure: the caller
    /// measures this, the rule only formats it.
    pub surface_chars: u64,

    // ── V17 Phase F — graduation rules (`adopt.*`) ──────────────────────
    /// Phase F1: redundant same-file re-read PAIRS per session over the last
    /// 10 sessions (pairs ÷ sessions scanned, from
    /// `GraphIndex::redundant_read_candidates`). `None` when this project has
    /// recorded no reads. Drives `adopt.read_advisor.v1`.
    pub redundant_reads_per_session: Option<f64>,
    /// Phase F1: distinct sessions backing `redundant_reads_per_session` — its
    /// own sample floor (the rule wants ≥10 sessions of evidence).
    pub redundant_read_sessions: u64,
    /// Phase F2: STRICTLY `harness_versions.e1_status` trimmed+lowercased ==
    /// `"pass"` — NOT merely `!e1_blocked()`. "Verified OK" must mean *proven*:
    /// an `"unverified"` E1 (the default) must never auto-graduate a hook we've
    /// never seen work. Gates `adopt.read_advisor.v1`.
    pub e1_pass: bool,

    // ── V32 Phase C3 — detection updater ────────────────────────────────
    /// Components with a newer bundle recorded but not taken (check-only mode,
    /// or blocked by `min_app_version`). Read from the updater's state file via
    /// `detection::updater::advisor_signals`; empty is the healthy steady state
    /// (`auto` applies and clears it, so a standing entry means a decision is
    /// waiting).
    pub detection_updates: Vec<crate::offload::detection::updater::AvailableUpdate>,
    /// Components whose last update attempt was **refused**, with the reason.
    /// The old data is still live either way — this reports a component that is
    /// freezing, not one that broke. A channel that could not be reached is
    /// deliberately NOT here (#46): nothing was refused, so nothing to report.
    pub detection_update_failures: Vec<crate::offload::detection::updater::FailedUpdate>,
    /// Components that have not got any fresher for
    /// `updater::STALLED_AFTER_CHECKS` consecutive checks, for whatever reason
    /// — unreachable, refusing, or offered nothing takeable. The updater applies
    /// the threshold and suppresses the entry while a takeable offer stands; a
    /// non-empty entry here already means "long enough to mean it, and nothing
    /// else is saying it".
    pub detection_update_stalled: Vec<crate::offload::detection::updater::StalledUpdate>,
    /// The signature layer switched on with no rules to match against — the
    /// consumer for `signature::reload` failing (#48, D-2). `None` is both
    /// healthy states: rules are live, or the user switched the layer off.
    /// Unlike the three above this is a fact about the DATA ON DISK rather than
    /// about the update channel, which is why nothing else here could stand in
    /// for it.
    pub detection_signature_down: Option<crate::offload::detection::signature::SignatureDown>,
    /// A user rule file in `rules.d/local/` that does not compile (#48, U-4).
    /// Distinct from the field above: there the layer has NOTHING to match with,
    /// here it is matching fine and a file the user wrote is being skipped. It
    /// only became a silent condition once U-4 stopped letting it veto the
    /// update channel — before that it was loud, and wrong about why.
    pub detection_local_rules_broken: Option<crate::offload::detection::updater::BrokenLocalRules>,
    /// Components whose live rule directory is SHORT of files a rollback could
    /// not put back (#48, M-11). Unlike the field above, the files are not
    /// broken — they are absent, which is why nothing that compiles what is on
    /// disk can report it. Empty is the healthy steady state.
    pub detection_rules_incomplete: Vec<crate::offload::detection::updater::RulesIncomplete>,
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
    dismissed
        .iter()
        .any(|d| d.rule_id == rule_id && d.signature == signature)
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
    if sig.session_count >= MIN_SESSIONS {
        out.extend(tuning_rules(sig, advisor_disable_proposed));
        // V17 Phase F graduation rules share the tuning floor: enabling a
        // feature project-wide is as consequential as retuning one.
        out.extend(adopt_rules(sig, advisor_disable_proposed));
    }

    // V17 Phase E — the lean-surface rule carries its OWN session floor
    // (`SURFACE_LEAN_MIN_SESSIONS`, higher than the tuning floor), so it's
    // evaluated unconditionally like the drift canaries rather than under the
    // `MIN_SESSIONS` gate.
    out.extend(surface_rules(sig));

    // Apply cooldown, last so it covers every rule class uniformly: a rule
    // the user just applied doesn't speak again until the root has seen
    // APPLY_COOLDOWN_SESSIONS further sessions — the rates it would re-judge
    // are cumulative and still dominated by pre-apply data. Warn-only rules
    // have no Apply and thus never accrue a record; they pass untouched.
    out.retain(|p| !in_apply_cooldown(sig, p.rule_id));
    out
}

/// Whether `rule_id` was applied recently enough that it must stay quiet.
/// `saturating_add` so a hand-edited huge stored count can't wrap; a stored
/// count AHEAD of the live one (root DB pruned/rebuilt) just extends the
/// cooldown until the count catches back up — fail-quiet, matching the
/// advisor's posture everywhere else.
fn in_apply_cooldown(sig: &Signals, rule_id: &str) -> bool {
    sig.applied.iter().any(|a| {
        a.rule_id == rule_id
            && sig.session_count < a.session_count.saturating_add(APPLY_COOLDOWN_SESSIONS)
    })
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
                current: format!(
                    "{} Claude sessions without token fields",
                    sig.claude_sessions
                ),
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

    // V17.1 — drift.subagent_transcripts.v1: the Claude OOB tap reported
    // that sub-agent traffic is visible in neither of the two known
    // transcript locations (or the launcher tool was renamed). One event is
    // enough — the tap rate-limits itself to one report per session, and a
    // moved contract is a fact. Signature = the seen Claude version, so a
    // dismissal holds until the next harness update (same boundary as the
    // version tripwire — this drift IS a harness-update symptom).
    if !sig.subagent_drift.is_empty() {
        let mut what = sig.subagent_drift.clone();
        what.sort();
        what.dedup();
        let signature = version_signature(&sig.claude_last_seen);
        if !is_dismissed(&sig.dismissed, RULE_DRIFT_SUBAGENT, &signature) {
            out.push(Proposal {
                setting: String::new(),
                current: what.join(", "),
                proposed: "sub-agent transcripts tailed (usage + agents-active tracked)"
                    .to_string(),
                rationale: format!(
                    "The Claude transcript tap reported sub-agent contract drift this run: \
                     {}. Until the tail is re-pointed, sub-agent token spend may be missing \
                     from the Usage section and/or the agents-active avatar hold may be \
                     dead — verify the transcript layout per MAINTENANCE.md.",
                    what.join("; ")
                ),
                rule_id: RULE_DRIFT_SUBAGENT,
                signature,
                warn_only: true,
                action: None,
            });
        }
    }

    // V32 C3 — detection.update_available.v1: a newer detection bundle was
    // found and not taken. This is the consumer for check-only mode: without
    // it, "check-only" would mean "silently record a version nobody reads".
    // Warn-only — Apply lives in Settings → Tools → Detection, next to the
    // versions and the revert button, because taking an update is not a
    // settings write.
    for u in &sig.detection_updates {
        let signature = format!("{}:{}", u.component, u.available);
        if is_dismissed(&sig.dismissed, RULE_DETECTION_UPDATE_AVAILABLE, &signature) {
            continue;
        }
        out.push(Proposal {
            setting: String::new(),
            current: if u.installed.is_empty() {
                "(shipped)".to_string()
            } else {
                u.installed.clone()
            },
            proposed: u.available.clone(),
            rationale: format!(
                "A newer injection-detection {} bundle ({}) is available and was not applied — \
                 this component is set to check-only, or the bundle needs a newer cImp. Detection \
                 data decays without updates, so a component that keeps declining them slowly \
                 stops matching what attackers actually send.{} Review and take it from Settings \
                 → Tools → Detection (Apply), or switch the component to auto.",
                u.component,
                u.available,
                if u.notes.trim().is_empty() {
                    String::new()
                } else {
                    format!(" Curator's note: {}.", u.notes.trim())
                }
            ),
            rule_id: RULE_DETECTION_UPDATE_AVAILABLE,
            signature,
            warn_only: true,
            action: None,
        });
    }

    // V32 C3 — detection.update_failed.v1: a bundle was refused. Nothing
    // broke (the old data is still live and the updater never degrades to
    // no-detection), but a component whose every candidate is rejected is
    // frozen, and freezing quietly is the failure this rule exists to prevent.
    for f in &sig.detection_update_failures {
        // `f.signature`, not `f.version`: a manifest-level refusal has no
        // version, and signing those `component:` made one dismissal a
        // permanent mute on every later refusal (#46).
        let signature = format!("{}:{}", f.component, f.signature);
        if is_dismissed(&sig.dismissed, RULE_DETECTION_UPDATE_FAILED, &signature) {
            continue;
        }
        out.push(Proposal {
            setting: String::new(),
            current: format!("{} update rejected", f.component),
            proposed: "a bundle that passes validation".to_string(),
            rationale: format!(
                "The injection-detection {} update{} was REJECTED before activation, and the \
                 previous data is still live: {}. Nothing is degraded right now, but this \
                 component is not getting fresher either — check the manifest/bundle per \
                 MAINTENANCE.md → \"Detection updater\", then re-run Check now.",
                f.component,
                if f.version.is_empty() {
                    String::new()
                } else {
                    format!(" to `{}`", f.version)
                },
                f.reason
            ),
            rule_id: RULE_DETECTION_UPDATE_FAILED,
            signature,
            warn_only: true,
            action: None,
        });
    }

    // V32 C3 / #46, #48 — detection.update_stalled.v1: a week of checks has
    // left this component no fresher. Nothing is broken, so this is NOT a
    // rejection card; it is the freshness canary decision 13 asks for, firing
    // at the point where "one bad check" has become "this component has stopped
    // getting fresher". It says nothing about the CAUSE — an outage and a
    // channel that refuses everything it serves are the same event from here —
    // because the cause is in the last outcome, quoted verbatim, and because
    // the two rules that do speak to cause can both be dismissed away.
    // Any check that comes back current resets the streak, so this cannot fire
    // on a working install.
    for s in &sig.detection_update_stalled {
        // Bucketed by the threshold: dismissing holds for roughly another
        // week's worth of failures rather than forever, and a recovery resets
        // the streak so a fresh outage starts the count (and the signature)
        // over.
        let signature = format!(
            "{}:{}",
            s.component,
            s.streak / crate::offload::detection::updater::STALLED_AFTER_CHECKS
        );
        if is_dismissed(&sig.dismissed, RULE_DETECTION_UPDATE_STALLED, &signature) {
            continue;
        }
        out.push(Proposal {
            setting: String::new(),
            current: format!("{} updates have stalled", s.component),
            proposed: "a component that is getting fresher".to_string(),
            rationale: format!(
                "The injection-detection {} component has not taken an update for {} checks in a \
                 row, so it is no longer getting fresher. Nothing is degraded — the data you have \
                 is still live and still scanning — but signatures and weights decay, so a \
                 component that stays frozen eventually means missing what attackers have started \
                 sending. The last check said: {}. If that is a network or proxy problem, check \
                 access to the manifest URL shown in Settings → Tools → Detection; if a bundle is \
                 being refused, see MAINTENANCE.md → \"Detection updater\"; or set the component \
                 to `off` if this machine is deliberately offline.",
                s.component, s.streak, s.reason
            ),
            rule_id: RULE_DETECTION_UPDATE_STALLED,
            signature,
            warn_only: true,
            action: None,
        });
    }

    // V32 Phase C / #48 — detection.signature_down.v1: the signature layer is
    // ON and has nothing to match with. The one detection card that is about
    // the DATA rather than the channel, and the consumer decision 13's
    // "never silently degrades to no-detection" was missing: without it a
    // rules directory that compiles to nothing reports every page clean while
    // the badge, the activity feed and the three cards above all say the layer
    // is fine. Warn-only — the fix is a file on disk (or Reload rules), not a
    // settings write.
    if let Some(d) = &sig.detection_signature_down {
        // Signed by what the user would look at: the directory plus the counts.
        // A dismissal holds for that state and re-raises when it changes, which
        // includes the case where a partial recovery leaves the layer still
        // disarmed for a different reason.
        let signature = format!("{}:{}:{}", d.dir, d.files_loaded, d.files_failed);
        if !is_dismissed(&sig.dismissed, RULE_DETECTION_SIGNATURE_DOWN, &signature) {
            out.push(Proposal {
                setting: String::new(),
                current: format!(
                    "{} file(s) loaded, {} rule(s){}",
                    d.files_loaded,
                    d.rules,
                    if d.failed.is_empty() {
                        String::new()
                    } else {
                        format!(" — {} rejected: {}", d.files_failed, d.failed.join(", "))
                    }
                ),
                proposed: "a rules directory that compiles".to_string(),
                rationale: format!(
                    "The injection-detection SIGNATURE layer is switched on and has no rules to \
                     match against, so every external result it screens comes back clean because \
                     there is nothing to compare it with — not because it is. cImp looked in {}. \
                     If a previously loaded set is still in memory it keeps screening with that, \
                     but it will not survive a restart. Fix or remove the rejected file(s) — a \
                     broken rule in `rules.d/local/` is the usual cause — then press Reload rules \
                     in Settings → Tools → Detection.",
                    d.dir
                ),
                rule_id: RULE_DETECTION_SIGNATURE_DOWN,
                signature,
                warn_only: true,
                action: None,
            });
        }
    }

    // V32 Phase C3 / #48 U-4 — detection.local_rules_broken.v1: a rule file the
    // USER wrote is on disk and does not compile, so it is skipped while the
    // rest of the layer runs. Suppressed while the card above is up (same
    // folder, louder problem) — the updater's `broken_local_rules` applies that
    // suppression at the source, so the two can never both fire.
    if let Some(b) = &sig.detection_local_rules_broken {
        // The signature covers BOTH lists (#48, M-13): a dismissal of "your
        // file does not compile" must not also silence "and this other rule of
        // yours is now live under a different name", and a rename appearing
        // later must re-raise a card the user already dismissed.
        let renamed_sig: Vec<String> = b.renamed.iter().map(|r| r.describe()).collect();
        // Unchanged when there are no renames, so an existing dismissal of the
        // broken-file card survives this change rather than re-firing once for
        // everyone.
        let signature = if renamed_sig.is_empty() {
            b.failed.join(",")
        } else {
            format!("{}|{}", b.failed.join(","), renamed_sig.join(","))
        };
        if !is_dismissed(
            &sig.dismissed,
            RULE_DETECTION_LOCAL_RULES_BROKEN,
            &signature,
        ) {
            // Two conditions, described in their own words. Folding them into
            // one sentence is how a rule that IS matching ends up reported as
            // "not screening anything" — the family of bug this milestone keeps
            // finding, in miniature.
            let mut current = Vec::new();
            if !b.failed.is_empty() {
                current.push(format!(
                    "{} rejected: {}",
                    b.failed.len(),
                    b.failed.join(", ")
                ));
            }
            if !b.renamed.is_empty() {
                current.push(format!(
                    "{} renamed: {}",
                    b.renamed.len(),
                    renamed_sig.join(", ")
                ));
            }
            // The counts ride the `current` line in both cases, because without
            // them the card reads as an outage.
            current.push(format!(
                "{} file(s), {} rule(s) still live",
                b.files_loaded, b.rules
            ));
            let mut rationale = String::new();
            if !b.failed.is_empty() {
                rationale.push_str(
                    "Your own detection rule file(s) in `rules.d/local/` do not compile, so they \
                     are being skipped: the patterns you wrote are not screening anything, while \
                     the rest of the layer runs normally and reports nothing wrong. A file is also \
                     rejected when it collides on a rule IDENTIFIER that cImp could not rename \
                     around — another compile error in the same file, or every renamed form taken \
                     too. ",
                );
            }
            if !b.renamed.is_empty() {
                rationale.push_str(
                    "A rule you wrote in `rules.d/local/` declares an IDENTIFIER the shipped \
                     bundle now also uses, and YARA identifiers must be unique across the set. \
                     Rather than drop your rule or refuse the update, cImp loaded your rule under \
                     a `custom_` identifier — it is live and still matching, but a hit reports the \
                     renamed identifier, so anything of yours keyed on the old name (a saved \
                     search, a log filter) will not see it. Your file on disk was NOT modified. \
                     Renaming the rule in your own file takes the name back. ",
                );
            }
            rationale.push_str(&format!(
                "cImp looked in {}. After editing, press Reload rules in Settings → Tools → \
                 Detection.",
                b.dir
            ));
            out.push(Proposal {
                setting: String::new(),
                current: current.join("; "),
                proposed: "a `rules.d/local/` that compiles under its own names".to_string(),
                rationale,
                rule_id: RULE_DETECTION_LOCAL_RULES_BROKEN,
                signature,
                warn_only: true,
                action: None,
            });
        }
    }

    // V32 Phase C3 / #48 M-11 — detection.rules_incomplete.v1: a rollback could
    // not put every file back, so the live set is genuinely short. NOT
    // suppressed by any of the cards above: this one is the only signal in the
    // module that says something is degraded RIGHT NOW, and the refusal card it
    // most often accompanies says the opposite in so many words.
    for r in &sig.detection_rules_incomplete {
        let signature = format!("{}:{}", r.component, r.files.join(","));
        if is_dismissed(&sig.dismissed, RULE_DETECTION_RULES_INCOMPLETE, &signature) {
            continue;
        }
        out.push(Proposal {
            setting: String::new(),
            current: format!("{} missing: {}", r.component, r.files.join(", ")),
            proposed: "a complete rule set".to_string(),
            rationale: format!(
                "An interrupted or failed detection update could not put {} rule file(s) back \
                 into {} ({}), so the signature layer is running with FEWER rules than it should \
                 — a real, current reduction in coverage, not a stale warning. The files are not \
                 lost: they are still in the retained copy under `detection-updates/previous/`, \
                 and cImp retries the restore on every update check and every launch. The usual \
                 cause is something holding the file open — antivirus real-time scanning, an \
                 editor, or a file manager sitting in that folder. Close it and restart cImp; if \
                 that does not clear it, Revert from Settings → Tools → Detection restores the \
                 retained copy whole.",
                r.files.len(),
                r.dir,
                r.files.join(", ")
            ),
            rule_id: RULE_DETECTION_RULES_INCOMPLETE,
            signature,
            warn_only: true,
            action: None,
        });
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
                             overhead and loses memory's read tracking. With V17 shell \
                             interception live, whole-file reads (`cat`, `Get-Content`) are \
                             already caught, so a persistently high rate points at RESIDUAL \
                             escape routes (`sed -n`, `head`, `tail`, redirections) the strict \
                             parser can't intercept — better off disabled.",
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

/// The V17 Phase E lean-surface rule. Carries its own session floor
/// (`SURFACE_LEAN_MIN_SESSIONS`); a single call to any hidden tool WITHIN THE
/// last `HIDEABLE_RECENCY_WINDOW_DAYS` moves `hideable_tool_calls` off zero and
/// silences it (fixed `"zero-usage"` signature — no rate to bucket).
fn surface_rules(sig: &Signals) -> Vec<Proposal> {
    let mut out = Vec::new();
    if !sig.graph.lean_tools
        && sig.session_count >= SURFACE_LEAN_MIN_SESSIONS
        && sig.hideable_tool_calls == 0
    {
        let signature = "zero-usage".to_string();
        if !is_dismissed(&sig.dismissed, RULE_SURFACE_LEAN, &signature) {
            out.push(Proposal {
                setting: "graph.lean_tools".to_string(),
                current: "false".to_string(),
                proposed: "true".to_string(),
                rationale: format!(
                    "None of the cold-tail graph tools (graph_cycles, graph_dead_exports, \
                     graph_struct_search, graph_path, graph_architecture) were called in the last \
                     {} days (over {} sessions of use) — hiding them trims the tool descriptors \
                     from the ~{} chars (est.) cache-written once per session. \
                     Advertisement-only: each still answers if called by name, so nothing breaks.",
                    HIDEABLE_RECENCY_WINDOW_DAYS, sig.session_count, sig.surface_chars
                ),
                rule_id: RULE_SURFACE_LEAN,
                signature,
                warn_only: false,
                action: None,
            });
        }
    }
    out
}

/// The V17 Phase F graduation rules (`adopt.*`). Behind the global
/// cold-start floor (enforced by the caller, same as the tuning rules).
/// `advisor_disable_proposed` suppresses `adopt.read_advisor.v1` when a drift
/// rule already proposed turning the advisor off — never propose ENABLING
/// what drift says is broken.
fn adopt_rules(sig: &Signals, advisor_disable_proposed: bool) -> Vec<Proposal> {
    let mut out = Vec::new();

    // adopt.read_advisor.v1 — the advisor is off, the project keeps redundantly
    // re-reading big files (no edit in between), and the E1 deny-reason
    // contract is PROVEN (not merely unverified): propose turning it on.
    if !sig.graph.read_advisor && sig.e1_pass && !advisor_disable_proposed {
        if let Some(rate) = sig.redundant_reads_per_session {
            if rate >= ADOPT_REDUNDANT_HIGH && sig.redundant_read_sessions >= ADOPT_MIN_SESSIONS {
                // Signature = rounded redundant-reads-per-session: a materially
                // changed rate (a different whole number of redundant re-reads
                // per session) re-fires past a dismissal; within-integer noise
                // stays suppressed.
                let signature = (rate.round() as u64).to_string();
                if !is_dismissed(&sig.dismissed, RULE_ADOPT_ADVISOR, &signature) {
                    out.push(Proposal {
                        setting: "graph.read_advisor".to_string(),
                        current: "false".to_string(),
                        proposed: "true".to_string(),
                        rationale: format!(
                            "This project redundantly re-read the same large files ~{:.1} times \
                             per session across {} sessions with no edit in between (est. — \
                             external tools may have changed the file between reads), and the E1 \
                             deny-reason contract is verified — turning the read advisor on would \
                             substitute an outline for those repeat full reads.",
                            rate, sig.redundant_read_sessions
                        ),
                        rule_id: RULE_ADOPT_ADVISOR,
                        signature,
                        warn_only: false,
                        action: None,
                    });
                }
            }
        }
    }

    // adopt.read_advisor_substitute.v1 — the advisor is on in advise-only mode,
    // reminders almost never lead to a full re-read (the outline is enough),
    // and shell bypass is low: propose upgrading to substitute mode (inject the
    // outline body in place of the repeat read). Mutually exclusive with
    // `RULE_ADVISOR_LINES` by construction — that rule needs reread_rate ≥
    // REREAD_HIGH (0.5), this needs ≤ SUBSTITUTE_REREAD_LOW (0.2); the ranges
    // can't overlap (pinned by `substitute_and_min_lines_rules_never_co_fire`).
    if sig.graph.read_advisor && sig.graph.read_advisor_mode == "advise" {
        if let Some(rate) = sig.advisor_reread_rate {
            // A missing bypass rate (the advisor never bypassed this run) is
            // "not high", so it doesn't block the upgrade.
            let bypass_ok = sig.bypass_rate.is_none_or(|b| b < BYPASS_HIGH);
            if rate <= SUBSTITUTE_REREAD_LOW
                && sig.advisor_reread_samples >= SUBSTITUTE_MIN_SAMPLES
                && bypass_ok
            {
                let signature = bucket10(rate);
                if !is_dismissed(&sig.dismissed, RULE_ADOPT_SUBSTITUTE, &signature) {
                    out.push(Proposal {
                        setting: "graph.read_advisor_mode".to_string(),
                        current: sig.graph.read_advisor_mode.clone(),
                        proposed: "substitute".to_string(),
                        rationale: format!(
                            "Only {:.0}% of read-advisor reminders were followed by a full \
                             re-read (n={} reminders) and shell bypass is low — the outline is \
                             evidently enough, so switching from advise to substitute mode can \
                             inject the outline body in place of the repeat read.",
                            rate * 100.0,
                            sig.advisor_reread_samples
                        ),
                        rule_id: RULE_ADOPT_SUBSTITUTE,
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
        if !advisor_disable_proposed
            && sig.advisor_reread_samples >= MIN_REMINDS
            && rate >= REREAD_HIGH
        {
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
        if sig.injection_follow_samples >= MIN_INJECTIONS && sig.budget_maxed_samples >= MIN_TURNS {
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
        assert_eq!(
            ids,
            vec![RULE_MIN_SCORE, RULE_ADVISOR_LINES, RULE_TURN_BUDGET]
        );
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
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADVISOR_LINES));

        let mut sig = extreme_signals();
        sig.advisor_reread_rate = Some(0.1); // below REREAD_HIGH
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADVISOR_LINES));
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
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADVISOR_LINES));

        let mut sig = extreme_signals();
        sig.budget_maxed_rate = None;
        assert!(!evaluate(&sig).iter().any(|p| p.rule_id == RULE_TURN_BUDGET));
    }

    #[test]
    fn apply_cooldown_suppresses_the_applied_rule_until_enough_new_sessions() {
        // Applied at the current session count: quiet, even at an extreme rate.
        let mut sig = extreme_signals();
        sig.applied = vec![AppliedRule {
            rule_id: RULE_MIN_SCORE.to_string(),
            root: String::new(), // root-filtering is the caller's job
            session_count: sig.session_count,
        }];
        let ids: Vec<&str> = evaluate(&sig).iter().map(|p| p.rule_id).collect();
        assert!(
            !ids.contains(&RULE_MIN_SCORE),
            "freshly applied rule must stay quiet"
        );
        // Other rules are untouched by another rule's cooldown.
        assert!(ids.contains(&RULE_ADVISOR_LINES));
        assert!(ids.contains(&RULE_TURN_BUDGET));

        // One session short of expiry: still quiet.
        let mut sig_short = extreme_signals();
        sig_short.session_count = MIN_SESSIONS + APPLY_COOLDOWN_SESSIONS - 1;
        sig_short.applied = vec![AppliedRule {
            rule_id: RULE_MIN_SCORE.to_string(),
            root: String::new(),
            session_count: MIN_SESSIONS,
        }];
        assert!(!evaluate(&sig_short)
            .iter()
            .any(|p| p.rule_id == RULE_MIN_SCORE));

        // Cooldown elapsed: re-fires (any bucket — an apply is not a dismissal).
        let mut sig_done = extreme_signals();
        sig_done.session_count = MIN_SESSIONS + APPLY_COOLDOWN_SESSIONS;
        sig_done.applied = vec![AppliedRule {
            rule_id: RULE_MIN_SCORE.to_string(),
            root: String::new(),
            session_count: MIN_SESSIONS,
        }];
        assert!(evaluate(&sig_done)
            .iter()
            .any(|p| p.rule_id == RULE_MIN_SCORE));
    }

    #[test]
    fn apply_cooldown_covers_drift_disable_proposals_too() {
        // Applying "disable read_advisor" flips the setting, which already
        // gates the rule off — but if the user re-enables it within the
        // cooldown, the drift rule must not fire again off the same stale rate.
        let mut sig = read_reason_signals();
        sig.applied = vec![AppliedRule {
            rule_id: RULE_DRIFT_READ_REASON.to_string(),
            root: String::new(),
            session_count: sig.session_count,
        }];
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_READ_REASON));
    }

    #[test]
    fn dismissal_suppresses_the_same_bucket_but_a_changed_bucket_refires() {
        let mut sig = extreme_signals(); // unused = 1.0 -> bucket "10"
        sig.dismissed = vec![DismissedRule {
            rule_id: RULE_MIN_SCORE.to_string(),
            signature: "10".to_string(),
        }];
        let ids: Vec<&str> = evaluate(&sig).iter().map(|p| p.rule_id).collect();
        assert!(
            !ids.contains(&RULE_MIN_SCORE),
            "same-bucket dismissal must suppress"
        );
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
            let val: u32 = p
                .proposed
                .parse()
                .expect("proposed value must be a u32 string");
            match p.setting.as_str() {
                "graph.context_min_score" => g.context_min_score = val,
                "graph.read_advisor_min_lines" => g.read_advisor_min_lines = val,
                "graph.context_turn_budget_chars" => g.context_turn_budget_chars = val,
                other => panic!("unrecognized proposal setting: {other}"),
            }
        }
        assert_ne!(
            (
                g.context_min_score,
                g.read_advisor_min_lines,
                g.context_turn_budget_chars
            ),
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
        assert!(!evaluate(&sig_eq)
            .iter()
            .any(|p| p.rule_id == RULE_TURN_BUDGET));

        // A comfortably large current still gets a real reduction proposed.
        let sig_large = extreme_signals(); // default context_turn_budget_chars = 6_000
        assert!(evaluate(&sig_large)
            .iter()
            .any(|p| p.rule_id == RULE_TURN_BUDGET));
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
        assert!(!evaluate(&sig_above)
            .iter()
            .any(|p| p.rule_id == RULE_MIN_SCORE));

        // Just below the ceiling: one more raise is still reasonable.
        let mut sig_below = extreme_signals();
        sig_below.graph.context_min_score = MIN_SCORE_CEILING - 1;
        assert!(evaluate(&sig_below)
            .iter()
            .any(|p| p.rule_id == RULE_MIN_SCORE));
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
        assert!(evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_VERSION));

        let sig_ok = Signals {
            claude_last_seen: "2.2.0".to_string(),
            claude_last_verified: "2.2.0".to_string(),
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
            claude_last_seen: "2.2.0".to_string(),
            claude_last_verified: "2.1.14".to_string(),
            dismissed: vec![DismissedRule {
                rule_id: RULE_DRIFT_VERSION.to_string(),
                signature: "2.2.0".to_string(),
            }],
            ..Signals::default()
        };
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_VERSION));

        // The NEXT version change re-fires despite the old dismissal.
        sig.claude_last_seen = "2.3.0".to_string();
        assert!(evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_VERSION));
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
            .find(|p| p.rule_id == RULE_DRIFT_READ_REASON)
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
            .any(|p| p.rule_id == RULE_DRIFT_READ_REASON));

        let mut sig = read_reason_signals();
        sig.advisor_reread_samples = DRIFT_MIN_REMINDS - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_READ_REASON));

        let mut sig = read_reason_signals();
        sig.advisor_reread_rate = Some(0.8); // high for tuning, below drift's 0.9
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_READ_REASON));
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
        assert!(
            !ids.contains(&RULE_ADVISOR_LINES),
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
            session_count: DRIFT_SILENT_MIN_SESSIONS,
            large_reread_pairs: DRIFT_SILENT_MIN_REREADS,
            remind_count: 0,
            claude_last_seen: "2.2.0".to_string(),
            graph,
            ..Signals::default()
        };
        let props = evaluate(&base);
        let p = props
            .iter()
            .find(|p| p.rule_id == RULE_DRIFT_HOOK_SILENT)
            .expect("fires");
        assert!(p.warn_only);
        assert!(p.setting.is_empty());
        assert_eq!(p.signature, "2.2.0"); // re-fires per harness version

        let mut sig = base.clone();
        sig.remind_count = 1; // one remind reached the loopback ⇒ hook alive
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_HOOK_SILENT));

        let mut sig = base.clone();
        sig.large_reread_pairs = DRIFT_SILENT_MIN_REREADS - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_HOOK_SILENT));

        let mut sig = base.clone();
        sig.session_count = DRIFT_SILENT_MIN_SESSIONS - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_HOOK_SILENT));

        let mut sig = base;
        sig.graph.read_advisor = false;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_HOOK_SILENT));
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
            .find(|p| p.rule_id == RULE_DRIFT_INJECTION_UNSEEN)
            .expect("fires");
        assert!(p.warn_only);

        // 10% follow is unhealthy but NOT "never reaches the model" — the
        // tuning rule's territory, not the drift rule's.
        let mut sig = base.clone();
        sig.injection_follow_rate = Some(0.10);
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_INJECTION_UNSEEN));

        let mut sig = base;
        sig.graph.context_injection = false;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_INJECTION_UNSEEN));
    }

    #[test]
    fn usage_fields_gone_fires_only_when_every_claude_session_is_tokenless() {
        let base = Signals {
            claude_sessions: 3,
            claude_tokenless_sessions: 3,
            claude_last_seen: "2.2.0".to_string(),
            ..Signals::default()
        };
        assert!(evaluate(&base)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_USAGE_FIELDS));

        // One healthy session ⇒ the schema didn't change, that session is
        // just odd.
        let mut sig = base.clone();
        sig.claude_tokenless_sessions = 2;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_USAGE_FIELDS));

        // Below the floor a single tokenless session could be a fluke.
        let mut sig = base;
        sig.claude_sessions = DRIFT_MIN_TOKENLESS - 1;
        sig.claude_tokenless_sessions = DRIFT_MIN_TOKENLESS - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_USAGE_FIELDS));
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
            .find(|p| p.rule_id == RULE_DRIFT_PAYLOAD)
            .expect("fires");
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
    fn subagent_drift_fires_on_any_subagent_drift_event() {
        let summary = "subagents/*.jsonl present but no Task/Agent launch tool_use recognized";
        let sig = Signals {
            subagent_drift: vec![summary.to_string()],
            claude_last_seen: "2.2.0".to_string(),
            ..Signals::default()
        };
        let props = evaluate(&sig);
        let p = props
            .iter()
            .find(|p| p.rule_id == RULE_DRIFT_SUBAGENT)
            .expect("fires");
        assert!(p.warn_only);
        assert!(p.rationale.contains(summary));
        // Version-keyed signature: a dismissal holds until the next harness
        // update re-fires the rule (same boundary as the version tripwire).
        assert_eq!(p.signature, "2.2.0");

        // Dismissed for this version ⇒ quiet.
        let mut sig = sig;
        sig.dismissed = vec![DismissedRule {
            rule_id: RULE_DRIFT_SUBAGENT.to_string(),
            signature: "2.2.0".to_string(),
        }];
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_SUBAGENT));

        // No events ⇒ silent.
        assert!(!evaluate(&Signals::default())
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_SUBAGENT));
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
            .find(|p| p.rule_id == RULE_DRIFT_READ_BYPASS)
            .expect("fires");
        assert_eq!(p.setting, "graph.read_advisor");
        assert_eq!(p.proposed, "false");

        let mut sig = base.clone();
        sig.bypass_rate = Some(0.3); // below BYPASS_HIGH
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_READ_BYPASS));

        let mut sig = base;
        sig.bypass_samples = DRIFT_MIN_BYPASS_REMINDS - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_DRIFT_READ_BYPASS));
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
            .find(|p| p.rule_id == RULE_DRIFT_READ_REASON)
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

    // ── V17 Phase E — surface.lean.v1 ───────────────────────────────────

    /// Signals for the lean-surface rule: enough sessions, zero hideable calls,
    /// lean off. `surface_chars` non-zero so the rationale reads honestly.
    fn surface_lean_signals() -> Signals {
        Signals {
            session_count: SURFACE_LEAN_MIN_SESSIONS,
            hideable_tool_calls: 0,
            surface_chars: 9_000,
            graph: GraphSettings::default(), // lean_tools = false
            ..Signals::default()
        }
    }

    #[test]
    fn surface_lean_fires_on_zero_calls_after_enough_sessions() {
        let props = evaluate(&surface_lean_signals());
        let p = props
            .iter()
            .find(|p| p.rule_id == RULE_SURFACE_LEAN)
            .expect("fires");
        assert_eq!(p.setting, "graph.lean_tools");
        assert_eq!(p.current, "false");
        assert_eq!(p.proposed, "true");
        assert_eq!(p.signature, "zero-usage");
        assert!(!p.warn_only);
        assert!(p.rationale.contains("9,000") || p.rationale.contains("9000"));

        // Real, assignable bool field (Apply-switch guard).
        let val: bool = p.proposed.parse().expect("bool string");
        let g = GraphSettings {
            lean_tools: val,
            ..GraphSettings::default()
        };
        assert!(g.lean_tools);
    }

    #[test]
    fn surface_lean_silenced_by_a_single_hideable_call() {
        let mut sig = surface_lean_signals();
        sig.hideable_tool_calls = 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_SURFACE_LEAN));
    }

    #[test]
    fn surface_lean_respects_its_session_floor() {
        let mut sig = surface_lean_signals();
        sig.session_count = SURFACE_LEAN_MIN_SESSIONS - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_SURFACE_LEAN));
    }

    #[test]
    fn surface_lean_silent_when_already_lean() {
        let mut sig = surface_lean_signals();
        sig.graph.lean_tools = true;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_SURFACE_LEAN));
    }

    #[test]
    fn surface_lean_honors_dismiss_and_apply_cooldown() {
        // Dismissed at its fixed signature ⇒ silent.
        let mut sig = surface_lean_signals();
        sig.dismissed = vec![DismissedRule {
            rule_id: RULE_SURFACE_LEAN.to_string(),
            signature: "zero-usage".to_string(),
        }];
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_SURFACE_LEAN));

        // Freshly applied ⇒ quiet until APPLY_COOLDOWN_SESSIONS more sessions.
        let mut sig = surface_lean_signals();
        sig.applied = vec![AppliedRule {
            rule_id: RULE_SURFACE_LEAN.to_string(),
            root: String::new(),
            session_count: sig.session_count,
        }];
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_SURFACE_LEAN));

        // Cooldown elapsed ⇒ re-fires.
        let mut sig = surface_lean_signals();
        sig.session_count = SURFACE_LEAN_MIN_SESSIONS + APPLY_COOLDOWN_SESSIONS;
        sig.applied = vec![AppliedRule {
            rule_id: RULE_SURFACE_LEAN.to_string(),
            root: String::new(),
            session_count: SURFACE_LEAN_MIN_SESSIONS,
        }];
        assert!(evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_SURFACE_LEAN));
    }

    // ── V17 Phase F — graduation rules (adopt.*) ────────────────────────

    /// Signals for `adopt.read_advisor.v1`: advisor OFF, E1 proven, a high
    /// redundant-read rate over a full window, past the tuning session floor.
    fn adopt_advisor_signals() -> Signals {
        Signals {
            session_count: MIN_SESSIONS,
            e1_pass: true,
            redundant_reads_per_session: Some(5.0),
            redundant_read_sessions: ADOPT_MIN_SESSIONS,
            graph: GraphSettings::default(), // read_advisor = false
            ..Signals::default()
        }
    }

    #[test]
    fn adopt_advisor_fires_and_proposes_enabling_a_real_bool_field() {
        let props = evaluate(&adopt_advisor_signals());
        let p = props
            .iter()
            .find(|p| p.rule_id == RULE_ADOPT_ADVISOR)
            .expect("fires");
        assert_eq!(p.setting, "graph.read_advisor");
        assert_eq!(p.current, "false");
        assert_eq!(p.proposed, "true");
        assert_eq!(p.signature, "5"); // 5.0 pairs/session rounded
        assert!(!p.warn_only);
        assert!(
            p.rationale
                .contains("external tools may have changed the file"),
            "must carry the est. caveat verbatim"
        );
        // Real, assignable bool field (Apply-switch guard).
        let val: bool = p.proposed.parse().expect("bool string");
        let g = GraphSettings {
            read_advisor: val,
            ..GraphSettings::default()
        };
        assert!(g.read_advisor);
    }

    #[test]
    fn adopt_advisor_respects_each_threshold() {
        // Rate below 3.0/session.
        let mut sig = adopt_advisor_signals();
        sig.redundant_reads_per_session = Some(2.9);
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_ADVISOR));

        // Fewer than 10 sessions of evidence.
        let mut sig = adopt_advisor_signals();
        sig.redundant_read_sessions = ADOPT_MIN_SESSIONS - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_ADVISOR));

        // No data at all.
        let mut sig = adopt_advisor_signals();
        sig.redundant_reads_per_session = None;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_ADVISOR));

        // Advisor already on.
        let mut sig = adopt_advisor_signals();
        sig.graph.read_advisor = true;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_ADVISOR));
    }

    #[test]
    fn adopt_advisor_stays_silent_until_e1_is_proven_pass() {
        // The default (`e1_pass = false`, i.e. "unverified"/"fail") must NOT
        // auto-graduate a hook we've never seen work — the whole point of the
        // strict `== "pass"` check rather than `!e1_blocked()`.
        let mut sig = adopt_advisor_signals();
        sig.e1_pass = false;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_ADVISOR));
    }

    #[test]
    fn adopt_advisor_suppressed_while_a_drift_read_rule_is_firing() {
        // The full `evaluate` path can't co-occur (drift-disable needs the
        // advisor ON, adopt needs it OFF), so pin the guard directly on
        // `adopt_rules`: `advisor_disable_proposed = true` must silence it.
        let sig = adopt_advisor_signals();
        assert!(adopt_rules(&sig, false)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_ADVISOR));
        assert!(!adopt_rules(&sig, true)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_ADVISOR));
    }

    #[test]
    fn adopt_advisor_dismissal_refires_on_a_changed_bucket() {
        let mut sig = adopt_advisor_signals(); // rate 5.0 -> signature "5"
        sig.dismissed = vec![DismissedRule {
            rule_id: RULE_ADOPT_ADVISOR.to_string(),
            signature: "5".to_string(),
        }];
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_ADVISOR));

        // A materially higher rate (rounds to a different integer) re-fires.
        sig.redundant_reads_per_session = Some(8.0);
        assert!(evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_ADVISOR));
    }

    #[test]
    fn adopt_advisor_honors_the_apply_cooldown() {
        let mut sig = adopt_advisor_signals();
        sig.applied = vec![AppliedRule {
            rule_id: RULE_ADOPT_ADVISOR.to_string(),
            root: String::new(),
            session_count: sig.session_count,
        }];
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_ADVISOR));
    }

    /// Signals for `adopt.read_advisor_substitute.v1`: advisor ON in advise
    /// mode, reminders rarely lead to a full re-read, no shell bypass.
    fn adopt_substitute_signals() -> Signals {
        let graph = GraphSettings {
            read_advisor: true, // mode defaults to "advise"
            ..GraphSettings::default()
        };
        Signals {
            advisor_reread_rate: Some(0.1),
            advisor_reread_samples: SUBSTITUTE_MIN_SAMPLES,
            session_count: MIN_SESSIONS,
            graph,
            ..Signals::default()
        }
    }

    #[test]
    fn adopt_substitute_fires_and_proposes_the_mode_string() {
        let props = evaluate(&adopt_substitute_signals());
        let p = props
            .iter()
            .find(|p| p.rule_id == RULE_ADOPT_SUBSTITUTE)
            .expect("fires");
        assert_eq!(p.setting, "graph.read_advisor_mode");
        assert_eq!(p.current, "advise");
        assert_eq!(p.proposed, "substitute");
        assert!(!p.warn_only);
    }

    #[test]
    fn adopt_substitute_respects_each_threshold() {
        // Reread rate above 20% (the outline evidently isn't enough).
        let mut sig = adopt_substitute_signals();
        sig.advisor_reread_rate = Some(0.3);
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_SUBSTITUTE));

        // Too few reminders.
        let mut sig = adopt_substitute_signals();
        sig.advisor_reread_samples = SUBSTITUTE_MIN_SAMPLES - 1;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_SUBSTITUTE));

        // Shell bypass at/above BYPASS_HIGH ⇒ don't lean harder on the advisor.
        let mut sig = adopt_substitute_signals();
        sig.bypass_rate = Some(BYPASS_HIGH);
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_SUBSTITUTE));

        // Advisor off.
        let mut sig = adopt_substitute_signals();
        sig.graph.read_advisor = false;
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_SUBSTITUTE));

        // Already in substitute mode.
        let mut sig = adopt_substitute_signals();
        sig.graph.read_advisor_mode = "substitute".to_string();
        assert!(!evaluate(&sig)
            .iter()
            .any(|p| p.rule_id == RULE_ADOPT_SUBSTITUTE));
    }

    #[test]
    fn substitute_and_min_lines_rules_never_co_fire_across_the_rate_range() {
        // Structural mutual exclusion: RULE_ADVISOR_LINES needs reread_rate ≥
        // REREAD_HIGH (0.5); RULE_ADOPT_SUBSTITUTE needs ≤ SUBSTITUTE_REREAD_LOW
        // (0.2). Sweep the whole [0,1] range with both rules' other conditions
        // satisfied and assert no single input fires both.
        let graph = GraphSettings {
            read_advisor: true, // mode "advise", so substitute is eligible
            ..GraphSettings::default()
        };
        for i in 0..=20 {
            let rate = i as f64 * 0.05;
            let sig = Signals {
                advisor_reread_rate: Some(rate),
                advisor_reread_samples: MIN_REMINDS, // clears both sample floors (20)
                session_count: MIN_SESSIONS,
                graph: graph.clone(),
                ..Signals::default()
            };
            let ids: Vec<&str> = evaluate(&sig).iter().map(|p| p.rule_id).collect();
            let both = ids.contains(&RULE_ADOPT_SUBSTITUTE) && ids.contains(&RULE_ADVISOR_LINES);
            assert!(!both, "rate={rate} fired both rules: {ids:?}");
        }
    }

    // ── V32 Phase C3 — detection-updater canaries ───────────────────────

    use crate::offload::detection::updater::{
        AvailableUpdate, FailedUpdate, StalledUpdate, STALLED_AFTER_CHECKS,
    };

    /// The consumer for check-only mode: a recorded offer becomes a card that
    /// names both versions and points at where to take it.
    #[test]
    fn an_available_detection_update_raises_a_warn_only_card() {
        let sig = Signals {
            detection_updates: vec![AvailableUpdate {
                component: "rules".into(),
                installed: String::new(),
                available: "2026.09.01".into(),
                notes: "multilingual variant".into(),
            }],
            ..Signals::default()
        };
        let p = evaluate(&sig)
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_UPDATE_AVAILABLE)
            .expect("fires");
        assert!(p.warn_only, "there is nothing safe to auto-apply");
        assert!(p.setting.is_empty());
        assert_eq!(p.proposed, "2026.09.01");
        assert_eq!(p.current, "(shipped)", "no installed version yet");
        assert!(p.rationale.contains("rules"));
        assert!(p.rationale.contains("multilingual variant"), "{}", p.rationale);
        assert_eq!(p.signature, "rules:2026.09.01");
    }

    /// A rejected bundle is a card too — nothing is degraded, but a component
    /// that refuses every candidate is freezing, and freezing quietly is the
    /// failure the rule exists to catch.
    #[test]
    fn a_rejected_detection_update_raises_a_card_that_says_nothing_broke() {
        let sig = Signals {
            detection_update_failures: vec![FailedUpdate {
                component: "rules".into(),
                version: "2026.08.07".into(),
                signature: "2026.08.07".into(),
                reason: "false-positive smoke failed: readme.txt matched Foo".into(),
            }],
            ..Signals::default()
        };
        let p = evaluate(&sig)
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_UPDATE_FAILED)
            .expect("fires");
        assert!(p.warn_only);
        assert!(p.rationale.contains("previous data is still live"));
        assert!(p.rationale.contains("false-positive smoke failed"));
        assert_eq!(p.signature, "rules:2026.08.07");
    }

    /// #46's compounding defect: two DIFFERENT manifest-level refusals used to
    /// share the signature `rules:` (both versionless), so dismissing a 404
    /// permanently muted a containment violation. The updater now hands the
    /// rule a per-reason signature, and the rule must key on it.
    #[test]
    fn dismissing_one_versionless_refusal_does_not_mute_a_different_one() {
        let refusal = |sig_key: &str, reason: &str| FailedUpdate {
            component: "rules".into(),
            version: String::new(),
            signature: sig_key.into(),
            reason: reason.into(),
        };
        let dismissed = vec![DismissedRule {
            rule_id: RULE_DETECTION_UPDATE_FAILED.to_string(),
            signature: "rules:reason:aaaaaaaaaaaaaaaa".to_string(),
        }];
        let same = Signals {
            detection_update_failures: vec![refusal(
                "reason:aaaaaaaaaaaaaaaa",
                "manifest schema 2 is not supported",
            )],
            dismissed: dismissed.clone(),
            ..Signals::default()
        };
        assert!(
            !evaluate(&same)
                .iter()
                .any(|p| p.rule_id == RULE_DETECTION_UPDATE_FAILED),
            "the dismissed refusal stays dismissed"
        );
        let other = Signals {
            detection_update_failures: vec![refusal(
                "reason:bbbbbbbbbbbbbbbb",
                "file points outside the manifest's own directory",
            )],
            dismissed,
            ..Signals::default()
        };
        let p = evaluate(&other)
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_UPDATE_FAILED)
            .expect("a containment violation still fires");
        assert!(p.rationale.contains("outside the manifest"), "{}", p.rationale);
    }

    /// A week of unreachable checks is a card — and it says the channel is
    /// unreachable, NOT that a bundle was rejected. That distinction is the
    /// whole of #46.
    #[test]
    fn a_stalled_update_channel_raises_its_own_card_not_a_rejection() {
        let sig = Signals {
            detection_update_stalled: vec![StalledUpdate {
                component: "rules".into(),
                streak: STALLED_AFTER_CHECKS,
                reason: "could not reach the update channel: GET https://…: HTTP 404".into(),
            }],
            ..Signals::default()
        };
        let ids: Vec<&str> = evaluate(&sig).iter().map(|p| p.rule_id).collect();
        assert!(
            !ids.contains(&RULE_DETECTION_UPDATE_FAILED),
            "an outage must never render as a refusal: {ids:?}"
        );
        let p = evaluate(&sig)
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_UPDATE_STALLED)
            .expect("fires");
        assert!(p.warn_only);
        assert!(p.setting.is_empty(), "nothing here is a settings write");
        assert!(
            !p.rationale.to_ascii_lowercase().contains("rejected"),
            "{}",
            p.rationale
        );
        assert!(p.rationale.contains("not getting fresher") || p.rationale.contains("fresher"));
        assert!(p.rationale.contains("HTTP 404"), "the reason is diagnosable");
        assert_eq!(p.signature, "rules:1");
    }

    /// #48/A1-5: the same card carries a channel that is perfectly REACHABLE
    /// and refuses everything it serves. The rule must not claim an outage it
    /// knows nothing about — the cause is whatever the last outcome says, and
    /// this signal's job is only "nothing is landing".
    #[test]
    fn the_stall_card_does_not_diagnose_the_cause_it_quotes_the_last_outcome() {
        let sig = Signals {
            detection_update_stalled: vec![StalledUpdate {
                component: "rules".into(),
                streak: STALLED_AFTER_CHECKS,
                reason: "checksum mismatch on `core.yar` (expected ab…, got cd…)".into(),
            }],
            ..Signals::default()
        };
        let p = evaluate(&sig)
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_UPDATE_STALLED)
            .expect("fires for a refusing channel too");
        let lower = p.rationale.to_ascii_lowercase();
        assert!(
            !lower.contains("unreachable") && !lower.contains("could not reach"),
            "nothing here knows the channel is unreachable: {}",
            p.rationale
        );
        assert!(p.rationale.contains("checksum mismatch"), "{}", p.rationale);
    }

    /// The dismissal re-fires after another threshold's worth of failures, so a
    /// permanently dead channel is raised roughly weekly rather than once.
    #[test]
    fn a_dismissed_stall_card_refires_after_another_threshold_of_failures() {
        let stalled = |streak: u32| Signals {
            detection_update_stalled: vec![StalledUpdate {
                component: "rules".into(),
                streak,
                reason: "offline".into(),
            }],
            dismissed: vec![DismissedRule {
                rule_id: RULE_DETECTION_UPDATE_STALLED.to_string(),
                signature: "rules:1".to_string(),
            }],
            ..Signals::default()
        };
        let fires = |s: &Signals| {
            evaluate(s)
                .iter()
                .any(|p| p.rule_id == RULE_DETECTION_UPDATE_STALLED)
        };
        assert!(!fires(&stalled(STALLED_AFTER_CHECKS)), "dismissed");
        assert!(
            !fires(&stalled(STALLED_AFTER_CHECKS * 2 - 1)),
            "still inside the dismissed bucket"
        );
        assert!(
            fires(&stalled(STALLED_AFTER_CHECKS * 2)),
            "another threshold's worth of silence re-raises it"
        );
    }

    /// Dismissal is keyed to the VERSION, so declining one bundle does not
    /// silence the next — the whole point of a freshness canary.
    #[test]
    fn a_dismissed_detection_card_refires_on_the_next_version() {
        let update = |v: &str| AvailableUpdate {
            component: "rules".into(),
            installed: "2026.07.01".into(),
            available: v.into(),
            notes: String::new(),
        };
        let dismissed = vec![DismissedRule {
            rule_id: RULE_DETECTION_UPDATE_AVAILABLE.to_string(),
            signature: "rules:2026.08.07".to_string(),
        }];
        let same = Signals {
            detection_updates: vec![update("2026.08.07")],
            dismissed: dismissed.clone(),
            ..Signals::default()
        };
        assert!(!evaluate(&same)
            .iter()
            .any(|p| p.rule_id == RULE_DETECTION_UPDATE_AVAILABLE));
        let next = Signals {
            detection_updates: vec![update("2026.09.01")],
            dismissed,
            ..Signals::default()
        };
        assert!(evaluate(&next)
            .iter()
            .any(|p| p.rule_id == RULE_DETECTION_UPDATE_AVAILABLE));
    }

    /// The healthy steady state — auto mode applying updates and clearing both
    /// records — says nothing at all.
    #[test]
    fn a_healthy_updater_raises_no_detection_card() {
        let ids: Vec<&str> = evaluate(&Signals::default())
            .iter()
            .map(|p| p.rule_id)
            .collect();
        assert!(!ids.contains(&RULE_DETECTION_UPDATE_AVAILABLE));
        assert!(!ids.contains(&RULE_DETECTION_UPDATE_FAILED));
        assert!(!ids.contains(&RULE_DETECTION_UPDATE_STALLED));
        assert!(!ids.contains(&RULE_DETECTION_SIGNATURE_DOWN));
    }

    // ── V32 Phase C / #48 D-2 — the disarmed signature layer ────────────

    use crate::offload::detection::signature::SignatureDown;

    /// The consumer half of D-2. Before this rule existed, a rules directory
    /// that compiled to nothing had NO consumer: `scan` returned empty for the
    /// rest of the process's life, every page reported clean, and all four
    /// signal channels said the opposite — the badge is derived from settings
    /// toggles, no activity row is written on reload, and the stall card
    /// literally says "Nothing is degraded".
    #[test]
    fn a_disarmed_signature_layer_raises_a_warn_only_card() {
        let sig = Signals {
            detection_signature_down: Some(SignatureDown {
                dir: r"C:\cimp\detection\rules.d".into(),
                files_loaded: 0,
                files_failed: 3,
                rules: 0,
                failed: vec!["injection_core.yar".into(), "local/mine.yar".into()],
            }),
            ..Signals::default()
        };
        let p = evaluate(&sig)
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_SIGNATURE_DOWN)
            .expect("a layer with no rules must say so");
        assert!(
            p.warn_only,
            "the fix is a file on disk, not a settings write"
        );
        assert!(p.setting.is_empty());
        assert!(p.action.is_none());
        // The card must not read as reassurance: the whole defect was surfaces
        // claiming a disarmed layer was fine.
        assert!(
            p.rationale.contains("no rules to match against"),
            "{}",
            p.rationale
        );
        assert!(
            p.rationale.contains(r"C:\cimp\detection\rules.d"),
            "names where to look: {}",
            p.rationale
        );
        assert!(p.current.contains("injection_core.yar"), "{}", p.current);
        assert_eq!(p.signature, r"C:\cimp\detection\rules.d:0:3");
    }

    /// The signature is the state the user looked at, so a dismissal holds for
    /// that state and re-raises when the directory changes underneath it — a
    /// half-repaired directory that is still disarmed is a new fact.
    #[test]
    fn a_dismissed_signature_down_card_refires_when_the_directory_changes() {
        let down = |loaded: usize, failed: usize| Signals {
            detection_signature_down: Some(SignatureDown {
                dir: "/rules.d".into(),
                files_loaded: loaded,
                files_failed: failed,
                rules: 0,
                failed: vec!["a.yar".into()],
            }),
            dismissed: vec![DismissedRule {
                rule_id: RULE_DETECTION_SIGNATURE_DOWN.to_string(),
                signature: "/rules.d:0:3".to_string(),
            }],
            ..Signals::default()
        };
        let fires = |s: &Signals| {
            evaluate(s)
                .iter()
                .any(|p| p.rule_id == RULE_DETECTION_SIGNATURE_DOWN)
        };
        assert!(!fires(&down(0, 3)), "dismissed for the state it described");
        assert!(
            fires(&down(1, 2)),
            "one file repaired and the layer is STILL disarmed — a new fact"
        );
    }

    /// `None` is both healthy states — rules are live, or the user switched the
    /// layer off — and the producer resolves the switch, so nothing here has to
    /// know about settings.
    #[test]
    fn no_signature_signal_means_no_signature_card() {
        assert!(!evaluate(&Signals {
            detection_signature_down: None,
            ..Signals::default()
        })
        .iter()
        .any(|p| p.rule_id == RULE_DETECTION_SIGNATURE_DOWN));
    }

    // ── #48 U-4 — a user rule that does not compile ─────────────────────

    use crate::offload::detection::signature::RenamedRule;
    use crate::offload::detection::updater::BrokenLocalRules;

    fn broken_local(files: &[&str]) -> Signals {
        local_rules(files, &[])
    }

    fn local_rules(files: &[&str], renamed: &[(&str, &str, &str)]) -> Signals {
        Signals {
            detection_local_rules_broken: Some(BrokenLocalRules {
                dir: r"C:\cimp\detection\rules.d".into(),
                failed: files.iter().map(|f| (*f).to_string()).collect(),
                renamed: renamed
                    .iter()
                    .map(|(file, from, to)| RenamedRule {
                        file: (*file).to_string(),
                        from: (*from).to_string(),
                        to: (*to).to_string(),
                    })
                    .collect(),
                files_loaded: 3,
                rules: 12,
            }),
            ..Signals::default()
        }
    }

    /// The consumer for U-4's other half. Stopping a broken `local/` file from
    /// vetoing the update channel is only half a fix: without a card the user's
    /// own rules are silently not protecting them, and the only trace is a
    /// `warn!` line. The card must say what is skipped AND that the rest is
    /// live, or it reads as "detection is off".
    #[test]
    fn a_broken_user_rule_raises_a_warn_only_card() {
        let p = evaluate(&broken_local(&["local/mine.yar"]))
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_LOCAL_RULES_BROKEN)
            .expect("a skipped user rule must say so");
        assert!(p.warn_only, "the fix is a file on disk");
        assert!(p.setting.is_empty());
        assert!(p.action.is_none());
        assert!(p.current.contains("local/mine.yar"), "{}", p.current);
        assert!(
            p.current.contains("12 rule(s) still live"),
            "the card must not read as an outage: {}",
            p.current
        );
        assert!(p.rationale.contains("do not compile"), "{}", p.rationale);
        // The identifier-collision cause is named, because it is the one an
        // update can introduce without the user's file changing at all.
        assert!(p.rationale.contains("IDENTIFIER"), "{}", p.rationale);
        assert!(
            p.rationale.contains(r"C:\cimp\detection\rules.d"),
            "{}",
            p.rationale
        );
    }

    /// Signed by the failing file NAMES, so a dismissal holds for the files the
    /// user looked at and re-raises when a different file breaks.
    #[test]
    fn a_dismissed_broken_rule_card_refires_for_a_different_file() {
        let dismissed = vec![DismissedRule {
            rule_id: RULE_DETECTION_LOCAL_RULES_BROKEN.to_string(),
            signature: "local/mine.yar".to_string(),
        }];
        let fires = |files: &[&str]| {
            let mut s = broken_local(files);
            s.dismissed = dismissed.clone();
            evaluate(&s)
                .iter()
                .any(|p| p.rule_id == RULE_DETECTION_LOCAL_RULES_BROKEN)
        };
        assert!(!fires(&["local/mine.yar"]), "dismissed for what it named");
        assert!(
            fires(&["local/other.yar"]),
            "a different file is a new fact"
        );
        assert!(
            fires(&["local/mine.yar", "local/other.yar"]),
            "a widened set is a new fact"
        );
    }

    /// **#48/M-13 — a renamed rule is a NOTICE, not an outage, and it must
    /// still reach the user.**
    ///
    /// The collision that used to freeze the update channel now resolves by
    /// loading the user's rule under a `custom_` identifier. That is a silent
    /// success unless something says so, and a silent success is what every
    /// finding in this milestone turned out to be. So the card fires with no
    /// broken file at all, and it must describe the rename in the rename's own
    /// words — never in the broken file's.
    ///
    /// What would this still pass with? Not a card that merely mentions the
    /// file: it pins the OLD and NEW identifiers (the actionable half — the old
    /// name is what a user's saved search keys on), the "not modified" promise
    /// that justifies not touching their file, and the absence of the
    /// "do not compile" sentence, which would be false about a rule that is
    /// matching.
    #[test]
    fn a_renamed_user_rule_raises_a_card_that_does_not_call_it_broken() {
        let p = evaluate(&local_rules(
            &[],
            &[("local/mine.yar", "Dup_Rule", "custom_Dup_Rule")],
        ))
        .into_iter()
        .find(|p| p.rule_id == RULE_DETECTION_LOCAL_RULES_BROKEN)
        .expect("a renamed user rule must say so");
        assert!(p.warn_only, "the fix is a file on disk");
        assert!(p.current.contains("Dup_Rule"), "{}", p.current);
        assert!(p.current.contains("custom_Dup_Rule"), "{}", p.current);
        assert!(p.current.contains("local/mine.yar"), "{}", p.current);
        assert!(
            p.current.contains("12 rule(s) still live"),
            "the card must not read as an outage: {}",
            p.current
        );
        assert!(
            !p.rationale.contains("do not compile"),
            "a renamed rule IS matching; describing it as broken is the exact lie this \
             milestone keeps finding: {}",
            p.rationale
        );
        assert!(p.rationale.contains("still matching"), "{}", p.rationale);
        assert!(
            p.rationale.contains("NOT modified"),
            "the promise that justifies renaming at load time must be stated: {}",
            p.rationale
        );
    }

    /// A dismissal of the broken-file card must not also silence a rename that
    /// shows up later — the two are different facts on one card, so the
    /// signature carries both.
    #[test]
    fn a_dismissed_broken_rule_card_refires_when_a_rule_is_renamed() {
        let dismissed = vec![DismissedRule {
            rule_id: RULE_DETECTION_LOCAL_RULES_BROKEN.to_string(),
            signature: "local/mine.yar".to_string(),
        }];
        let fires = |renamed: &[(&str, &str, &str)]| {
            let mut s = local_rules(&["local/mine.yar"], renamed);
            s.dismissed = dismissed.clone();
            evaluate(&s)
                .iter()
                .any(|p| p.rule_id == RULE_DETECTION_LOCAL_RULES_BROKEN)
        };
        assert!(!fires(&[]), "dismissed for exactly what it named");
        assert!(
            fires(&[("local/other.yar", "Dup", "custom_Dup")]),
            "a rename is a new fact the dismissal never covered"
        );
    }

    /// `None` is the healthy steady state, and the producer owns every reason to
    /// stay quiet (layer off, everything compiles with no collision, the failure
    /// is in a bundle file, or the layer is disarmed and the louder card is
    /// already up).
    #[test]
    fn no_broken_rule_signal_means_no_card() {
        assert!(!evaluate(&Signals::default())
            .iter()
            .any(|p| p.rule_id == RULE_DETECTION_LOCAL_RULES_BROKEN));
    }

    // ── #48 M-11 — the live rule set is SHORT of files ──────────────────

    use crate::offload::detection::updater::RulesIncomplete;

    fn incomplete(files: &[&str]) -> Signals {
        Signals {
            detection_rules_incomplete: vec![RulesIncomplete {
                component: "rules".into(),
                files: files.iter().map(|f| (*f).to_string()).collect(),
                dir: r"C:\cimp\detection\rules.d".into(),
            }],
            ..Signals::default()
        }
    }

    /// The consumer M-11 needs, and the reason it could not be folded into
    /// `detection.update_failed.v1`: that card's whole reassurance is "nothing
    /// is degraded right now, the previous data is still live", and this one
    /// exists because that sentence is false.
    #[test]
    fn an_incomplete_rule_set_raises_its_own_card_naming_the_missing_files() {
        let p = evaluate(&incomplete(&["core.yar"]))
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_RULES_INCOMPLETE)
            .expect("a short rule set must say so");
        assert!(p.warn_only, "the fix is a file handle, not a setting");
        assert!(p.setting.is_empty());
        assert!(p.action.is_none());
        assert!(p.current.contains("core.yar"), "{}", p.current);
        assert!(
            p.rationale.contains("FEWER rules"),
            "the card must say it is degraded NOW: {}",
            p.rationale
        );
        assert!(
            p.rationale.contains("not lost") && p.rationale.contains("retries"),
            "…and that it is recoverable, or the user will not wait for it: {}",
            p.rationale
        );
        assert!(
            p.rationale.contains(r"C:\cimp\detection\rules.d"),
            "{}",
            p.rationale
        );
    }

    /// Signed by the missing file names, so a dismissal holds for the set the
    /// user looked at and re-raises if it grows.
    #[test]
    fn a_dismissed_incomplete_card_refires_when_more_files_go_missing() {
        let dismissed = vec![DismissedRule {
            rule_id: RULE_DETECTION_RULES_INCOMPLETE.to_string(),
            signature: "rules:core.yar".to_string(),
        }];
        let fires = |files: &[&str]| {
            let mut s = incomplete(files);
            s.dismissed = dismissed.clone();
            evaluate(&s)
                .iter()
                .any(|p| p.rule_id == RULE_DETECTION_RULES_INCOMPLETE)
        };
        assert!(!fires(&["core.yar"]), "dismissed for what it named");
        assert!(
            fires(&["core.yar", "extra.yar"]),
            "a wider gap is a new fact"
        );
    }

    /// Empty is the healthy steady state, and — unlike the two cards above —
    /// this one is NOT suppressed by any of them: it is the only signal in the
    /// detection family that reports a present, ongoing loss of coverage, so it
    /// has to be able to fire alongside a refusal card that says the opposite.
    #[test]
    fn an_incomplete_card_fires_even_beside_a_refusal_card() {
        assert!(!evaluate(&Signals::default())
            .iter()
            .any(|p| p.rule_id == RULE_DETECTION_RULES_INCOMPLETE));

        let mut s = incomplete(&["core.yar"]);
        s.detection_update_failures = vec![crate::offload::detection::updater::FailedUpdate {
            component: "rules".into(),
            version: "2026.08.09".into(),
            signature: "2026.08.09".into(),
            reason: "the activated bundle did not load cleanly".into(),
        }];
        let ids: Vec<&str> = evaluate(&s).iter().map(|p| p.rule_id).collect();
        assert!(ids.contains(&RULE_DETECTION_RULES_INCOMPLETE), "{ids:?}");
        assert!(ids.contains(&RULE_DETECTION_UPDATE_FAILED), "{ids:?}");
    }
}

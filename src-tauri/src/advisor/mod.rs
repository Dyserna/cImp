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
//!
//! **What lives where** (V42 R7). This file owns the signal surface
//! ([`Signals`], [`Proposal`]), the rule catalogue ([`RULE_REFERENCE`] /
//! `ALL_RULE_IDS`), the tuning/adopt/surface rules, and the dismissal and
//! apply-cooldown filters every rule class passes through. The two advisors
//! that share nothing but those filters have their own modules: [`drift`] (is
//! the harness contract still holding?) and [`detection`] (is the
//! injection-detection layer still getting fresher?). They were one 744-line
//! `drift_rules` until R7.

use crate::settings::{AppliedRule, DismissedRule, GraphSettings};

mod detection;
mod drift;

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
//
// V35 Phase E (milestone locked decision 5) changed the ENVELOPE, not the
// detectors. Every threshold, sample floor and warn-only/Apply choice below is
// untouched; what changed is that six of the eight now speak as
// [`RULE_DRIFT_CAPABILITY`] about a **named capability** from
// `harness::contract`, with the constant that saw the symptom carried as
// *evidence* rather than as the notice's own id. The two exceptions are stated
// on their consts.

/// The ONE drift-class notice id (V35 Phase E; matrix draft § 3, consumer 2).
///
/// Signature = `<capability>:<evidence rule>:<the detector's own re-fire key>`.
///
/// * The first two fields are what makes a dismissal hold per
///   **(capability, evidence)** pair, so silencing "the usage fields are gone"
///   never silences "the sub-agent layout moved".
/// * The third is each detector's ORIGINAL signature, kept verbatim, so every
///   rule's re-fire boundary survives consolidation unchanged — a bucketed rate
///   still re-fires on a materially changed rate, a version-keyed rule still
///   re-fires on the next harness update. Dropping it would have quietly turned
///   every drift dismissal into a permanent one.
///
/// **Deploy note:** notices dismissed under the old per-rule ids re-fire ONCE
/// under this id after the upgrade. Accepted — a dismissal is keyed to the id
/// the user dismissed, and re-keying the stored records would be a migration
/// that guesses at intent.
pub const RULE_DRIFT_CAPABILITY: &str = "drift.capability.v1";

/// The harness version tripwire. **NOT** consolidated into
/// [`RULE_DRIFT_CAPABILITY`]: it is not evidence about one capability — it says
/// the whole harness moved — and it keeps its own `mark_verified` action.
///
/// **V35 Phase F made it the cannot-verify fallback.** Before, it fired on
/// every Claude Code auto-update whether or not anything broke, which trained
/// the reflexive *Mark verified* that disarmed it. Now the update triggers
/// `harness::verify`: all-pass advances `claude_last_verified` by itself (this
/// rule's condition is then false), and failures raise a
/// [`RULE_DRIFT_CAPABILITY`] notice per broken capability (this rule is
/// suppressed by `verify::tripwire_superseded`, since a second card naming the
/// same event is noise). What is left for it is the case nothing else can
/// speak for: no auto-verify record for this build at all, or one that could
/// not reach a verdict.
pub const RULE_DRIFT_VERSION: &str = "drift.harness_version.v1";
/// Evidence for `claude.hook.pretooluse_deny` (V35 Phase E). Still the id the
/// detector is *named* by — in `Capability::drift_rule`, in the notice
/// signature and in `MAINTENANCE.md` — but no longer a notice id of its own.
pub const RULE_DRIFT_READ_REASON: &str = "drift.read_reason.v1";
/// Evidence for `claude.hook.pretooluse_deny`.
pub const RULE_DRIFT_HOOK_SILENT: &str = "drift.read_hook_silent.v1";
/// Evidence for `claude.hook.user_prompt_submit`.
pub const RULE_DRIFT_INJECTION_UNSEEN: &str = "drift.injection_unseen.v1";
/// Evidence for `claude.transcript.usage`.
pub const RULE_DRIFT_USAGE_FIELDS: &str = "drift.usage_fields_gone.v1";
/// Evidence for whichever capability reported the malformed payload — several
/// rows name this rule, and `contract::capability_for_payload_shim` resolves each
/// report to exactly one of them through the `drift_token` column.
///
/// Also, uniquely, still a **notice id in its own right**, and the reason has
/// narrowed rather than gone away. It used to be the two beacons: they reported
/// through this route and had no registry row, so their reports could not be
/// attributed. V35 Phase I gave them rows and 2026-08-17 closed the last
/// unattributed reporter (the auto-check route, now `post_edit_hook`), so what
/// remains in the un-consolidated channel is a shim name **nobody declared** — a
/// forged one, or a future reporter added without a registry row. A report the
/// matrix cannot place is a real signal about a reporter the matrix does not
/// cover, and dropping it to satisfy a one-notice-source count would be exactly
/// the "computed then discarded" failure this milestone exists to remove.
pub const RULE_DRIFT_PAYLOAD: &str = "drift.payload.v1";
/// Evidence for `claude.hook.pretooluse_deny`.
pub const RULE_DRIFT_READ_BYPASS: &str = "drift.read_bypass.v1";
/// Evidence for `claude.transcript.subagents`.
pub const RULE_DRIFT_SUBAGENT: &str = "drift.subagent_transcripts.v1";

// ── V32 Phase C3 — detection-updater canaries ───────────────────────────
//
// Locked decision 13's "every signal has its consumer": the updater records
// what it found and what it refused, and these two rules are what turn those
// records into something the user actually sees. Both are warn-only — the fix
// is a button in Settings → Injection protection → Injection detection, not a
// settings write the Apply path could make — and both carry their own trigger
// (a fact, not a statistic), so neither sits behind the global `MIN_SESSIONS`
// floor.

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

// ── the rule reference (V40 Phase F, locked decision 23) ────────────────
//
// The Code Intelligence panel used to carry this whole table as a hard-coded
// tooltip string, restating numbers THIS file owns and naming one harness's
// mechanisms while doing it ("Claude Code version", "≥2 Claude sessions", "the
// PreToolUse hook"). Two problems: a threshold changed here left the tooltip
// lying, and rules that fire per registered harness read as being about one.
//
// It is published instead. Each row states the CONDITION in the rule's own
// terms; the fix POINTER — which mechanism of which harness to look at — is
// `Capability::drift_hint()` and reaches the user on the card that actually
// fired, where it can name the harness that raised it.

/// One rule's entry in the reference the Advisor panel shows.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct RuleReference {
    /// The rule id, verbatim — the same string the proposal carries.
    pub id: &'static str,
    /// What has to be true for it to fire, and what it proposes.
    pub thresholds: &'static str,
}

/// Every rule the advisor can raise, with its firing condition.
///
/// Ids come from the constants above rather than being spelled again, so a
/// rename cannot leave a row pointing at a rule that no longer exists, and
/// `the_rule_reference_covers_every_rule` fails the build if a rule is added
/// without one.
pub const RULE_REFERENCE: &[RuleReference] = &[
    RuleReference {
        id: RULE_MIN_SCORE,
        thresholds: "≥5 sessions, ≥200 injections, ≥70% never re-touched → raise context_min_score.",
    },
    RuleReference {
        id: RULE_ADVISOR_LINES,
        thresholds: "≥5 sessions, ≥20 reminders, ≥50% re-read anyway → raise read_advisor_min_lines.",
    },
    RuleReference {
        id: RULE_TURN_BUDGET,
        thresholds: "≥5 sessions, ≥200 injections, ≥50 turns, ≥70% unread AND ≥50% turns maxed → lower context_turn_budget_chars.",
    },
    RuleReference {
        id: RULE_SURFACE_LEAN,
        thresholds: "≥10 sessions, 0 calls to any cold-tail graph tool (cycles, dead_exports, struct_search, path, architecture) → enable lean_tools (hide them from the advertised surface; they still answer if called).",
    },
    RuleReference {
        id: RULE_ADOPT_ADVISOR,
        thresholds: "≥5 sessions, E1 verified (pass), ≥3 redundant large re-reads per session across ≥10 sessions (est.; external tools may have changed the file between reads) → enable read_advisor.",
    },
    RuleReference {
        id: RULE_ADOPT_SUBSTITUTE,
        thresholds: "read_advisor on in advise mode, ≥20 reminders, ≤20% re-read anyway, low shell bypass → switch read_advisor_mode to substitute.",
    },
    RuleReference {
        id: RULE_DRIFT_CAPABILITY,
        thresholds: "a recorded contract check failed for one declared capability → that capability is degraded; the card names the harness and the mechanism.",
    },
    RuleReference {
        id: RULE_DRIFT_VERSION,
        thresholds: "a harness's installed version ≠ its last-verified one, and no automatic check could reach a verdict → re-verify its contracts (Mark verified).",
    },
    RuleReference {
        id: RULE_DRIFT_READ_REASON,
        thresholds: "≥15 reminders, ≥90% immediately re-read → the deny reason isn’t reaching the model; disable read_advisor.",
    },
    RuleReference {
        id: RULE_DRIFT_HOOK_SILENT,
        thresholds: "≥3 sessions, ≥10 large re-reads (est.), 0 reminders → the harness's pre-tool gate isn’t firing.",
    },
    RuleReference {
        id: RULE_DRIFT_INJECTION_UNSEEN,
        thresholds: "≥5 sessions, ≥30 injections, ≤2% follow → injected context likely never reaches the model.",
    },
    RuleReference {
        id: RULE_DRIFT_USAGE_FIELDS,
        thresholds: "≥2 sessions of one harness, all without token fields → its transcript usage schema changed.",
    },
    RuleReference {
        id: RULE_DRIFT_PAYLOAD,
        thresholds: "any shim-reported payload missing required fields.",
    },
    RuleReference {
        id: RULE_DRIFT_READ_BYPASS,
        thresholds: "≥10 reminders, ≥40% answered via shell reads (est.) → disable read_advisor.",
    },
    RuleReference {
        id: RULE_DRIFT_SUBAGENT,
        thresholds: "sub-agent turns observed but no sub-agent transcripts read → the transcript layout moved.",
    },
    RuleReference {
        id: RULE_DETECTION_UPDATE_AVAILABLE,
        thresholds: "a newer detection bundle exists and was not applied — the component is in check-only mode, or the bundle needs a newer app build.",
    },
    RuleReference {
        id: RULE_DETECTION_UPDATE_FAILED,
        thresholds: "the last update attempt was REFUSED (bad checksum, a bundle that would not compile, a failed smoke control, an artifact outside the curated directory). The old data is still live.",
    },
    RuleReference {
        id: RULE_DETECTION_UPDATE_STALLED,
        thresholds: "a component got no fresher for a week of consecutive checks, whatever the reason.",
    },
    RuleReference {
        id: RULE_DETECTION_SIGNATURE_DOWN,
        thresholds: "the signature layer is ON and has nothing to match with — the rules directory is unreadable, or every file in it was rejected.",
    },
    RuleReference {
        id: RULE_DETECTION_LOCAL_RULES_BROKEN,
        thresholds: "one of your own rules in rules.d/local/ does not compile and is skipped, or is live under a renamed identifier because a shipped rule took its name.",
    },
    RuleReference {
        id: RULE_DETECTION_RULES_INCOMPLETE,
        thresholds: "a rollback could not put every file back, so rules.d is missing shipped rule files that still exist in the retained copy.",
    },
];

/// The one sentence that is about the panel rather than about a rule.
pub const RULE_REFERENCE_FOOTER: &str =
    "After an Apply, that rule stays quiet for 3 further sessions so fresh post-change data can \
     accumulate before it re-evaluates.";

/// Every rule id this module can raise. The coverage test's other half — a new
/// `RULE_*` constant that is not added here fails nothing, which is why the
/// test asserts the two lists against each other by LENGTH as well as by
/// membership.
#[cfg_attr(not(test), allow(dead_code))]
const ALL_RULE_IDS: &[&str] = &[
    RULE_MIN_SCORE,
    RULE_ADVISOR_LINES,
    RULE_TURN_BUDGET,
    RULE_SURFACE_LEAN,
    RULE_ADOPT_ADVISOR,
    RULE_ADOPT_SUBSTITUTE,
    RULE_DRIFT_CAPABILITY,
    RULE_DRIFT_VERSION,
    RULE_DRIFT_READ_REASON,
    RULE_DRIFT_HOOK_SILENT,
    RULE_DRIFT_INJECTION_UNSEEN,
    RULE_DRIFT_USAGE_FIELDS,
    RULE_DRIFT_PAYLOAD,
    RULE_DRIFT_READ_BYPASS,
    RULE_DRIFT_SUBAGENT,
    RULE_DETECTION_UPDATE_AVAILABLE,
    RULE_DETECTION_UPDATE_FAILED,
    RULE_DETECTION_UPDATE_STALLED,
    RULE_DETECTION_SIGNATURE_DOWN,
    RULE_DETECTION_LOCAL_RULES_BROKEN,
    RULE_DETECTION_RULES_INCOMPLETE,
];

/// **One harness's drift signals** (V40 Phase C, locked decision 23).
///
/// `Signals` used to carry six `claude_*` scalars — `claude_last_seen`,
/// `claude_last_verified`, `claude_auto_verify`, `claude_sessions`,
/// `claude_tokenless_sessions` and `subagent_drift` — on the core signal
/// struct. Three of them had no OpenCode twin at all, which is not a gap in the
/// data but a gap in the RULES: `drift.version.v1` could only ever fire for one
/// product, so a second harness could auto-update through a contract change and
/// nothing anywhere would say so.
///
/// The fields are the same six, minus the prefix. What changed is that there is
/// one of these per registered harness and every rule loops.
#[derive(Clone, Debug, Default)]
pub struct DriftSignals {
    /// Latest version of this harness seen in its own telemetry (empty until a
    /// tab of it has run), and the version its contracts were last verified
    /// against — `Settings::harness[<id>]`, written by the tap, the tab spawn
    /// and `harness_mark_verified`.
    pub last_seen: String,
    pub last_verified: String,
    /// The last automatic verification run for this harness, as recorded by
    /// [`crate::harness::verify`]. Two rules read it, both through that module
    /// so the interpretation lives once:
    ///
    /// * [`RULE_DRIFT_VERSION`] is the **cannot-verify fallback** — it fires
    ///   only when this record cannot speak for the seen version
    ///   (`verify::tripwire_superseded`);
    /// * each recorded failure raises its own [`RULE_DRIFT_CAPABILITY`] notice
    ///   naming the capability, the layer that saw it and the `wired_in`
    ///   modules (`verify::notifiable_failures`).
    ///
    /// `None` where auto-verify has never completed — a genuinely different
    /// state from "ran and passed", and exactly when the fallback is wanted.
    pub auto_verify: Option<crate::settings::AutoVerify>,
    /// `drift.usage_fields_gone.v1`: this harness's sessions in the window, and
    /// how many recorded NO token-bearing `usage_stat` rows (a usage-payload
    /// change ⇒ the parser stops matching ⇒ token totals all zero).
    ///
    /// **Populated for the DEFAULT harness only today**, and the reason is a
    /// seam a later phase owns rather than a decision here: the graph's session
    /// query still filters one agent literal (`graph/service.rs`, locked
    /// decision 20, Phase D). A harness with no counts simply never trips the
    /// floor, which is the correct answer for one whose sessions cannot be
    /// counted yet — not a silent zero standing in for a real one.
    pub sessions: u64,
    pub tokenless_sessions: u64,
    /// `drift.subagent_transcripts.v1`: summaries of this harness's own
    /// sub-agent contract-drift reports this run. Empty = healthy.
    pub subagent_drift: Vec<String>,
}

/// Every registered harness's [`DriftSignals`], keyed by id.
///
/// A `BTreeMap` so iteration order is stable and a notice list does not shuffle
/// between polls.
pub type HarnessDriftSignals = std::collections::BTreeMap<crate::harness::HarnessId, DriftSignals>;

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
    /// **Per harness** (V40 Phase C, locked decision 23) — see
    /// [`DriftSignals`] for the six fields this replaced and why they could not
    /// stay scalars.
    pub harness: HarnessDriftSignals,
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
    /// Feature 3 (`drift.payload.v1`): distinct "shim: missing-fields"
    /// summaries from `contract_drift` Activity events this run. Empty =
    /// no payload drift observed.
    pub contract_drift: Vec<String>,
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
    /// `"pass"` — NOT merely "the gate is not blocking"
    /// (`harness::contract::gate`, which also passes `"unverified"`). V35
    /// Phase E retired `e1_blocked()` and deliberately left this check alone;
    /// the two are not interchangeable. "Verified OK" must mean *proven*:
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
    /// V35 Phase E: for a [`RULE_DRIFT_CAPABILITY`] notice, the harness
    /// capability it is about (`harness::contract::Capability::id`) — the join
    /// key shared with the gate query and the Settings window. `None` for every
    /// rule that is not capability-scoped.
    ///
    /// Carried as a field rather than left for the UI to dig out of
    /// [`Self::signature`], which is documented as opaque: a card that parsed
    /// the signature would be a second implementation of the signature format,
    /// which is the class of mirror this phase removed.
    pub capability: Option<&'static str>,
    /// V40 Phase C (locked decision 23): the harness this notice is ABOUT, for
    /// the rules that evaluate per registered harness. `None` for a rule that is
    /// not about one.
    ///
    /// Carried because the card's `mark_verified` action has to name a harness:
    /// before this, "Mark verified" wrote the DEFAULT harness's row whatever
    /// notice you clicked it on, so an OpenCode version notice would have
    /// stamped Claude's — and OpenCode had no version notice to click, which is
    /// how that went unnoticed.
    pub harness: Option<&'static str>,
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
    // ── CARD ORDER IS OBSERVABLE ──────────────────────────────────────────
    //
    // The panel renders this vector in order (`{#each advice.proposals}` — no
    // sort anywhere between here and the DOM), so these three calls are a
    // user-visible contract, not an implementation detail. The odd-looking one
    // is the middle: `detection::rules` runs BETWEEN the two halves of `drift`
    // because the V32 detection block was appended into the middle of the old
    // `drift_rules`, ahead of Feature 4's bypass rule. V42 R7 split the
    // function without reshuffling anyone's cards; tidying the order is a
    // separate, deliberate change. `tests::the_card_order_is_pinned` is the
    // pin.
    let mut out = drift::rules(sig);
    out.extend(detection::rules(sig));
    out.extend(drift::read_bypass(sig));
    // V35 Phase E: `out` holds ONLY drift and detection proposals at this
    // point, and the two that carry an Apply are exactly the two that propose
    // turning the advisor off (no detection rule proposes a settings write at
    // all). Asked as "does drift propose disabling the advisor" rather than by
    // listing two rule ids — which is both what the suppression below actually
    // means and what survived the rule ids consolidating into one.
    let advisor_disable_proposed = out
        .iter()
        .any(|p| p.setting == "graph.read_advisor" && p.proposed == "false");

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
    // are cumulative and still dominated by pre-apply data.
    //
    // Warn-only proposals are exempt, and V35 Phase E is what made the
    // exemption load-bearing rather than cosmetic. A warn-only rule has no
    // Apply button, so it can never accrue a cooldown record of its own and
    // this filter was always a no-op for it. Now that every capability drift
    // notice shares ONE `rule_id`, it would stop being a no-op: applying the
    // read advisor's disable proposal writes an `AppliedRule` for
    // `drift.capability.v1`, which would silence every OTHER capability's
    // warn-only notice — a transcript-usage break going quiet because the user
    // acted on an unrelated read-advisor card. `AppliedRule` is keyed by
    // `rule_id` alone (a Settings wire type), so the exemption is the fix that
    // needs no migration.
    out.retain(|p| p.warn_only || !in_apply_cooldown(sig, p.rule_id));
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
                capability: None,
                harness: None,
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
                        capability: None,
                        harness: None,
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
            let bypass_ok = sig.bypass_rate.is_none_or(|b| b < drift::BYPASS_HIGH);
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
                        capability: None,
                        harness: None,
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
                        capability: None,
                        harness: None,
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
                    capability: None,
                    harness: None,
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
                        capability: None,
                        harness: None,
                    });
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    /// **Every rule the advisor can raise has a reference row** (locked
    /// decision 23, V40 Phase F).
    ///
    /// The panel's rule reference is backend-published now. A rule with no row
    /// is a card a user can read and then find nothing about — the shape this
    /// milestone's tests exist to catch — and a row with an id no constant
    /// spells is a line about a rule that cannot fire.
    #[test]
    fn the_rule_reference_covers_every_rule() {
        use super::{ALL_RULE_IDS, RULE_REFERENCE};
        let referenced: std::collections::BTreeSet<&str> =
            RULE_REFERENCE.iter().map(|r| r.id).collect();
        let declared: std::collections::BTreeSet<&str> = ALL_RULE_IDS.iter().copied().collect();
        assert_eq!(
            referenced.len(),
            RULE_REFERENCE.len(),
            "the rule reference lists the same rule twice"
        );
        let missing: Vec<&&str> = declared.difference(&referenced).collect();
        assert!(missing.is_empty(), "rules with no reference row: {missing:?}");
        let extra: Vec<&&str> = referenced.difference(&declared).collect();
        assert!(extra.is_empty(), "reference rows for rules nothing declares: {extra:?}");
        for r in RULE_REFERENCE {
            assert!(!r.thresholds.is_empty(), "{}: an empty reference row", r.id);
        }
    }

    use super::*;

    /// The card order [`saturated_signals`] produces, as it stood before the
    /// V42 R7 split and after it. Spelled as strings rather than built from
    /// the `RULE_*` constants on purpose: this list is a RECORD of what users
    /// see, so a rule id that changes has to be retyped here deliberately
    /// instead of following the constant silently.
    ///
    /// `drift.capability.v1/<evidence>` is one consolidated drift notice; the
    /// bare `drift.payload.v1` is the un-attributed payload channel.
    const EXPECTED_CARD_ORDER: &[&str] = &[
        "drift.harness_version.v1",
        "drift.capability.v1/drift.usage_fields_gone.v1",
        "drift.capability.v1/drift.subagent_transcripts.v1",
        "drift.capability.v1/drift.read_reason.v1",
        "drift.capability.v1/drift.read_hook_silent.v1",
        "drift.capability.v1/drift.injection_unseen.v1",
        "drift.capability.v1/drift.payload.v1",
        "drift.payload.v1",
        "detection.update_available.v1",
        "detection.update_failed.v1",
        "detection.update_stalled.v1",
        "detection.signature_down.v1",
        "detection.local_rules_broken.v1",
        "detection.rules_incomplete.v1",
        "drift.capability.v1/drift.read_bypass.v1",
        "advisor.raise_context_min_score.v1",
        "advisor.lower_context_turn_budget_chars.v1",
        "surface.lean.v1",
    ];

    // ── V42 R7 — the two seams the advisor split had to keep ────────────

    /// **The thresholds in [`RULE_REFERENCE`] are the ones the rules use.**
    ///
    /// The published reference restates numbers this module owns ("≥5
    /// sessions, ≥200 injections") in prose, and until V42 R7 nothing checked
    /// them against the constants: bumping `MIN_INJECTIONS` left the panel
    /// telling every user a threshold that had not been true since the bump,
    /// with no test anywhere going red. Every fragment below is built with
    /// `format!` from the constant, so a const change now forces the prose to
    /// follow it.
    ///
    /// Only the numeric claims are pinned — the rest of a row is prose about
    /// what the rule means, which is not a number and cannot drift out of sync
    /// with one.
    #[test]
    fn the_rule_reference_prose_states_the_real_thresholds() {
        let pct = |v: f64| format!("{:.0}%", v * 100.0);
        let row = |id: &str| {
            RULE_REFERENCE
                .iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| panic!("no reference row for {id}"))
        };
        let claims: Vec<(&str, Vec<String>)> = vec![
            (
                RULE_MIN_SCORE,
                vec![
                    format!("≥{MIN_SESSIONS} sessions"),
                    format!("≥{MIN_INJECTIONS} injections"),
                    format!("≥{} never re-touched", pct(UNUSED_HIGH)),
                ],
            ),
            (
                RULE_ADVISOR_LINES,
                vec![
                    format!("≥{MIN_SESSIONS} sessions"),
                    format!("≥{MIN_REMINDS} reminders"),
                    format!("≥{} re-read anyway", pct(REREAD_HIGH)),
                ],
            ),
            (
                RULE_TURN_BUDGET,
                vec![
                    format!("≥{MIN_SESSIONS} sessions"),
                    format!("≥{MIN_INJECTIONS} injections"),
                    format!("≥{MIN_TURNS} turns"),
                    format!("≥{} unread", pct(UNUSED_HIGH)),
                    format!("≥{} turns maxed", pct(BUDGET_MAXED_HIGH)),
                ],
            ),
            (
                RULE_SURFACE_LEAN,
                vec![format!("≥{SURFACE_LEAN_MIN_SESSIONS} sessions")],
            ),
            (
                RULE_ADOPT_ADVISOR,
                vec![
                    format!("≥{MIN_SESSIONS} sessions"),
                    format!("≥{ADOPT_REDUNDANT_HIGH:.0} redundant"),
                    format!("across ≥{ADOPT_MIN_SESSIONS} sessions"),
                ],
            ),
            (
                RULE_ADOPT_SUBSTITUTE,
                vec![
                    format!("≥{SUBSTITUTE_MIN_SAMPLES} reminders"),
                    format!("≤{} re-read anyway", pct(SUBSTITUTE_REREAD_LOW)),
                ],
            ),
            (
                RULE_DRIFT_READ_REASON,
                vec![
                    format!("≥{} reminders", drift::DRIFT_MIN_REMINDS),
                    format!("≥{} immediately re-read", pct(drift::READ_REASON_HIGH)),
                ],
            ),
            (
                RULE_DRIFT_HOOK_SILENT,
                vec![
                    format!("≥{} sessions", drift::DRIFT_SILENT_MIN_SESSIONS),
                    format!("≥{} large re-reads", drift::DRIFT_SILENT_MIN_REREADS),
                ],
            ),
            (
                RULE_DRIFT_INJECTION_UNSEEN,
                vec![
                    format!("≥{} sessions", drift::DRIFT_UNSEEN_MIN_SESSIONS),
                    format!("≥{} injections", drift::DRIFT_UNSEEN_MIN_INJECTIONS),
                    format!("≤{} follow", pct(drift::INJECTION_UNSEEN_LOW)),
                ],
            ),
            (
                RULE_DRIFT_USAGE_FIELDS,
                vec![format!("≥{} sessions", drift::DRIFT_MIN_TOKENLESS)],
            ),
            (
                RULE_DRIFT_READ_BYPASS,
                vec![
                    format!("≥{} reminders", drift::DRIFT_MIN_BYPASS_REMINDS),
                    format!("≥{} answered via shell reads", pct(drift::BYPASS_HIGH)),
                ],
            ),
        ];
        for (id, fragments) in &claims {
            let r = row(id);
            for f in fragments {
                assert!(
                    r.thresholds.contains(f.as_str()),
                    "{id}: the reference row no longer states `{f}` — a threshold constant \
                     changed and the published prose did not follow it.\nrow: {}",
                    r.thresholds
                );
            }
        }
        assert!(
            RULE_REFERENCE_FOOTER.contains(&format!("{APPLY_COOLDOWN_SESSIONS} further sessions")),
            "the footer no longer states APPLY_COOLDOWN_SESSIONS: {RULE_REFERENCE_FOOTER}"
        );
    }

    /// Signals with every rule that CAN co-fire firing at once.
    ///
    /// The mutually exclusive ones are deliberately absent: `adopt.*` and
    /// `advisor.raise_read_advisor_min_lines.v1` are suppressed by
    /// construction whenever a drift rule proposes disabling the read advisor,
    /// which this fixture does (that suppression has its own tests).
    fn saturated_signals() -> Signals {
        Signals {
            // tuning: 0% follow ⇒ 100% unused, every sample floor cleared
            injection_follow_rate: Some(0.0),
            injection_follow_samples: MIN_INJECTIONS,
            advisor_reread_rate: Some(1.0),
            advisor_reread_samples: MIN_REMINDS,
            budget_maxed_rate: Some(1.0),
            budget_maxed_samples: MIN_TURNS,
            session_count: SURFACE_LEAN_MIN_SESSIONS,
            graph: GraphSettings {
                read_advisor: true,
                context_injection: true,
                ..GraphSettings::default()
            },
            // drift: the hook rules
            remind_count: 0,
            large_reread_pairs: drift::DRIFT_SILENT_MIN_REREADS,
            contract_drift: vec![
                "read_hook: session_id".to_string(),
                "nosuchshim: tool_name".to_string(),
            ],
            bypass_rate: Some(1.0),
            bypass_samples: drift::DRIFT_MIN_BYPASS_REMINDS,
            // drift: the per-harness rules
            harness: [(
                crate::harness::DEFAULT_HARNESS,
                DriftSignals {
                    last_seen: "9.9.9".to_string(),
                    last_verified: "1.0.0".to_string(),
                    auto_verify: None,
                    sessions: drift::DRIFT_MIN_TOKENLESS,
                    tokenless_sessions: drift::DRIFT_MIN_TOKENLESS,
                    subagent_drift: vec!["no sub-agent transcripts".to_string()],
                },
            )]
            .into_iter()
            .collect(),
            // surface.lean.v1
            hideable_tool_calls: 0,
            surface_chars: 9_000,
            // every detection canary
            detection_updates: vec![crate::offload::detection::updater::AvailableUpdate {
                component: "rules".into(),
                installed: String::new(),
                available: "2026.09.01".into(),
                notes: String::new(),
            }],
            detection_update_failures: vec![crate::offload::detection::updater::FailedUpdate {
                component: "rules".into(),
                version: "2026.08.07".into(),
                signature: "2026.08.07".into(),
                reason: "checksum".into(),
            }],
            detection_update_stalled: vec![crate::offload::detection::updater::StalledUpdate {
                component: "rules".into(),
                streak: crate::offload::detection::updater::STALLED_AFTER_CHECKS,
                reason: "unreachable".into(),
            }],
            detection_signature_down: Some(crate::offload::detection::signature::SignatureDown {
                dir: "C:/rules.d".into(),
                files_loaded: 0,
                files_failed: 1,
                rules: 0,
                failed: vec!["local/mine.yar".into()],
            }),
            detection_local_rules_broken: Some(
                crate::offload::detection::updater::BrokenLocalRules {
                    dir: "C:/rules.d".into(),
                    failed: vec!["local/mine.yar".into()],
                    renamed: Vec::new(),
                    files_loaded: 3,
                    rules: 12,
                },
            ),
            detection_rules_incomplete: vec![crate::offload::detection::updater::RulesIncomplete {
                component: "rules".into(),
                dir: "C:/rules.d".into(),
                files: vec!["core.yar".into()],
            }],
            ..Signals::default()
        }
    }

    /// **Card order is a user-visible contract** (V42 R7).
    ///
    /// `evaluate` returns a `Vec<Proposal>` and the Advisor panel renders it in
    /// order — `{#each advice.proposals}` in `CodeIntelligenceView.svelte`,
    /// with no sort between here and the DOM. R7 split one 744-line
    /// `drift_rules` into `drift` + `detection`, and the detection block sat in
    /// the MIDDLE of it (ahead of `drift.read_bypass.v1`), so the obvious
    /// "drift, then detection" composition would have quietly reshuffled every
    /// user's cards. This pins the whole sequence rather than that one seam:
    /// any future rule that lands in a different place has to say so here.
    ///
    /// Keyed by `(rule_id, evidence)` because the consolidated drift notices
    /// all carry `drift.capability.v1` — the evidence is the middle field of
    /// the signature (see `drift::tests::is_drift`).
    #[test]
    fn the_card_order_is_pinned() {
        let cards: Vec<String> = evaluate(&saturated_signals())
            .iter()
            .map(|p| match p.rule_id {
                RULE_DRIFT_CAPABILITY => format!(
                    "{}/{}",
                    p.rule_id,
                    p.signature.split(':').nth(1).unwrap_or("?")
                ),
                other => other.to_string(),
            })
            .collect();
        assert_eq!(cards, EXPECTED_CARD_ORDER, "the advisor card order moved");
    }

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
        // strict `== "pass"` check rather than "the capability gate is not
        // blocking", which passes `"unverified"` too.
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
        sig.bypass_rate = Some(drift::BYPASS_HIGH);
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

}

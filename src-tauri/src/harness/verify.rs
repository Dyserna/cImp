//! V35 Phase F — **auto-verify on version change**: run the canaries, advance
//! `claude_last_verified` by itself, and only speak when something actually
//! broke.
//!
//! # The problem this closes
//!
//! `drift.harness_version.v1` fires on *every* Claude Code auto-update whether
//! or not anything broke. Re-running the MAINTENANCE.md recipes costs ten
//! minutes and the update almost never broke anything, so the rational response
//! became clicking **Mark verified** without running them — which disarmed the
//! control guarding all the others (milestone § Why, gap 3). The tripwire was
//! the one *leading* signal cImp had, and it had been trained away.
//!
//! # What happens now
//!
//! 1. The OOB tap records a changed `claude_last_seen` (unchanged from V16).
//! 2. [`on_claude_version_changed`] wakes a single background thread.
//! 3. **L1** — the four embedded fixture canaries
//!    ([`crate::harness::canary::run_embedded`]) run in-process, in
//!    milliseconds, with no CLI needed.
//! 4. **L2** — [`crate::harness::probe::run_for`] drives the *installed* CLI
//!    for the same harness.
//! 5. **Zero `Fail` ⇒ `claude_last_verified` advances on its own** and no
//!    Advisor card appears at all. Otherwise the version stays put and each
//!    failing capability is recorded so the Advisor can raise a
//!    `drift.capability.v1` notice naming it, its evidence, and the `wired_in`
//!    modules that break.
//!
//! # The verdict rule, and why `Unknown` cannot block (locked decision 8)
//!
//! **Advance iff nothing FAILED.** [`probe::Outcome::Unknown`] and
//! [`probe::Outcome::Transition`] never block. An absent CLI, a session with no
//! tool call to read, or an upstream *improvement* (OpenCode growing auth) must
//! not hold a version hostage — modelling those as failures is precisely the
//! alarm fatigue this phase exists to remove, and it would put us back to a card
//! on every update with extra steps.
//!
//! The honest consequence, recorded rather than hidden: on a machine where the
//! CLI cannot be probed, a version advances on **L1 evidence alone** (the
//! embedded fixtures still parse substantively). That is strictly more than the
//! reflexive *Mark verified* it replaces, and L1 is the layer that catches *our*
//! readers regressing — which is the failure mode a user cannot otherwise see.
//!
//! # What this deliberately does not touch
//!
//! `e1_status` / `d0_status` — the Tier-D `Behavior` spikes (D0, E1,
//! OpenCode-veto). No payload reveals whether a deny reason reached the *model*,
//! so no probe can settle them and nothing here writes them. **Mark verified**
//! survives for exactly those rows (locked decision 7), which is what makes the
//! button mean something again.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::harness::contract::{self, Harness};
use crate::harness::probe::{self, Outcome, ProbeResult};
use crate::settings::{AutoVerify, AutoVerifyFailure};

// ── evidence vocabulary ─────────────────────────────────────────────────────

/// An L1 embedded fixture canary saw it. Recorded in
/// [`AutoVerifyFailure::evidence`] and carried into the Advisor notice's
/// signature — the middle field of `<capability>:<evidence>:<key>`, exactly
/// where a V16 detector's rule id goes, because it answers the same question:
/// *what saw this?*
pub const EVIDENCE_CANARY: &str = "harness.canary.l1";
/// An L2 live probe against the installed CLI saw it.
pub const EVIDENCE_PROBE: &str = "harness.probe.l2";
/// A recorded evidence string this build does not recognize — a hand-edited
/// settings file, or a record written by a newer cImp. Parenthesized like
/// `contract::UNKNOWN_CAPABILITY` so it can never be mistaken for a real token.
pub const EVIDENCE_UNRECOGNIZED: &str = "(unrecognized evidence)";

/// The `&'static str` for a recorded evidence string. The Advisor needs a
/// `'static` token for the notice signature and the record only carries a
/// `String`, so the mapping lives here — beside the constants — rather than as
/// a `match` in `advisor.rs` that could grow a third spelling.
pub fn evidence_const(recorded: &str) -> &'static str {
    match recorded {
        EVIDENCE_CANARY => EVIDENCE_CANARY,
        EVIDENCE_PROBE => EVIDENCE_PROBE,
        _ => EVIDENCE_UNRECOGNIZED,
    }
}

// ── bounds ──────────────────────────────────────────────────────────────────

/// Overall wall-clock budget for one auto-verify run.
///
/// Enforced **between layers** rather than by abandoning work mid-probe: every
/// individual step is already bounded (`SERVE_READY_TIMEOUT` 10s,
/// `HELP_TIMEOUT` 20s, `HTTP_TIMEOUT` 5s, a 512 KiB transcript tail), so a
/// deadline checked before the L2 group caps the total at this plus the longest
/// step. Killing a probe mid-flight would leave a `claude --help` or an
/// `opencode serve` child behind, which is a worse failure than a slow answer:
/// the point of the bound is that the background thread cannot live forever, not
/// that it hits a stopwatch exactly.
const OVERALL_CAP: Duration = Duration::from_secs(90);

/// How many times the worker re-checks for a newer version before giving up.
///
/// A trigger arriving while a run is in flight is DROPPED (single-flight), so
/// the worker re-reads `claude_last_seen` after each run instead — otherwise an
/// update landing mid-probe would be verified only at the next app start. A
/// round only happens for a version this worker has not already answered for,
/// so it terminates on its own; the cap bounds the pathological case of a CLI
/// updating faster than the probes finish.
const MAX_ROUNDS: usize = 3;

// ── the report ──────────────────────────────────────────────────────────────

/// One capability's answer in an auto-verify run.
#[derive(Debug, Clone)]
pub struct Answer {
    /// The registry row's own `id` — the join key the record, the Advisor
    /// notice and the gate all speak.
    pub id: &'static str,
    /// [`EVIDENCE_CANARY`] or [`EVIDENCE_PROBE`].
    pub evidence: &'static str,
    /// Reused from the L2 probe wholesale rather than re-modelled: a canary's
    /// `Ok`/`Err` maps onto `Pass`/`Fail`, and the four-value outcome is the one
    /// thing locked decision 8 is about. A second enum here would be a second
    /// place for "unavailable is not broken" to be got wrong.
    pub outcome: Outcome,
}

/// Everything one auto-verify run learned.
#[derive(Debug, Clone)]
pub struct Report {
    pub harness: Harness,
    /// One [`Answer`] per capability driven, L1 first.
    pub answers: Vec<Answer>,
    /// The [`OVERALL_CAP`] was already spent before the L2 group started, so
    /// the probes did not run. Recorded, never scored: an unrun probe is
    /// `Unknown` by definition and `Unknown` does not block.
    pub capped: bool,
}

impl Report {
    /// Every capability that FAILED, in run order.
    pub fn failures(&self) -> impl Iterator<Item = &Answer> {
        self.answers.iter().filter(|a| a.outcome.is_fail())
    }

    /// **The verdict** (locked decision 8): advance iff nothing failed.
    pub fn advances(&self) -> bool {
        self.failures().next().is_none()
    }

    /// `(pass, fail, unknown, transition)` — the one-line summary the log
    /// carries, so a background run leaves evidence even when it changes
    /// nothing.
    pub fn tally(&self) -> (usize, usize, usize, usize) {
        let count = |l: &str| self.answers.iter().filter(|a| a.outcome.label() == l).count();
        (
            count("pass"),
            count("fail"),
            count("unknown"),
            count("transition"),
        )
    }

    /// The persisted record for this run.
    ///
    /// `status` and `failures` are derived from the SAME answers in one place,
    /// which is what keeps the invariant on [`AutoVerify`] (status is `fail` iff
    /// the list is non-empty) true by construction rather than by convention.
    pub fn record(&self, version: &str, at_ms: u64) -> AutoVerify {
        let failures: Vec<AutoVerifyFailure> = self
            .failures()
            .map(|a| AutoVerifyFailure {
                capability: a.id.to_string(),
                evidence: a.evidence.to_string(),
                detail: a.outcome.detail().to_string(),
            })
            .collect();
        AutoVerify {
            version: version.to_string(),
            at_ms,
            status: if failures.is_empty() {
                AutoVerify::PASS.to_string()
            } else {
                AutoVerify::FAIL.to_string()
            },
            failures,
        }
    }
}

// ── running it ──────────────────────────────────────────────────────────────

/// Drive one harness's L1 canaries and L2 probes. Blocking; spawns child
/// processes for the L2 half, so this belongs on a background thread.
pub fn verify(harness: Harness) -> Report {
    verify_until(harness, Instant::now() + OVERALL_CAP)
}

fn verify_until(harness: Harness, deadline: Instant) -> Report {
    let mut answers: Vec<Answer> = Vec::new();

    // L1 — free, no CLI needed, and the layer that catches OUR readers
    // regressing. Runs first for that reason: if the fixtures no longer parse
    // substantively the probes cannot tell us anything the canaries have not
    // already said, and it costs milliseconds to know.
    for id in crate::harness::canary::EMBEDDED {
        let Some(cap) = contract::get(id) else {
            // Unreachable in a built binary — `canary::tests::
            // embedded_canaries_are_exactly_the_declared_ones` pins the join —
            // but reported rather than skipped, because a silently dropped
            // canary is a check that stopped running with nobody noticing.
            answers.push(Answer {
                id,
                evidence: EVIDENCE_CANARY,
                outcome: Outcome::Unknown {
                    why: "the embedded canary names no registry row, so its harness is unknown"
                        .to_string(),
                },
            });
            continue;
        };
        if cap.harness != harness {
            continue;
        }
        let outcome = match crate::harness::canary::run_embedded(cap.id) {
            Some(Ok(())) => Outcome::Pass {
                detail: "the embedded fixture still produces substantive output".to_string(),
            },
            Some(Err(detail)) => Outcome::Fail { detail },
            None => Outcome::Unknown {
                why: "no embedded canary is wired for this capability".to_string(),
            },
        };
        answers.push(Answer {
            id: cap.id,
            evidence: EVIDENCE_CANARY,
            outcome,
        });
    }

    // L2 — the installed CLI. Skipped wholesale once the budget is spent.
    let capped = Instant::now() >= deadline;
    if !capped {
        answers.extend(probe::run_for(harness).into_iter().map(answer_from_probe));
    }

    Report {
        harness,
        answers,
        capped,
    }
}

fn answer_from_probe(r: ProbeResult) -> Answer {
    Answer {
        id: r.id,
        evidence: EVIDENCE_PROBE,
        outcome: r.outcome,
    }
}

// ── what the panel reads (V35 Phase G) ──────────────────────────────────────

/// One completed run, kept **in memory only**, per harness.
///
/// The persisted [`AutoVerify`] record is failures-only by design (it is what
/// the Advisor needs, and a per-capability result table would be a growing
/// Settings field plus the schema bump the milestone's deploy-trap note warns
/// about). That is enough to raise a card and enough to gate an advance, but it
/// cannot answer *this* row's question in the Harness health panel: a row the
/// record does not name might have passed, or might not have been checkable at
/// all, and the disk cannot tell those apart.
///
/// So the full four-value answer lives here, for the life of the process, and
/// the panel prefers it when present. Losing it on restart is the right
/// trade — a run is cheap to repeat (*Run checks now*), and the alternative is
/// storing state whose staleness would be invisible.
///
/// It is also the ONLY place an OpenCode run is reported: Phase F persists a
/// Claude record and nothing else, and inventing an `opencode_auto_verify`
/// field to make the button symmetric would be exactly the stored state this
/// phase does not need.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub harness: Harness,
    /// The harness version the run was made against — empty when cImp has never
    /// observed one (a CLI that has not written a transcript yet).
    pub version: String,
    /// Wall-clock ms the run finished.
    pub at_ms: u64,
    /// The L2 group was skipped for budget. Recorded, never scored.
    pub capped: bool,
    /// Every answer, in run order — including the `unknown`s and `transition`s
    /// the record deliberately drops.
    pub answers: Vec<Answer>,
}

/// At most one entry per harness — a short list rather than a map because
/// [`Harness`] is a two-value enum in practice and a linear scan of two is not
/// worth a hash. Poisoning is swallowed on both sides: a panicking writer must
/// degrade the panel to "no run recorded", never take the Settings window down
/// with it.
static LAST_RUNS: Mutex<Vec<RunSummary>> = Mutex::new(Vec::new());

/// Record a finished run, replacing this harness's previous one.
fn remember(report: &Report, version: &str) {
    let summary = RunSummary {
        harness: report.harness,
        version: version.to_string(),
        at_ms: crate::activity::now_ms(),
        capped: report.capped,
        answers: report.answers.clone(),
    };
    let Ok(mut runs) = LAST_RUNS.lock() else {
        return;
    };
    runs.retain(|r| r.harness != summary.harness);
    runs.push(summary);
}

/// The last run this process made for `harness`, if any.
pub fn last_run(harness: Harness) -> Option<RunSummary> {
    LAST_RUNS
        .lock()
        .ok()?
        .iter()
        .find(|r| r.harness == harness)
        .cloned()
}

/// Whether a verify worker is running right now — the panel's in-flight state,
/// and the reason a second *Run checks now* click is a no-op rather than a
/// second set of child processes.
pub fn in_flight() -> bool {
    IN_FLIGHT.load(Ordering::SeqCst)
}

// ── the triggers ────────────────────────────────────────────────────────────

/// Set while a verify worker is alive. A second trigger is DROPPED rather than
/// queued — the worker re-reads `claude_last_seen` after each round, so
/// dropping loses nothing except a duplicate run.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Clears [`IN_FLIGHT`] however the worker leaves — including by panicking,
/// which a bare `store` at the end of the closure would not cover. A stuck flag
/// would disable auto-verify for the rest of the process's life, silently.
struct InFlight;

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.store(false, Ordering::SeqCst);
    }
}

/// Trigger (a): the version writer just recorded a **changed**
/// `claude_last_seen`. Called from `settings::note_harness_version`, which is
/// the one place that write happens.
///
/// Non-blocking by construction — the caller (the OOB transcript tap, mid
/// session) must never wait on a probe.
pub fn on_claude_version_changed() {
    spawn_worker("claude version change");
}

/// Trigger (b): once at app startup, when the seen and verified versions do not
/// match.
///
/// This is what covers the cases trigger (a) structurally cannot: an update
/// that happened while cImp was closed (the common one — both CLIs self-update
/// on their own schedule), a hand-edited `settings.json`, and a run that was
/// dropped for single-flight and never re-armed. Cheap: two string comparisons
/// against a mtime-cached read, and no thread at all in the overwhelmingly
/// common already-verified case.
///
/// It deliberately re-runs a version whose last auto-verify **failed** rather
/// than trusting the stored record. A failure is the one state that can be
/// fixed on *our* side — the reader is repaired and cImp is rebuilt, with the
/// harness version unchanged — and a card that could only be cleared by an
/// upstream update or a manual click would be a stale alarm by construction.
/// The cost is one background run per launch while a failure stands, which is
/// exactly the situation where re-checking is worth its seconds.
pub fn spawn_startup_check() {
    let hv = crate::settings::read_global_harness_versions();
    if hv.claude_last_seen.trim().is_empty()
        || hv.claude_last_seen.trim() == hv.claude_last_verified.trim()
    {
        return;
    }
    spawn_worker("startup (seen != verified)");
}

/// Trigger (c), V35 Phase G: the Settings → Harness health panel's **Run
/// checks now**, for one harness.
///
/// The pointer above is deliberately unadorned prose: `settings::
/// frontend_mirrors::every_settings_pointer_names_a_real_sidebar_section` (the
/// #48 F-18 tripwire) matches the sidebar label immediately after the arrow, so
/// wrapping the section name in emphasis makes the pointer resolve to nothing.
///
/// Returns whether a run was STARTED. `false` means one was already in flight
/// and this click was dropped — the panel surfaces that rather than queueing a
/// second set of `claude --help` / `opencode serve` children, and the single
/// flight is shared with the automatic triggers on purpose: they do the same
/// work, and two of them racing would double the child processes to learn the
/// same thing twice.
///
/// Unlike the automatic triggers this does **not** require `seen != verified`.
/// A user asking for the checks is asking about the build installed right now,
/// whatever cImp last stamped.
pub fn run_now(harness: Harness) -> bool {
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        tracing::debug!(?harness, "harness verify already running; Run checks now was a no-op");
        return false;
    }
    let spawned = std::thread::Builder::new()
        .name("harness-verify-manual".to_string())
        .spawn(move || {
            let _guard = InFlight;
            manual_run(harness);
        });
    if let Err(e) = spawned {
        IN_FLIGHT.store(false, Ordering::SeqCst);
        tracing::warn!(error = %e, "manual harness verify thread could not be spawned");
        return false;
    }
    true
}

/// One **Run checks now** click.
///
/// The Claude arm goes through [`run_once`] — the same write path Phase F's
/// automatic run uses, so a manual run records the same `claude_auto_verify`
/// and advances `claude_last_verified` under the same all-pass rule. A second
/// spelling of that write is how the button and the worker would come to
/// disagree about what "verified" means.
fn manual_run(harness: Harness) {
    let hv = crate::settings::read_global_harness_versions();
    match harness {
        Harness::Claude => {
            let seen = hv.claude_last_seen.trim().to_string();
            if seen.is_empty() {
                // Nothing to stamp a record against: cImp has never observed
                // this CLI write a transcript. Run the checks anyway — the
                // per-capability answers are what the panel shows — but write
                // NO record. An `AutoVerify` stamped with an empty version
                // would compare equal to every other empty version, and the
                // all-pass advance would overwrite a real `claude_last_verified`
                // with "".
                let report = verify(Harness::Claude);
                tracing::info!(tally = ?report.tally(), "manual harness verify ran with no known version; not recorded");
                remember(&report, "");
                return;
            }
            run_once(&seen);
        }
        other => {
            // No persisted record exists for any other harness (see
            // [`RunSummary`]), so the in-memory summary IS the result.
            let version = match other {
                Harness::OpenCode => hv.opencode_last_seen.trim().to_string(),
                _ => String::new(),
            };
            let report = verify(other);
            let (pass, fail, unknown, transition) = report.tally();
            tracing::info!(
                harness = ?other,
                version = %version,
                pass, fail, unknown, transition,
                capped = report.capped,
                "manual harness verify finished"
            );
            remember(&report, &version);
        }
    }
}

fn spawn_worker(why: &'static str) {
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        tracing::debug!(trigger = why, "harness auto-verify already running; trigger dropped");
        return;
    }
    // A plain OS thread, not a tokio task: the work is blocking end to end
    // (child processes, socket reads, file tails) and both call sites — the
    // transcript tap's `spawn_blocking` and Tauri's `setup` — are places where
    // parking a runtime worker for a minute would be a defect.
    let spawned = std::thread::Builder::new()
        .name("harness-verify".to_string())
        .spawn(move || {
            let _guard = InFlight;
            worker(why);
        });
    if let Err(e) = spawned {
        IN_FLIGHT.store(false, Ordering::SeqCst);
        tracing::warn!(error = %e, "harness auto-verify thread could not be spawned");
    }
}

/// One worker's life: verify the currently-seen version, then re-check in case
/// the tap saw a newer one while we were probing.
fn worker(why: &'static str) {
    // The version this worker has already answered for. A run that FAILED
    // leaves `seen != verified`, so without this the loop below would re-run
    // the same version until `MAX_ROUNDS` — burning three probe runs to learn
    // the same thing three times.
    let mut ran: Option<String> = None;
    for round in 0..MAX_ROUNDS {
        let hv = crate::settings::read_global_harness_versions();
        let seen = hv.claude_last_seen.trim().to_string();
        if seen.is_empty() || seen == hv.claude_last_verified.trim() {
            return;
        }
        if ran.as_deref() == Some(seen.as_str()) {
            return;
        }
        tracing::info!(trigger = why, round, version = %seen, "harness auto-verify starting");
        run_once(&seen);
        ran = Some(seen);
    }
}

/// Verify `version` and write the outcome — the ONE place `claude_last_verified`
/// advances without a click.
fn run_once(version: &str) {
    let report = verify(Harness::Claude);
    let record = report.record(version, crate::activity::now_ms());
    let advance = report.advances();
    let (pass, fail, unknown, transition) = report.tally();
    // V35 Phase G: keep the FULL answer set in memory too. The record about to
    // be written keeps failures only, so without this the Harness health panel
    // could never distinguish "passed" from "could not be checked" for a row
    // that did not fail.
    remember(&report, version);

    // Both the record and the advance go through `mutate_global_harness_versions`
    // in ONE call. Never `save_settings`, never the overlay path: `harness_versions`
    // is banned from the project overlay diff (`ipc/commands.rs`) and is written
    // out-of-band by the tap, tab spawn and `harness_mark_verified` — a Settings
    // save carrying it would be the defect the milestone's deploy-trap note names.
    // One call also means the record and the advance cannot disagree: either the
    // file write lands or neither does.
    let version = version.to_string();
    let stamp = version.clone();
    let res = crate::settings::mutate_global_harness_versions(move |hv| {
        hv.claude_auto_verify = Some(record.clone());
        // Re-read inside the write: the tap may have seen a NEWER version while
        // the probes ran. Stamping `claude_last_verified` with a version we did
        // not verify would be a lie — and the worker's next round picks the new
        // one up.
        if advance && hv.claude_last_seen.trim() == stamp {
            hv.claude_last_verified = stamp.clone();
        }
    });

    match res {
        Ok(_) if advance => tracing::info!(
            harness = ?report.harness,
            version = %version,
            pass, fail, unknown, transition,
            capped = report.capped,
            "harness auto-verify passed; claude_last_verified advanced with no user action"
        ),
        Ok(_) => tracing::warn!(
            harness = ?report.harness,
            version = %version,
            pass, fail, unknown, transition,
            failures = %report.failures().map(|a| a.id).collect::<Vec<_>>().join(", "),
            "harness auto-verify FAILED; version not advanced, Advisor will name the capabilities"
        ),
        Err(e) => tracing::warn!(error = %e, version = %version, "harness auto-verify could not record its outcome"),
    }
}

// ── what the Advisor reads (V35 Phase F) ────────────────────────────────────

/// Whether a recorded run supersedes the version tripwire for `seen`.
///
/// `drift.harness_version.v1` becomes the **cannot-verify fallback**: it speaks
/// only when nothing else can. It is superseded exactly when auto-verify ran
/// against this same version and found failures — because then the Advisor
/// raises one `drift.capability.v1` notice per failing capability, naming what
/// broke and where, and a second card saying "the version moved, go check by
/// hand" would be noise beside it.
///
/// Note what does NOT supersede it: a recorded `pass` (a pass advances the
/// version, so if the versions still differ the advance did not land and the
/// tripwire is the only remaining signal), a record for a different version,
/// no record at all (a run that could not complete writes none), and a status
/// this build does not recognize. Those are all *cannot verify*, which is the
/// case the tripwire is kept for.
pub fn tripwire_superseded(record: Option<&AutoVerify>, seen: &str) -> bool {
    record.is_some_and(|r| {
        r.version == seen && r.status == AutoVerify::FAIL && !r.failures.is_empty()
    })
}

/// The failures the Advisor should raise a notice for, or an empty slice.
///
/// Three conditions, all of them about not speaking out of turn:
/// * the record is for the version currently INSTALLED (`seen`) — an older
///   record says nothing about today's build;
/// * `seen != verified` — a user who recorded a manual verification of this
///   exact version has out-ranked the automatic one, and re-raising the notice
///   would make **Mark verified** feel broken;
/// * there is something to name (global principle 5 — a `fail` with an empty
///   list would render a card with no fix pointer).
pub fn notifiable_failures<'a>(
    record: Option<&'a AutoVerify>,
    seen: &str,
    verified: &str,
) -> &'a [AutoVerifyFailure] {
    match record {
        Some(r) if r.version == seen && seen != verified && !r.failures.is_empty() => &r.failures,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(id: &'static str, outcome: Outcome) -> Answer {
        Answer {
            id,
            evidence: EVIDENCE_PROBE,
            outcome,
        }
    }

    fn report(answers: Vec<Answer>) -> Report {
        Report {
            harness: Harness::Claude,
            answers,
            capped: false,
        }
    }

    /// **The locked verdict rule** (milestone decision 8). `Unknown` and
    /// `Transition` never block; one `Fail` does.
    ///
    /// This is the assertion the whole phase rests on: if `Unknown` blocked,
    /// every machine without the CLI on PATH would sit on a permanent card —
    /// the alarm fatigue V35 exists to remove, reintroduced through the feature
    /// that was supposed to remove it.
    #[test]
    fn only_a_failure_blocks_the_auto_advance() {
        let unknown = Outcome::Unknown {
            why: "CLI absent".to_string(),
        };
        let transition = Outcome::Transition {
            note: "auth landed".to_string(),
        };
        let pass = Outcome::Pass {
            detail: "ok".to_string(),
        };

        assert!(report(vec![]).advances(), "an empty run cannot fail");
        assert!(report(vec![answer("a", pass.clone())]).advances());
        assert!(report(vec![answer("a", unknown.clone())]).advances());
        assert!(report(vec![answer("a", transition.clone())]).advances());
        assert!(report(vec![
            answer("a", pass),
            answer("b", unknown),
            answer("c", transition),
        ])
        .advances());

        assert!(!report(vec![answer(
            "a",
            Outcome::Fail {
                detail: "drifted".to_string()
            }
        )])
        .advances());
    }

    /// The record's `status` and `failures` are two views of one fact, and the
    /// evidence + detail survive into it — that is what the Advisor card is
    /// built from, so a record that dropped them would produce a notice naming
    /// a capability with no reason.
    #[test]
    fn a_record_agrees_with_itself() {
        let clean = report(vec![answer(
            "claude.transcript.usage",
            Outcome::Pass {
                detail: "ok".to_string(),
            },
        )])
        .record("2.2.0", 1234);
        assert_eq!(clean.status, AutoVerify::PASS);
        assert!(clean.failures.is_empty());
        assert_eq!(clean.version, "2.2.0");
        assert_eq!(clean.at_ms, 1234);

        let broken = report(vec![
            answer(
                "claude.transcript.usage",
                Outcome::Pass {
                    detail: "ok".to_string(),
                },
            ),
            Answer {
                id: "claude.statusline.stdin",
                evidence: EVIDENCE_CANARY,
                outcome: Outcome::Fail {
                    detail: "context_window.used_percentage gone".to_string(),
                },
            },
        ])
        .record("2.2.0", 7);
        assert_eq!(broken.status, AutoVerify::FAIL);
        assert_eq!(broken.failures.len(), 1);
        assert_eq!(broken.failures[0].capability, "claude.statusline.stdin");
        assert_eq!(broken.failures[0].evidence, EVIDENCE_CANARY);
        assert_eq!(
            broken.failures[0].detail,
            "context_window.used_percentage gone"
        );
        assert_eq!(evidence_const(&broken.failures[0].evidence), EVIDENCE_CANARY);
    }

    /// An evidence string this build does not know must not be silently
    /// promoted to one it does — a hand-edited record would otherwise put an
    /// arbitrary token into a notice signature.
    #[test]
    fn an_unrecognized_evidence_string_stays_unrecognized() {
        assert_eq!(evidence_const(EVIDENCE_CANARY), EVIDENCE_CANARY);
        assert_eq!(evidence_const(EVIDENCE_PROBE), EVIDENCE_PROBE);
        assert_eq!(evidence_const("harness.canary"), EVIDENCE_UNRECOGNIZED);
        assert_eq!(evidence_const(""), EVIDENCE_UNRECOGNIZED);
    }

    fn failed_record(version: &str) -> AutoVerify {
        AutoVerify {
            version: version.to_string(),
            at_ms: 1,
            status: AutoVerify::FAIL.to_string(),
            failures: vec![AutoVerifyFailure {
                capability: "claude.statusline.stdin".to_string(),
                evidence: EVIDENCE_CANARY.to_string(),
                detail: "context_window gone".to_string(),
            }],
        }
    }

    /// The tripwire's new job: speak only when nothing else can.
    #[test]
    fn the_tripwire_is_superseded_only_by_a_failure_for_this_exact_version() {
        assert!(!tripwire_superseded(None, "2.2.0"), "never run ⇒ fallback");
        assert!(tripwire_superseded(Some(&failed_record("2.2.0")), "2.2.0"));
        assert!(
            !tripwire_superseded(Some(&failed_record("2.1.0")), "2.2.0"),
            "a record for an older build says nothing about this one"
        );

        // A pass that somehow left the versions apart (the advance did not
        // land) keeps the tripwire — it is the only signal left.
        let passed = AutoVerify {
            version: "2.2.0".to_string(),
            at_ms: 1,
            status: AutoVerify::PASS.to_string(),
            failures: Vec::new(),
        };
        assert!(!tripwire_superseded(Some(&passed), "2.2.0"));

        // A status this build does not know (a hand edit, or a record written
        // by a newer cImp) is NOT read as a failure: the fallback speaking is
        // the safe direction, silently suppressing a card is not.
        let unrecognized = AutoVerify {
            version: "2.2.0".to_string(),
            at_ms: 1,
            status: "FAILED".to_string(),
            failures: vec![AutoVerifyFailure::default()],
        };
        assert!(!tripwire_superseded(Some(&unrecognized), "2.2.0"));

        // A `fail` with nothing named would raise a card with no fix pointer.
        let empty_fail = AutoVerify {
            version: "2.2.0".to_string(),
            at_ms: 1,
            status: AutoVerify::FAIL.to_string(),
            failures: Vec::new(),
        };
        assert!(!tripwire_superseded(Some(&empty_fail), "2.2.0"));
    }

    /// A manual **Mark verified** out-ranks a stale automatic failure.
    ///
    /// Without this the button would look broken: the user re-runs the recipes,
    /// records the verification, and the same capability card comes straight
    /// back because a record from before the fix is still on disk.
    #[test]
    fn a_manual_verification_silences_a_stale_auto_failure() {
        let rec = failed_record("2.2.0");
        assert_eq!(notifiable_failures(Some(&rec), "2.2.0", "2.1.0").len(), 1);
        assert!(notifiable_failures(Some(&rec), "2.2.0", "2.2.0").is_empty());
        assert!(
            notifiable_failures(Some(&rec), "2.3.0", "2.1.0").is_empty(),
            "a record for the previous build must not speak about this one"
        );
        assert!(notifiable_failures(None, "2.2.0", "2.1.0").is_empty());
    }

    /// A real run of the CLAUDE half: the L1 canaries are driven, they pass,
    /// and every answer joins back to a registry row.
    ///
    /// The deadline is set in the past so the L2 probes are skipped — this test
    /// must never spawn `claude --help` or read a user's transcripts, and the
    /// skip path is itself worth pinning: `capped` is recorded, and the run
    /// still advances, because an unrun probe is `Unknown` and `Unknown` never
    /// blocks.
    #[test]
    fn an_l1_only_run_drives_this_harnesss_canaries_and_advances() {
        let report = verify_until(Harness::Claude, Instant::now() - Duration::from_secs(1));
        assert!(report.capped, "the L2 half must have been skipped");
        assert_eq!(
            report.answers.len(),
            4,
            "expected the four Claude canaries (V35 Phase L added \
             `claude.transcript.assistant_text`), got {:?}",
            report.answers.iter().map(|a| a.id).collect::<Vec<_>>()
        );
        for a in &report.answers {
            assert_eq!(a.evidence, EVIDENCE_CANARY);
            let cap = contract::get(a.id).expect("every answer names a registry row");
            assert_eq!(cap.harness, Harness::Claude, "wrong harness for {}", a.id);
            assert_eq!(
                a.outcome.label(),
                "pass",
                "{}: {}",
                a.id,
                a.outcome.detail()
            );
        }
        assert!(report.advances());
        assert_eq!(report.tally(), (4, 0, 0, 0));
        assert_eq!(report.record("2.2.0", 0).status, AutoVerify::PASS);
    }

    /// The other harness's canary is not run for Claude — a version change in
    /// one CLI must not report the other's readers.
    #[test]
    fn a_run_is_scoped_to_one_harness() {
        let claude = verify_until(Harness::Claude, Instant::now() - Duration::from_secs(1));
        assert!(!claude.answers.iter().any(|a| a.id.starts_with("opencode.")));
        let opencode = verify_until(Harness::OpenCode, Instant::now() - Duration::from_secs(1));
        assert_eq!(
            opencode.answers.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec!["opencode.sse.events"]
        );
        assert!(opencode.advances());
    }

    /// V35 Phase G: the in-memory run summary, which is what lets the panel
    /// tell "passed" from "could not be checked" for a row the stored record
    /// does not name.
    ///
    /// Keyed on [`Harness::Any`] deliberately. [`LAST_RUNS`] is process-wide
    /// and `harness::health` asks it about Claude and OpenCode, so writing a
    /// real harness here would leak into that module's fixtures depending on
    /// test order — the neutral marker is invisible to it.
    #[test]
    fn a_remembered_run_is_readable_and_replaced_per_harness() {
        assert!(
            last_run(Harness::Any).is_none(),
            "nothing is ever recorded for the neutral harness outside this test"
        );

        let first = Report {
            harness: Harness::Any,
            answers: vec![answer(
                "a",
                Outcome::Pass {
                    detail: "one".to_string(),
                },
            )],
            capped: true,
        };
        remember(&first, "1.0.0");
        let got = last_run(Harness::Any).expect("the run was recorded");
        assert_eq!(got.version, "1.0.0");
        assert!(got.capped, "the budget flag survives");
        assert!(got.at_ms > 0, "a run is stamped so the panel can age it");
        assert_eq!(got.answers.len(), 1);

        let second = Report {
            harness: Harness::Any,
            answers: vec![
                answer(
                    "b",
                    Outcome::Unknown {
                        why: "two".to_string(),
                    },
                ),
                answer(
                    "c",
                    Outcome::Fail {
                        detail: "three".to_string(),
                    },
                ),
            ],
            capped: false,
        };
        remember(&second, "2.0.0");
        let got = last_run(Harness::Any).expect("the run was recorded");
        assert_eq!(got.version, "2.0.0", "the newer run replaces the older one");
        assert!(!got.capped);
        assert_eq!(
            got.answers.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        // The `unknown` survives, which is the whole point: `Report::record`
        // drops it, and its silence in the stored record is indistinguishable
        // from a pass.
        assert_eq!(got.answers[0].outcome.label(), "unknown");
        assert_eq!(got.answers[1].outcome.label(), "fail");
    }

    /// Single-flight: the second trigger while one is in flight is dropped, and
    /// the flag is released however the worker leaves.
    ///
    /// Exercised through the guard rather than by racing real workers: what
    /// matters is that a `swap` sees the flag, and that a panicking worker
    /// cannot leave auto-verify disabled for the life of the process.
    #[test]
    fn the_in_flight_guard_admits_one_and_always_releases() {
        assert!(!IN_FLIGHT.swap(true, Ordering::SeqCst), "starts clear");
        assert!(
            IN_FLIGHT.swap(true, Ordering::SeqCst),
            "a second trigger sees the flag and is dropped"
        );
        {
            let _guard = InFlight;
        }
        assert!(
            !IN_FLIGHT.load(Ordering::SeqCst),
            "the guard clears the flag on drop — including on an unwind"
        );
    }
}

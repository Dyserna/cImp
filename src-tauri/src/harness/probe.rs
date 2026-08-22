//! V35 Phase D — the **L2 live probe**: `cimp --harness-canary [--json]`.
//!
//! # What this is, next to the canaries
//!
//! [`crate::harness::canary`] (L1) asks *"do we still parse the shape we
//! recorded?"* against committed fixtures, every `cargo test`, for free. It
//! cannot answer the other half — a fixture is a snapshot of the past, so L1
//! passes forever while upstream moves. This module asks *"is the recorded
//! shape still real?"* by driving the **installed** CLIs. It needs no app
//! instance, no loopback server and no settings file, so it runs from a
//! maintenance script or a shell as easily as from a future auto-run
//! (Phase F).
//!
//! # "Unavailable" is not "broken" (milestone locked decision 8)
//!
//! The whole design constraint. [`Outcome`] has four values, and only one of
//! them is a failure:
//!
//! * [`Outcome::Pass`] — the surface was driven and answered substantively.
//! * [`Outcome::Fail`] — the surface was driven and **drifted**. The only
//!   thing that makes the process exit non-zero.
//! * [`Outcome::Unknown`] — the probe could not run: CLI absent, no session to
//!   tail, no tool call in the window, or a probe class not implemented here.
//!   Reported with a reason, never counted as broken.
//! * [`Outcome::Transition`] — upstream changed **for the better**. A capability
//!   transition, not a red test. OpenCode growing server auth was the worked
//!   example until 2026-08-17, when that transition COMPLETED: cImp sets a
//!   password at every spawn and the probe now checks the auth contract instead
//!   of watching for it, so the variant is currently declared and unconstructed
//!   (see [`Outcome::Transition`]).
//!
//! Modelling the last two as failures would recreate exactly the alarm fatigue
//! this milestone exists to remove: `drift.harness_version.v1` fires on every
//! CLI auto-update, so the rational response became clicking *Mark verified*
//! without running anything — which disarmed the control guarding all the
//! others. A probe nobody trusts is worse than no probe.
//!
//! # What is probed, and what is only enumerated
//!
//! [`IMPLEMENTED`] holds the rows this phase actually drives — the ones
//! reachable without scripting a model turn. [`declared_unprobed`] holds the
//! others, each with the reason it cannot be, and they are **printed**
//! as `unknown` rather than silently omitted: a dependency that stops being
//! listed is a dependency that stopped being counted without anyone deciding
//! to. Neither list is hand-reconciled — `contract.rs`'s
//! `probes_and_the_matrix_agree` set-compares both against the registry in both
//! directions, and every capability must be in exactly one of them.
//!
//! `probe: Some(..)` is deliberately NOT set for the second list. Counting a
//! permanent-`unknown` emitter as coverage would let a row look probed while
//! nothing ever drives it — the "quality signal with no consumer" failure mode
//! the registry exists to prevent.
//!
//! # Where the probe BODIES live (V40 Phase A, locked decision 17)
//!
//! This module is the harness-neutral **runner**: the outcome model, the report
//! shape, the `cimp --harness-canary` CLI, the declared-vs-driven ledger and the
//! order harnesses are driven in. What is actually driven against an installed
//! CLI lives with the harness it is true of — `harness/claude/probe.rs`,
//! `harness/opencode/probe.rs` — and reaches here through
//! [`HarnessPlugin::probe`](crate::harness::plugin::HarnessPlugin::probe). There
//! is no harness `match` here; [`drive`] asks the registry.
//!
//! The privacy discipline the transcript probes follow travels with them: they
//! read a **real** session JSONL, so nothing is written, nothing is copied, and
//! every detail string carries counts and field names only. See
//! `harness/claude/probe.rs`'s module docs.
//!
//! # Capture-on-success (V35 Phase H)
//!
//! Since Phase H the probes ALSO hand over the payloads they read, so a run
//! that found no drift can leave a known-good corpus behind
//! ([`crate::harness::capture`]). Two properties of that are load-bearing here:
//!
//! * **Nothing about the report changed.** [`ProbeResult`], [`Outcome`] and the
//!   order [`run`] assembles them in are exactly what `verify.rs` and
//!   `health.rs` already consume. Capture is a side channel ([`Driven`]), not a
//!   reshaping — a phase that made the report carry payloads would have put
//!   transcript content into the Advisor and the Settings panel.
//! * **This module still writes nothing.** It hands raw text to `capture`,
//!   which scrubs at the boundary. The privacy discipline above (details carry
//!   counts and field names only) is unchanged and is *why* the two are
//!   separate: a detail string is printed, a capture is scrubbed and filed.

use std::time::Duration;

use serde_json::Value;

use crate::harness::capture::{self, Observed};
use crate::harness::contract::{self, Harness, Seam};

// ── outcome model ───────────────────────────────────────────────────────────

/// The result of driving one capability. See the module docs — only
/// [`Outcome::Fail`] affects the exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Driven, and the answer was substantive. `detail` says what was observed
    /// (counts, ids, statuses) so a passing run is still evidence.
    Pass { detail: String },
    /// Driven, and it drifted. The **only** non-zero-exit outcome.
    Fail { detail: String },
    /// Could not be driven. Never a failure (locked decision 8).
    Unknown { why: String },
    /// Upstream changed for the better; the capability moves rather than
    /// breaks. Never a failure.
    ///
    /// **Declared and, since 2026-08-17, unconstructed in production** — the same
    /// posture `Seam::A` and `Harness::ANY` have in the registry, and for the same
    /// reason: a vocabulary needs the rung named even when nothing is standing on
    /// it. Its one constructor was `noauth_outcome`, watching for OpenCode to grow
    /// server auth. It did, cImp now sets a password at every spawn
    /// (`opencode.route.noauth`, Tier D → B), and the improvement this variant
    /// existed to announce is the contract the probe checks. So there is no probe
    /// watching for an upstream improvement today, which is a fact worth being
    /// able to read rather than one to hide behind a deleted variant — every
    /// consumer (`verify.rs`'s advance rule, `label`, `detail`, the printer,
    /// `--json`) still handles it, and the next D→C→B migration will construct it
    /// again.
    #[allow(dead_code)]
    Transition { note: String },
}

impl Outcome {
    /// Stable machine token, used by `--json` and as the human column.
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Pass { .. } => "pass",
            Outcome::Fail { .. } => "fail",
            Outcome::Unknown { .. } => "unknown",
            Outcome::Transition { .. } => "transition",
        }
    }

    /// The human sentence attached to this outcome, whatever the variant calls
    /// its field. One accessor so the printer has no `match`.
    pub fn detail(&self) -> &str {
        match self {
            Outcome::Pass { detail } | Outcome::Fail { detail } => detail,
            Outcome::Unknown { why } => why,
            Outcome::Transition { note } => note,
        }
    }

    pub fn is_fail(&self) -> bool {
        matches!(self, Outcome::Fail { .. })
    }
}

/// One line of the report: a capability id, its registry coordinates (so the
/// report is readable without opening `contract.rs`) and what the probe found.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub id: &'static str,
    pub harness: &'static str,
    pub tier: &'static str,
    pub outcome: Outcome,
}

impl ProbeResult {
    /// Build a row without consulting the registry — **tests only**, so a
    /// module that needs a `ProbeResult` fixture (`harness::capture`'s) does not
    /// have to pick a real capability id just to get one.
    #[cfg(test)]
    pub(crate) fn for_test(id: &'static str, outcome: Outcome) -> Self {
        ProbeResult::new(id, outcome)
    }

    pub(in crate::harness) fn new(id: &'static str, outcome: Outcome) -> Self {
        // Coordinates come from the registry rather than being repeated here —
        // a row that changed tier must not report its old one.
        let cap = contract::get(id);
        ProbeResult {
            id,
            harness: cap.map(|c| harness_name(c.harness)).unwrap_or("?"),
            tier: cap.map(|c| tier_name(c.tier)).unwrap_or("?"),
            outcome,
        }
    }
}

/// The wire token for a harness. `pub(crate)` since V35 Phase G: the *Harness
/// health* payload speaks the same tokens the `--harness-canary` report and its
/// `--json` twin print, so a user reading the panel and a user reading the CLI
/// report are looking at one vocabulary — and the panel's *Run checks now*
/// passes the token straight back over IPC.
pub(crate) fn harness_name(h: Harness) -> &'static str {
    h.token()
}

/// The inverse of [`harness_name`], for a token arriving from the frontend.
/// Lives beside it so the two spellings can never part company, and returns
/// `None` rather than defaulting — a mistyped token must fail the IPC call, not
/// silently drive the wrong CLI. [`Harness::ANY`] is deliberately not
/// accepted: it names no installed product, so there is nothing to run.
pub(crate) fn harness_from_name(s: &str) -> Option<Harness> {
    Harness::from_id(s)
}

/// The wire token for a seam tier — `pub(crate)` for the same reason.
pub(crate) fn tier_name(t: Seam) -> &'static str {
    match t {
        Seam::A => "A",
        Seam::B => "B",
        Seam::C => "C",
        Seam::D => "D",
    }
}

// ── what this phase drives, and what it only enumerates ─────────────────────

/// The capability ids [`run`] actually drives against an installed CLI, in the
/// order it drives them. `opencode.tool_registry` is first on purpose: it is
/// the security-relevant one, the standing manual maintenance obligation, and
/// the reason Phase D exists at all.
const IMPLEMENTED: &[&str] = &[
    "opencode.tool_registry",
    "opencode.route.noauth",
    "claude.flag.session_id",
    "claude.flag.settings_overlay",
    "claude.transcript.usage",
    "claude.transcript.tool_result",
    "claude.transcript.identity",
    // V35 Phase L. Driven for the same one-tail cost as the three above, and
    // worth driving precisely BECAUSE it is now a fallback: a fallback nobody
    // checks is what turns the primary's failure into a mute tab.
    "claude.transcript.assistant_text",
    // V39. Same tail again, and the same argument one step further: this row's
    // loss is invisible even in the tab — only a driver waiting on a delegation
    // ever notices, ten minutes later.
    "claude.transcript.stop_reason",
];

/// Every other registry row, with the reason this phase does not drive it.
/// These are emitted as `unknown` — enumerated, not faked and not omitted.
///
/// Two distinct reasons, and the difference matters for planning:
///
/// * *needs a scripted turn* — mechanically probeable, just not for free: it
///   costs an API call and a few seconds, which the milestone accepts at
///   version-change cadence but not at every tab spawn. These become real
///   probes when someone pays for the harness.
/// * *no probe can settle it* — the Tier-D `Dep::Behavior` rows. No payload
///   reveals whether a deny reason reached the *model* or whether a `throw`
///   inside a harness plugin ran. They stay manual spikes (D0 / E1 /
///   OpenCode-veto) by locked decision 7, and *Mark verified* survives for
///   exactly these.
/// The rows the RUNNER declares permanently unprobed — the harness-neutral ones
/// only.
///
/// Locked decision 17: each harness's own rows and their reason prose moved into
/// its plugin ([`crate::harness::plugin::HarnessPlugin::declared_unprobed`]);
/// what stays here is the row whose contract is stated about a *tab*, which no
/// plugin owns. [`declared_unprobed`] is the joined view.
const DECLARED_UNPROBED_NEUTRAL: &[(&str, &str)] = &[
    // ── V39 Phase B: delegation (locked decision 16) ─────────────────────────
    (
        "delegation.worker",
        "no probe can settle it: the property is that a REAL turn typed into a REAL TUI comes          back readable, which needs a live worker tab and a live model call — the scripted-turn          class, doubled. Covered meanwhile by the fail-closed gate itself (preflight refuses a          tab with no completion signal rather than typing into it), by the recorded          input-profile spike the gate reads, and by V39 live-verify recipes 1/2/10",
    ),
];

/// Every declared-unprobed row: the neutral ones plus every registered
/// harness's.
fn declared_unprobed_rows() -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> = DECLARED_UNPROBED_NEUTRAL.to_vec();
    for h in crate::harness::registry::all() {
        if let Some(p) = h.plugin() {
            out.extend(p.declared_unprobed().iter().copied());
        }
    }
    out
}

/// The ids this module drives — the L2 half of the registry join key, consumed
/// by `contract.rs`'s `probes_and_the_matrix_agree`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn implemented_probes() -> &'static [&'static str] {
    IMPLEMENTED
}

/// The ids this module enumerates as a permanent `unknown`. Deliberately a
/// SEPARATE list from [`implemented_probes`] — see the module docs.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn declared_unprobed() -> &'static [&'static str] {
    // A `&[&str]` view over the (id, reason) pairs, built once. The reasons
    // stay attached to their ids in the source (a bare id list would rot into
    // unexplained entries) but the cross-check only needs the keys.
    static IDS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    IDS.get_or_init(|| declared_unprobed_rows().iter().map(|(id, _)| *id).collect())
}

// ── timings and bounds, all deliberate ──────────────────────────────────────

/// Gap between readiness polls while a probed child boots.
///
/// The one timing the runner keeps: both harnesses' probes poll a child they
/// just started (`opencode serve` answering its first request, `claude --help`
/// producing output), and a single interval means the two cannot drift into
/// answering "how often do we look?" differently.
pub(in crate::harness) const SERVE_POLL_INTERVAL: Duration = Duration::from_millis(200);

// ── entry point ─────────────────────────────────────────────────────────────

/// Run every probe and print the report. Returns the process exit code:
/// **non-zero iff at least one capability FAILED**. `unknown` and `transition`
/// never affect it.
pub fn run(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");

    // OpenCode first — one `opencode serve` child serves both its probes, and
    // `opencode.tool_registry` is the one this phase exists for. Both groups go
    // through [`run_for`], the same per-harness dispatch V35 Phase F's
    // auto-verify uses, so the CLI and the background run can never drive
    // different sets.
    let mut produced: Vec<ProbeResult> = Vec::new();
    for harness in drive_order() {
        produced.extend(run_for(harness));
    }

    // The report is assembled in DECLARED order, not in the order the probe
    // functions happened to answer. That is what makes [`IMPLEMENTED`]
    // load-bearing at runtime rather than a list the cross-check compares
    // against nothing: a declared probe whose function stopped emitting a row
    // becomes a loud `unknown` here instead of vanishing from the report.
    let mut results: Vec<ProbeResult> = Vec::new();
    for id in IMPLEMENTED {
        match produced.iter().position(|r| r.id == *id) {
            Some(at) => results.push(produced.remove(at)),
            None => results.push(ProbeResult::new(
                id,
                Outcome::Unknown {
                    why: "declared as an implemented probe, but no probe function produced a \
                          result for it this run — this is a defect in harness/probe.rs, not an \
                          upstream signal"
                        .to_string(),
                },
            )),
        }
    }
    // Anything produced but undeclared would be a probe testing something the
    // matrix never recorded. `probes_and_the_matrix_agree` makes that a build
    // failure; it is still appended rather than dropped, because silently
    // discarding a result is the one thing worse than reporting an odd one.
    results.append(&mut produced);
    for (id, why) in declared_unprobed_rows() {
        results.push(ProbeResult::new(
            id,
            Outcome::Unknown {
                why: why.to_string(),
            },
        ));
    }

    let failed = results.iter().filter(|r| r.outcome.is_fail()).count();
    if json {
        print_json(&results);
    } else {
        print_human(&results);
    }
    i32::from(failed > 0)
}

/// Drive the probes belonging to ONE harness (V35 Phase F).
///
/// The auto-verify that runs on a Claude version change has no business
/// spawning `opencode serve`, and vice versa — so the split lives here, beside
/// the probe functions, rather than being re-derived by the caller from
/// [`IMPLEMENTED`] and the registry's `harness` column. [`run`] uses it too, so
/// there is exactly one mapping from harness to probe functions.
///
/// Only rows this module actually DRIVES are returned. `DECLARED_UNPROBED` is
/// deliberately not included: `run` prints those as `unknown` because a CLI
/// report that stopped listing a dependency would be a dependency that stopped
/// being counted, but an auto-verify that scored them would be padding its
/// evidence with rows nothing ever checks.
pub(crate) fn run_for(harness: Harness) -> Vec<ProbeResult> {
    let driven = drive(harness);
    // V35 Phase H. Here rather than at each caller so the background
    // auto-verify, the Settings panel's *Run checks now* and
    // `cimp --harness-canary` all capture identically — a trigger that had to
    // be remembered at three call sites is a trigger that would be missing from
    // one of them. Silent and best-effort; it cannot affect the verdict.
    capture::on_success(&driven);
    driven.results
}

/// Everything one harness's probes produced: the report rows, the raw payloads
/// they read, and the CLI version they read them from (V35 Phase H).
///
/// A separate struct rather than extra fields on [`ProbeResult`] because the
/// two travel to different places and must keep doing so — the rows go to the
/// Advisor, the Settings panel and stdout, the payloads go to disk after a
/// scrub. `version` is `""` when it could not be observed, which
/// [`capture::write_into`] turns into "nothing to capture" (locked decision 6).
#[derive(Debug, Clone)]
pub(crate) struct Driven {
    pub harness: Harness,
    pub results: Vec<ProbeResult>,
    pub observed: Vec<Observed>,
    pub version: String,
}

/// Drive one harness and keep what was observed. [`run_for`] is this plus the
/// capture trigger; `cimp --harness-capture` calls it directly, because that
/// command writes whatever the outcome and so cannot go through the
/// success-only trigger.
pub(crate) fn drive(harness: Harness) -> Driven {
    // V40 Phase A: the DISPATCH is the registry's, not a `match` here. A harness
    // with no plugin (the neutral `Harness::ANY`, which names no installed
    // product) drives nothing — the same answer the old `Any` arm gave, arrived
    // at without naming a vendor.
    let out = harness.plugin().map(|p| p.probe()).unwrap_or_default();
    Driven {
        harness,
        results: out.results,
        observed: out.observed,
        // A CLI that has produced no observable version leaves the stamp to the
        // version cImp last recorded for it — written by the OOB tap and by tab
        // spawn from a real `--version`, so it is an observation too, just an
        // older one. Empty when there has never been either.
        version: if out.version.trim().is_empty() {
            recorded_version(harness)
        } else {
            out.version
        },
    }
}

/// The order harnesses are driven in.
///
/// Registry order, except that a harness whose probes share **one expensive
/// child process** goes first: `opencode serve` answers every OpenCode probe,
/// and starting it while another harness's probes run would hold it open for no
/// reason. Declared by the plugin
/// ([`crate::harness::plugin::HarnessPlugin::probes_share_one_child`]) rather
/// than hard-coded as a literal order here, so the reason travels with the
/// harness it is true of.
pub(crate) fn drive_order() -> Vec<Harness> {
    let mut out: Vec<Harness> = crate::harness::registry::all().collect();
    out.sort_by_key(|h| !h.plugin().is_some_and(|p| p.probes_share_one_child()));
    out
}

/// The version cImp last recorded for `harness`, from the physical global
/// settings file. The fallback stamp for [`drive`] — never the primary, because
/// a stale record would file today's shapes under yesterday's release.
fn recorded_version(harness: Harness) -> String {
    let hv = crate::settings::read_global_harness_versions();
    harness
        .plugin()
        .map(|p| p.recorded_version(&hv))
        .unwrap_or_default()
}

/// `--json`: a flat array of the same records the human report prints, one per
/// probed capability. A top-level array (rather than an envelope) keeps
/// `| jq '.[] | select(.outcome=="fail")'` the obvious thing to type; the exit
/// code already carries the summary a wrapper object would.
fn print_json(results: &[ProbeResult]) {
    let arr: Vec<Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "harness": r.harness,
                "tier": r.tier,
                "outcome": r.outcome.label(),
                "detail": r.outcome.detail(),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&Value::Array(arr)).unwrap_or_else(|_| "[]".to_string())
    );
}

/// One line per capability plus a tally. The tally names the failures again at
/// the end, because the interesting line is otherwise buried among eleven
/// `unknown`s.
fn print_human(results: &[ProbeResult]) {
    println!(
        "cimp {} — harness live probe (L2). Non-zero exit iff something FAILED.",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    let width = results.iter().map(|r| r.id.len()).max().unwrap_or(30);
    for r in results {
        println!(
            "  {:<11} {:<width$}  [{}] {}",
            r.outcome.label().to_uppercase(),
            r.id,
            r.tier,
            r.outcome.detail(),
            width = width
        );
    }
    println!();
    let count = |l: &str| results.iter().filter(|r| r.outcome.label() == l).count();
    println!(
        "  {} pass, {} fail, {} unknown, {} transition",
        count("pass"),
        count("fail"),
        count("unknown"),
        count("transition")
    );
    let failed: Vec<&str> = results
        .iter()
        .filter(|r| r.outcome.is_fail())
        .map(|r| r.id)
        .collect();
    if failed.is_empty() {
        return;
    }
    println!();
    println!("  DRIFT: {}", failed.join(", "));
    for r in results.iter().filter(|r| r.outcome.is_fail()) {
        // The registry already knows which modules break; printing them turns
        // the report into a fix pointer instead of an alert.
        if let Some(cap) = contract::get(r.id) {
            println!("    {} — see {}", r.id, cap.wired_in.join(", "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Only `Fail` is a failure — the load-bearing half of locked decision 8.
    #[test]
    fn only_failures_are_failures() {
        assert!(Outcome::Fail {
            detail: "x".into()
        }
        .is_fail());
        for ok in [
            Outcome::Pass {
                detail: "x".into(),
            },
            Outcome::Unknown { why: "x".into() },
            Outcome::Transition { note: "x".into() },
        ] {
            assert!(
                !ok.is_fail(),
                "`{}` must never affect the exit code (locked decision 8)",
                ok.label()
            );
            assert!(!ok.detail().is_empty(), "every outcome must say why");
        }
    }



    /// The two lists partition the registry and the reasons are substantive —
    /// the registry side of this is `contract::tests::probes_and_the_matrix_agree`,
    /// this side guards the prose (global principle 5: a blank reason would
    /// satisfy the id check while recording nothing).
    #[test]
    fn every_declared_unprobed_row_says_why() {
        assert_eq!(
            declared_unprobed().len(),
            declared_unprobed_rows().len(),
            "the id view must not drop entries"
        );
        for (id, why) in declared_unprobed_rows() {
            assert!(
                why.trim().len() > 30,
                "{id}: say what would be needed to probe it, not merely that it is unprobed"
            );
            assert!(
                why.contains("scripted turn") || why.contains("no probe can settle"),
                "{id}: an unprobed row is either scripted-turn-shaped or unsettleable; say which"
            );
        }
        let implemented: BTreeSet<&str> = implemented_probes().iter().copied().collect();
        for id in declared_unprobed() {
            assert!(
                !implemented.contains(id),
                "{id} is both driven and enumerated as a permanent unknown"
            );
        }
        assert_eq!(
            implemented.len(),
            IMPLEMENTED.len(),
            "duplicate id in IMPLEMENTED"
        );
    }
}

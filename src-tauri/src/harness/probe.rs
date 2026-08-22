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
//! [`IMPLEMENTED`] holds the eight rows this phase actually drives — the ones
//! reachable without scripting a model turn. [`DECLARED_UNPROBED`] holds the
//! other sixteen, each with the reason it cannot be, and they are **printed**
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
//! # Reading a real transcript, printing none of it
//!
//! The three `claude.transcript.*` probes tail a **real** session JSONL, which
//! carries user prompts, file contents, tool output and plausibly credentials
//! (locked decision 4). So: nothing is written, nothing is copied, and every
//! detail string carries **counts and field names only** — never a payload
//! value, never the transcript path, never the session id. The single
//! exception is the CLI build string from the `version` field, which is
//! harness metadata (it is what the `harness_versions` tripwire records) and is
//! the useful half of what `claude.transcript.identity` proves.
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

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::harness::capture::{self, Observed};
use crate::harness::contract::{self, Harness, Seam};
use crate::harness::opencode::tools::{OPENCODE_NATIVE_REVIEWED_UNGATED, OPENCODE_NATIVE_TABLE};

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
    /// posture `Seam::A` and `Harness::Any` have in the registry, and for the same
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

    fn new(id: &'static str, outcome: Outcome) -> Self {
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
    match h {
        Harness::Claude => "claude",
        Harness::OpenCode => "opencode",
        Harness::Any => "any",
    }
}

/// The inverse of [`harness_name`], for a token arriving from the frontend.
/// Lives beside it so the two spellings can never part company, and returns
/// `None` rather than defaulting — a mistyped token must fail the IPC call, not
/// silently drive the wrong CLI. [`Harness::Any`] is deliberately not
/// accepted: it names no installed product, so there is nothing to run.
pub(crate) fn harness_from_name(s: &str) -> Option<Harness> {
    match s {
        "claude" => Some(Harness::Claude),
        "opencode" => Some(Harness::OpenCode),
        _ => None,
    }
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
const DECLARED_UNPROBED: &[(&str, &str)] = &[
    (
        "claude.hook.user_prompt_submit",
        "needs a scripted turn (L2 residual): proving the stdout envelope reaches the model \
         requires installing a temporary hook via --settings and running one real prompt",
    ),
    (
        "claude.hook.precompact",
        "needs a scripted turn AND spike D0: whether the additionalContext reaches the compaction \
         prompt is a Behavior dep no payload reveals",
    ),
    (
        "claude.hook.pretooluse_deny",
        "needs a scripted turn AND spike E1: whether the deny reason reaches the model is a \
         Behavior dep no payload reveals",
    ),
    (
        "claude.hook.posttooluse",
        "needs a scripted turn (L2 residual): the payload only exists while a real Edit/Write is \
         being made",
    ),
    (
        "claude.hook.notification",
        "needs a scripted turn (L2 residual), and the open question — which of the flat and \
         nested payload shapes this build sends — only answers itself when a real permission \
         prompt fires",
    ),
    (
        "perm.tui_scrape",
        "no probe can settle it: a scrape of rendered TUI chrome. Re-characterized in minutes \
         with RUST_LOG=perm_capture=debug; the real fix is the D→C→B migration of decision 2",
    ),
    // V35 Phase L's three pushed rows. Same answer as their Phase J siblings
    // above and for the same reason: a hook payload exists only while a real
    // turn produces one, so nothing here can be driven without scripting a
    // model. What is NOT deferred with them is their silence — the Phase L
    // quiet detector reports a served capability that stops pushing, in
    // production, on the live wire, which is the half a scripted probe would
    // have been worst at anyway.
    (
        "claude.hook.stop",
        "needs a scripted turn (L2 residual): `last_assistant_message` exists only when a real \
         turn finishes. The open question is a Behavior dep besides — whether its rendering of a \
         multi-block message matches the transcript reader's join",
    ),
    (
        "claude.hook.tool_result",
        "needs a scripted turn (L2 residual): the payload exists only while a real tool call \
         returns, and the property worth proving is that the all-tools matcher fires for tools \
         the sibling entry does not name",
    ),
    (
        "claude.hook.subagent",
        "needs a scripted turn (L2 residual) AND a session that happens to launch a sub-agent — \
         the same 'an absence proves nothing' problem `claude.transcript.subagents` has, one \
         layer up",
    ),
    (
        "claude.transcript.subagents",
        "needs a scripted turn (L2 residual): a transcript tail can only show the subagents/ \
         layout if the tailed session happened to launch a sub-agent, so an absence proves \
         nothing and a presence is luck",
    ),
    (
        "claude.statusline.stdin",
        "needs a scripted turn (L2 residual): the payload exists only when the CLI invokes the \
         statusLine command, so probing it means running a turn with an overlay installed",
    ),
    (
        "opencode.sse.events",
        "needs a scripted turn (L2 residual): GET /event on an idle server streams nothing, so \
         the event kinds only arrive if a real agent turn is driven",
    ),
    (
        "opencode.route.push",
        "needs a scripted turn (L2 residual): the dangerous half is `noReply` losing its meaning, \
         which is only observable as an agent turn that should not have started",
    ),
    (
        "claude.hook.taint_beacon",
        "needs a scripted turn (L2 residual): the hook only fires when a real turn reaches for \
         WebFetch/WebSearch, and the property worth proving is that the beacon LANDED before the \
         tool ran — an ordering, not a payload shape. Unchanged by the 2026-08-17 http migration, \
         which moved the row to Tier B: what it bought is app-observable DELIVERY, which is a \
         production signal rather than something this probe can drive",
    ),
    (
        "claude.hook.checkpoint_beacon",
        "needs a scripted turn (L2 residual), and the load-bearing half is an ORDERING no fixture \
         can express: that the tool call does not begin until the hook's response arrives. Since \
         2026-08-17 that ordering is upstream's DOCUMENTED deny contract rather than an observed \
         behaviour, so what a probe would add is confirmation, not coverage",
    ),
    (
        "opencode.plugin.load_all",
        "no probe can settle it, and it is inside the TCB: nothing outside a harness can verify \
         that a control inside it ran. A plugin that loads but skips the `throw` looks fully \
         functional. Manual OpenCode-veto spike; Phase I's `chp` handshake at least makes a STALE \
         plugin a mismatch instead of a mystery",
    ),
    // ── V39 Phase B: delegation (locked decision 16) ─────────────────────────
    (
        "delegation.worker",
        "no probe can settle it: the property is that a REAL turn typed into a REAL TUI comes          back readable, which needs a live worker tab and a live model call — the scripted-turn          class, doubled. Covered meanwhile by the fail-closed gate itself (preflight refuses a          tab with no completion signal rather than typing into it), by the recorded          input-profile spike the gate reads, and by V39 live-verify recipes 1/2/10",
    ),
    (
        "claude.input.profile",
        "no probe can settle it: whether a bracketed paste plus a submit yields exactly ONE turn          is a `Dep::Behavior` visible only as a real turn in a real TUI. Manual input-profile          spike, outcome in `harness_versions.input_profile_status` — the same class as D0/E1, and          `Mark verified` survives for exactly these",
    ),
    (
        "opencode.input.profile",
        "no probe can settle it — same behaviour, same spike, same recorded outcome as          `claude.input.profile`",
    ),
];

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
    IDS.get_or_init(|| DECLARED_UNPROBED.iter().map(|(id, _)| *id).collect())
}

// ── timings and bounds, all deliberate ──────────────────────────────────────

/// How long to wait for `opencode serve` to answer its first request. Ten
/// seconds is the brief's figure and is ~5× the observed cold start; past it
/// the probe reports `unknown`, never a failure.
const SERVE_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Gap between readiness polls while the server boots.
const SERVE_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Per-request socket timeout once the server is up.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to allow `claude --help` before giving up (⇒ `unknown`).
const HELP_TIMEOUT: Duration = Duration::from_secs(20);

/// How much of the newest transcript to read, from the END. A transcript grows
/// without bound and the probe only needs *recent* evidence, so this is a tail
/// rather than a scan — and a bounded read is also the privacy posture: the
/// less of a user's session that enters this process, the better.
const TAIL_BYTES: u64 = 512 * 1024;
/// …and at most this many parsed lines out of that window, so a transcript of
/// very short lines cannot balloon the working set.
const TAIL_LINES: usize = 600;

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
    produced.extend(run_for(Harness::OpenCode));
    produced.extend(run_for(Harness::Claude));

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
    for (id, why) in DECLARED_UNPROBED {
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
    let (results, observed, version) = match harness {
        Harness::Claude => {
            let (mut results, help) = probe_claude_flags();
            let (transcript, mut observed, version) = probe_claude_transcript();
            results.extend(transcript);
            // Both flag rows are answered from ONE `claude --help`, and each
            // gets its own copy of it. The duplication is deliberate: the file
            // name IS the join key, so a reader who was sent here by a failing
            // `claude.flag.settings_overlay` finds a file with that name rather
            // than a shared blob they have to know the provenance of. It is
            // ~20 KiB twice, bounded by the retention sweep.
            if let Some(help) = help {
                observed.push(Observed::new(
                    "claude.flag.session_id",
                    "txt",
                    help.clone(),
                ));
                observed.push(Observed::new("claude.flag.settings_overlay", "txt", help));
            }
            (results, observed, version)
        }
        Harness::OpenCode => probe_opencode(),
        // No seeded row is harness-neutral yet (CHP, milestone decision 9, is
        // what will produce the first one), so there is nothing to drive.
        Harness::Any => (Vec::new(), Vec::new(), String::new()),
    };
    Driven {
        harness,
        results,
        observed,
        // A CLI that has produced no observable version leaves the stamp to the
        // version cImp last recorded for it — written by the OOB tap and by tab
        // spawn from a real `--version`, so it is an observation too, just an
        // older one. Empty when there has never been either.
        version: if version.trim().is_empty() {
            recorded_version(harness)
        } else {
            version
        },
    }
}

/// The version cImp last recorded for `harness`, from the physical global
/// settings file. The fallback stamp for [`drive`] — never the primary, because
/// a stale record would file today's shapes under yesterday's release.
fn recorded_version(harness: Harness) -> String {
    let hv = crate::settings::read_global_harness_versions();
    match harness {
        Harness::Claude => hv.claude_last_seen.trim().to_string(),
        Harness::OpenCode => hv.opencode_last_seen.trim().to_string(),
        Harness::Any => String::new(),
    }
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

// ── minimal blocking HTTP ───────────────────────────────────────────────────

/// A loopback HTTP/1.1 GET, returning `(status, body)`. Hand-rolled for the
/// reason the deleted beacon shims hand-rolled theirs: this runs before any
/// runtime exists, and a probe that needed an async stack to ask one question
/// would be harder to trust than the question is worth.
fn http_get(port: u16, path: &str) -> Option<(u16, String)> {
    http_get_as(port, path, None)
}

/// [`http_get`] with an optional `Authorization` header value.
///
/// 2026-08-17: the OpenCode child is now spawned with a server password, so every
/// route — including the readiness poll, since upstream has **no unauthenticated
/// health route** — has to present a credential. The unauthenticated form above
/// stays, and is not a leftover: it is one half of what
/// [`noauth_outcome`] proves.
///
/// The credential goes in the header and nowhere else. Upstream also accepts an
/// `auth_token` query parameter, and a present-but-wrong one WINS over a correct
/// header — so a probe that hedged by sending both would 401 against a perfectly
/// healthy server. Same rule the reader follows
/// (`harness::opencode::config::server_basic_auth`).
fn http_get_as(port: u16, path: &str, auth: Option<&str>) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT)).ok()?;
    let authorization = auth
        .map(|v| format!("Authorization: {v}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Accept: application/json\r\n\
         {authorization}\
         Connection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp);
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let status: u16 = resp
        .lines()
        .next()?
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())?;
    let body = resp
        .find("\r\n\r\n")
        .map(|at| resp[at + 4..].to_string())
        .unwrap_or_default();
    Some((status, body))
}

/// Reserve a free loopback port by binding `127.0.0.1:0` and releasing it —
/// the same trick `tabs::config::alloc_loopback_port` uses for a real OpenCode
/// tab, with the same small race window.
///
/// **Why not `--port 0`.** `opencode serve --help` documents `--port` as
/// `[number] [default: 0]`, which reads like ephemeral-port support; the
/// installed 1.18.13 answers `--port 0` by listening on **4096**, its fixed
/// default. Taking a port from the OS is therefore not belt-and-braces: it is
/// what keeps the probe off a port a user's own OpenCode server may already
/// hold, and off the port a second probe would collide on.
fn alloc_loopback_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|addr| addr.port())
}

// ── opencode: the serve child ───────────────────────────────────────────────

/// A live `opencode serve`, killed on drop.
struct Serve {
    child: std::process::Child,
    port: u16,
    /// 2026-08-17: the `Authorization: Basic …` value this child's server
    /// requires, because the probe spawns it WITH a server password — which is
    /// what makes `opencode.route.noauth` provable in both directions instead of
    /// being a watch for auth to arrive.
    ///
    /// `None` only if the credential could not be built at all (an empty
    /// password, which upstream reads as "auth off"); the probe then reports
    /// `unknown` rather than testing a contract it did not set up.
    auth: Option<String>,
}

impl Drop for Serve {
    fn drop(&mut self) {
        // `opencode` is a Bun binary that forks children (two grandchildren
        // observed per server), so a bare kill would leave an HTTP server bound
        // to our port after the probe exits. Same tree-kill idiom the audit and
        // checks runners use, in its blocking form.
        crate::procutil::kill_tree_blocking(&mut self.child);
    }
}

/// Start `opencode serve` on a free loopback port and wait for it to answer.
/// `Err(why)` is an `unknown` reason, never a failure — the CLI being absent or
/// slow says nothing about whether upstream drifted.
fn start_opencode_serve() -> Result<Serve, String> {
    let binary = crate::pty::resolve_command("opencode").map_err(|_| {
        "`opencode` is not on PATH (nor in ebin/) — nothing to probe. Not a failure: an \
         uninstalled harness cannot drift."
            .to_string()
    })?;
    let port = alloc_loopback_port()
        .ok_or_else(|| "could not reserve a loopback port for `opencode serve`".to_string())?;
    // 2026-08-17: the probe sets a password on its own child, because the
    // contract it now checks is "these documented env vars enforce Basic auth",
    // not "the server answers anybody". Generated by the same function the tab
    // spawn uses, so the probe cannot pass on a credential shape production does
    // not produce.
    let password = crate::harness::opencode::config::new_server_password();
    let auth = crate::harness::opencode::config::server_basic_auth(&password);

    let mut cmd = Command::new(&binary);
    for (name, value) in crate::harness::opencode::config::server_auth_env(&password) {
        cmd.env(name, value);
    }
    cmd.arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .arg("--hostname")
        .arg("127.0.0.1")
        .stdin(Stdio::null())
        // Piped-but-undrained output deadlocks a chatty child, and the probe
        // reads its answers over HTTP rather than from stdout, so both streams
        // go to null. The cost is that a startup error is invisible — which is
        // exactly what the readiness timeout below reports as `unknown`.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // A neutral cwd: OpenCode resolves project config (and `.opencode/`)
        // from the working directory, so probing from wherever the maintenance
        // script happened to run would make the answer depend on the caller.
        .current_dir(std::env::temp_dir());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::procutil::CREATE_NO_WINDOW);
    }
    crate::procutil::own_process_group_std(&mut cmd);

    // Through the spawn gate like every other cImp spawn — see `spawn_gate`.
    let child = crate::spawn_gate::spawn_std(&mut cmd)
        .map_err(|e| format!("`opencode serve` could not be spawned: {e}"))?;
    let serve = Serve {
        child,
        port,
        auth: auth.clone(),
    };

    // Readiness = the server answers an HTTP request, not merely that the
    // socket accepts. Bun binds before its routes are mounted. It has to be an
    // AUTHENTICATED request: there is no unauthenticated health route (only three
    // static asset paths bypass auth), so an unauthenticated poll would be
    // answering "does it 401 yet" — a question about the wrong thing, and one
    // that would go on succeeding if the routes never mounted.
    let deadline = Instant::now() + SERVE_READY_TIMEOUT;
    while Instant::now() < deadline {
        if http_get_as(port, "/experimental/tool/ids", auth.as_deref()).is_some() {
            return Ok(serve);
        }
        std::thread::sleep(SERVE_POLL_INTERVAL);
    }
    Err(format!(
        "`opencode serve` did not answer on 127.0.0.1:{port} within {}s",
        SERVE_READY_TIMEOUT.as_secs()
    ))
}

/// The two OpenCode probes that share one server child.
fn probe_opencode() -> (Vec<ProbeResult>, Vec<Observed>, String) {
    let serve = match start_opencode_serve() {
        Ok(s) => s,
        Err(why) => {
            return (
                vec![
                    ProbeResult::new(
                        "opencode.tool_registry",
                        Outcome::Unknown { why: why.clone() },
                    ),
                    ProbeResult::new("opencode.route.noauth", Outcome::Unknown { why }),
                ],
                Vec::new(),
                String::new(),
            );
        }
    };

    let auth = serve.auth.as_deref();
    let ids = http_get_as(serve.port, "/experimental/tool/ids", auth);
    // A declared route (`GET /session/:id`) rather than the one above, so the
    // auth question is asked of a surface cImp actually depends on. The id is
    // deliberately one that cannot exist: a 404 still proves the request was
    // ACCEPTED (processed and answered) rather than refused for want of a
    // credential, and inventing a real session would mean writing to the user's
    // OpenCode state.
    let session = http_get_as(
        serve.port,
        "/session/cimp-harness-probe-does-not-exist",
        auth,
    );
    // The other half of the new contract: the SAME two routes with no credential
    // at all must be refused. Both halves are needed — "authenticated calls
    // work" alone is also true of a server enforcing nothing.
    let ids_unauth = http_get(serve.port, "/experimental/tool/ids");
    let session_unauth = http_get(serve.port, "/session/cimp-harness-probe-does-not-exist");

    let results = vec![
        ProbeResult::new("opencode.tool_registry", tool_registry_outcome(ids.as_ref())),
        ProbeResult::new(
            "opencode.route.noauth",
            noauth_outcome(
                serve.auth.is_some(),
                &[
                    AuthPair {
                        route: "GET /experimental/tool/ids",
                        authed: ids.as_ref().map(|(s, _)| *s),
                        unauthed: ids_unauth.as_ref().map(|(s, _)| *s),
                    },
                    AuthPair {
                        route: "GET /session/:id",
                        authed: session.as_ref().map(|(s, _)| *s),
                        unauthed: session_unauth.as_ref().map(|(s, _)| *s),
                    },
                ],
            ),
        ),
    ];
    // The registry listing is the payload worth keeping: it is the one this
    // phase exists for, and a diff of it is exactly how "which tool id appeared"
    // gets answered. Kept only when the route answered a usable body — a 404 or
    // an error page would file an error message under a version number.
    let observed = ids
        .as_ref()
        .filter(|(status, _)| *status == 200)
        .map(|(_, body)| vec![Observed::new("opencode.tool_registry", "json", body.clone())])
        .unwrap_or_default();

    (results, observed, serve_version(serve.port, auth))
}

/// The OpenCode build the probes just ran against, from the server child that
/// is already up.
///
/// **Version-stamping only** (V35 Phase H), which is why `GET /global/health`
/// is not a registry row: nothing cImp does depends on it, no user-visible
/// feature degrades if it moves, and the entire cost of losing it is that a
/// capture falls back to the version the tab spawn recorded — or is skipped.
/// Declaring it as a capability would put a row in the matrix that can never
/// fail, which is the padding the registry's own tests exist to prevent.
///
/// It is asked of the running child rather than by spawning `opencode
/// --version`: the probe already paid for a server, and a second process to
/// learn a string it can ask for over an open socket is a cost with no answer
/// attached.
fn serve_version(port: u16, auth: Option<&str>) -> String {
    http_get_as(port, "/global/health", auth)
        .filter(|(status, _)| *status == 200)
        .and_then(|(_, body)| serde_json::from_str::<Value>(&body).ok())
        .and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

/// Diff the live tool registry against what cImp has classified.
///
/// **Only genuinely unclassified ids fail.** The gate table
/// (`OPENCODE_NATIVE_TABLE`) is allowlist-only by design, so an id absent from
/// it ships UNGATED — but five ids are absent *deliberately*, with reviewed
/// reasons, and those live in `OPENCODE_NATIVE_REVIEWED_UNGATED`. Failing on
/// them would make this probe permanently red on an unchanged upstream, i.e.
/// the crying-wolf failure locked decision 8 forbids. So the test is
/// `live − (gated ∪ reviewed) = ∅`, and what is left over is an id **nobody
/// has ever looked at**.
///
/// The other direction — a table id upstream no longer serves — is a note, not
/// a failure: a tool that does not exist cannot be exploited, and gating a name
/// the harness does not serve costs nothing (`patch` has been such a row since
/// V12, on purpose).
fn tool_registry_outcome(ids: Option<&(u16, String)>) -> Outcome {
    let Some((status, body)) = ids else {
        return Outcome::Unknown {
            why: "`GET /experimental/tool/ids` returned no response at all".to_string(),
        };
    };
    if *status == 404 {
        return Outcome::Unknown {
            why: "`GET /experimental/tool/ids` is 404 — the EXPERIMENTAL route has moved or been \
                  retired. Not scored as drift (decision 8), but this is the route the whole \
                  native-tool gate is verified through: find its replacement and update this \
                  probe plus docs/HARNESS-NATIVE-TOOLS.md §3, or the gate goes back to being \
                  checked by a human remembering to."
                .to_string(),
        };
    }
    if *status != 200 {
        return Outcome::Unknown {
            why: format!("`GET /experimental/tool/ids` answered HTTP {status}"),
        };
    }
    let Some(live) = serde_json::from_str::<Value>(body)
        .ok()
        .as_ref()
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<BTreeSet<String>>()
        })
    else {
        return Outcome::Unknown {
            why: "`GET /experimental/tool/ids` answered 200 with a body that is not an array of \
                  strings — the route's SHAPE changed, so the registry cannot be diffed"
                .to_string(),
        };
    };
    if live.is_empty() {
        // Global principle 5: empty is not absent. An empty array would make
        // the subtraction below vacuously clean.
        return Outcome::Unknown {
            why: "`GET /experimental/tool/ids` answered 200 with an EMPTY list — nothing to diff, \
                  and an empty registry would make this probe pass while proving nothing"
                .to_string(),
        };
    }

    let gated: BTreeSet<&str> = OPENCODE_NATIVE_TABLE.iter().map(|(n, _, _)| *n).collect();
    let reviewed: BTreeSet<&str> = OPENCODE_NATIVE_REVIEWED_UNGATED
        .iter()
        .map(|(n, _)| *n)
        .collect();

    let unclassified: Vec<&str> = live
        .iter()
        .map(String::as_str)
        .filter(|id| !gated.contains(id) && !reviewed.contains(id))
        .collect();
    let vanished: Vec<&str> = gated
        .iter()
        .chain(reviewed.iter())
        .copied()
        .filter(|id| !live.contains(*id))
        .collect();

    let vanished_note = if vanished.is_empty() {
        String::new()
    } else {
        format!(
            " Declared but NOT served upstream (a note, not drift — a tool that does not exist \
             cannot be exploited): {}.",
            vanished.join(", ")
        )
    };

    if unclassified.is_empty() {
        Outcome::Pass {
            detail: format!(
                "{} live tool ids, all classified ({} gated by OPENCODE_NATIVE_TABLE, {} reviewed \
                 and deliberately ungated).{vanished_note}",
                live.len(),
                live.iter().filter(|id| gated.contains(id.as_str())).count(),
                live.iter().filter(|id| reviewed.contains(id.as_str())).count(),
            ),
        }
    } else {
        Outcome::Fail {
            detail: format!(
                "UNCLASSIFIED OpenCode tool id(s) — ungated and nothing fails at runtime: {}. \
                 Classify each one: add it to `offload::toolclass::OPENCODE_NATIVE_TABLE` (with \
                 its class and its `mutates_fs` flag) if it touches files, runs processes or \
                 reaches the network, or to `OPENCODE_NATIVE_REVIEWED_UNGATED` with the reason \
                 gating it would buy nothing. This is a security decision and belongs in \
                 review.{vanished_note}",
                unclassified.join(", ")
            ),
        }
    }
}

/// One route, observed twice: with this run's credential and with none.
///
/// Both halves are the point. "Authenticated calls work" is also true of a server
/// enforcing nothing, and "unauthenticated calls are refused" alone would be true
/// of a server cImp can no longer talk to — so the verdict needs the pair.
struct AuthPair {
    /// Route label. A label, not a URL: the detail strings this feeds carry
    /// field names and counts only.
    route: &'static str,
    /// Status with `Authorization: Basic …`, or `None` if the route did not
    /// answer at all.
    authed: Option<u16>,
    /// Status with no credential.
    unauthed: Option<u16>,
}

/// Whether the documented `OPENCODE_SERVER_PASSWORD` / `OPENCODE_SERVER_USERNAME`
/// pair really does enforce Basic auth on the routes cImp depends on — and
/// whether cImp's own credential is still accepted.
///
/// **This replaced a watch with a check on 2026-08-17.** The row used to be Tier
/// D: cImp sent no credential, the probe confirmed the server still answered
/// anybody, and a 401 was reported as `Transition` ("auth landed — wire a
/// token"). Auth has landed, cImp now sets a per-spawn password at tab launch,
/// and the row is Tier B — so the probe's job flipped with it. What it proves:
///
/// * **unauthenticated ⇒ refused** on every probed route. Anything else means
///   the documented env vars did not take effect, i.e. every OpenCode tab cImp
///   launches is hosting an unauthenticated server on loopback while the code
///   believes otherwise. That is a security control that stopped enforcing, so
///   it is a **`Fail`** — the one direction locked decision 8 does want scored.
/// * **authenticated ⇒ accepted**. A 401/403 with a correct header means the
///   scheme moved (a changed username default, or the credential is no longer
///   read from the header), and the tap and the V30 push are dark until it is
///   rewired. Also a `Fail`, and the one the `VisibleOff` degradation is written
///   for.
///
/// A 404 counts as accepted, deliberately: the session-route probe asks for an id
/// that cannot exist, and "processed and answered" is exactly what
/// distinguishes acceptance from refusal.
///
/// `unknown` — never a failure — covers every way the question could not be
/// asked: a route that answered neither way, or a run whose own credential could
/// not be built (an empty password disables auth upstream, so a probe without one
/// would report a passing server as broken).
fn noauth_outcome(credentialed: bool, pairs: &[AuthPair]) -> Outcome {
    if !credentialed {
        return Outcome::Unknown {
            why: "this probe run could not build a server credential of its own, so the child was \
                  spawned without one — an empty `OPENCODE_SERVER_PASSWORD` disables auth \
                  upstream, and testing an unsecured server against the auth contract would \
                  report a healthy build as broken"
                .to_string(),
        };
    }
    let observed: Vec<(&str, u16, u16)> = pairs
        .iter()
        .filter_map(|p| Some((p.route, p.authed?, p.unauthed?)))
        .collect();
    if observed.is_empty() {
        return Outcome::Unknown {
            why: "no probed route answered both with and without a credential, so nothing can be \
                  said about auth"
                .to_string(),
        };
    }
    let rendered = observed
        .iter()
        .map(|(route, authed, unauthed)| format!("{route} → {authed} authenticated, {unauthed} not"))
        .collect::<Vec<_>>()
        .join(", ");
    let refused = |status: u16| status == 401 || status == 403;

    let unenforced: Vec<&str> = observed
        .iter()
        .filter(|(_, _, unauthed)| !refused(*unauthed))
        .map(|(route, _, _)| *route)
        .collect();
    if !unenforced.is_empty() {
        return Outcome::Fail {
            detail: format!(
                "AUTH NOT ENFORCED on {} of {} probed route(s) despite \
                 `OPENCODE_SERVER_PASSWORD` being set on the server child ({rendered}). Every \
                 OpenCode tab cImp launches is then hosting an unauthenticated HTTP server on \
                 loopback — where `POST /session/:id/message` without `noReply` starts a real \
                 agent turn — while `harness/opencode/config.rs` believes the password closed it. \
                 Unenforced: {}.",
                unenforced.len(),
                observed.len(),
                unenforced.join(", ")
            ),
        };
    }
    let rejected: Vec<&str> = observed
        .iter()
        .filter(|(_, authed, _)| refused(*authed))
        .map(|(route, _, _)| *route)
        .collect();
    if !rejected.is_empty() {
        return Outcome::Fail {
            detail: format!(
                "cImp's own credential was REFUSED on {} of {} probed route(s) ({rendered}). The \
                 Basic-auth scheme moved: check the username default and that the credential is \
                 still read from the `Authorization` header, then rewire \
                 `harness/opencode/config.rs::server_basic_auth`. Until then this tab's live \
                 session tap and the V30 push fanout are off. Refused: {}.",
                rejected.len(),
                observed.len(),
                rejected.join(", ")
            ),
        };
    }
    Outcome::Pass {
        detail: format!(
            "the documented server-password env pair enforces Basic auth on all {} probed \
             route(s), and cImp's own credential is accepted ({rendered}). The Tier-D \
             unauthenticated-loopback exposure is closed for every tab cImp launches.",
            observed.len()
        ),
    }
}

// ── claude: spawn flags via --help ──────────────────────────────────────────

/// Every option token `claude --help` declares. Parsed from the OPTION COLUMN
/// only (commander.js indents an option definition by exactly two spaces and
/// wraps its description far to the right), because `--settings` and friends
/// also appear inside other options' prose — a naive substring search finds
/// `--settings` in `--bare`'s description and would report a deleted flag as
/// present.
fn help_option_tokens(help: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in help.lines() {
        if !line.starts_with("  -") || line.starts_with("   ") {
            continue;
        }
        // `  -c, --continue    Continue …` → the definition is everything
        // before the first double-space gap.
        let def = line.trim_start();
        let def = def.split("  ").next().unwrap_or(def);
        for tok in def.split([',', ' ', '\t']) {
            let tok = tok.trim();
            if tok.starts_with('-') && tok.len() > 1 {
                out.insert(tok.to_string());
            }
        }
    }
    out
}

/// `claude --help`, or an `unknown` reason. Stdout and stderr are joined
/// because a CLI is free to print usage to either.
fn claude_help() -> Result<String, String> {
    let binary = crate::pty::resolve_command("claude").map_err(|_| {
        "`claude` is not on PATH (nor in ebin/) — nothing to probe. Not a failure: an \
         uninstalled harness cannot drift."
            .to_string()
    })?;
    let mut cmd = Command::new(&binary);
    cmd.arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::procutil::CREATE_NO_WINDOW);
    }
    // `output()` reads both pipes to EOF, so there is no deadlock to bound —
    // but a hung child would hang the probe, so it is spawned and reaped with a
    // deadline rather than blocked on.
    // Through the spawn gate like every other cImp spawn — see `spawn_gate`.
    let mut child = crate::spawn_gate::spawn_std(&mut cmd)
        .map_err(|e| format!("`claude --help` could not be spawned: {e}"))?;
    let deadline = Instant::now() + HELP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(SERVE_POLL_INTERVAL),
            Ok(None) => {
                crate::procutil::kill_tree_blocking(&mut child);
                return Err(format!(
                    "`claude --help` did not exit within {}s",
                    HELP_TIMEOUT.as_secs()
                ));
            }
            Err(e) => return Err(format!("`claude --help` could not be waited on: {e}")),
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("`claude --help` output could not be read: {e}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(text)
}

/// The declared flags of one capability row, in declaration order. Read from
/// the registry rather than repeated here, so a row that grows a flag grows the
/// probe with it.
fn declared_flags(id: &str) -> Vec<&'static str> {
    contract::get(id)
        .map(|c| {
            c.depends_on
                .iter()
                .filter_map(|d| match d {
                    contract::Dep::Flag(f) => Some(*f),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The two Tier-B spawn-flag rows. Both are answered from one `claude --help`.
///
/// Returns that help text alongside the rows when it was readable **and
/// parseable as an option list** (V35 Phase H) — an unrecognizable help screen
/// makes both rows `unknown`, and filing an unreadable one as a known-good
/// capture would seed the corpus with the shape a later diff is supposed to
/// flag.
fn probe_claude_flags() -> (Vec<ProbeResult>, Option<String>) {
    let (session_id, settings) = ("claude.flag.session_id", "claude.flag.settings_overlay");
    let help = match claude_help() {
        Ok(h) => h,
        Err(why) => {
            return (
                vec![
                    ProbeResult::new(session_id, Outcome::Unknown { why: why.clone() }),
                    ProbeResult::new(settings, Outcome::Unknown { why }),
                ],
                None,
            );
        }
    };
    let tokens = help_option_tokens(&help);
    // The anti-cry-wolf guard. If the help format changes enough that the
    // option column stops being recognizable, EVERY flag reads as missing and
    // the probe reports two loud false failures. Below this floor the parse
    // itself is what is unknown, so say that instead.
    if tokens.len() < 10 {
        let why = format!(
            "`claude --help` no longer parses as an option list ({} option tokens found, expected \
             dozens) — the probe cannot tell a renamed flag from a reformatted help screen",
            tokens.len()
        );
        return (
            vec![
                ProbeResult::new(session_id, Outcome::Unknown { why: why.clone() }),
                ProbeResult::new(settings, Outcome::Unknown { why }),
            ],
            None,
        );
    }

    let mut out = Vec::new();
    for id in [session_id, settings] {
        let declared = declared_flags(id);
        let missing: Vec<&str> = declared
            .iter()
            .copied()
            .filter(|f| !tokens.contains(*f))
            .collect();
        let outcome = if declared.is_empty() {
            Outcome::Unknown {
                why: "the registry row declares no `Dep::Flag`, so there is nothing to check"
                    .to_string(),
            }
        } else if missing.is_empty() {
            let extra = if id == settings {
                " NOTE: the deeper half of this row — whether the installed CLI still HONORS the \
                 `hooks` / `statusLine` / `permissions` keys inside the overlay — needs a scripted \
                 turn and is NOT covered here (issue #64 stays open)."
            } else {
                ""
            };
            Outcome::Pass {
                detail: format!(
                    "all {} declared flag(s) still declared by `claude --help`: {}.{extra}",
                    declared.len(),
                    declared.join(", ")
                ),
            }
        } else {
            Outcome::Fail {
                detail: format!(
                    "`claude --help` no longer declares: {}. Declared flags still present: {}. \
                     A vanished selector is not cosmetic — cImp puts these on the child's argv.",
                    missing.join(", "),
                    declared
                        .iter()
                        .filter(|f| tokens.contains(**f))
                        .copied()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        };
        out.push(ProbeResult::new(id, outcome));
    }
    (out, Some(help))
}

// ── claude: the transcript tail ─────────────────────────────────────────────

/// A bounded window onto the newest real transcript. The parsed lines leave
/// this module only through [`Observed`], on the way to a scrub; `session_id` is
/// used only as the expected value for
/// [`crate::harness::claude::read::record_names_session`].
struct Tail {
    lines: Vec<Value>,
    session_id: String,
    /// Non-JSON / non-object lines in the window, reported as a count so a
    /// wholesale format change (JSONL → something else) is visible rather than
    /// silently reducing the sample.
    unparsed: usize,
}

/// The newest `*.jsonl` under `~/.claude/projects/`, preferring the project the
/// probe was run in. Path discovery goes through `oob::claude` — the tap's own
/// helpers — so the probe cannot verify a layout the tap does not read.
fn newest_transcript() -> Option<PathBuf> {
    let root = crate::harness::claude::read::projects_root()?;
    if let Some(here) = std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::harness::claude::read::project_root(&cwd))
        .and_then(|dir| crate::harness::claude::read::newest_jsonl(&dir))
    {
        return Some(here);
    }
    // No transcript for this project: fall back to the newest anywhere, since
    // the shapes being probed are harness-wide and not project-specific.
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&root).ok()?.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(candidate) = crate::harness::claude::read::newest_jsonl(&entry.path()) else {
            continue;
        };
        let Ok(mtime) = candidate.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            newest = Some((mtime, candidate));
        }
    }
    newest.map(|(_, p)| p)
}

/// Read the last [`TAIL_BYTES`] of `path` and parse up to [`TAIL_LINES`]
/// trailing JSON objects out of it. The first (probably partial) line of the
/// window is dropped.
fn read_tail(path: &PathBuf) -> Option<Tail> {
    use std::io::{Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let text = if from > 0 {
        // Mid-file window: the first line is a fragment, not a record.
        text.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
    } else {
        text.as_str()
    };

    let mut lines: Vec<Value> = Vec::new();
    let mut unparsed = 0usize;
    for raw in text.lines().rev() {
        if raw.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(raw) {
            Ok(v) if v.is_object() => lines.push(v),
            _ => unparsed += 1,
        }
        if lines.len() >= TAIL_LINES {
            break;
        }
    }
    lines.reverse();
    Some(Tail {
        lines,
        session_id: path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string(),
        unparsed,
    })
}

/// The three `claude.transcript.*` rows, all read from one tail — plus, since
/// V35 Phase H, up to [`capture::LINES_PER_CAPABILITY`] of the lines that
/// actually satisfied each row's substantiveness predicate, and the CLI build
/// string those lines carry.
fn probe_claude_transcript() -> (Vec<ProbeResult>, Vec<Observed>, String) {
    let ids = [
        "claude.transcript.usage",
        "claude.transcript.tool_result",
        "claude.transcript.identity",
        "claude.transcript.assistant_text",
        "claude.transcript.stop_reason",
    ];
    let unknown = |why: String| {
        (
            ids.iter()
                .map(|id| ProbeResult::new(id, Outcome::Unknown { why: why.clone() }))
                .collect::<Vec<_>>(),
            Vec::new(),
            String::new(),
        )
    };
    let tail = newest_transcript().and_then(|p| read_tail(&p));
    let Some(tail) = tail else {
        return unknown(
            "no Claude Code session transcript found under ~/.claude/projects — nothing to tail. \
             Not a failure: an unused harness cannot drift."
                .to_string(),
        );
    };
    if tail.lines.is_empty() {
        return unknown(format!(
            "the newest transcript's last {} KiB held no parseable JSON object ({} unparsed \
             lines) — the artifact may no longer be JSONL",
            TAIL_BYTES / 1024,
            tail.unparsed
        ));
    }

    let results = vec![
        ProbeResult::new(ids[0], usage_outcome(&tail)),
        ProbeResult::new(ids[1], tool_result_outcome(&tail)),
        ProbeResult::new(ids[2], identity_outcome(&tail)),
        ProbeResult::new(ids[3], assistant_text_outcome(&tail)),
        ProbeResult::new(ids[4], stop_reason_outcome(&tail)),
    ];
    // Only rows that PASSED contribute lines. A line that failed the
    // substantiveness predicate is not a known-good shape, and the harness-level
    // gate in `capture::on_success` would not save us here: an `unknown` row
    // sits in a run that can still be all-pass overall.
    let mut observed = Vec::new();
    for (id, lines) in [
        (ids[0], substantive_lines(&tail, usage_is_substantive)),
        (ids[1], substantive_lines(&tail, tool_result_is_substantive)),
        (
            ids[2],
            substantive_lines(&tail, |l| identity_is_substantive(l, &tail.session_id)),
        ),
        (ids[3], substantive_lines(&tail, assistant_text_is_substantive)),
        (ids[4], substantive_lines(&tail, stop_reason_is_substantive)),
    ] {
        let passed = results
            .iter()
            .any(|r| r.id == id && matches!(r.outcome, Outcome::Pass { .. }));
        if passed && !lines.is_empty() {
            observed.push(Observed::new(id, "jsonl", lines.join("\n")));
        }
    }
    (results, observed, newest_cli_version(&tail))
}

/// Up to [`capture::LINES_PER_CAPABILITY`] transcript lines satisfying `keep`,
/// re-serialized. The **newest** ones (the tail is in file order), because a
/// shape that changed recently is the one a diff is looking for.
///
/// Re-serialized rather than kept as raw text: `read_tail` already parsed them,
/// carrying the raw bytes alongside would double the window's footprint, and a
/// canonical form makes the corpus diff on structure instead of on whitespace.
fn substantive_lines(tail: &Tail, keep: impl Fn(&Value) -> bool) -> Vec<String> {
    tail.lines
        .iter()
        .rev()
        .filter(|l| keep(l))
        .take(capture::LINES_PER_CAPABILITY)
        .filter_map(|l| serde_json::to_string(l).ok())
        .collect()
}

/// The newest CLI build string in the window — the version a Claude capture is
/// stamped with (V35 Phase H, locked decision 6).
///
/// Read from the transcript's own `version` field, which is what
/// `claude.transcript.identity` already proves is there and what the
/// `harness_versions` tripwire is fed by. Newest rather than the whole set: a
/// window can straddle an auto-update, and a capture belongs to the build that
/// produced its newest lines.
fn newest_cli_version(tail: &Tail) -> String {
    tail.lines
        .iter()
        .rev()
        .find_map(crate::harness::claude::read::cli_version_of)
        .unwrap_or_default()
        .to_string()
}

/// `message.usage.*` still produces substantive turns.
///
/// The failure predicate needs an INDEPENDENT witness that a turn happened,
/// because `parse_usage_line` returning nothing is equally consistent with "the
/// field moved" and "this window holds no assistant lines". The witness is the
/// count of `type == "assistant"` lines carrying a `message`: if there are
/// some and none of them yields a substantive `Turn`, the shape moved.
fn usage_outcome(tail: &Tail) -> Outcome {
    let assistant = tail
        .lines
        .iter()
        .filter(|l| {
            l.get("type").and_then(Value::as_str) == Some("assistant") && l.get("message").is_some()
        })
        .count();
    if assistant == 0 {
        return Outcome::Unknown {
            why: format!(
                "no `type: \"assistant\"` line in the last {} transcript lines — nothing to read \
                 a usage block out of",
                tail.lines.len()
            ),
        };
    }

    let (mut turns, mut substantive, mut cached) = (0usize, 0usize, 0usize);
    for line in &tail.lines {
        let Some(crate::graph::UsageEvent::Turn {
            cache_read,
            cache_make,
            ..
        }) = crate::harness::claude::read::parse_usage_line(line, crate::graph::UsageOrigin::Session)
        else {
            continue;
        };
        turns += 1;
        if usage_is_substantive(line) {
            substantive += 1;
        }
        if cache_read > 0 || cache_make > 0 {
            cached += 1;
        }
    }

    if substantive == 0 {
        return Outcome::Fail {
            detail: format!(
                "{assistant} assistant line(s) in the window, {turns} parsed as a usage Turn, but \
                 NONE carried a substantive reading (non-empty `message.id` + `message.model` + \
                 `usage.input_tokens` > 0 + `usage.output_tokens` > 0). Every one of those \
                 readers ends in `unwrap_or(0)`, so a rename shows up as zeros in the Usage tab \
                 and nothing errors."
            ),
        };
    }
    Outcome::Pass {
        detail: format!(
            "{substantive}/{turns} usage Turn(s) substantive out of {assistant} assistant line(s); \
             cache counters non-zero on {cached}. (The cache pair is REPORTED, not asserted: \
             prompt caching can legitimately be off for an account, so failing on it would be a \
             false alarm.)"
        ),
    }
}

/// One line yields a substantive usage `Turn`: non-empty `message.id`, a
/// `message.model`, and both token counts above zero.
///
/// **The one spelling of the predicate** (V35 Phase H). [`usage_outcome`] counts
/// with it and [`substantive_lines`] selects with it, so the corpus can never
/// contain a line the probe would not have accepted — two spellings would let
/// the capture drift into recording shapes the canary already rejects.
fn usage_is_substantive(line: &Value) -> bool {
    let Some(crate::graph::UsageEvent::Turn {
        msg_id,
        model,
        in_tok,
        out_tok,
        ..
    }) = crate::harness::claude::read::parse_usage_line(line, crate::graph::UsageOrigin::Session)
    else {
        return false;
    };
    !msg_id.is_empty() && model.is_some_and(|m| !m.is_empty()) && in_tok > 0 && out_tok > 0
}

/// One `tool_result` block was read with an id and a non-empty body. See
/// [`usage_is_substantive`] for why this is a named function.
fn tool_result_is_sized(id: &str, chars: usize) -> bool {
    !id.is_empty() && chars > 0
}

/// One line carries at least one sized `tool_result`.
fn tool_result_is_substantive(line: &Value) -> bool {
    crate::harness::claude::read::extract_tool_results(line)
        .iter()
        .any(|(id, chars)| tool_result_is_sized(id, *chars))
}

/// One line yields at least one non-empty speakable block, with a keyed dedup
/// id. See [`usage_is_substantive`] for why this is a named function.
fn assistant_text_is_substantive(line: &Value) -> bool {
    crate::harness::claude::read::assistant_texts(line)
        .iter()
        .any(|(key, text)| key.contains(':') && !text.trim().is_empty())
}

/// `message.content[].text` still yields speakable prose (V35 Phase L).
///
/// Independent witness, on the same discipline as its two siblings: the count
/// of assistant lines that carry a `message.content` ARRAY. A window with such
/// lines and no readable text block is drift — the reader would run, find
/// nothing, and the tab would go mute with no error anywhere. A window with no
/// assistant content arrays at all is simply not evidence.
///
/// Deliberately NOT a failure when every content array holds only `thinking` or
/// `tool_use` blocks: that is a real and normal shape (a turn that only called
/// tools), and treating it as drift would fire on ordinary sessions.
fn assistant_text_outcome(tail: &Tail) -> Outcome {
    let with_content = tail
        .lines
        .iter()
        .filter(|l| {
            l.get("type").and_then(Value::as_str) == Some("assistant")
                && crate::harness::claude::read::message_parts(l).is_some()
        })
        .count();
    if with_content == 0 {
        return Outcome::Unknown {
            why: format!(
                "no assistant line with a `message.content[]` array in the last {} transcript \
                 lines — nothing to read a text block out of",
                tail.lines.len()
            ),
        };
    }
    let text_blocks: usize = tail
        .lines
        .iter()
        .map(|l| crate::harness::claude::read::assistant_texts(l).len())
        .sum();
    let substantive = tail
        .lines
        .iter()
        .filter(|l| assistant_text_is_substantive(l))
        .count();
    if substantive == 0 {
        return Outcome::Fail {
            detail: format!(
                "{with_content} assistant line(s) carry a `message.content[]` array but NONE \
                 yielded a speakable text block ({text_blocks} extracted) — \
                 `content[].type == \"text\"` or `content[].text` has moved. A tab with no `Stop` \
                 push would go silently mute."
            ),
        };
    }
    Outcome::Pass {
        detail: format!(
            "{substantive}/{with_content} assistant line(s) yielded speakable prose \
             ({text_blocks} text block(s) total)"
        ),
    }
}

/// One line declares a stop reason at all — the field the turn boundary is
/// read from. See [`usage_is_substantive`] for why this is a named function.
fn stop_reason_is_substantive(line: &Value) -> bool {
    line.get("type").and_then(Value::as_str) == Some("assistant")
        && line
            .get("message")
            .and_then(|m| m.get("stop_reason"))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
}

/// `message.stop_reason` still says where a turn ends (V39).
///
/// The delegation completion feed's boundary on the FALLBACK path: with no
/// `Stop` push for a tab, this field is the only thing in the transcript that
/// says which assistant message is the turn's last one.
///
/// Independent witness on the same discipline as its three siblings, and a
/// different field path from the thing witnessed: the count of assistant lines
/// carrying a `message` object at all. Such lines with NO readable stop reason
/// between them is drift — every turn then reads as still running, no
/// completion is ever filed, and a delegation waits out its whole deadline
/// before reporting `timeout` for a turn that ended in seconds.
///
/// Deliberately NOT a failure when every stop reason in the window is
/// `tool_use`: a window can legitimately hold nothing but tool-calling turns.
/// What is asserted is that the FIELD still reads; the split between turn-end
/// and mid-turn is reported instead, so a build that started answering the same
/// value for everything is visible in the detail line rather than silently
/// passing.
fn stop_reason_outcome(tail: &Tail) -> Outcome {
    let assistant = tail
        .lines
        .iter()
        .filter(|l| {
            l.get("type").and_then(Value::as_str) == Some("assistant") && l.get("message").is_some()
        })
        .count();
    if assistant == 0 {
        return Outcome::Unknown {
            why: format!(
                "no assistant line in the last {} transcript lines — nothing to read a stop \
                 reason out of",
                tail.lines.len()
            ),
        };
    }
    let declared = tail
        .lines
        .iter()
        .filter(|l| stop_reason_is_substantive(l))
        .count();
    if declared == 0 {
        return Outcome::Fail {
            detail: format!(
                "{assistant} assistant line(s) in the window and NONE carries a readable \
                 `message.stop_reason` — the fallback reader can no longer tell a turn's end \
                 from a tool pause, so a delegation into a tab with no `Stop` push files no \
                 completion at all and waits out its entire deadline"
            ),
        };
    }
    let ended = tail
        .lines
        .iter()
        .filter(|l| crate::harness::claude::read::is_turn_end(l))
        .count();
    Outcome::Pass {
        detail: format!(
            "{declared}/{assistant} assistant line(s) declare a stop reason; {ended} read as the \
             END of a turn and {} as mid-turn (a window of nothing but tool-calling turns is \
             normal, so only the field itself is asserted)",
            declared.saturating_sub(ended)
        ),
    }
}

/// One line carries BOTH identity fields: a top-level `sessionId` naming its own
/// file, and a `version`.
///
/// Stricter than [`identity_outcome`]'s pass condition, which accepts the two
/// facts from different lines — deliberately, because a capture wants one line
/// that demonstrates the whole shape rather than two that each demonstrate half.
fn identity_is_substantive(line: &Value, session_id: &str) -> bool {
    crate::harness::claude::read::record_names_session(line, session_id)
        && crate::harness::claude::read::cli_version_of(line).is_some()
}

/// `message.content[].tool_result` still yields sized results.
///
/// Independent witness again, and a different field path so the witness cannot
/// fail for the same reason as the thing witnessed: `tool_use` blocks share
/// `message.content[]` with `tool_result` but none of `tool_use_id` /
/// `is_error` / `content`. A window with tool calls but no readable results is
/// drift; a window with no tool calls at all is simply not evidence.
fn tool_result_outcome(tail: &Tail) -> Outcome {
    let tool_uses = tail
        .lines
        .iter()
        .filter_map(crate::harness::claude::read::message_parts)
        .flat_map(|parts| parts.iter())
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("tool_use"))
        .count();

    let mut results = 0usize;
    let mut sized = 0usize;
    for line in &tail.lines {
        for (id, chars) in crate::harness::claude::read::extract_tool_results(line) {
            results += 1;
            if tool_result_is_sized(&id, chars) {
                sized += 1;
            }
        }
    }
    // `is_error` is a SECOND reader of the same blocks (`tool_result_is_error`,
    // the commit-provenance guard) — counted so a flag stuck at one value is
    // visible, but never asserted: a healthy session may contain no failed
    // tool call at all.
    let errors = tail
        .lines
        .iter()
        .filter_map(crate::harness::claude::read::message_parts)
        .flat_map(|parts| parts.iter())
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter(|p| crate::harness::claude::read::tool_result_is_error(p))
        .count();

    if tool_uses == 0 && results == 0 {
        return Outcome::Unknown {
            why: format!(
                "no tool call in the last {} transcript lines (neither a `tool_use` block nor a \
                 readable `tool_result`), so an empty result set proves nothing",
                tail.lines.len()
            ),
        };
    }
    if sized == 0 {
        return Outcome::Fail {
            detail: format!(
                "{tool_uses} `tool_use` block(s) in the window but {results} readable \
                 `tool_result`(s) with a non-empty `tool_use_id` and >0 chars. \
                 `extract_tool_results` skips a block whose id it cannot read, so this degrades to \
                 an EMPTY set — indistinguishable from a user turn that ran no tools, and the row \
                 has no V16 rule lagging it."
            ),
        };
    }
    Outcome::Pass {
        detail: format!(
            "{sized}/{results} tool_result block(s) read with an id and >0 chars, against \
             {tool_uses} `tool_use` block(s); `is_error` true on {errors} (reported, not \
             asserted — a session with no failed tool call is normal)."
        ),
    }
}

/// Top-level `sessionId` and `version` still identify a transcript line.
///
/// This row is the inverse of a lagging indicator: `drift.harness_version.v1`
/// is *fed* by `version`, so losing the field silences the tripwire instead of
/// firing it. That is precisely why it gets a leading check.
fn identity_outcome(tail: &Tail) -> Outcome {
    let named = tail
        .lines
        .iter()
        .filter(|l| crate::harness::claude::read::record_names_session(l, &tail.session_id))
        .count();
    let versions: BTreeSet<&str> = tail
        .lines
        .iter()
        .filter_map(crate::harness::claude::read::cli_version_of)
        .collect();
    let sidechain = tail
        .lines
        .iter()
        .filter(|l| l.get("isSidechain").and_then(Value::as_bool) == Some(true))
        .count();
    let meta = tail
        .lines
        .iter()
        .filter(|l| l.get("isMeta").and_then(Value::as_bool) == Some(true))
        .count();

    let mut broken: Vec<&str> = Vec::new();
    if named == 0 {
        broken.push("`sessionId` (no line in the window names its own file's session)");
    }
    if versions.is_empty() {
        broken.push("`version` (no line carries a CLI build string)");
    }
    if !broken.is_empty() {
        return Outcome::Fail {
            detail: format!(
                "over {} transcript line(s) the identity fields are gone: {}. Losing `sessionId` \
                 breaks the H-2 own-record predicate the live-session registry is gated on; \
                 losing `version` SILENCES `drift.harness_version.v1` rather than firing it.",
                tail.lines.len(),
                broken.join(" and ")
            ),
        };
    }
    Outcome::Pass {
        detail: format!(
            "{named}/{} line(s) carry a matching top-level `sessionId`; CLI build string(s) seen: \
             {}; `isSidechain` on {sidechain}, `isMeta` on {meta} (both reported, not asserted — \
             a session with no sub-agent and no synthetic line is normal).",
            tail.lines.len(),
            versions.into_iter().collect::<Vec<_>>().join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The option-column parser is the whole reason the two flag probes can
    /// fail honestly, and it has one specific trap: `claude --help` mentions
    /// `--settings` inside `--bare`'s *description*, so a substring search
    /// would report a deleted flag as present. Anchored on the two-space
    /// option column, with a wrapped continuation line to prove the anchor.
    #[test]
    fn help_option_tokens_reads_the_option_column_only() {
        let help = "\
Usage: claude [options] [command] [prompt]

Options:
  --add-dir <directories...>            Additional directories to allow tool
                                        access to
  --bare                                Minimal mode: … Explicitly provide
                                        context via: --system-prompt[-file],
                                        --settings, --agents, --plugin-dir.
  -c, --continue                        Continue the most recent conversation
  --allowedTools, --allowed-tools <tools...>
      Comma or space-separated list of tool names
";
        let tokens = help_option_tokens(help);
        assert!(tokens.contains("--add-dir"));
        assert!(tokens.contains("--bare"));
        assert!(tokens.contains("-c"));
        assert!(tokens.contains("--continue"));
        // The wrapped definition line is still an option column line.
        assert!(tokens.contains("--allowedTools"));
        assert!(tokens.contains("--allowed-tools"));
        // …and the trap: named only in prose, so NOT declared.
        assert!(
            !tokens.contains("--settings"),
            "a flag mentioned inside another option's description must not read as declared — \
             that is how a DELETED flag would probe as present"
        );
        assert!(!tokens.contains("--agents"));
        assert!(!tokens.contains("--system-prompt[-file],"));
    }

    /// The registry is what tells the flag probe which flags to look for, so a
    /// row that grows a `Dep::Flag` grows the probe with it — and a row that
    /// lost them all must not silently probe nothing (that is the
    /// `declared.is_empty()` → `unknown` branch).
    #[test]
    fn declared_flags_comes_from_the_registry() {
        let session = declared_flags("claude.flag.session_id");
        assert!(session.contains(&"--session-id"), "{session:?}");
        assert!(
            session.len() >= 4,
            "the row declares the competing selectors too: {session:?}"
        );
        assert_eq!(declared_flags("claude.flag.settings_overlay"), ["--settings"]);
        // Only `Dep::Flag` — a `ConfigKey` is not a command-line flag and must
        // not be looked for in `--help`.
        assert!(declared_flags("claude.transcript.usage").is_empty());
        assert!(declared_flags("no.such.capability").is_empty());
    }

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

    /// The registry diff: only ids in NEITHER list fail, a reviewed-ungated id
    /// does not, and a vanished declared id is a note rather than drift.
    #[test]
    fn tool_registry_fails_only_on_genuinely_unclassified_ids() {
        let body = |ids: &[&str]| {
            (
                200u16,
                serde_json::to_string(&ids.iter().collect::<Vec<_>>()).unwrap(),
            )
        };

        // Everything cImp already knows about — gated and reviewed-ungated.
        let known: Vec<&str> = OPENCODE_NATIVE_TABLE
            .iter()
            .map(|(n, _, _)| *n)
            .chain(OPENCODE_NATIVE_REVIEWED_UNGATED.iter().map(|(n, _)| *n))
            .collect();
        let all = body(&known);
        assert!(
            matches!(tool_registry_outcome(Some(&all)), Outcome::Pass { .. }),
            "a live registry cImp has fully classified must PASS"
        );

        // One id nobody has looked at ⇒ Fail, naming it.
        let mut with_new = known.clone();
        with_new.push("exfiltrate");
        let drifted = body(&with_new);
        let outcome = tool_registry_outcome(Some(&drifted));
        assert!(outcome.is_fail(), "{outcome:?}");
        assert!(outcome.detail().contains("exfiltrate"), "{outcome:?}");

        // A declared id upstream stopped serving ⇒ still Pass, with a note.
        let shrunk = body(&known[1..]);
        let outcome = tool_registry_outcome(Some(&shrunk));
        assert!(!outcome.is_fail(), "{outcome:?}");
        assert!(outcome.detail().contains(known[0]), "{outcome:?}");
    }

    /// Everything the route can answer other than a clean 200-with-ids is
    /// `unknown`, never a failure — including the two cases global principle 5
    /// warns about (an empty list, and a 200 whose body is the wrong shape),
    /// which would otherwise make the diff vacuously clean and PASS.
    #[test]
    fn a_broken_tool_ids_route_is_unknown_not_failure() {
        for (label, resp) in [
            ("no response", None),
            ("404", Some((404u16, "not found".to_string()))),
            ("500", Some((500u16, String::new()))),
            ("wrong shape", Some((200u16, "{\"tools\":[]}".to_string()))),
            ("empty list", Some((200u16, "[]".to_string()))),
        ] {
            let outcome = tool_registry_outcome(resp.as_ref());
            assert!(
                matches!(outcome, Outcome::Unknown { .. }),
                "{label}: expected unknown, got {outcome:?}"
            );
            assert!(!outcome.is_fail(), "{label}");
        }
    }

    /// The auth contract, both directions — and the two ways of getting it wrong
    /// that are genuinely failures rather than noise.
    ///
    /// This test replaced `opencode_growing_auth_is_a_transition_not_a_failure`
    /// on 2026-08-17. That one pinned the OLD contract: cImp sent no credential,
    /// an unauthenticated 200 was the pass, and a 401 was the `Transition`
    /// ("upstream got better — go wire a token"). The token is wired, so a 401 on
    /// an unauthenticated call is now the PASS and its absence is the failure.
    /// Nothing about locked decision 8 changed: a control that stopped enforcing
    /// is drift in the bad direction, and every could-not-ask case below is still
    /// `unknown`.
    #[test]
    fn opencode_server_auth_is_proven_in_both_directions() {
        let pair = |route: &'static str, authed: u16, unauthed: u16| AuthPair {
            route,
            authed: Some(authed),
            unauthed: Some(unauthed),
        };
        let ids = "GET /experimental/tool/ids";
        let sess = "GET /session/:id";

        // The healthy shape: credential accepted (200 on one route, 404 on the
        // deliberately-nonexistent session id), no credential refused.
        let outcome = noauth_outcome(true, &[pair(ids, 200, 401), pair(sess, 404, 401)]);
        assert!(matches!(outcome, Outcome::Pass { .. }), "{outcome:?}");

        // Auth silently not enforced — the password had no effect. Names the
        // route, because "which surface is open" is the whole answer.
        let outcome = noauth_outcome(true, &[pair(ids, 200, 200), pair(sess, 404, 401)]);
        assert!(outcome.is_fail(), "{outcome:?}");
        assert!(outcome.detail().contains(ids), "{outcome:?}");

        // cImp's own credential refused — the scheme moved and the tap is dark.
        let outcome = noauth_outcome(true, &[pair(ids, 401, 401), pair(sess, 404, 403)]);
        assert!(outcome.is_fail(), "{outcome:?}");
        assert!(
            outcome.detail().contains("harness/opencode/config.rs"),
            "{outcome:?}"
        );

        // …and every could-not-ask case is `unknown`, never a failure.
        for (label, credentialed, pairs) in [
            ("no credential of our own", false, vec![pair(ids, 200, 401)]),
            ("nothing answered", true, vec![]),
            (
                "answered only one way",
                true,
                vec![AuthPair {
                    route: ids,
                    authed: Some(200),
                    unauthed: None,
                }],
            ),
        ] {
            let outcome = noauth_outcome(credentialed, &pairs);
            assert!(
                matches!(outcome, Outcome::Unknown { .. }),
                "{label}: {outcome:?}"
            );
            assert!(!outcome.is_fail(), "{label}");
        }
    }

    /// A transcript window with no evidence in it is `unknown`; one with an
    /// independent witness but no readable shape is `fail`. This is the
    /// distinction that keeps the transcript probes from crying wolf on a fresh
    /// session while still catching a real rename.
    #[test]
    fn transcript_probes_need_an_independent_witness_to_fail() {
        let tail = |lines: &[&str]| Tail {
            lines: lines
                .iter()
                .map(|l| serde_json::from_str(l).expect("test fixture json"))
                .collect(),
            session_id: "sess-1".to_string(),
            unparsed: 0,
        };

        // No assistant line at all ⇒ nothing to read usage out of.
        let quiet = tail(&[r#"{"type":"user","sessionId":"sess-1","version":"2.1.232"}"#]);
        assert!(matches!(usage_outcome(&quiet), Outcome::Unknown { .. }));
        // …and no tool call at all ⇒ an empty result set proves nothing.
        assert!(matches!(
            tool_result_outcome(&quiet),
            Outcome::Unknown { .. }
        ));

        // An assistant line whose token fields were renamed: the witness says a
        // turn happened, the reader says zero. That is the silent-zeros class.
        let renamed = tail(&[
            r#"{"type":"assistant","sessionId":"sess-1","version":"2.1.232","message":
                {"id":"m1","model":"claude-x","usage":{"inputTokens":10,"outputTokens":5}}}"#,
        ]);
        let outcome = usage_outcome(&renamed);
        assert!(outcome.is_fail(), "{outcome:?}");

        // The healthy shape passes.
        let healthy = tail(&[
            r#"{"type":"assistant","sessionId":"sess-1","version":"2.1.232","message":
                {"id":"m1","model":"claude-x","usage":{"input_tokens":10,"output_tokens":5,
                "cache_read_input_tokens":7,"cache_creation_input_tokens":3}}}"#,
        ]);
        assert!(matches!(usage_outcome(&healthy), Outcome::Pass { .. }));

        // tool_result: a `tool_use` block is the witness; a renamed
        // `tool_use_id` empties the result set silently.
        let tools_broken = tail(&[
            r#"{"type":"assistant","sessionId":"sess-1","message":{"id":"m1","content":
                [{"type":"tool_use","id":"t1","name":"Read"}]}}"#,
            r#"{"type":"user","sessionId":"sess-1","message":{"content":
                [{"type":"tool_result","toolUseId":"t1","content":"hello"}]}}"#,
        ]);
        assert!(tool_result_outcome(&tools_broken).is_fail());

        let tools_ok = tail(&[
            r#"{"type":"assistant","sessionId":"sess-1","message":{"id":"m1","content":
                [{"type":"tool_use","id":"t1","name":"Read"}]}}"#,
            r#"{"type":"user","sessionId":"sess-1","message":{"content":
                [{"type":"tool_result","tool_use_id":"t1","content":"hello"}]}}"#,
        ]);
        assert!(matches!(
            tool_result_outcome(&tools_ok),
            Outcome::Pass { .. }
        ));

        // identity: both fields present ⇒ pass; either gone ⇒ fail.
        assert!(matches!(identity_outcome(&healthy), Outcome::Pass { .. }));
        let no_version = tail(&[r#"{"type":"user","sessionId":"sess-1"}"#]);
        assert!(identity_outcome(&no_version).is_fail());
        let wrong_session = tail(&[r#"{"type":"user","sessionId":"other","version":"2.1.232"}"#]);
        assert!(identity_outcome(&wrong_session).is_fail());
    }

    /// Nothing the probe prints may carry transcript CONTENT. The detail
    /// strings are counts and field names by construction; this pins the
    /// construction, because the readers are handed real user data and the
    /// report is meant to be pasted into an issue.
    #[test]
    fn transcript_details_carry_no_payload() {
        let secret = "hunter2-do-not-print";
        let tail = Tail {
            lines: vec![
                serde_json::from_str(&format!(
                    r#"{{"type":"assistant","sessionId":"s","version":"2.1.232","message":
                       {{"id":"m1","model":"claude-x","usage":{{"input_tokens":1,
                       "output_tokens":1}},"content":[{{"type":"tool_use","id":"t1",
                       "name":"Read","input":{{"file_path":"{secret}"}}}}]}}}}"#
                ))
                .unwrap(),
                serde_json::from_str(&format!(
                    r#"{{"type":"user","sessionId":"s","message":{{"content":
                       [{{"type":"tool_result","tool_use_id":"t1","content":"{secret}"}}]}}}}"#
                ))
                .unwrap(),
            ],
            session_id: "s".to_string(),
            unparsed: 0,
        };
        for outcome in [
            usage_outcome(&tail),
            tool_result_outcome(&tail),
            identity_outcome(&tail),
        ] {
            assert!(
                !outcome.detail().contains(secret),
                "a probe detail leaked transcript payload: {}",
                outcome.detail()
            );
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
            DECLARED_UNPROBED.len(),
            "the id view must not drop entries"
        );
        for (id, why) in DECLARED_UNPROBED {
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

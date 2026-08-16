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
//! * [`Outcome::Transition`] — upstream changed **for the better** (OpenCode
//!   growing auth is the worked example). A capability transition, not a red
//!   test.
//!
//! Modelling the last two as failures would recreate exactly the alarm fatigue
//! this milestone exists to remove: `drift.harness_version.v1` fires on every
//! CLI auto-update, so the rational response became clicking *Mark verified*
//! without running anything — which disarmed the control guarding all the
//! others. A probe nobody trusts is worse than no probe.
//!
//! # What is probed, and what is only enumerated
//!
//! [`IMPLEMENTED`] holds the seven rows this phase actually drives — the ones
//! reachable without scripting a model turn. [`DECLARED_UNPROBED`] holds the
//! other eleven, each with the reason it cannot be, and they are **printed**
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
//! Capture-on-success (design doc § 4.1) is Phase H and is deliberately absent:
//! this module never creates a file.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::harness::contract::{self, Harness, Seam};
use crate::offload::toolclass::{OPENCODE_NATIVE_REVIEWED_UNGATED, OPENCODE_NATIVE_TABLE};

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

fn harness_name(h: Harness) -> &'static str {
    match h {
        Harness::Claude => "claude",
        Harness::OpenCode => "opencode",
        Harness::Any => "any",
    }
}

fn tier_name(t: Seam) -> &'static str {
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
        "opencode.plugin.load_all",
        "no probe can settle it, and it is inside the TCB: nothing outside a harness can verify \
         that a control inside it ran. A plugin that loads but skips the `throw` looks fully \
         functional. Manual OpenCode-veto spike; Phase I's `chp` handshake at least makes a STALE \
         plugin a mismatch instead of a mystery",
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
    // `opencode.tool_registry` is the one this phase exists for.
    let mut produced: Vec<ProbeResult> = Vec::new();
    produced.extend(probe_opencode());
    produced.extend(probe_claude_flags());
    produced.extend(probe_claude_transcript());

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
/// same reason `context_hook::post_context` is: this runs before any runtime
/// exists, and a probe that needed an async stack to ask one question would be
/// harder to trust than the question is worth.
fn http_get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT)).ok()?;
    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Accept: application/json\r\n\
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

    let mut cmd = Command::new(&binary);
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

    let child = cmd
        .spawn()
        .map_err(|e| format!("`opencode serve` could not be spawned: {e}"))?;
    let serve = Serve { child, port };

    // Readiness = the server answers an HTTP request, not merely that the
    // socket accepts. Bun binds before its routes are mounted.
    let deadline = Instant::now() + SERVE_READY_TIMEOUT;
    while Instant::now() < deadline {
        if http_get(port, "/experimental/tool/ids").is_some() {
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
fn probe_opencode() -> Vec<ProbeResult> {
    let serve = match start_opencode_serve() {
        Ok(s) => s,
        Err(why) => {
            return vec![
                ProbeResult::new("opencode.tool_registry", Outcome::Unknown { why: why.clone() }),
                ProbeResult::new("opencode.route.noauth", Outcome::Unknown { why }),
            ];
        }
    };

    let ids = http_get(serve.port, "/experimental/tool/ids");
    // A declared route (`GET /session/:id`) rather than the one above, so the
    // auth question is asked of a surface cImp actually depends on. The id is
    // deliberately one that cannot exist: a 404 still proves the request was
    // not rejected for want of a credential, and inventing a real session would
    // mean writing to the user's OpenCode state.
    let session = http_get(serve.port, "/session/cimp-harness-probe-does-not-exist");

    vec![
        ProbeResult::new("opencode.tool_registry", tool_registry_outcome(ids.as_ref())),
        ProbeResult::new(
            "opencode.route.noauth",
            noauth_outcome(ids.as_ref(), session.as_ref()),
        ),
    ]
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

/// Whether OpenCode's local server still serves cImp's routes unauthenticated.
///
/// A 401/403 is the **good** news case (locked decision 8): auth landed, and
/// the response is to wire a token into `oob/opencode.rs`, not to file a bug.
/// Note the CLI already warns `OPENCODE_SERVER_PASSWORD is not set; server is
/// unsecured` on 1.18.13, so the mechanism exists — this row is watching for
/// the day it becomes mandatory.
fn noauth_outcome(ids: Option<&(u16, String)>, session: Option<&(u16, String)>) -> Outcome {
    let statuses: Vec<(&str, u16)> = [
        ("GET /experimental/tool/ids", ids),
        ("GET /session/:id", session),
    ]
    .into_iter()
    .filter_map(|(what, r)| r.map(|(s, _)| (what, *s)))
    .collect();

    if statuses.is_empty() {
        return Outcome::Unknown {
            why: "the server answered neither probed route, so nothing can be said about auth"
                .to_string(),
        };
    }
    let rendered = statuses
        .iter()
        .map(|(what, s)| format!("{what} → {s}"))
        .collect::<Vec<_>>()
        .join(", ");
    if statuses.iter().any(|(_, s)| *s == 401 || *s == 403) {
        return Outcome::Transition {
            note: format!(
                "AUTH LANDED — OpenCode now rejects unauthenticated localhost calls ({rendered}). \
                 This is an upstream IMPROVEMENT, not drift: wire the token into \
                 `oob/opencode.rs` (tap + V30 push) and retire this watch. Until then the live \
                 session tap and the push fanout are off."
            ),
        };
    }
    Outcome::Pass {
        detail: format!(
            "unauthenticated localhost calls still served ({rendered}); no Authorization header \
             sent. Double-edged by design: the tap and push work, and the local server remains an \
             unauthenticated loopback surface."
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
    let mut child = cmd
        .spawn()
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
fn probe_claude_flags() -> Vec<ProbeResult> {
    let (session_id, settings) = ("claude.flag.session_id", "claude.flag.settings_overlay");
    let help = match claude_help() {
        Ok(h) => h,
        Err(why) => {
            return vec![
                ProbeResult::new(session_id, Outcome::Unknown { why: why.clone() }),
                ProbeResult::new(settings, Outcome::Unknown { why }),
            ];
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
        return vec![
            ProbeResult::new(session_id, Outcome::Unknown { why: why.clone() }),
            ProbeResult::new(settings, Outcome::Unknown { why }),
        ];
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
    out
}

// ── claude: the transcript tail ─────────────────────────────────────────────

/// A bounded window onto the newest real transcript. Carries no payload: the
/// parsed lines never leave this module, and `session_id` is used only as the
/// expected value for [`crate::oob::claude::record_names_session`].
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
    let root = crate::oob::claude::projects_root()?;
    if let Some(here) = std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::oob::claude::project_root(&cwd))
        .and_then(|dir| crate::oob::claude::newest_jsonl(&dir))
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
        let Some(candidate) = crate::oob::claude::newest_jsonl(&entry.path()) else {
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

/// The three `claude.transcript.*` rows, all read from one tail.
fn probe_claude_transcript() -> Vec<ProbeResult> {
    let ids = [
        "claude.transcript.usage",
        "claude.transcript.tool_result",
        "claude.transcript.identity",
    ];
    let tail = newest_transcript().and_then(|p| read_tail(&p));
    let Some(tail) = tail else {
        let why = "no Claude Code session transcript found under ~/.claude/projects — nothing to \
                   tail. Not a failure: an unused harness cannot drift."
            .to_string();
        return ids
            .iter()
            .map(|id| ProbeResult::new(id, Outcome::Unknown { why: why.clone() }))
            .collect();
    };
    if tail.lines.is_empty() {
        let why = format!(
            "the newest transcript's last {} KiB held no parseable JSON object ({} unparsed \
             lines) — the artifact may no longer be JSONL",
            TAIL_BYTES / 1024,
            tail.unparsed
        );
        return ids
            .iter()
            .map(|id| ProbeResult::new(id, Outcome::Unknown { why: why.clone() }))
            .collect();
    }

    vec![
        ProbeResult::new(ids[0], usage_outcome(&tail)),
        ProbeResult::new(ids[1], tool_result_outcome(&tail)),
        ProbeResult::new(ids[2], identity_outcome(&tail)),
    ]
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
            msg_id,
            model,
            in_tok,
            out_tok,
            cache_read,
            cache_make,
            ..
        }) = crate::oob::claude::parse_usage_line(line, crate::graph::UsageOrigin::Session)
        else {
            continue;
        };
        turns += 1;
        if !msg_id.is_empty()
            && model.is_some_and(|m| !m.is_empty())
            && in_tok > 0
            && out_tok > 0
        {
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
        .filter_map(crate::oob::claude::message_parts)
        .flat_map(|parts| parts.iter())
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("tool_use"))
        .count();

    let mut results = 0usize;
    let mut sized = 0usize;
    for line in &tail.lines {
        for (id, chars) in crate::oob::claude::extract_tool_results(line) {
            results += 1;
            if !id.is_empty() && chars > 0 {
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
        .filter_map(crate::oob::claude::message_parts)
        .flat_map(|parts| parts.iter())
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter(|p| crate::oob::claude::tool_result_is_error(p))
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
        .filter(|l| crate::oob::claude::record_names_session(l, &tail.session_id))
        .count();
    let versions: BTreeSet<&str> = tail
        .lines
        .iter()
        .filter_map(crate::oob::claude::cli_version_of)
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

    /// A 401/403 is upstream getting BETTER, so it must transition rather than
    /// fail — the worked example of locked decision 8.
    #[test]
    fn opencode_growing_auth_is_a_transition_not_a_failure() {
        let ok = (200u16, "[]".to_string());
        let denied = (401u16, String::new());
        let not_found = (404u16, String::new());

        assert!(matches!(
            noauth_outcome(Some(&ok), Some(&not_found)),
            Outcome::Pass { .. }
        ));
        for pair in [
            (Some(&denied), Some(&ok)),
            (Some(&ok), Some(&denied)),
            (Some(&denied), Some(&denied)),
        ] {
            let outcome = noauth_outcome(pair.0, pair.1);
            assert!(
                matches!(outcome, Outcome::Transition { .. }),
                "{outcome:?}"
            );
            assert!(!outcome.is_fail());
            assert!(outcome.detail().contains("oob/opencode.rs"), "{outcome:?}");
        }
        assert!(matches!(
            noauth_outcome(None, None),
            Outcome::Unknown { .. }
        ));
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

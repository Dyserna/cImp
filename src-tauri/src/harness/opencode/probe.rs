//! **OpenCode's L2 probes** — the bodies `harness/probe.rs`'s neutral runner
//! drives through [`HarnessPlugin::probe`](crate::harness::plugin::HarnessPlugin::probe).
//!
//! V40 Phase A, locked decision 17: the runner keeps the report shape, the
//! outcome model and the `cimp --harness-canary` CLI; **what** is driven against
//! an installed OpenCode — the `opencode serve` child, its two routes, the
//! native-tool registry diff and the auth contract — lives here, with the
//! harness it is true of. Moved verbatim: same text, same assertions, same
//! fixtures.
//!
//! The privacy posture the module docs of [`crate::harness::probe`] state
//! applies unchanged: nothing is written from here, and every detail string
//! carries counts and field names only.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::harness::capture::Observed;
use crate::harness::probe::{Outcome, ProbeResult, SERVE_POLL_INTERVAL};
use crate::harness::opencode::tools::{OPENCODE_NATIVE_REVIEWED_UNGATED, OPENCODE_NATIVE_TABLE};

// ── timings and bounds, all deliberate ──────────────────────────

/// How long to wait for `opencode serve` to answer its first request. Ten
/// seconds is the brief's figure and is ~5× the observed cold start; past it
/// the probe reports `unknown`, never a failure.
const SERVE_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Gap between readiness polls while the server boots.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to allow `claude --help` before giving up (⇒ `unknown`).

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
        crate::procutil::reap_probe_child(super::harness_plugin::me(), &mut self.child);
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
pub(in crate::harness) fn probe_opencode() -> (Vec<ProbeResult>, Vec<Observed>, String) {
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

    // The GATED set is the classed rows: a `class: None` row (today `list`,
    // which exists only for its memory kind) makes no gating claim, so counting
    // it as classified would let an ungated id pass this subtraction.
    let gated: BTreeSet<&str> = OPENCODE_NATIVE_TABLE
        .iter()
        .filter(|t| t.class.is_some())
        .map(|t| t.name)
        .collect();
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

/// This probe's row in the external-process spawn ledger (V40 Phase E, locked
/// decision 26) — see the sibling in `harness/claude/probe.rs`.
pub(crate) const SPAWN_SITES: &[crate::spawn_ledger::SpawnSite] = &[
    crate::spawn_ledger::SpawnSite {
        file: "harness/opencode/probe.rs",
        symbol: "start_opencode_serve",
        spawns: "opencode serve --port <free> --hostname 127.0.0.1",
        class: crate::spawn_ledger::SpawnClass::HostSpawn,
        count: 1,
        reason: "The other half of the same V35 Phase D probe, same trigger and same posture as \
                 `harness/claude/probe.rs`: a fixed name through `pty::resolve_command`, a \
                 literal argv, no model input, and deliberately unsandboxed because the point is \
                 to observe the REAL installed harness. This child also gets a free loopback \
                 port and is reaped through `kill_tree_blocking` on drop, because it forks its \
                 own children.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

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
            .filter(|t| t.class.is_some())
            .map(|t| t.name)
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
}

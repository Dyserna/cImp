//! `offload::loopback`'s unit tests — the module's own `#[cfg(test)] mod tests`,
//! moved out of `loopback.rs` verbatim (V42 R1, #114). Same module path, same
//! `use super::*`, no test added, removed, or edited.
//!
//! Many of these are **source-scanning** tests: they `include_str!` the
//! production file(s) and assert on the text, so that a gate deleted from a
//! handler fails a test rather than a review. Since V42 R4 (#115) the route
//! surface is a DIRECTORY, so they read a list of files — [`ROUTE_SOURCES`],
//! declared beside the dispatch in `mod.rs` — rather than one `include_str!`.
//! A scan that kept reading the file a handler used to be in would be green
//! about code it no longer covers, which for a security assertion is the same
//! thing as being deleted.

use super::*;

// V42 R2/R3 (#114) moved the discovery and taint-latch halves of this module
// to `offload::discovery` and `offload::latch`. What follows is what only the
// TESTS reach for, imported here so each production `use` stays exactly as
// wide as production needs.
use crate::offload::latch::{
    override_row, unlatch_clear_row, ClearBasis, LatchOverride, TabLatch,
};
use crate::offload::toolclass::{Latch, ToolClass, WriteTaint};

use crate::offload::discovery::{
    dispatch_discovery_report, report_skipped_to_app, resolve_external_root, responds,
    select_verified, ChildIdentity, DISCOVERY_REPORT_TIMEOUT, DISCOVERY_SKIPPED_PATH,
};

use crate::harness::claude::hook as claude_hook;

/// V40 Phase C moved the ingress here; the source-scanning tests that read
/// a handler's body follow it.
const HOOK_SRC: &str = include_str!("../../harness/claude/hook.rs");

/// **Every file a loopback route handler can live in**, paired with its source.
///
/// The route surface ([`super::ROUTE_SOURCES`], one row per family file since
/// V42 R4 (#115)) plus the Claude plugin's ingress, which V40 Phase C moved
/// twelve handlers into. The source-scanning tests below take a LIST rather
/// than a file, so a handler that moves between families keeps its scanners
/// instead of quietly losing them: a scan that kept reading `mod.rs` after the
/// routes moved out would be green about code it no longer covers, and for a
/// security assertion green-about-nothing is the failure mode.
fn route_surface() -> Vec<(&'static str, &'static str)> {
    let mut files = ROUTE_SOURCES.to_vec();
    files.push(("harness/claude/hook.rs", HOOK_SRC));
    files
}

/// Whether `line` opens a top-level item whose signature is `sig`, whatever
/// visibility it wears.
///
/// V40 Phase C made moved items `pub(crate)`; V42 R4 (#115) made every item a
/// family file publishes `pub(super)`. The item is still top-level, which is
/// the property the column-0 `}` terminator depends on — so the scan reads
/// through the modifier rather than growing a case per spelling (which is how
/// a scan comes to silently match nothing).
fn declares(line: &str, sig: &str) -> bool {
    if line.starts_with(sig) {
        // A caller that spells the visibility in `sig` itself (`pub fn
        // proxy_base_for(`) is pinning THAT too, and must not be read
        // through.
        return true;
    }
    past_visibility(line).starts_with(sig)
}

/// `line` with a leading visibility modifier stripped: `pub`, `pub(crate)`,
/// `pub(super)`, `pub(in some::path)`. Returns `line` unchanged when there is
/// none.
///
/// Column 0 is not negotiable and nothing is trimmed: everything these scans
/// assert about is a TOP-LEVEL item, and the column-0 `}` terminator
/// [`fn_body`] relies on is sound only for one.
fn past_visibility(line: &str) -> &str {
    match line.strip_prefix("pub") {
        None => line,
        Some(tail) => tail
            .strip_prefix('(')
            .and_then(|t| t.split_once(") "))
            .map(|(_scope, after)| after)
            .or_else(|| tail.strip_prefix(' '))
            .unwrap_or(tail),
    }
}

/// The module named by a top-level `mod NAME;` declaration, whatever
/// visibility it wears.
///
/// V42 review, RV-7. [`the_source_scanners_read_every_route_file`] scraped
/// `mod.rs` with a bare `strip_prefix("mod ")`, so a family file declared
/// `pub(crate) mod x;` was invisible to it — and invisible on BOTH sides of
/// the join it feeds: such a file would be missing from the scrape AND from
/// `ROUTE_SOURCES`, the two shortened lists would agree, and the one test
/// whose job is to notice an unscanned route file would be green about exactly
/// that.
fn mod_name(line: &str) -> Option<&str> {
    past_visibility(line)
        .strip_prefix("mod ")?
        .strip_suffix(';')
}

/// [`fn_body`] over a LIST of files: the one that declares `sig` is found
/// first, and its body scanned.
///
/// **Exactly one file may declare it.** Two copies of a handler is itself the
/// failure these scans exist to catch, and a scan that read whichever came
/// first would be pinning one of them while the other ran.
fn fn_body_in(files: &[(&'static str, &'static str)], sig: &str) -> String {
    let named: Vec<&'static str> = files
        .iter()
        .filter(|(_, src)| src.lines().any(|l| declares(l, sig)))
        .map(|(file, _)| *file)
        .collect();
    assert_eq!(
        named.len(),
        1,
        "`{sig}` is declared in {named:?} — exactly one file in the route surface must"
    );
    let src = files
        .iter()
        .find(|(file, _)| *file == named[0])
        .expect("the file just found")
        .1;
    fn_body(src, sig)
}

/// The files in `files` whose **code** contains `needle`, for the scans whose
/// assertion is about PRESENCE somewhere in the surface rather than about one
/// item's body.
///
/// V42 review, RV-9. This searched the raw source, so a doc comment naming the
/// signature or the header was enough to satisfy it — and both call sites are
/// security assertions ("the exec roots derive from the app, never from a
/// request body"; "the tab-identity headers are actually matched"). A scan a
/// comment can satisfy is a scan that keeps passing after the code it names is
/// deleted.
///
/// [`crate::rustsrc::uncommented`], not `code_of`: one of the two needles IS a
/// string literal (`"x-cimp-tab" =>`, a match arm on a header name), and the
/// strong pass blanks it — which would have replaced "a comment can satisfy
/// this" with "nothing can", the same vacuity wearing the opposite face.
fn files_containing(files: &[(&'static str, &'static str)], needle: &str) -> Vec<&'static str> {
    files
        .iter()
        .filter(|(rel, src)| crate::rustsrc::uncommented(rel, src).contains(needle))
        .map(|(file, _)| *file)
        .collect()
}

/// The control for [`files_containing`] (V42 review, RV-9), permanent rather
/// than a plant-and-revert: the inputs are synthetic, so this asserts the
/// property directly instead of asserting that today's production text happens
/// to have it.
#[test]
fn files_containing_reads_code_and_not_prose() {
    let needle = "fn hook_exec_roots(ctx: &RouteCtx";
    let commented = format!("// {needle}, settings: &S) -> Vec<PathBuf>\nfn other() {{}}\n");
    let real = format!("{needle}, settings: &S) -> Vec<PathBuf> {{\n}}\n");

    // The control's own premise: the RAW text does contain the needle, which
    // is precisely what the pre-RV-9 `src.contains(needle)` matched. If this
    // ever stops holding, the assertion below is passing on nothing.
    assert!(
        commented.contains(needle),
        "the commented fixture must still contain the needle as raw text"
    );

    assert!(
        files_containing(&[("prose.rs", Box::leak(commented.into_boxed_str()))], needle)
            .is_empty(),
        "a doc/line comment satisfied a scan whose whole assertion is that the CODE does it"
    );
    assert_eq!(
        files_containing(&[("real.rs", Box::leak(real.into_boxed_str()))], needle),
        vec!["real.rs"],
        "the scan stopped seeing a real declaration"
    );

    // …and the deliberate scope limit: a needle that IS a literal must still
    // be found, because the header scan below looks for a match arm.
    const ARM: &str = "match h { \"x-cimp-tab\" => 1, _ => 0 };\n";
    assert_eq!(
        files_containing(&[("arm.rs", ARM)], "\"x-cimp-tab\" =>"),
        vec!["arm.rs"],
        "blanking string literals would make the header scan match nothing at all"
    );
}


/// V32 Phase G: the default posture — both feature switches on. Every
/// pre-Phase-G latch test asserted this implicitly, so it is the value they
/// keep asserting; the switched-off behaviour has its own tests below.
const ON: GatePolicy = GatePolicy {
    latch: true,
    quarantine: true,
};

/// The provenance a NATIVE route states (#48, F-3): cImp's own dispatch,
/// no fetched page in view. What every pre-F-3 `gate` test was implicitly
/// asserting, since none of them was an intake.
const NO_CONTENT: CallProvenance<'static> = CallProvenance::internal();

/// #48 F-16: what `/graph_run` actually states — a native route that knows
/// which project the call runs against. Any test that drives a
/// PERSISTENT-WRITE through `gate` must use this rather than [`NO_CONTENT`],
/// because that path writes a `MemoryQuarantine` row and `record_flag`'s
/// tripwire refuses to let one be filed under no project.
const NATIVE_IN_PROJECT: CallProvenance<'static> = CallProvenance::internal_in(TEST_ROOT);
/// The provenance the `/latch/beacon` route states — always `Http`.
const BEACON_PROV: CallProvenance<'static> = CallProvenance::http();

/// The project root the test scopes claim. A real scope's root is resolved
/// from the tab's settings entry (`tab_root_key`); the tests care only that
/// it is carried through to the row, so one fixed value keeps the
/// assertions readable.
const TEST_ROOT: &str = "P:\\proj";

#[test]
fn find_subslice_locates_header_end() {
    let hay = b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody";
    let pos = find_subslice(hay, b"\r\n\r\n").unwrap();
    assert_eq!(&hay[pos..pos + 4], b"\r\n\r\n");
}

#[test]
fn authorized_requires_exact_bearer() {
    let req = Request {
        method: "POST".into(),
        path: "/run".into(),
        auth: Some("Bearer abc123".into()),
        cimp: CimpHeaders::default(),
        body: Vec::new(),
    };
    assert!(authorized(&req, "abc123"));
    assert!(!authorized(&req, "nope"));
    let none = Request {
        method: "GET".into(),
        path: "/describe".into(),
        auth: None,
        cimp: CimpHeaders::default(),
        body: Vec::new(),
    };
    assert!(!authorized(&none, "abc123"));
}

// ── V30 Phase B: /events subscriber identity + the push frame ─────────

#[test]
fn query_param_reads_the_events_identity() {
    let path = "/events?tab=claude-2&consumer=claude&channels=1";
    assert_eq!(query_param(path, "tab"), Some("claude-2"));
    assert_eq!(query_param(path, "consumer"), Some("claude"));
    assert_eq!(query_param(path, "channels"), Some("1"));
    assert_eq!(query_param(path, "nope"), None);
    // A prefix must not match a different key, and a bare path has none.
    assert_eq!(query_param("/events?consumer=opencode", "consume"), None);
    assert_eq!(query_param("/events", "tab"), None);
    // The pre-V30 child sends no query at all: it must still parse as the
    // default consumer, with no tab and no channels.
    let legacy = Request {
        method: "GET".into(),
        path: "/events".into(),
        auth: None,
        cimp: CimpHeaders::default(),
        body: Vec::new(),
    };
    assert_eq!(
        consumer_from_token(query_param(&legacy.path, "consumer")),
        Some(Consumer::Harness(crate::harness::DEFAULT_HARNESS))
    );
    assert!(!matches!(
        query_param(&legacy.path, "channels"),
        Some("1") | Some("true")
    ));
}

// ── V40 review H-1: ONE identity per grant-bearing route ──────────────

/// The grant a call is served under and the taint latch that judges it
/// name the **same harness**, on every route that resolves a consumer.
///
/// The regression: `?consumer=offload` was folded onto Claude's granted
/// server set by [`Consumer::conservative_grant`] while its latch key was
/// derived from the RAW token, resolving to the activity source
/// `"offload"` — which is no configured tab of any harness, so
/// `latch_scope` answered `Unknown`, `LatchRegistry::gate` took its
/// documented fail-open and the EXTERNAL budget went uncharged. Claude's
/// servers with Claude's latch switched off, on `/mcp/call`, `/run` and
/// `/graph_run` alike. Develop had no such spelling, because its
/// `source_for_consumer` answered `"claude"` for every token it did not
/// recognise.
#[test]
fn a_grant_bearing_route_resolves_one_identity_for_the_grant_and_the_latch() {
    // A registered harness resolves to itself, and keys its own latch.
    for h in crate::harness::registry::all() {
        let token = h
            .descriptor()
            .expect("a registered id has a descriptor")
            .consumer;
        let (consumer, agent) =
            proxy_identity(Some(token)).expect("a registered consumer resolves");
        assert_eq!(consumer, Consumer::Harness(h));
        assert_eq!(agent, h.token(), "{token}'s latch key is its own");
    }

    // Absent: the pre-V30 wire-compatibility default, not a guess.
    let (consumer, agent) = proxy_identity(None).expect("an absent consumer resolves");
    assert_eq!(consumer, Consumer::Harness(crate::harness::DEFAULT_HARNESS));
    assert_eq!(agent, crate::harness::DEFAULT_HARNESS.token());

    // cImp's OWN in-app consumer. It is still served under
    // `conservative_grant()` — and the latch key is now THAT harness's.
    let (consumer, agent) = proxy_identity(Some("offload")).expect("`offload` resolves");
    assert_eq!(consumer, Consumer::conservative_grant());
    let Consumer::Harness(served_as) = consumer else {
        panic!("`conservative_grant` answers a harness");
    };
    assert_eq!(
        agent,
        served_as.token(),
        "an in-app consumer served out of a harness's grants must be judged under \
         that harness's latch"
    );
    assert_ne!(agent, "offload");
    assert_ne!(agent, crate::graph::UNKNOWN_SOURCE);

    // A token nobody declared is REFUSED — not degraded to an unscoped,
    // ungated, unattributed call (locked decision 2).
    for token in ["codex", "audit", "", "claude-code", "unknown", "  "] {
        assert_eq!(
            proxy_identity(Some(token)),
            None,
            "{token:?} names nothing this proxy serves and must be refused"
        );
    }
}

/// The structural half of the finding: every grant-bearing handler must go
/// through [`proxy_identity`], so a route added later cannot re-open the
/// split by deriving its latch key from the caller's raw claim.
#[test]
fn every_grant_bearing_handler_resolves_through_proxy_identity() {
    for (handler, route) in [
        ("async fn handle_mcp_list(", "/mcp/list"),
        ("async fn handle_mcp_call(", "/mcp/call"),
        ("async fn handle_run(", "/run"),
        ("async fn handle_graph_run(", "/graph_run"),
    ] {
        // V42 R4 (#115): these four are in three different family files
        // now, so the body is looked up ACROSS the surface rather than by
        // slicing one — a handler that moved must not fall out of the scan.
        let body = fn_body_in(ROUTE_SOURCES, handler);
        assert!(
            body.contains("proxy_identity("),
            "{route} must resolve its consumer through `proxy_identity` — the grant \
             and the taint latch have to name one harness (V40 review H-1)"
        );
    }
}

/// The wire contract the child's SSE parser depends on: one `event:` line,
/// one single-line `data:` line, blank-line terminated — even when the
/// pushed content itself contains newlines (serde escapes them).
#[test]
fn push_frame_is_a_single_line_sse_data_payload() {
    let notice = PushNotice::new(
        "line one\nline two\r\nline three",
        &[],
        [("kind", "audit_done"), ("seq", "3")],
    );
    let frame = String::from_utf8(push_frame(&notice)).unwrap();
    let lines: Vec<&str> = frame.split('\n').collect();
    assert_eq!(lines[0], "event: push");
    assert!(lines[1].starts_with("data: "));
    // event / data / "" / "" — exactly one data line, blank-line terminated.
    assert_eq!(lines.len(), 4, "frame was: {frame:?}");
    assert_eq!(lines[2], "");
    assert_eq!(lines[3], "");
    let round: PushNotice = serde_json::from_str(&lines[1]["data: ".len()..]).unwrap();
    assert_eq!(round, notice);
}

#[test]
fn token_is_long_and_random() {
    let a = make_token();
    let b = make_token();
    assert_ne!(a, b);
    assert!(a.len() >= 32);
}

// ── NC-2: permission-hook classification + tab mapping ─────────────────

// ── V24 Phase F: the memory-ingress usage arm ──────────────────────────
//
// V40 Phase I (issue #107 item 2): the BODY these drove is the harness's
// now, and its pure mapping is tested beside it in
// `harness::opencode::hook`. What is still core's — and still tested here —
// is what the handler does with the neutral `MemoryEvent`: which
// `graph::UsageEvent` it builds, which session it files it against, and
// that the row lands in a real store. So these go through the same two
// steps `handle_memory_event` takes, without a live listener.

/// Read a `/memory/event` body the way the handler does — the sending
/// harness's plugin — and build the `graph::UsageEvent` the handler builds
/// from a `Turn`. `None` when the body carries no recordable turn.
fn memory_turn(json: serde_json::Value) -> Option<(String, crate::graph::UsageEvent)> {
    use crate::harness::plugin::MemoryEventKind;
    let plugin = crate::harness::HarnessId::from_id("opencode")
        .and_then(|h| h.plugin())
        .expect("opencode is registered");
    let parsed = plugin
        .memory_event(json.to_string().as_bytes())
        .expect("this harness serves the memory route")
        .expect("the fixture parses");
    match parsed.kind {
        MemoryEventKind::Turn {
            target,
            origin,
            msg_id,
            model,
            in_tok,
            out_tok,
            cache_read,
            cache_make,
        } => Some((
            target,
            crate::graph::UsageEvent::Turn {
                msg_id,
                model,
                in_tok,
                out_tok,
                cache_read,
                cache_make,
                origin: origin.to_string(),
            },
        )),
        _ => None,
    }
}

#[test]
fn usage_body_well_formed_records_session_turn() {
    // No parent → recorded against the reporting session, in the declared
    // main lane.
    let (target, event) = memory_turn(serde_json::json!({
        "cwd": ".", "agent": "opencode", "kind": "usage",
        "session_id": "ses_main", "msg_id": "msg_1", "model": "qwen3-coder",
        "in_tok": 100, "out_tok": 40, "cache_read": 20, "cache_make": 5,
    }))
    .expect("well-formed body yields an event");
    assert_eq!(target, "ses_main");
    match &event {
        crate::graph::UsageEvent::Turn {
            msg_id,
            model,
            in_tok,
            out_tok,
            cache_read,
            cache_make,
            origin,
        } => {
            assert_eq!(msg_id, "msg_1");
            assert_eq!(model.as_deref(), Some("qwen3-coder"));
            assert_eq!(
                (*in_tok, *out_tok, *cache_read, *cache_make),
                (100, 40, 20, 5)
            );
            assert_eq!(origin, "session");
        }
        _ => panic!("expected a Turn event"),
    }

    // Recording it lands a real turn row (est_only clears).
    let dir = std::env::temp_dir().join(format!("cimp-usage-sess-{}", uuid::Uuid::new_v4()));
    let idx = crate::graph::GraphIndex::open(&dir, ".ckg").expect("open");
    idx.record_usage_event(&target, "opencode", &event, 100)
        .unwrap();
    let series = idx.usage_turn_series("ses_main").unwrap();
    assert_eq!(series.len(), 1);
    assert_eq!(series[0].msg_id, "msg_1");
    assert_eq!(series[0].origin, "session");
    assert_eq!(series[0].tokens.get("input"), Some(100));
    drop(idx);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn usage_body_with_parent_rolls_up_as_agent() {
    // A child (sub-agent) session's spend is attributed to the PARENT in
    // the declared sub-agent lane — mirrors the Claude sub-agent contract.
    let (target, event) = memory_turn(serde_json::json!({
        "kind": "usage", "session_id": "ses_child", "parent_session_id": "ses_parent",
        "msg_id": "msg_a", "model": "qwen3-coder",
        "in_tok": 7, "out_tok": 3, "cache_read": 0, "cache_make": 0,
    }))
    .expect("child body yields an event");
    assert_eq!(target, "ses_parent", "spend rolls up to the parent");
    match &event {
        crate::graph::UsageEvent::Turn { origin, .. } => {
            assert_eq!(origin, "agent");
        }
        _ => panic!("expected a Turn event"),
    }

    let dir = std::env::temp_dir().join(format!("cimp-usage-parent-{}", uuid::Uuid::new_v4()));
    let idx = crate::graph::GraphIndex::open(&dir, ".ckg").expect("open");
    idx.record_usage_event(&target, "opencode", &event, 100)
        .unwrap();
    // The turn lives on the parent, not the child.
    assert_eq!(idx.usage_turn_series("ses_parent").unwrap().len(), 1);
    assert!(idx.usage_turn_series("ses_child").unwrap().is_empty());
    let series = idx.usage_turn_series("ses_parent").unwrap();
    assert_eq!(series[0].origin, "agent");
    drop(idx);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn usage_body_malformed_or_empty_is_ignored() {
    // Missing msg_id → no turn.
    assert!(memory_turn(serde_json::json!({
        "kind": "usage", "session_id": "s", "in_tok": 10,
    }))
    .is_none());
    // Empty msg_id → no turn.
    assert!(memory_turn(serde_json::json!({
        "kind": "usage", "session_id": "s", "msg_id": "", "in_tok": 10,
    }))
    .is_none());
    // All-zero token totals (degenerate/creation emit) → skipped.
    assert!(memory_turn(serde_json::json!({
        "kind": "usage", "session_id": "s", "msg_id": "m",
        "in_tok": 0, "out_tok": 0, "cache_read": 0, "cache_make": 0,
    }))
    .is_none());
}

#[test]
fn usage_upsert_by_msg_id_does_not_duplicate() {
    // The plugin emits the final turn twice (spike-confirmed) — same msg_id,
    // so the second overwrites the first in place rather than appending.
    let dir = std::env::temp_dir().join(format!("cimp-usage-dup-{}", uuid::Uuid::new_v4()));
    let idx = crate::graph::GraphIndex::open(&dir, ".ckg").expect("open");
    for out in [10u64, 20u64] {
        let (target, event) = memory_turn(serde_json::json!({
            "kind": "usage", "session_id": "ses", "msg_id": "dup",
            "in_tok": 50, "out_tok": out, "cache_read": 0, "cache_make": 0,
        }))
        .expect("event");
        idx.record_usage_event(&target, "opencode", &event, 100)
            .unwrap();
    }
    let series = idx.usage_turn_series("ses").unwrap();
    assert_eq!(series.len(), 1, "duplicate msg_id upserts, not appends");
    assert_eq!(series[0].tokens.get("output"), Some(20), "last emit wins");
    drop(idx);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A harness that serves no memory ingress records nothing** (V40 Phase
/// I, issue #107 item 2).
///
/// The route's body shape belongs to whoever generated the producer. Core
/// used to declare it and read it for whatever `agent` the body asserted,
/// which meant a harness cImp taps in-process — and which has no producer
/// for this route at all — was still served out of another harness's
/// vocabulary. `None` from the plugin is the fail-closed answer locked
/// decision 2 asks for everywhere else.
#[test]
fn a_harness_with_no_memory_ingress_answers_none() {
    let body = serde_json::json!({
        "kind": "usage", "session_id": "s", "msg_id": "m", "in_tok": 10,
    })
    .to_string();
    let has = |id: &str| {
        crate::harness::HarnessId::from_id(id)
            .and_then(|h| h.plugin())
            .and_then(|p| p.memory_event(body.as_bytes()))
            .is_some()
    };
    assert!(has("opencode"), "the generated plugin's own route");
    assert!(
        !has("claude"),
        "this harness's tool and usage events come from the transcript tap; it posts nothing \\
             here, so serving it out of another harness's body shape would be a guess"
    );
}

#[test]
fn discovery_round_trips() {
    let d = Discovery {
        port: 8123,
        token: "tok".into(),
        pid: 42,
        root: "P:\\proj".into(),
    };
    let s = serde_json::to_string(&d).unwrap();
    let back: Discovery = serde_json::from_str(&s).unwrap();
    assert_eq!(back.port, 8123);
    assert_eq!(back.token, "tok");
    assert_eq!(back.pid, 42);
    // Legacy files (pre-root) still parse: `root` defaults empty.
    let legacy: Discovery = serde_json::from_str(r#"{"port":1,"token":"t","pid":9}"#).unwrap();
    assert_eq!(legacy.root, "");
}

fn disc(pid: u32, port: u16, root: &str) -> Discovery {
    Discovery {
        port,
        token: format!("tok{pid}"),
        pid,
        root: root.to_string(),
    }
}

/// A project root or child cwd, spelled the way the host platform spells
/// one: `P:\<tail>` on Windows — byte-identical to the literals these
/// fixtures used to hard-code — and `/<tail>` everywhere else.
///
/// **Why this cannot be a hard-coded literal.** Discovery routing is
/// component-wise ([`is_ancestor_or_equal`]), and `Path::new(r"P:\proj\b")`
/// is a SINGLE `Component::Normal` on Linux. Every root/hint pair below
/// would therefore stop matching, step 1 of [`select_answering`] would rank
/// nothing, and the tests would either panic on `.expect("match")` or —
/// worse — pass through the sole-entry fallback while asserting nothing
/// about the ranking they were written for. The properties being pinned
/// here (F-11, F-26, F-28, decision 30's probe budget) are not
/// Windows-specific, so their coverage must not be either.
fn proj(tail: &str) -> String {
    if cfg!(windows) {
        format!(r"P:\{}", tail.replace('/', "\\"))
    } else {
        format!("/{tail}")
    }
}

/// [`proj`] as a `PathBuf`, for the cwd/hint side of the same pairs.
fn proj_path(tail: &str) -> PathBuf {
    PathBuf::from(proj(tail))
}

/// The preference ORDER on its own: every candidate answers and there is no
/// legacy store.
///
/// These four cases predate locked decision 30 and used to call
/// `select_discovery`, which now probes real sockets and reads the real
/// `.cimp-offload.json` next to the test binary. Stubbing liveness all-live
/// is what keeps them a statement about ORDERING — the property they were
/// written for — rather than about the machine they run on.
fn select_all_live(entries: Vec<Discovery>, hint: Option<&Path>) -> Option<Discovery> {
    select_verified(entries, hint, |_| true, || None)
}

#[test]
fn select_discovery_routes_by_root() {
    // Two instances off one install: a child whose cwd is inside project
    // B must reach B's instance, never last-writer-wins.
    let entries = vec![
        disc(1, 1001, &proj("proj/a")),
        disc(2, 1002, &proj("proj/b")),
    ];
    let picked = select_all_live(entries, Some(&proj_path("proj/b/src"))).expect("match");
    assert_eq!(picked.pid, 2);
}

#[test]
fn select_discovery_deepest_matching_root_wins() {
    // Nested checkouts: the closest (deepest) serving instance wins.
    let entries = vec![
        disc(1, 1001, &proj("proj")),
        disc(2, 1002, &proj("proj/nested")),
    ];
    let picked = select_all_live(entries, Some(&proj_path("proj/nested/src"))).expect("match");
    assert_eq!(picked.pid, 2);
    // A hint outside the nested root resolves to the outer instance.
    let entries = vec![
        disc(1, 1001, &proj("proj")),
        disc(2, 1002, &proj("proj/nested")),
    ];
    let picked = select_all_live(entries, Some(&proj_path("proj/other"))).expect("match");
    assert_eq!(picked.pid, 1);
}

#[cfg(windows)]
#[test]
fn select_discovery_is_case_insensitive_on_windows() {
    let entries = vec![disc(1, 1001, "p:\\PROJ\\A")];
    let picked =
        select_all_live(entries, Some(Path::new("P:\\proj\\a\\deep"))).expect("match");
    assert_eq!(picked.pid, 1);
}

#[test]
fn select_discovery_sole_entry_wins_without_a_root_match() {
    // One running instance is unambiguous even when the hint doesn't
    // land inside its root (e.g. an agent launched outside any project).
    let entries = vec![disc(7, 1007, "P:\\elsewhere")];
    let picked = select_all_live(entries, Some(Path::new("Q:\\other"))).expect("sole entry");
    assert_eq!(picked.pid, 7);
}

/// #48 F-26 — the repro two `graph/mcp.rs` comments used to document, pinned
/// as a test because the wrong version of that sentence produced a false PASS
/// in live verification (the tester truncated one file, saw a served call, and
/// recorded "no fallback reachable").
///
/// A truncated per-instance entry is dropped by `read_all_discoveries`'s
/// `filter_map(… .ok())`, so selection sees it not at all — and that is
/// exactly step 3's cue: the legacy `.cimp-offload.json` still resolves, the
/// app is still reached, and nothing goes headless. `ProxyMiss::NoInstance`
/// needs BOTH stores unusable.
#[test]
fn a_corrupt_per_instance_entry_still_resolves_through_the_legacy_file() {
    let hint = proj_path("proj/src");
    let legacy = disc(99, 4444, "");
    // The corrupted `<pid>.json` is simply absent from the entry list.
    let picked = select_verified(vec![], Some(&hint), |_| true, || Some(legacy.clone()))
        .expect("the legacy store still resolves");
    assert_eq!(picked.pid, 99, "step 3 was not consulted");
    assert_eq!(picked.port, 4444);
    // And only when the first two preferences produce nothing: a matching
    // per-instance entry must never be overridden by the legacy file.
    let picked = select_verified(
        vec![disc(1, 1001, &proj("proj"))],
        Some(&hint),
        |_| true,
        || panic!("the legacy store must not be read when a per-instance entry matches"),
    )
    .expect("match");
    assert_eq!(picked.pid, 1);
    // Decision 30 added a second condition to step 3 that F-26's original
    // wording did not have: the legacy entry must ANSWER too. A legacy file
    // naming a dead endpoint is not a resolution — it is the last candidate,
    // and `NoInstance` is the honest answer once it fails.
    assert!(
        select_verified(vec![], Some(&hint), |_| false, || Some(legacy.clone())).is_none(),
        "a dead legacy entry must not resolve (#48 F-11)"
    );
}

/// The single-write trigger F-26 named, **re-pointed by locked decision 30**.
///
/// **This test changed meaning.** It was written as
/// `a_deeper_well_formed_entry_outranks_the_running_instance` and it pinned
/// F-11's DEFECT on purpose — a deeper well-formed entry outranked the real
/// instance *whatever its port*, so ONE `Write` steered a child onto a dead
/// endpoint (and chose `ProxyMiss::Transport` as the reason the system would
/// report). Its author left the defect pinned so that a green suite could not
/// be mistaken for F-11 being closed. Decision 30 closed it, so the assertion
/// is now the post-fix invariant, and the name says which half is which:
///
/// * a deeper entry that **answers** legitimately still wins — the
///   deepest-root preference is deliberately kept (dropping it reintroduces
///   "project A's child talks to project B's app");
/// * a deeper entry that does **not** answer no longer wins, and the real
///   instance below it does.
///
/// What it still does NOT claim: that a planted entry cannot win. An attacker
/// who binds the port they wrote answers the probe. Decision 30's accepted
/// bound is "one write plus a listener", and this test is the pin on the
/// *write-only* half — see [`responds`].
#[test]
fn a_deeper_entry_outranks_the_running_instance_only_while_it_answers() {
    let real = disc(10, 4000, &proj("proj"));
    let planted = disc(11, 1, &proj("proj/sub"));
    let hint = proj_path("proj/sub/deeper");
    let no_legacy = || panic!("a matching per-instance entry answers");

    // Half 1 — a deeper entry that answers is still preferred. Depth, not
    // liveness, is what ranks; liveness only filters.
    let picked = select_verified(
        vec![real.clone(), planted.clone()],
        Some(&hint),
        |_| true,
        no_legacy,
    )
    .expect("a matching entry exists");
    assert_eq!(picked.pid, 11, "the deepest LIVE entry must still win");

    // Half 2 — the finding itself: the deeper entry is dead, so the running
    // instance underneath it serves the call instead of the child going
    // headless. `dead` is keyed on the port, which is the one thing the
    // planted file cannot fake without a listener.
    let picked = select_verified(
        vec![real, planted],
        Some(&hint),
        |d| d.port != 1,
        no_legacy,
    )
    .expect("the shallower live instance resolves");
    assert_eq!(picked.pid, 10, "a dead deeper entry must not win (#48 F-11)");
    assert_eq!(picked.port, 4000);
}

/// #48 F-28 — one `Write` no longer disarms decision 14's native-web sensor.
///
/// `taint_beacon::dispatch` resolves its endpoint with `read_discovery_for`
/// and is **fail-open by design**: no endpoint means no beacon, silently, so a
/// `WebFetch` stops contaminating the tab. That fail-open is correct and is
/// deliberately unchanged — the defect was that the resolution it failed open
/// *from* was steerable by one file write.
///
/// Driven through the beacon's own resolution shape (its cwd is the project
/// directory Claude spawned it in) against a REAL socket for the live
/// instance, so the probe itself — not a stubbed closure — is what rejects the
/// planted entry. F-28 keeps its own live-verification row; this is the unit
/// pin, not a substitute for it.
#[test]
fn a_planted_dead_entry_no_longer_disarms_the_native_web_beacon() {
    let live = fake_instance("tok-live");
    // The planted file: well-formed, a DEEPER root than the running
    // instance's, and a port nothing is listening on.
    let planted = disc(4242, dead_port(), &proj("proj/sub"));
    let real = Discovery {
        port: live,
        token: "tok-live".into(),
        pid: 10,
        root: proj("proj"),
    };
    let cwd = proj_path("proj/sub/pkg");

    let picked = select_verified(vec![real, planted], Some(&cwd), responds, || None)
        .expect("the beacon still finds the running instance");
    assert_eq!(
        picked.port, live,
        "the beacon must reach the live instance, not the planted endpoint"
    );
    assert_eq!(picked.token, "tok-live");
}

/// The probe is an authentication check as well as a liveness check: a socket
/// that answers but does not recognize the entry's token is somebody else's
/// process, and accepting it would let a planted file borrow a real port.
#[test]
fn the_probe_accepts_only_an_endpoint_that_honours_this_entrys_token() {
    let port = fake_instance("tok-right");
    let right = Discovery {
        port,
        token: "tok-right".into(),
        pid: 1,
        root: String::new(),
    };
    let wrong = Discovery {
        token: "tok-wrong".into(),
        ..right.clone()
    };
    assert!(responds(&right), "the real token must answer 200");
    assert!(!responds(&wrong), "a 401 is not an answer");
    assert!(
        !responds(&Discovery {
            port: dead_port(),
            ..right
        }),
        "a dead port is not an answer"
    );
}

/// The latency bound is a property, not a comment: a resolution probes at
/// most [`MAX_DISCOVERY_PROBES`] candidates however many entries exist, so the
/// worst case a hook shim can be made to pay is bounded by the constant and
/// not by how many files an attacker wrote.
#[test]
fn a_resolution_never_probes_more_than_its_budget() {
    let hint = proj_path("proj/a/b/c/d/e/f/g/h");
    // Twenty matching entries, each deeper than the last, none answering.
    let entries: Vec<Discovery> = (0..20)
        .map(|i| disc(i, 1000 + i as u16, &proj("proj")))
        .collect();
    let probes = std::cell::Cell::new(0usize);
    let picked = select_verified(
        entries,
        Some(&hint),
        |_| {
            probes.set(probes.get() + 1);
            false
        },
        || Some(disc(99, 4444, "")),
    );
    assert!(picked.is_none(), "nothing answered, so nothing resolves");
    assert!(
        probes.get() <= MAX_DISCOVERY_PROBES,
        "probed {} candidates, budget is {MAX_DISCOVERY_PROBES}",
        probes.get()
    );
    // And the ceiling is a non-answer, never a free pass: exhausting the
    // budget must not let an UNVERIFIED entry through.
    assert!(probes.get() > 0, "the budget must actually be spent");
}

/// A sole per-instance entry that is dead now falls through to the legacy
/// file. New with decision 30 and deliberate: a hard-killed instance leaves
/// its `<pid>.json` behind (removal is graceful-exit only), so "the sole
/// surviving entry" can be a corpse while `.cimp-offload.json` names a live
/// instance. Previously that child went headless.
#[test]
fn a_dead_sole_entry_falls_through_to_the_legacy_store() {
    let hint = PathBuf::from("Q:\\other");
    let picked = select_verified(
        vec![disc(7, 1007, "P:\\elsewhere")],
        Some(&hint),
        |d| d.pid == 99,
        || Some(disc(99, 4444, "")),
    )
    .expect("the legacy store names a live instance");
    assert_eq!(picked.pid, 99);
}

// ── #48 F-32 / locked decision 37 — the child→app discovery report ───────

/// A ledger local to one test. The production one is process-global and the
/// suite runs concurrently, so a test that used it would be racing its
/// neighbours on the shared `(no tab identity)` bucket — which is exactly
/// the bucket the key-space property is about.
fn test_ledger() -> HashMap<String, outbound::Doubling> {
    HashMap::new()
}

/// The facts a configured tab resolves to, stood in for. The `Cell` records
/// whether the closure ran at all: an identity-less report must never reach
/// the app-side resolvers, because there is no tab to resolve.
fn facts_probe(
    called: &std::cell::Cell<bool>,
) -> impl FnOnce(&str, &'static str) -> TabFacts + '_ {
    move |tab, _agent| {
        called.set(true);
        TabFacts {
            root: format!("P:\\proj\\{tab}"),
            session: Some("sess-f32".to_string()),
        }
    }
}

fn skipped_body(tab: Option<&str>, skipped: u32) -> DiscoverySkippedBody {
    DiscoverySkippedBody {
        tab: tab.map(str::to_string),
        consumer: None,
        skipped,
    }
}

/// **Decision 37's bar, clauses (1), (2) and (5).** A token-holder can cause
/// a row; it cannot make that row name a tab that is not configured, claim
/// `Headless` when the truth is unknown, carry a root or a session it chose,
/// state a count no genuine child could produce, or move anything.
///
/// Asserted **through** `record_discovery_skipped` with `test_rows`
/// observing the row the producer actually wrote — not by calling
/// `tab_identity` beside it and comparing to itself, which is the shape that
/// let three findings survive their fixes here. Deleting the `tab_identity`
/// call from the producer fails this test.
#[test]
fn a_forged_discovery_report_cannot_claim_a_tab_or_choose_what_the_row_says() {
    use crate::activity::Attribution;
    let s = settings_with_tabs(&["f32-real"]);
    let mut ledger = test_ledger();
    let row_for = |ledger: &mut HashMap<String, outbound::Doubling>,
                   body: &DiscoverySkippedBody|
     -> Option<crate::activity::ActivityRecord> {
        outbound::test_rows::reset();
        let called = std::cell::Cell::new(false);
        record_discovery_skipped(&s, body, |k| claim_in(ledger, k), facts_probe(&called));
        let mut rows = outbound::test_rows::drain();
        assert!(rows.len() <= 1, "one report, at most one row");
        let row = rows.pop();
        // The app-side resolvers run for a CONFIGURED tab and for nothing
        // else: there is no tab to resolve a root or a session for.
        if let Some(r) = &row {
            assert_eq!(
                called.get(),
                matches!(&r.entry.tab, Attribution::Tab(_)),
                "the app-side facts were resolved for a non-tab (or not for a tab)"
            );
        }
        row
    };

    // A configured id is the only one that becomes a tab — and its root and
    // session come from the app, not from the wire (there is no wire field
    // for either).
    let real = row_for(&mut ledger, &skipped_body(Some("f32-real"), 1)).expect("a row");
    assert_eq!(
        real.entry.tab,
        Attribution::Tab("f32-real".to_string())
    );
    assert_eq!(real.entry.root, "P:\\proj\\f32-real");
    assert_eq!(real.entry.session.as_deref(), Some("sess-f32"));
    assert_eq!(real.entry.source, "discovery_skipped");
    assert_eq!(real.entry.tool, "discovery");
    assert!(
        real.entry.ok,
        "containment WORKED — a denial-shaped row would say cImp blocked the child"
    );
    assert!(
        real.request.contains("\"origin\": \"http\""),
        "a local process asserted this, and the row must say so: {}",
        real.request
    );

    // An id naming no configured tab is `Unrecognized`, never `Tab` and
    // never `Headless`, and it carries no root and no session.
    let forged = row_for(&mut ledger, &skipped_body(Some("f32-not-a-tab"), 1)).expect("a row");
    assert_eq!(
        forged.entry.tab,
        Attribution::Unrecognized("f32-not-a-tab".to_string())
    );
    assert!(
        forged.entry.root.is_empty() && forged.entry.session.is_none(),
        "a forged id must not be able to file a security row under a project"
    );

    // No id at all is `Unattributed` — "this writer does not know" — and
    // explicitly NOT `Headless`, which is the positive claim "a worker run
    // with no tab behind it". That collapse is F-20's defect and F-29's; a
    // body-supplied tab is indistinguishable from an invented one, so this
    // frame cannot make the positive claim.
    let anon = row_for(&mut ledger, &skipped_body(None, 1)).expect("a row");
    assert_eq!(anon.entry.tab, Attribution::Unattributed);
    assert_ne!(anon.entry.tab, Attribution::Headless);

    // An unbounded invented id cannot choose how many bytes of a capped feed
    // one report occupies — bounded AFTER classification, so truncation can
    // never fold a long id onto a configured one.
    // (Its own ledger: the sentinel bucket is shared — which is the point of
    // the key-space assertion below — so in the main one this report would
    // land between powers of two and be correctly suppressed.)
    let long = "x".repeat(4096);
    let big = row_for(&mut test_ledger(), &skipped_body(Some(&long), 1)).expect("a row");
    let Attribution::Unrecognized(id) = &big.entry.tab else {
        panic!("a 4096-char id is not a tab: {:?}", big.entry.tab);
    };
    assert!(id.chars().count() <= BEACON_TOOL_MAX + 1, "{}", id.len());
    assert!(
        !is_configured_tab(&s, "claude", id),
        "truncation is not a forgery"
    );

    // The count is caller-asserted and the row says so, clamped to the probe
    // budget a genuine resolution cannot exceed.
    let huge = row_for(&mut ledger, &skipped_body(Some("f32-real"), 9999)).expect("a row");
    assert!(huge.response.contains("skipped 6 candidate"), "{}", huge.response);
    assert!(huge.response.contains("clamped"), "{}", huge.response);
    assert!(
        huge.response.contains("CALLER-ASSERTED"),
        "the honesty clause is not optional: {}",
        huge.response
    );

    // A report of zero skips is not a report — and it is what a malformed or
    // empty body degrades to, so neither writes anything.
    assert!(row_for(&mut ledger, &skipped_body(Some("f32-real"), 0)).is_none());
    for raw in [
        &b"{ not json"[..],
        &b""[..],
        &b"null"[..],
        &br#"{"skipped":"lots"}"#[..],
    ] {
        let body: DiscoverySkippedBody = serde_json::from_slice(raw).unwrap_or_default();
        assert_eq!(bounded_skips(body.skipped), None, "{raw:?}");
    }

    // The whole exchange keyed exactly two ledger buckets: the one
    // configured tab, plus ONE sentinel shared by every identity-less report
    // — the anonymous one and the invented-id one landed in the same bucket.
    assert_eq!(
        ledger.len(),
        2,
        "the key space is the user's tab list plus a sentinel: {:?}",
        ledger.keys().collect::<Vec<_>>()
    );
}

/// **Decision 37's bar, clause (6): the response is not the signal.**
///
/// Two halves, and each is load-bearing on its own. The reply is a single
/// constant reached by the handler's only exit — so a prober learns nothing
/// from bad JSON, an unknown tab, an anonymous tab, `skipped: 0`,
/// `skipped: 9999`, a row written or a row suppressed — **while the rows
/// those inputs produce differ**, which is what stops the property from
/// being satisfied trivially by a handler that does nothing at all.
#[test]
fn the_discovery_report_answers_identically_on_every_path() {
    // Half 1: the bytes. One constant, one exit, no branch before it.
    assert_eq!(DISCOVERY_ACK, br#"{"ok":true}"#);
    let body = handler_body("handle_discovery_skipped");
    assert_eq!(
        body.matches("write_").count(),
        1,
        "a second writer is a second response shape: {body}"
    );
    assert!(body.contains("DISCOVERY_ACK"), "{body}");
    for forbidden in ["write_json", "400", "return ", "?;", "if ", "match "] {
        assert!(
            !body.contains(forbidden),
            "`{forbidden}` in this handler is a path the reply could diverge on: {body}"
        );
    }

    // Half 2: the rows DO differ, so the constant reply is hiding something.
    let s = settings_with_tabs(&["f32-t2"]);
    let mut ledger = test_ledger();
    let wrote = |ledger: &mut HashMap<String, outbound::Doubling>, b: &DiscoverySkippedBody| {
        outbound::test_rows::reset();
        let called = std::cell::Cell::new(false);
        record_discovery_skipped(&s, b, |k| claim_in(ledger, k), facts_probe(&called));
        !outbound::test_rows::drain().is_empty()
    };
    // Reports 1 and 2 for this scope write; report 3 is folded into the
    // next power of two. Same reply, different store.
    assert!(wrote(&mut ledger, &skipped_body(Some("f32-t2"), 1)));
    assert!(wrote(&mut ledger, &skipped_body(Some("f32-t2"), 1)));
    assert!(
        !wrote(&mut ledger, &skipped_body(Some("f32-t2"), 1)),
        "the third report is suppressed — and answers identically"
    );
    assert!(!wrote(&mut ledger, &skipped_body(Some("f32-t2"), 0)));
    outbound::test_rows::reset();
}

/// **Decision 37's bar, clauses (3) and (4): a flood costs `log2(n)` rows in
/// its own lane and none anywhere else.**
///
/// Three halves. The doubling itself, asserted on the `suppressed` counts
/// and not merely on how many rows appear — a plain global cap would also
/// produce "a small number". The key space, which is the assertion
/// `/activity/contract_drift` has no equivalent of and the one that would
/// have caught F-37. And the lane, which comes free with the `Screen`
/// variant and is proved for every screen by
/// `activity::tests::no_screen_can_evict_another_screens_rows`.
#[test]
fn a_flood_of_discovery_reports_costs_log2_rows_and_evicts_nothing() {
    // Half 1 — the ledger. 200 reports on one key write 8 rows, at 1, 2, 4,
    // 8, 16, 32, 64, 128, and each names how many it stands for.
    let mut ledger = test_ledger();
    let mut written: Vec<(u32, u32)> = Vec::new();
    for _ in 0..200 {
        if let outbound::DoublingRow::Write { total, suppressed } =
            claim_in(&mut ledger, "claude:one-scope")
        {
            written.push((total, suppressed));
        }
    }
    assert_eq!(
        written,
        vec![
            (1, 0),
            (2, 0),
            (4, 1),
            (8, 3),
            (16, 7),
            (32, 15),
            (64, 31),
            (128, 63)
        ],
        "the magnitude of a loop must survive in the window, not be inferred \
         from the absence of rows"
    );

    // Half 2 — the key space. Ten thousand DISTINCT invented tab ids get ONE
    // bucket and log2-many rows, because the key is the identity the app
    // resolved and not the string the caller typed.
    outbound::test_rows::reset();
    let s = settings_with_tabs(&["f32-t3"]);
    let mut invented = test_ledger();
    for i in 0..10_000u32 {
        let called = std::cell::Cell::new(false);
        record_discovery_skipped(
            &s,
            &skipped_body(Some(&format!("invented-{i}")), 1),
            |k| claim_in(&mut invented, k),
            facts_probe(&called),
        );
    }
    assert_eq!(
        invented.len(),
        1,
        "ten thousand invented ids must not buy ten thousand counters: {:?}",
        invented.keys().collect::<Vec<_>>()
    );
    let rows = outbound::test_rows::drain();
    assert_eq!(
        rows.len(),
        14,
        "10 000 reports must cost log2 rows, not 10 000"
    );
    assert!(rows.iter().all(|r| r.entry.source == "discovery_skipped"));

    // Half 3 — the lane. Declaring the variant is what buys it; the
    // every-screen-against-every-screen matrix in `activity` covers this one
    // as both flooder and victim with no edit to that test, which is the
    // property being relied on here.
    assert!(
        outbound::Screen::ALL.contains(&outbound::Screen::DiscoverySkipped),
        "a screen missing from ALL shares the catch-all lane instead of \
         getting its own guaranteed window"
    );
}

// ── #48 F-37 / locked decision 42 — the contract-drift ledger ────────────

fn drift_ledger() -> HashMap<&'static str, outbound::Doubling> {
    HashMap::new()
}

fn drift_body(shim: &str, missing: &[&str], session: Option<&str>) -> ContractDriftBody {
    ContractDriftBody {
        shim: shim.to_string(),
        missing: missing.iter().map(|m| (*m).to_string()).collect(),
        session_id: session.map(str::to_string),
    }
}

/// **F-37's whole point: the KEY SPACE, not the map's size.**
///
/// The old ledger was a `HashSet<(shim, session_id)>` with both halves off
/// the wire, so a token-holder could mint unlimited entries and evict the
/// 400-row graph lane — genuine security rows included. The bar is the one
/// `/activity/discovery_skipped` already meets: key on something the caller
/// does not control.
///
/// Three halves. The doubling, asserted on the `suppressed` counts rather
/// than on how many rows appear (a plain global cap would also produce "a
/// small number"). The key space, asserted as **membership of a compile-time
/// list** and not as `len() < something` — an implementation that merely
/// evicted or cleared a caller-keyed map when it got big would still hold
/// caller strings, and would pass a size assertion. And the total ceiling,
/// which is what "bounded" means here: five shims plus one sentinel, for
/// every possible input, forever.
///
/// **What this would still pass if the implementation were wrong:** it does
/// not check the row's *contents* (that is the next test), and it does not
/// check that the sentinel is shared by the *right* strings — a classifier
/// that sent every name including the real ones to the sentinel would pass
/// halves 1 and 2 and fail half 3.
#[test]
fn a_flood_of_contract_drift_reports_keys_a_fixed_list_and_costs_log2_rows() {
    // Half 1 — the doubling. 200 reports on one shim write 8 rows, at 1, 2,
    // 4 … 128, each naming how many it stands for.
    let mut ledger = drift_ledger();
    let mut written: Vec<(u32, u32)> = Vec::new();
    for _ in 0..200 {
        if let outbound::DoublingRow::Write { total, suppressed } =
            drift_claim_in(&mut ledger, "read_hook")
        {
            written.push((total, suppressed));
        }
    }
    assert_eq!(
        written,
        vec![
            (1, 0),
            (2, 0),
            (4, 1),
            (8, 3),
            (16, 7),
            (32, 15),
            (64, 31),
            (128, 63)
        ],
        "the magnitude of a flood must survive in the window, not be inferred \
         from the absence of rows"
    );

    // Half 2 — the key space. Ten thousand invented shims, each with its own
    // invented session (the old key's second half), get ONE bucket and
    // log2-many rows, because the key is a classification and not a string
    // the caller typed.
    let mut invented = drift_ledger();
    let mut rows = 0;
    for i in 0..10_000u32 {
        let body = drift_body(
            &format!("invented-{i}"),
            &["session_id"],
            Some(&format!("sess-{i}")),
        );
        if contract_drift_row(&body, |k| drift_claim_in(&mut invented, k)).is_some() {
            rows += 1;
        }
    }
    assert_eq!(
        invented.len(),
        1,
        "ten thousand invented names must not buy ten thousand counters: {:?}",
        invented.keys().collect::<Vec<_>>()
    );
    assert_eq!(invented.keys().copied().collect::<Vec<_>>(), [DRIFT_SHIM_UNKNOWN]);
    assert_eq!(rows, 14, "10 000 reports must cost log2 rows, not 10 000");

    // Half 3 — the ceiling. Every real shim keeps its own counter; everything
    // else shares one; and no input of any kind can key anything else,
    // because the key type is `&'static str` from `DRIFT_SHIMS`.
    let mut all = drift_ledger();
    for shim in crate::harness::ingress::drift_tokens() {
        let body = drift_body(shim, &["cwd"], None);
        assert!(contract_drift_row(&body, |k| drift_claim_in(&mut all, k)).is_some());
    }
    for junk in ["", "   ", "read_hook ", "READ_HOOK", "read", "read_hook2", "🙂"] {
        let body = drift_body(junk, &["cwd"], Some("s"));
        let _ = contract_drift_row(&body, |k| drift_claim_in(&mut all, k));
    }
    assert_eq!(
        all.len(),
        crate::harness::ingress::drift_tokens().len() + 1,
        "the key space is the shim list plus one sentinel: {:?}",
        all.keys().collect::<Vec<_>>()
    );
    for key in all.keys() {
        assert!(
            crate::harness::ingress::drift_tokens().contains(key)
                || *key == DRIFT_SHIM_UNKNOWN,
            "a caller-supplied string reached the ledger's key space: {key}"
        );
    }
    // Trimming is the one normalisation, and it is not a prefix rule:
    // an invented name that merely starts with a real one is the sentinel.
    assert_eq!(drift_shim_key("  read_hook  "), "read_hook");
    assert_eq!(drift_shim_key("read_hook-forged"), DRIFT_SHIM_UNKNOWN);
    assert_eq!(
        drift_shim_key(&format!("read_hook{}", "x".repeat(5_000))),
        DRIFT_SHIM_UNKNOWN,
        "classification must see the whole string, never a truncation of it"
    );
}

/// **The string half of the same class: what one drift report may put IN the
/// row.**
///
/// `ActivityStore::record` truncates `request` and `response` and **not**
/// `target` — and `target` is what `ipc::commands::advisor_signals` copies
/// verbatim into a user-facing signal. So the shim name, the session id and
/// the whole `missing` list reached a capped ring at whatever length a caller
/// chose. Bounded here, after classification, exactly like F-39's id.
///
/// **What this would still pass if the implementation were wrong:** it says
/// nothing about *control characters* in those strings — that is Phase D's
/// concern at the surfaces that render, and `bounded_id`'s doc says so. It
/// would also pass a bound applied before classification, which is why the
/// key-space test above asserts the ordering separately.
#[test]
fn a_forged_contract_drift_report_cannot_choose_how_many_bytes_a_row_costs() {
    // The honest case first: byte-identical to the plain `join(", ")` this
    // replaced, so the bound costs a real report nothing.
    let mut ledger = drift_ledger();
    let real = contract_drift_row(
        &drift_body("read_hook", &["session_id", "cwd"], Some("sess-1")),
        |k| drift_claim_in(&mut ledger, k),
    )
    .expect("the first report from a shim always writes");
    assert_eq!(real.entry.target, "read_hook: session_id, cwd");
    assert_eq!(real.entry.source, "harness");
    assert_eq!(real.entry.tool, "contract_drift");
    assert_eq!(real.entry.session.as_deref(), Some("sess-1"));
    assert_eq!(real.entry.tab, crate::activity::Attribution::Unattributed);
    assert!(!real.entry.ok, "a drift report is never `ok`");
    assert!(
        real.request.contains("report 1 from this shim this app run, 0 folded into it"),
        "a folded report must be countable from the row that stands for it: {}",
        real.request
    );

    // The forged case: every caller-supplied string bounded, and the row
    // still filed under the sentinel rather than under `read_hook`.
    let huge_missing: Vec<String> = (0..5_000).map(|i| format!("{}{i}", "f".repeat(4096))).collect();
    let borrowed: Vec<&str> = huge_missing.iter().map(String::as_str).collect();
    let long = "x".repeat(4096);
    let mut forged_ledger = drift_ledger();
    let forged = contract_drift_row(
        &drift_body(&long, &borrowed, Some(&long)),
        |k| drift_claim_in(&mut forged_ledger, k),
    )
    .expect("a first report writes");
    assert_eq!(
        forged_ledger.keys().copied().collect::<Vec<_>>(),
        [DRIFT_SHIM_UNKNOWN]
    );
    // `shim: ` + at most MAX_DRIFT_MISSING bounded names + the overflow note.
    let ceiling = (BEACON_TOOL_MAX + 1) * (MAX_DRIFT_MISSING + 1) + 64;
    assert!(
        forged.entry.target.chars().count() <= ceiling,
        "{} chars reached a row the store does not truncate",
        forged.entry.target.chars().count()
    );
    assert!(
        forged.entry.session.as_deref().unwrap().chars().count() <= BEACON_TOOL_MAX + 1,
        "the session column is a join key, not a payload"
    );
    assert!(
        forged.entry.target.contains("(+4988 more)"),
        "a cut list must say how much was cut: {}",
        forged.entry.target
    );

    // A shim that sends nothing but empty strings still produces a row that
    // reads honestly — "empty" must not be spelled the same way as a name.
    let mut empty_ledger = drift_ledger();
    let empty = contract_drift_row(&drift_body("", &[], None), |k| {
        drift_claim_in(&mut empty_ledger, k)
    })
    .expect("a first report writes");
    assert_eq!(empty.entry.target, ": ");
    assert_eq!(empty.entry.session.as_deref(), Some(""));
}

// ── V35 Phase I — CHP: the hello row and the observation seam ────────────

/// A hello writes ONE row, says what the artifact declared, and a
/// flip-flopping caller costs `log2(n)` rows rather than one per hello.
///
/// The row shape matters as much as the bound: the target has to name the
/// tab, the protocol version and the sizes of the two declaration lists,
/// because that is what a reader diffing "before the upgrade / after the
/// upgrade" actually compares.
#[test]
fn a_hello_row_names_the_version_and_costs_log2_rows_under_a_flood() {
    let mut ledger: HashMap<String, outbound::Doubling> = HashMap::new();
    let serves = vec!["prompt".to_string(), "tool.gate".to_string()];
    let first = hello_row(
        "opencode",
        "opencode-1",
        crate::harness::chp::CHP_VERSION,
        "1.18.13",
        &serves,
        1,
        |k| claim_in(&mut ledger, k),
    )
    .expect("the first hello writes a row");
    assert_eq!(first.entry.source, "harness");
    assert_eq!(first.entry.tool, "chp_hello");
    assert_eq!(
        first.entry.kind,
        crate::activity::ActivityKind::Graph.as_str(),
        "the hello rides the lane `contract_drift` already uses for harness facts"
    );
    assert!(first.entry.ok, "a hello is a healthy event, not a flag");
    assert_eq!(
        first.entry.tab,
        crate::activity::Attribution::Tab("opencode-1".to_string()),
        "the tab was validated against the configured list before this point"
    );
    assert!(
        first
            .entry
            .target
            .contains(&format!("chp {}", crate::harness::chp::CHP_VERSION)),
        "{}",
        first.entry.target
    );
    assert!(first.entry.target.contains("v1.18.13"), "{}", first.entry.target);
    assert!(first.entry.target.contains("serves 2"), "{}", first.entry.target);
    assert!(first.entry.target.contains("cannot 1"), "{}", first.entry.target);
    assert!(first.request.contains("tool.gate"));
    // An undeclared version says so rather than rendering as an empty `v`.
    let quiet = hello_row("claude", "claude-1", 0, "", &[], 0, |k| {
        claim_in(&mut ledger, k)
    })
    .expect("a second key gets its own counter");
    assert!(
        quiet.entry.target.contains("version not declared"),
        "{}",
        quiet.entry.target
    );
    assert!(quiet.request.contains("(nothing declared)"));

    // The bound: 200 hellos from ONE tab write 8 rows, each stating how many
    // it stands for. The key is `agent:tab` and the tab is only ever reached
    // after `is_configured_tab` accepted it, so the key space is the user's
    // own tab list.
    let mut flood: HashMap<String, outbound::Doubling> = HashMap::new();
    let rows = (0..200)
        .filter(|_| {
            hello_row("opencode", "opencode-1", 1, "", &[], 0, |k| {
                claim_in(&mut flood, k)
            })
            .is_some()
        })
        .count();
    assert_eq!(rows, 8, "a re-hellowing plugin must cost log2 rows, not 200");
    assert_eq!(flood.len(), 1, "one tab, one counter");
}

/// The declaration lists are bounded before they reach the peer registry or
/// the Settings panel — the [`bounded_missing`] discipline, one route over.
#[test]
fn a_hellos_declarations_cannot_choose_how_much_of_the_panel_they_occupy() {
    let huge: Vec<String> = (0..500).map(|i| format!("{}-{i}", "x".repeat(400))).collect();
    let bounded = bounded_declarations(&huge);
    assert_eq!(bounded.len(), MAX_HELLO_DECLARATIONS);
    assert!(
        bounded.iter().all(|s| s.chars().count() <= BEACON_TOOL_MAX + 1),
        "each entry is truncated like every other caller-supplied id"
    );
    assert!(bounded_declarations(&[]).is_empty());
}

/// The observation seam, in the two directions that matter for "zero
/// behavior change": a pre-CHP body is READ, not rejected, and a route that
/// is not CHP is not observed at all.
///
/// The handler-side half (tab validation, the settings read) needs an
/// `AppHandle` and is covered by the route-surface tests plus
/// `harness::chp`'s own suite; what is asserted here is that the loopback's
/// pre-dispatch hook agrees with the protocol module about *what counts as a
/// CHP message*, which is the seam a new route would silently fall out of.
#[test]
fn the_chp_observation_reads_every_body_the_routes_actually_send() {
    // Exactly the body a pre-Phase-J `--context-hook` shim still posts — no `chp`.
    let (env, tab) = crate::harness::chp::envelope(
        "/context/retrieve",
        br#"{"cwd":"P:\\p","prompt":"hi","session_id":"s","agent":"claude","tab":"claude-1"}"#,
    )
    .expect("the pre-CHP Claude body is still observable");
    assert_eq!(env.chp, None);
    assert_eq!(crate::graph::source_for_consumer(env.agent_token()), "claude");
    assert_eq!(tab, "claude-1");

    // …and the body the generated plugin now sends. The literal is built
    // from `CHP_VERSION` rather than typed, because the point of this arm
    // is that the observer reads whatever the generator baked in — a
    // hard-coded number would turn every protocol bump into a red test
    // about nothing.
    let body = format!(
        "{{\"chp\":{},\"tab\":\"opencode-1\",\"consumer\":\"opencode\",\"tool\":\"webfetch\"}}",
        crate::harness::chp::CHP_VERSION
    );
    let (env, _) = crate::harness::chp::envelope("/latch/beacon", body.as_bytes())
        .expect("the plugin's beacon body");
    assert_eq!(env.chp, Some(crate::harness::chp::CHP_VERSION));
    assert_eq!(
        crate::graph::source_for_consumer(env.agent_token()),
        "opencode"
    );

    // Every route the containment table declares as CHP-carrying is one the
    // protocol module agrees to observe, and none of the non-CHP ones is.
    for row in ROUTE_CONTAINMENT {
        let observed = crate::harness::chp::is_push_route(row.path);
        let expected = matches!(
            row.path,
            "/context/retrieve"
                | "/context/compaction"
                | "/context/should_read"
                | "/context/post_edit"
                | "/memory/event"
                | "/permission/event"
                | "/latch/beacon"
                | "/latch/state"
                | "/workbench/tool_checkpoint"
                | "/activity/contract_drift"
                // V35 Phase L: the three realized read-path events. Their
                // Claude ingress twins are NOT here — a `/claude/hook/*`
                // route carries Claude's own body and its envelope rides
                // headers, which `note_chp` reads on the other branch.
                | "/session/assistant_text"
                | "/session/tool_result"
                | "/session/subagent"
                | "/session/output_started"
                | "/session/output_stopped"
                | "/session/subagents_active"
        );
        assert_eq!(
            observed, expected,
            "`{}` disagrees with `harness::chp::EVENTS` about whether it carries CHP — a new \
             route either belongs in the vocabulary (with a row in docs/CHP.md) or in this \
             list's negative half, deliberately",
            row.path
        );
    }
}

/// **The positive case, on the wire, and its negative control.** A skipped
/// planted entry reaches the app; a clean resolution says nothing.
///
/// Asserted on the BYTES a listening instance receives. `SKIPPED_CANDIDATES`
/// is deliberately not asserted anywhere here: that counter is F-11's, it
/// already works, and *the entire finding is that it has no user consumer* —
/// a test that asserts the counter re-pins the defect's shape.
#[test]
fn a_skipped_entry_is_reported_on_the_wire_and_a_clean_resolution_says_nothing() {
    let (port, seen) = recording_instance("tok-f32");
    let d = Discovery {
        port,
        token: "tok-f32".into(),
        pid: 4242,
        root: "P:\\proj".into(),
    };
    let who = ChildIdentity {
        consumer: "claude",
        tab: Some("claude-1"),
    };

    dispatch_discovery_report(&d, who, 2);
    let req = wait_for_request(&seen, 1).expect("the live instance receives the report");
    assert!(
        req.starts_with("POST /activity/discovery_skipped HTTP/1.1\r\n"),
        "{req}"
    );
    assert!(req.contains("Authorization: Bearer tok-f32\r\n"), "{req}");
    assert!(req.contains("\"skipped\":2"), "{req}");
    assert!(req.contains("\"tab\":\"claude-1\""), "{req}");
    assert!(req.contains("\"consumer\":\"claude\""), "{req}");
    // Nothing else rides along: no cwd, no path, no free text, and no
    // pid/port/root of the entries that were skipped.
    for absent in ["cwd", "root", "pid", "port", "session"] {
        assert!(!req.contains(absent), "`{absent}` on the wire: {req}");
    }

    // The count is clamped by the CHILD too, so the wire is honest rather
    // than relying on the far end to fix it.
    dispatch_discovery_report(&d, who, 9999);
    let req = wait_for_request(&seen, 2).expect("a second report");
    assert!(req.contains("\"skipped\":6"), "{req}");

    // A child with no `--tab` sends no `tab` key at all — absent, not null.
    dispatch_discovery_report(
        &d,
        ChildIdentity {
            consumer: "opencode",
            tab: None,
        },
        1,
    );
    let req = wait_for_request(&seen, 3).expect("a third report");
    assert!(!req.contains("\"tab\""), "{req}");
    assert!(req.contains("\"consumer\":\"opencode\""), "{req}");

    // **The negative control.** A resolution that skipped nothing posts
    // nothing — without this half, an implementation that reported
    // unconditionally would pass everything above.
    report_skipped_to_app(&d, who, 0);
    assert!(
        wait_for_request(&seen, 4).is_none(),
        "a clean resolution must be silent: {:?}",
        seen.lock().unwrap_or_else(PoisonError::into_inner).len()
    );

    // **Fail-open.** The endpoint is dead at report time: the dispatcher
    // returns normally, quickly, and reports nothing back to its caller —
    // its return type is `()`, so it cannot fail the child's real work.
    let dead = Discovery {
        port: dead_port(),
        ..d
    };
    let t0 = std::time::Instant::now();
    dispatch_discovery_report(&dead, who, 1);
    assert!(
        t0.elapsed() < DISCOVERY_REPORT_TIMEOUT * 4,
        "a dead endpoint must cost the stated bound at most: {:?}",
        t0.elapsed()
    );
}

/// **The hard constraint, as behaviour rather than a source scan for a call
/// that is not there.** `read_discovery_for` resolves an endpoint and sends
/// NOTHING of its own; only `proxy_base_for`, one frame up, may file the
/// skipped-candidate report.
///
/// **The callers that made this load-bearing are gone** — the two beacon
/// shims, deleted 2026-08-17, whose entire safety argument was that they
/// wrote nothing and awaited nothing on a tool call's path. The property is
/// kept rather than retired for two reasons: the split is what `proxy_base_for`
/// documents about itself, and the silent resolver is exactly what a future
/// fire-and-forget caller would reach for. A `grep` for a call that is not
/// there would stay green while a refactor moved the POST down into the
/// shared resolver; the socket assertion would not.
#[test]
fn the_discovery_report_never_reaches_the_hook_shims_path() {
    // The silent resolver's shape, against a real socket: one probe, and
    // nothing else, ever leaves this path.
    let (port, seen) = recording_instance("tok-shim");
    let live = Discovery {
        port,
        token: "tok-shim".into(),
        pid: 10,
        root: proj("proj"),
    };
    let planted = disc(4242, dead_port(), &proj("proj/sub"));
    let cwd = proj_path("proj/sub/pkg");
    let picked = select_verified(vec![live, planted], Some(&cwd), responds, || None)
        .expect("the shim still finds the running instance");
    assert_eq!(picked.port, port);
    std::thread::sleep(Duration::from_millis(250));
    let reqs = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert_eq!(reqs.len(), 1, "the resolver sent more than a probe: {reqs:?}");
    assert!(reqs[0].starts_with("GET /health "), "{}", reqs[0]);

    // …and structurally: the ONE report site sits in `proxy_base_for`, after
    // the `?` that proves an endpoint resolved. V42 R2 (#114) moved the
    // resolver to `offload/discovery.rs`; the scan follows the code.
    let src = include_str!("../discovery.rs");
    let resolver = fn_body(src, "pub fn proxy_base_for(");
    let after_q = resolver.find("let d = d?;").expect("the `?` is the guard");
    let report = resolver
        .find("report_skipped_to_app(")
        .expect("the report site");
    assert!(
        report > after_q,
        "reporting before the `?` would fire with no endpoint to report to"
    );
    assert!(
        !fn_body(src, "pub fn read_discovery_for(").contains("report_skipped_to_app"),
        "a report inside `read_discovery_for` is a write and a wait inside every \
         fire-and-forget caller of it — the shape the deleted beacon shims could \
         not survive, and the reason this resolver stays silent"
    );
    // The production ledger really is what the route claims: the process-wide
    // doubling map, not a per-call one that would bound nothing. This half is
    // about the APP side of the seam, so it reads the file the handler is in.
    // V42 R4 (#115) split the route surface again; the scan follows the code
    // by asking the whole surface which file declares the item.
    assert!(
        fn_body_in(ROUTE_SOURCES, "fn note_discovery_skipped(")
            .contains("claim_discovery_report"),
        "the handler must claim against the process ledger"
    );
}

/// A [`fake_instance`] that also **records every request it received**, so a
/// test can assert on what left a code path rather than on what that path
/// decided. Answers exactly as `fake_instance` does.
#[allow(clippy::type_complexity)]
fn recording_instance(token: &'static str) -> (u16, Arc<Mutex<Vec<String>>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut buf = [0u8; 2048];
            let n = conn.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let ok = req.contains(&format!("Authorization: Bearer {token}\r\n"));
            sink.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(req);
            let body: &[u8] = if ok { b"ok" } else { b"unauthorized" };
            let head = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                if ok { 200 } else { 401 },
                if ok { "OK" } else { "Unauthorized" },
                body.len()
            );
            let _ = conn.write_all(head.as_bytes());
            let _ = conn.write_all(body);
        }
    });
    (port, seen)
}

/// Wait up to 250 ms for the `n`th request to arrive, since the dispatcher
/// never waits for the peer. `None` ⇒ it never came, which is the assertion
/// the negative control needs.
fn wait_for_request(seen: &Arc<Mutex<Vec<String>>>, n: usize) -> Option<String> {
    for _ in 0..50 {
        if let Some(req) = seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(n - 1)
        {
            return Some(req.clone());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}


/// A port on the loopback interface that nothing is listening on: bound,
/// read, and released before the test uses the number. A `connect` to it is
/// refused immediately, which is also why a dead planted entry costs the
/// probe budget almost nothing in practice.
fn dead_port() -> u16 {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = l.local_addr().expect("addr").port();
    drop(l);
    port
}

/// A minimal stand-in for a running cImp instance: answers `GET /health`
/// with 200 when the bearer token matches and 401 when it does not, exactly
/// like `handle_conn` + `write_simple` do. Returns the port it bound.
///
/// A real socket rather than a stubbed closure because the property under
/// test is what [`responds`] puts on the wire — a stub would pass even if the
/// probe sent no `Authorization` header at all.
///
/// The accept loop runs on a **detached** thread and lives until the test
/// binary exits. Deliberate: a joinable guard would have to interrupt a thread
/// parked in `accept`, and a test that can hang waiting on its own diagnostic
/// helper is worse than one leaked thread per test.
fn fake_instance(token: &'static str) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut buf = [0u8; 512];
            let n = conn.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let ok = req.contains(&format!("Authorization: Bearer {token}\r\n"));
            let body: &[u8] = if ok { b"ok" } else { b"unauthorized" };
            let head = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                if ok { 200 } else { 401 },
                if ok { "OK" } else { "Unauthorized" },
                body.len()
            );
            let _ = conn.write_all(head.as_bytes());
            let _ = conn.write_all(body);
        }
    });
    port
}

#[test]
fn is_ancestor_or_equal_rejects_prefix_strings_and_unrelated() {
    assert!(is_ancestor_or_equal(
        &proj_path("proj/a"),
        &proj_path("proj/a")
    ));
    // Ancestry across a real component boundary — the case a hard-coded
    // `P:\proj\a` literal cannot express off Windows, where the whole
    // string is one component and this would pass vacuously.
    assert!(is_ancestor_or_equal(
        &proj_path("proj"),
        &proj_path("proj/a/deep")
    ));
    // Component-wise, not string-prefix: `<root>/a` is NOT an ancestor
    // of `<root>/ab`.
    assert!(!is_ancestor_or_equal(
        &proj_path("proj/a"),
        &proj_path("proj/ab")
    ));
    assert!(!is_ancestor_or_equal(
        &proj_path("proj/a/deep"),
        &proj_path("proj/a")
    ));
    assert!(!is_ancestor_or_equal(Path::new(""), &proj_path("proj")));
}

/// **V33: an unresolved `..` is refused by the ancestry walk itself, so
/// both routes that use it inherit the refusal.**
///
/// [`canon`] resolves `..` only for a path that EXISTS — `canonicalize`
/// fails on anything else and the raw string is kept — so a `..` reaches
/// this walk intact and a plain zip-compare calls it a descendant. Both
/// [`audit_admit`] step 3 and [`admitted_hook_root`] feed caller-supplied
/// strings in, which is why the answer lives in one place.
///
/// The Windows case is the one worth pinning, because it is the one that
/// looks safe and is not. `canon` adds a `\\?\` verbatim prefix on success
/// and not on failure, so the *plain* `P:\proj\..\..\evil` is rejected only
/// as a side effect of the prefixes disagreeing — nothing to do with `..`.
/// Spell the prefix yourself and the accident evaporates: before this
/// refusal, `\\?\P:\proj\..\..\evil` matched `\\?\P:\proj` and walked
/// through. Off Windows there is no prefix at all and the plain spelling
/// walked through too, so this is not a Windows-only property and its test
/// is not Windows-only either.
#[test]
fn is_ancestor_or_equal_refuses_an_unresolved_parent_dir() {
    let root = proj_path("proj");

    // The plain spelling, on either platform.
    assert!(!is_ancestor_or_equal(
        &root,
        &root.join("..").join("..").join("evil")
    ));
    // A `..` that does not even leave the root is still refused: this walk
    // cannot tell the difference, and every real caller sends a resolved
    // absolute path.
    assert!(!is_ancestor_or_equal(
        &root,
        &root.join("sub").join("..").join("evil")
    ));
    // The `root` side too — a discovery entry's `root` is file-supplied
    // (decision 30) and reaches `select_answering` unfiltered.
    assert!(!is_ancestor_or_equal(
        &root.join(".."),
        &proj_path("proj/a/deep")
    ));

    // Windows: the same escape with the verbatim prefix supplied by the
    // caller, which is what `canon` produces for the root side. This is the
    // spelling that actually matched before the refusal landed.
    if cfg!(windows) {
        assert!(!is_ancestor_or_equal(
            Path::new(r"\\?\P:\proj"),
            Path::new(r"\\?\P:\proj\..\..\evil")
        ));
        // Control: the same pair without the `..` still matches, so the
        // assertion above is about `..` and not about the prefix.
        assert!(is_ancestor_or_equal(
            Path::new(r"\\?\P:\proj"),
            Path::new(r"\\?\P:\proj\src")
        ));
    }
}

/// **V33: `/audit/run`'s step 3 gets the `..` refusal from the shared
/// helper, not from a copy of `/context/post_edit`'s check.**
///
/// The two routes answer a miss differently (a readable tool error here, an
/// empty-text fail-safe there) but they must agree on what a miss IS. This
/// asserts the agreement at the route, so deleting the shared refusal fails
/// here as well as at [`admitted_hook_root`]'s own test.
///
/// **What this deliberately does NOT claim.** Step 3 is a wrong-instance
/// guard, not a boundary: `cwd` is optional on the wire, so a body that
/// omits it skips the check entirely — and gains nothing by it, since the
/// scan root is `served_root` either way. The property pinned here is
/// consistency between the two path checks, not containment.
#[test]
fn audit_run_refuses_a_traversal_cwd_like_post_edit_does() {
    // A REAL directory on both sides, deliberately: `canon` then SUCCEEDS
    // for the served root and for the control cwd, so on Windows both carry
    // the `\\?\` prefix and the escape below is decided by the component
    // walk rather than by the prefix accident this fix exists to stop
    // relying on. A synthetic `P:\proj` would test the accident instead.
    let served = std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("the crate's own directory exists");
    let body = |cwd: &str| AuditRunBody {
        category: crate::audit::adapters::Category::Security,
        consumer: Some("claude".into()),
        cwd: Some(cwd.to_string()),
        tab: Some("tab-1".into()),
    };
    let admit = |b: &AuditRunBody| {
        audit_admit(
            &LatchRegistry::default(),
            b,
            &served,
            |_| true,
            |_, _| LatchScoping::Anonymous,
            |_| ON,
        )
    };

    // Control: a cwd inside the served root is admitted.
    let inside = served.join("src").to_string_lossy().into_owned();
    assert!(admit(&body(&inside)).is_ok(), "{inside}");

    // The traversal — spelled from the canonicalized root, i.e. WITH the
    // verbatim prefix on Windows — takes the wrong-instance refusal.
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let escape = format!("{}{sep}..{sep}..{sep}evil", served.display());
    let err = admit(&body(&escape)).expect_err(&escape);
    assert!(
        err.contains("this cImp instance serves"),
        "{escape} must take the wrong-instance refusal, got: {err}"
    );
}

#[test]
fn audit_run_body_parses_both_categories_and_rejects_junk() {
    use crate::audit::adapters::Category;
    // Both wire categories deserialize. `consumer` and `tab` stay optional
    // *on the wire* — H-8 enforces them in `audit_admit` instead, so a body
    // missing either becomes the route's readable tool error rather than a
    // bare 400 the model cannot act on. Only `category` is a parse error.
    let sec: AuditRunBody =
        serde_json::from_slice(br#"{"category":"security","consumer":"claude"}"#).unwrap();
    assert_eq!(sec.category, Category::Security);
    assert_eq!(sec.consumer.as_deref(), Some("claude"));
    let qual: AuditRunBody = serde_json::from_slice(br#"{"category":"quality"}"#).unwrap();
    assert_eq!(qual.category, Category::Quality);
    assert!(
        qual.consumer.is_none(),
        "consumer defaults to None when absent"
    );
    // A bad category word (or a missing `category`) is a clean parse error →
    // the route answers 400.
    assert!(serde_json::from_slice::<AuditRunBody>(br#"{"category":"bogus"}"#).is_err());
    assert!(serde_json::from_slice::<AuditRunBody>(br#"{"consumer":"x"}"#).is_err());
}

#[test]
fn graph_run_body_round_trips_the_v28_tab_field() {
    // V28: the per-tab MCP child tags `/graph_run` with the tab it serves.
    let tagged: GraphRunBody = serde_json::from_slice(
        br#"{"cwd":"P:\\proj","name":"context_recall","args":{},"consumer":"opencode","tab":"opencode"}"#,
    )
    .expect("tagged body parses");
    assert_eq!(tagged.tab.as_deref(), Some("opencode"));
    assert_eq!(tagged.consumer.as_deref(), Some("opencode"));
    assert_eq!(tagged.name, "context_recall");
}

#[test]
fn graph_run_body_still_accepts_pre_v28_bodies() {
    // Fail-open on the wire: a child spawned before the upgrade (or by hand)
    // sends no `tab` at all, and an explicit `null` must read the same. Both
    // resolve to `None`, i.e. the pre-V28 most-recent-session scoping — never
    // a 400 that would break the tool call.
    let absent: GraphRunBody =
        serde_json::from_slice(br#"{"name":"context_notes","args":{},"consumer":"claude"}"#)
            .expect("pre-V28 body still parses");
    assert!(absent.tab.is_none());
    assert!(absent.cwd.is_none());

    let null: GraphRunBody =
        serde_json::from_slice(br#"{"name":"context_notes","args":{},"tab":null}"#)
            .expect("explicit null parses");
    assert!(null.tab.is_none());

    // An unknown extra field (a NEWER child talking to an older app) is
    // likewise tolerated rather than rejected.
    let extra: GraphRunBody =
        serde_json::from_slice(br#"{"name":"context_notes","args":{},"future_field":1}"#)
            .expect("unknown fields ignored");
    assert!(extra.tab.is_none());
}

// ── V32 Phase B — the proxy's per-session taint latch ──────────────────

use crate::offload::toolclass::{
    REFUSAL_EXTERNAL_BLOCKED, REFUSAL_EXTERNAL_USER_LOCAL, REFUSAL_LOCAL_BLOCKED,
};

/// A scope for `tab`, claiming session `session` (`None` = the registry
/// withheld one). `claude` unless the test says otherwise.
fn scope(tab: &str, session: Option<&str>) -> LatchScope {
    LatchScope {
        agent: "claude",
        tab: tab.to_string(),
        session: session.map(str::to_string),
        root: TEST_ROOT.to_string(),
    }
}


// ── V39 Phase B: delegation rides the taint latch ───────────────────────

/// **A contaminated driver tab may not delegate.**
///
/// The same refusal `offload_task` gets under V32 C-1c, and for the same
/// reason: both hand a task to a fresh, permissive executor, and this one's
/// executor is a whole peer harness with its own tools. "The user asked for
/// it" does not launder the request — the task text is model-authored.
///
/// Asserted on [`delegate_admit`] rather than through the route, because
/// that IS the decision: the handler cannot reach a tab without passing
/// through it.
#[test]
fn a_contaminated_tab_is_refused_a_delegation_and_a_clean_one_is_not() {
    // Opaque inputs: `delegate_admit` never looks at either string -- the
    // scoping closure below is stubbed, so the tab id is a key and the
    // consumer is passed straight through. V39 wrote a real harness id and
    // a `claude-`prefixed tab here, which read as if the gate knew what a
    // harness was (V40 Phase G, locked decision 28).
    const WORKER_TAB: &str = "worker-deleg";
    let s = scope(WORKER_TAB, Some("ses"));
    // `LatchScope` is not `Clone`, so the closure rebuilds the same scope
    // rather than capturing one — the scope KEY is what the registry joins
    // on, and building it twice from the same inputs is the honest way to
    // say two calls are the same scope.
    let admit = |reg: &LatchRegistry| {
        delegate_admit(
            reg,
            DELEGATE_TOOL,
            crate::harness::DEFAULT_HARNESS.token(),
            Some(WORKER_TAB),
            |_, _| LatchScoping::Scoped(scope(WORKER_TAB, Some("ses"))),
            |_| ON,
        )
    };

    // A clean conversation may delegate…
    let reg = LatchRegistry::default();
    assert!(admit(&reg).is_ok(), "an unlatched tab may delegate");
    // …and delegating LATCHED it to Local, exactly as `offload_task` does.
    // The call is elective, so it moves the latch — this is the whole
    // difference between `LatchRoute::Delegation` and `LatchRoute::Hook`.
    assert_eq!(reg.snapshot()[0].latch(), "local");

    // A contaminated one may not.
    let reg2 = LatchRegistry::default();
    assert!(reg2
        .gate(Some(&s), LatchRoute::Proxied, "ddg__fetch_content", ON, NO_CONTENT)
        .is_ok());
    assert_eq!(reg2.snapshot()[0].latch(), "external");
    let refusal = admit(&reg2).expect_err("a contaminated tab must be refused");
    assert!(
        !refusal.trim().is_empty(),
        "a refusal with no reason is not one the model can act on"
    );
    // The SAME user-visible refusal `offload_task` produces — compared
    // rather than restated, so the two can never come to differ.
    let same = reg2
        .gate(Some(&s), LatchRoute::Native, "offload_task", ON, NO_CONTENT)
        .expect_err("offload_task is refused here too");
    assert_eq!(
        refusal, same,
        "delegation must be refused in exactly the shape `offload_task` is"
    );
    // Named, not just compared: an EXTERNAL latch is what blocks the
    // LOCAL-CAPABILITY side, so this is the local-blocked refusal — the
    // same sentence a latched tab gets for `read_file`.
    assert_eq!(refusal, REFUSAL_LOCAL_BLOCKED);
}

/// **The gate runs before anything is driven.**
///
/// A refused delegation must leave the worker tab exactly as it was: no
/// slot claimed, no read-only lock engaged, no `start` Events row, and not
/// one byte typed. All of that follows from ORDER inside `handle_delegate`,
/// so the order is what is asserted — on the source, because the property
/// is about what the handler can do rather than about what one run did.
#[test]
fn the_delegate_gate_runs_before_anything_is_driven() {
    let body = handler_body("handle_delegate");
    let gate_at = body
        .find("delegate_admit(")
        .expect("handle_delegate must gate");
    let drive_at = body
        .find("delegation::drive_watching(")
        .expect("handle_delegate must drive");
    assert!(
        gate_at < drive_at,
        "the taint gate must precede the drive call, or a refused delegation has already \
         locked the worker's keyboard and minted a `start` row"
    );
    // …and nothing that touches the worker happens before it either.
    let before = &body[..gate_at];
    for touches in ["delegation::drive", "set_driven", "record_row"] {
        assert!(
            !before.contains(touches),
            "`{touches}` runs before the taint gate in handle_delegate"
        );
    }
}

/// **V39 Phase C — a facade run needs no second gate, and this is why.**
///
/// A facade is reached through `offload_task`, which `/run` already gates
/// under V32 C-1c: a latched (injection-flagged) tab is refused there,
/// before `service.run` is called at all — and `delegation::drive` is only
/// ever reached from inside `service.run` (→ `run_on` → `run_facade`). So
/// the refusal happens before the engine exists for this call: no worker
/// resolved, no slot claimed, no lock engaged, no byte typed.
///
/// Adding a second gate in `run_facade` would be worse than redundant — it
/// would put a `delegate_task`-shaped refusal on a path the model reached
/// through `offload_task`, and the two say different things about what the
/// caller may do next. The property is about ORDER inside `handle_run`, so
/// order is what is asserted, on the source, exactly as the `/delegate`
/// ordering test above does it.
#[test]
fn a_facade_run_is_refused_by_offload_tasks_own_gate_before_the_engine() {
    let body = handler_body("handle_run");
    let gate_at = body
        .find("latches().gate(")
        .expect("handle_run must gate — V32 C-1c");
    let run_at = body
        .find("service.run(")
        .expect("handle_run must run the task");
    assert!(
        gate_at < run_at,
        "the taint gate must precede `service.run`, or a latched tab's facade run reaches \
         the delegation engine"
    );
    assert!(
        !body.contains("delegation::"),
        "the facade path must not reach the engine from this handler: it is entered from \
         `service.run`, downstream of the gate"
    );
    // The gate's tool name is `offload_task` for a facade exactly as for
    // every other backend — the driver asked for an offload and the kind of
    // backend it landed on is not the caller's business (decision 3).
    assert!(
        body.contains("offload_tool_name("),
        "the gated tool name must still be resolved by the offload naming funnel"
    );
}

/// **`LatchRoute::Delegation` is the fixed-name/elective corner**, and the
/// two properties that put it there are asserted rather than assumed.
///
/// If it ever inherited `Hook`'s non-engaging rule, a tab could delegate
/// unboundedly without ever latching; if it inherited `Native`'s
/// dispatchable rule, the gate would silently become a no-op (the bare name
/// is `unrouted` by design), which is precisely the gap this commit closes.
#[test]
fn the_delegation_route_both_refuses_and_latches() {
    let cls = toolclass::classify;
    assert!(
        LatchRoute::Delegation.can_execute(DELEGATE_TOOL, cls(DELEGATE_TOOL)),
        "the route states its own name, so M-2's not-dispatchable wave-through must not \
         apply — it would turn this gate back into a no-op"
    );
    assert!(
        !LatchRoute::Native.can_execute(DELEGATE_TOOL, cls(DELEGATE_TOOL)),
        "…while the bare name on a NATIVE route still reaches no dispatcher"
    );
    assert!(
        LatchRoute::Delegation.engages(),
        "a delegation is elective — unlike a hook, it must move the latch"
    );
    assert!(!LatchRoute::Hook.engages());
    assert!(!LatchRoute::Delegation.external_is_content());
    assert_eq!(cls(DELEGATE_TOOL), cls("offload_task"));
}

// ── V32 Phase G — the two switches over this gate ──────────────────────

/// The taint latch OFF: nothing latches, nothing is refused, and — because
/// an inert policy must leave no trace — `/status` does not sprout a row
/// showing a boundary that is not being enforced.
#[test]
fn a_disabled_latch_refuses_nothing_and_records_nothing() {
    let off = GatePolicy {
        latch: false,
        quarantine: false,
    };
    let reg = LatchRegistry::default();
    let s = scope("claude-off", Some("ses"));
    // The classic fetch-then-read sequence, which under ON closes the local
    // side after the first EXTERNAL call.
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            off,
            NO_CONTENT
        )
        .is_ok());
    for name in ["graph_snippet", "read_file", "context_note", "ddg__search"] {
        assert!(
            reg.gate(Some(&s), LatchRoute::Native, name, off, NO_CONTENT)
                .is_ok(),
            "{name} must not be refused with the latch off"
        );
    }
    assert!(
        reg.snapshot().is_empty(),
        "an inert gate must not create a latch row"
    );
}

/// Memory quarantine OFF: a write from a conversation that HAS read external
/// content is stored clean. (The read-side exclusion is deliberately not
/// gated — already-held notes stay held; see the Phase G amendment.)
#[test]
fn a_disabled_quarantine_stores_a_contaminated_write_clean() {
    let no_quarantine = GatePolicy {
        latch: true,
        quarantine: false,
    };
    let reg = LatchRegistry::default();
    let s = scope("claude-q", Some("ses"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            no_quarantine,
            NO_CONTENT
        )
        .is_ok());
    // The latch still engaged (that is a different feature)…
    assert_eq!(reg.snapshot()[0].latch(), "external");
    // …but the write is not held.
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Native,
            "context_note",
            no_quarantine,
            NO_CONTENT
        ),
        Ok(WriteTaint::Clean)
    );
}

/// The asymmetric combination the two switches exist to allow: latch OFF,
/// quarantine ON. Nothing is refused, but contamination is still tracked, so
/// a note written after a fetch is still held for review.
#[test]
fn quarantine_survives_a_disabled_latch_via_the_contamination_bit() {
    let quarantine_only = GatePolicy {
        latch: false,
        quarantine: true,
    };
    let reg = LatchRegistry::default();
    let s = scope("claude-mix", Some("ses"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            quarantine_only,
            NO_CONTENT
        )
        .is_ok());
    // The latch itself never moved — it is off.
    assert_eq!(reg.snapshot()[0].latch(), "open");
    assert!(reg.snapshot()[0].view.contaminated);
    // Local tools stay open (no latch)…
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            quarantine_only,
            NO_CONTENT
        )
        .is_ok());
    // …and the write is held anyway.
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Native,
            "context_note",
            quarantine_only,
            NO_CONTENT
        ),
        Ok(WriteTaint::Quarantined)
    );
}

/// A beacon under an inert policy engages nothing and creates no row — the
/// sensor hook may still be installed on a tab whose latch was switched off
/// after spawn, and it must not resurrect the feature.
#[test]
fn a_beacon_under_an_inert_policy_is_a_no_op() {
    let off = GatePolicy {
        latch: false,
        quarantine: false,
    };
    let reg = LatchRegistry::default();
    let s = scope("claude-beacon-off", Some("ses"));
    assert_eq!(
        reg.beacon(Some(&s), "WebFetch", off, BEACON_PROV),
        BeaconOutcome::inert()
    );
    assert!(reg.snapshot().is_empty());
}

#[test]
fn mcp_call_body_carries_the_v32_tab_field_and_tolerates_its_absence() {
    // V32 Phase B: the per-tab child now tags `/mcp/call` too, so the
    // proxy can key the call to that tab's session latch.
    let tagged: McpCallBody = serde_json::from_slice(
        br#"{"name":"ddg__fetch_content","arguments":{"url":"x"},"cwd":"P:\\proj","tab":"claude-2"}"#,
    )
    .expect("tagged body parses");
    assert_eq!(tagged.tab.as_deref(), Some("claude-2"));
    assert_eq!(tagged.name, "ddg__fetch_content");

    // Fail-open on the wire, exactly like `/graph_run`: a child from before
    // this field (or an explicit null) must still be served, unlatched.
    let absent: McpCallBody =
        serde_json::from_slice(br#"{"name":"ddg__search","arguments":{}}"#)
            .expect("pre-V32 body still parses");
    assert!(absent.tab.is_none());
    assert!(absent.cwd.is_none());
    let null: McpCallBody =
        serde_json::from_slice(br#"{"name":"ddg__search","arguments":{},"tab":null}"#)
            .expect("explicit null parses");
    assert!(null.tab.is_none());
}

/// Direction 1: the tab fetches the web first, so the content-bearing
/// (LOCAL-CAPABILITY) graph tools close for the rest of that session —
/// read-after-fetch is how an injected page steers later reads.
#[test]
fn external_first_closes_the_local_capability_side_for_that_tab() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());

    for blocked in [
        "graph_snippet",
        "graph_search_docs",
        "graph_semantic_docs",
        "graph_semantic_code",
    ] {
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, blocked, ON, NO_CONTENT),
            Err(REFUSAL_LOCAL_BLOCKED),
            "{blocked}"
        );
    }
    // The external side itself stays usable — the latch is exclusion, not
    // a kill switch.
    assert!(reg
        .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    assert_eq!(reg.snapshot()[0].latch(), "external");
}

/// Direction 2: the tab reads source text first, so the proxied servers
/// close — read-then-fetch is how secrets ride out on a fetch URL.
#[test]
fn local_capability_first_closes_the_external_side_for_that_tab() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());

    for blocked in ["ddg__search", "ddg__fetch_content", "context7__query-docs"] {
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Proxied, blocked, ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_BLOCKED),
            "{blocked}"
        );
    }
    // Local work continues, including the memory write (only an EXTERNAL
    // latch gates persistence).
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert!(reg
        .gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT)
        .is_ok());
    assert_eq!(reg.snapshot()[0].latch(), "local");
}

/// TRUSTED tools are immune in both directions and never latch anything:
/// a structural graph query or a memory read must not cost the session
/// either capability.
#[test]
fn trusted_tools_never_latch_and_are_never_refused() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    // V32 H-1: `graph_repo_map` was in this list until the 2026-08-08
    // re-review demoted it out of TRUSTED — see
    // `the_source_text_graph_readers_are_refused_at_the_proxy_gate` below,
    // which asserts the opposite verdict for it and for
    // `graph_struct_search` on this same route.
    for trusted in [
        "graph_outline",
        "graph_find_symbol",
        "context_recall",
        "context_notes",
    ] {
        assert!(
            reg.gate(Some(&s), LatchRoute::Native, trusted, ON, NO_CONTENT)
                .is_ok(),
            "{trusted}"
        );
    }
    assert!(reg.snapshot().is_empty() || reg.snapshot()[0].latch() == "open");

    // And under a latch of either kind they still answer.
    for (route, first) in [
        (LatchRoute::Proxied, "ddg__search"),
        (LatchRoute::Native, "graph_snippet"),
    ] {
        let reg = LatchRegistry::default();
        let s = scope("t", Some("s"));
        assert!(reg.gate(Some(&s), route, first, ON, NO_CONTENT).is_ok());
        for trusted in ["graph_outline", "context_recall", "context_notes"] {
            assert!(
                reg.gate(Some(&s), LatchRoute::Native, trusted, ON, NO_CONTENT)
                    .is_ok(),
                "{trusted} under {first}"
            );
        }
    }
}

/// **V32 H-1 (2026-08-08 re-review — C-1 reopened): `graph_struct_search`
/// and `graph_repo_map` are refused at the TAB gate.**
///
/// This is the second of the two enforcement paths and, for a
/// Claude/OpenCode tab, the only one: graph tools arrive on `/graph_run`,
/// which gates by name through [`LatchRegistry::gate`], and the proxy never
/// def-filters the graph surface (the per-session child caches `tools/list`
/// at connect). A fix verified only against the worker's `filter_defs` —
/// which is how C-1 survived `b80f5b8` — would leave this route wide open,
/// so it is asserted here rather than inferred from the class table.
#[test]
fn the_source_text_graph_readers_are_refused_at_the_proxy_gate() {
    for blocked in ["graph_struct_search", "graph_repo_map"] {
        // Contaminated conversation ⇒ refused with the fixed local string.
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, blocked, ON, NO_CONTENT),
            Err(REFUSAL_LOCAL_BLOCKED),
            "{blocked} must be refused once the conversation has read a page"
        );

        // …and used first it LATCHES the tab local, closing the web — the
        // accepted consequence of the demotion for a tab, not just for a
        // worker task.
        let reg = LatchRegistry::default();
        let s = scope("claude-2", Some("sess-b"));
        assert!(reg
            .gate(Some(&s), LatchRoute::Native, blocked, ON, NO_CONTENT)
            .is_ok());
        assert_eq!(reg.snapshot()[0].latch(), "local", "{blocked}");
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_BLOCKED),
            "{blocked}"
        );
    }
}

/// **C-1b + C-1c (2026-08-07 re-verification sweep): the two routes that
/// reached LOCAL-CAPABILITY without ever consulting `classify()`.**
///
/// `b80f5b8` demoted `run_check`/`security_audit`/`quality_audit`, but the
/// demotion only reached the offload worker's def-filtering path. The audit
/// tools arrive on `/audit/run` (their own MCP server, `cimp-code-audit`),
/// which held no `latches()` call at all; `offload_task`/`offload_batch`
/// arrive on `/run`, which held none either and was TRUSTED besides. Both
/// routes now gate here, so this pins the verdict both of them read.
#[test]
fn the_audit_and_offload_routes_are_local_capability_at_the_gate() {
    // An EXTERNAL-latched, contaminated conversation refuses all four.
    for blocked in [
        "security_audit",
        "quality_audit",
        "offload_task",
        "offload_batch",
    ] {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, blocked, ON, NO_CONTENT),
            Err(REFUSAL_LOCAL_BLOCKED),
            "{blocked} must be refused once the conversation has read a page"
        );
    }
    // …and in the other direction each of them LATCHES, closing the web for
    // the rest of the session. That is the accepted consequence of the
    // split, so it is asserted rather than discovered in the field.
    for first in [
        "security_audit",
        "quality_audit",
        "offload_task",
        "offload_batch",
    ] {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(Some(&s), LatchRoute::Native, first, ON, NO_CONTENT)
            .is_ok());
        assert_eq!(reg.snapshot()[0].latch(), "local", "{first}");
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_BLOCKED),
            "{first}"
        );
    }
    // The `/run` body's `tool` field is a LABEL, never a capability: only
    // the two real names survive the parse boundary, and both classify the
    // same, so no value a caller invents can change the verdict above.
    assert_eq!(offload_tool_name(Some("offload_batch")), "offload_batch");
    assert_eq!(offload_tool_name(Some(" offload_batch ")), "offload_batch");
    for raw in [None, Some(""), Some("offload_task"), Some("graph_outline")] {
        assert_eq!(offload_tool_name(raw), "offload_task", "{raw:?}");
    }
    // The `/audit/run` gate's name comes from the category, through the one
    // mapping the child's `tools/call` also uses.
    assert_eq!(
        crate::audit::mcp::tool_name_for(crate::audit::adapters::Category::Security),
        "security_audit"
    );
    assert_eq!(
        crate::audit::mcp::tool_name_for(crate::audit::adapters::Category::Quality),
        "quality_audit"
    );
}

// ── H-8 (2026-08-08 re-review): `/audit/run`'s gate is not opt-in ──────
//
// The finding: the gate's only identity input was `body.tab`, caller
// supplied and optional. Absent ⇒ `LatchScoping::Anonymous` ⇒ `scope()`
// `None` ⇒ `gate()` returned `Ok(Clean)` before classifying anything, and
// said nothing about it. Compounding, `consumer` was caller-asserted and
// unbounded while selecting which `expose_*` toggle was checked — including
// `"offload"`, which defaults true and which no legitimate caller sends.
//
// These drive [`audit_admit`], which is the route's ENTIRE pre-scan
// decision (the handler adds only body parsing, state resolution and the
// wire framing), so the ordering they assert is the ordering that ships.

/// A `/audit/run` body. `Security` throughout: the gate's tool name comes
/// from the category and both categories classify identically
/// (`the_audit_and_offload_routes_are_local_capability_at_the_gate`).
fn audit_body(consumer: Option<&str>, tab: Option<&str>) -> AuditRunBody {
    AuditRunBody {
        category: crate::audit::adapters::Category::Security,
        consumer: consumer.map(str::to_string),
        cwd: None,
        tab: tab.map(str::to_string),
    }
}

/// Drive [`audit_admit`] against `reg` with a fixed served root, an
/// `exposed` verdict and a pre-resolved scoping — the same three
/// dependencies the handler supplies from `AuditState` / `latch_scope` /
/// `GatePolicy::resolve`.
fn admit(
    reg: &LatchRegistry,
    body: &AuditRunBody,
    exposed: bool,
    scoping: LatchScoping,
) -> Result<&'static str, String> {
    audit_admit(
        reg,
        body,
        Path::new("P:\\proj"),
        |_| exposed,
        |_, _| scoping,
        |_| ON,
    )
}

/// H-8, half 1. A body with no usable tab identity is REFUSED — and the
/// refusal engages nothing, because it happens before any `LatchScope`
/// exists. The message names the remedy (restart the tab), because the only
/// legitimate way to arrive here is a child left over from a pre-C-1b
/// build.
#[test]
fn audit_run_refuses_a_body_with_no_tab_and_engages_no_latch() {
    for tab in [None, Some(""), Some("   "), Some("\t")] {
        let reg = LatchRegistry::default();
        let err = admit(
            &reg,
            &audit_body(Some("claude"), tab),
            true,
            // Unreachable: the refusal precedes scope resolution. Anything
            // here would be a scope the refusal must not have used.
            LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
        )
        .expect_err("a body with no tab identity must be refused");
        assert!(
            err.contains("restart this tab"),
            "the refusal must name the remedy, got {err:?}"
        );
        // The invariant, not the string: a refused request leaves the
        // registry exactly as it found it.
        assert!(
            reg.snapshot().is_empty(),
            "a refused request must not key a latch row ({tab:?})"
        );
    }
}

/// H-8, half 1 — the exploit, re-run. An EXTERNAL-latched (contaminated)
/// conversation that curls the route *with a tab* is refused by the gate;
/// the same conversation curling it *without* one — which used to return the
/// full gitleaks report while consulting no latch at all — is refused too.
#[test]
fn audit_run_refuses_a_contaminated_tab_with_or_without_an_id() {
    let reg = LatchRegistry::default();
    // Contaminate: one proxied fetch closes the local side for the session.
    assert!(reg
        .gate(
            Some(&scope("claude-1", Some("sess-a"))),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(reg.snapshot()[0].latch(), "external");

    // With its own identity: the gate now actually runs, and refuses.
    let err = admit(
        &reg,
        &audit_body(Some("claude"), Some("claude-1")),
        true,
        LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
    )
    .expect_err("a contaminated conversation must not run a local scanner");
    assert_eq!(err, REFUSAL_LOCAL_BLOCKED);

    // Dropping `tab` was the whole exploit — it is no longer an escape.
    let err = admit(
        &reg,
        &audit_body(Some("claude"), None),
        true,
        LatchScoping::Anonymous,
    )
    .expect_err("omitting `tab` must not opt the caller out of the gate");
    assert!(err.contains("restart this tab"), "{err:?}");

    // Neither refusal moved the latch (a refused call must never redefine
    // which side of the boundary the session is on).
    assert_eq!(reg.snapshot()[0].latch(), "external");
}

/// H-8, half 1 — the surviving no-scope path. An id naming no configured
/// tab keeps #45's behaviour: no registry row, no refusal (fail-open on a
/// TOOL route), and — this is the H-8 half — it is WARNED rather than
/// silent. The warn is written over `scope().is_none()`, so it covers
/// `Anonymous` too if step 4 ever regresses; that predicate is pinned here
/// because the log line itself is not observable from a unit test.
#[test]
fn audit_run_warns_but_still_runs_for_an_unknown_tab() {
    let reg = LatchRegistry::default();
    assert_eq!(
        admit(
            &reg,
            &audit_body(Some("claude"), Some("ghost")),
            true,
            LatchScoping::Unknown("ghost".into()),
        ),
        Ok("claude")
    );
    assert!(
        reg.snapshot().is_empty(),
        "#45's bound: an unknown id keys no registry entry"
    );
    // Both identity-less variants take the warn branch.
    assert!(LatchScoping::Unknown("ghost".into()).scope().is_none());
    assert!(LatchScoping::Anonymous.scope().is_none());
}

/// H-8 — containment must not be bought by breaking the route. A clean,
/// configured tab is admitted, and the scan engages that tab's latch (which
/// is also what proves the refusal tests above are asserting a registry the
/// success path really does write to).
#[test]
fn audit_run_admits_a_clean_configured_tab_and_engages_its_latch() {
    for (consumer, expect) in [
        (None, "claude"),
        (Some("claude"), "claude"),
        (Some("opencode"), "opencode"),
        (Some(" OpenCode "), "opencode"),
    ] {
        let reg = LatchRegistry::default();
        assert_eq!(
            admit(
                &reg,
                &audit_body(consumer, Some("claude-1")),
                true,
                LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
            ),
            Ok(expect),
            "{consumer:?}"
        );
        assert_eq!(
            reg.snapshot()[0].latch(),
            "local",
            "an admitted LOCAL-CAPABILITY scan closes the web side"
        );
    }
}

/// H-8, half 2. `consumer` is narrowed to the two consumers that actually
/// exist before it can select an `expose_*` toggle.
///
/// `"offload"` is the one that mattered: `AuditState::consumer_exposed`
/// maps it to `expose_offload`, which **defaults true**, while
/// `graph::source_for_consumer` maps it to `"claude"` — so a forged caller
/// passed a toggle no legitimate caller uses and latched as somebody else.
/// The `exposed` closure panics here, which is how the test proves no
/// toggle is selected at all rather than merely that the request failed.
#[test]
fn audit_run_rejects_a_consumer_outside_the_legitimate_set() {
    for bad in ["offload", "worker", "OFFLOAD", "claude ext", "clau de", "x"] {
        let reg = LatchRegistry::default();
        let body = audit_body(Some(bad), Some("claude-1"));
        let err = match audit_admit(
            &reg,
            &body,
            Path::new("P:\\proj"),
            |c| panic!("an expose toggle was selected for the rejected consumer {c:?}"),
            |_, _| panic!("identity was resolved for a rejected consumer"),
            |_| ON,
        ) {
            Ok(c) => panic!("{bad:?} must not be accepted as a consumer (got {c:?})"),
            Err(e) => e,
        };
        assert!(
            err.contains("does not serve the consumer"),
            "{err:?} ({bad})"
        );
        assert!(reg.snapshot().is_empty(), "{bad}");
    }
    // The set itself, and the two spellings the spawn paths actually send.
    assert_eq!(audit_consumers(), crate::harness::registry::harness_ids());
    assert_eq!(audit_consumer(None), Ok("claude"));
    assert_eq!(audit_consumer(Some("")), Ok("claude"));
    assert_eq!(audit_consumer(Some("  ")), Ok("claude"));
    assert_eq!(audit_consumer(Some("CLAUDE")), Ok("claude"));
    assert_eq!(audit_consumer(Some(" opencode ")), Ok("opencode"));
    // …and the value that reaches `consumer_exposed` is one of those two
    // literals, never the caller's string, so no `expose_*` toggle outside
    // the pair is reachable over HTTP.
    for c in audit_consumers() {
        assert_eq!(audit_consumer(Some(c)), Ok(c));
    }
}

/// H-8 — ordering. The two pre-existing refusals still come first (their
/// messages are the actionable ones), and neither leaves latch state
/// behind: a request that was never going to run must not engage the tab's
/// latch. Same registry the success path above writes to, so an empty
/// snapshot here is a real observation.
#[test]
fn audit_run_refusals_before_the_gate_leave_no_latch_state() {
    // Not exposed — refused before identity is even resolved.
    let reg = LatchRegistry::default();
    let err = admit(
        &reg,
        &audit_body(Some("opencode"), Some("opencode")),
        false,
        LatchScoping::Scoped(scope("opencode", Some("sess-a"))),
    )
    .expect_err("an opted-out consumer must be refused");
    assert!(err.contains("is not exposed to opencode"), "{err:?}");
    assert!(
        reg.snapshot().is_empty(),
        "expose refusal keyed a latch row"
    );

    // Misrouted (cwd outside this instance's served root) — likewise.
    let reg = LatchRegistry::default();
    let mut body = audit_body(Some("claude"), Some("claude-1"));
    body.cwd = Some("P:\\other-project".into());
    let err = audit_admit(
        &reg,
        &body,
        Path::new("P:\\proj"),
        |_| true,
        |_, _| LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
        |_| ON,
    )
    .expect_err("a misrouted child must be refused");
    assert!(err.contains("this cImp instance serves"), "{err:?}");
    assert!(reg.snapshot().is_empty(), "cwd refusal keyed a latch row");
}

/// The locked cross-module invariant, through the proxy: a server nobody
/// has classified is EXTERNAL, so calling it latches the session exactly
/// like `ddg__*` does.
#[test]
fn an_unknown_proxied_server_latches_as_external() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "somenewserver__anything",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(reg.snapshot()[0].latch(), "external");
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        ),
        Err(REFUSAL_LOCAL_BLOCKED)
    );
}

/// Locked decision 10, as built in Phase C2: a memory write under an
/// EXTERNAL latch is **quarantined, not refused** — the note is stored with
/// a `tainted` flag and withheld from every read path, so an injected page
/// still cannot plant a note that auto-injects into future clean sessions,
/// but a legitimate research conclusion is preserved for review instead of
/// being thrown away (the Phase A/B behaviour).
#[test]
fn context_note_is_quarantined_under_an_external_latch_only() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    // Unlatched: clean, and the write itself does not latch.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Clean)
    );
    assert_eq!(reg.snapshot()[0].latch(), "open");

    assert!(reg
        .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    // EXTERNAL-latched: proceeds, tainted — NOT `Err(REFUSAL_WRITE_BLOCKED)`.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Quarantined)
    );
    // ...and the quarantined write still does not move the latch.
    assert_eq!(reg.snapshot()[0].latch(), "external");
    // Reads of the same store stay open — quarantine is about persistence.
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Native,
            "context_recall",
            ON,
            NO_CONTENT
        ),
        Ok(WriteTaint::Clean)
    );
}

/// The other direction of the same rule: a LOCAL-CAPABILITY latch never
/// taints a write — only external content can contaminate persistence.
#[test]
fn a_local_latch_writes_clean() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(reg.snapshot()[0].latch(), "local");
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Clean)
    );
}

/// #48 F-20 — the three-way mapping, pinned to concrete values.
///
/// [`LatchScoping`] and [`crate::activity::Attribution`] were derived from the
/// same three facts, and the row's column exists to say which of the three a
/// call was. The `match` below is exhaustive on purpose: a fourth variant has
/// to be given a reading here rather than silently inheriting one.
#[test]
fn latch_scoping_maps_onto_exactly_one_attribution_state_each() {
    use crate::activity::Attribution;
    assert_eq!(LatchScoping::Anonymous.attribution(), Attribution::Headless);
    assert_eq!(
        LatchScoping::Unknown("ghost".to_string()).attribution(),
        Attribution::Unrecognized("ghost".to_string())
    );
    assert_eq!(
        LatchScoping::Scoped(scope("claude-1", Some("sess-a"))).attribution(),
        Attribution::Tab("claude-1".to_string())
    );
    for s in [
        LatchScoping::Anonymous,
        LatchScoping::Unknown("ghost".to_string()),
        LatchScoping::Scoped(scope("claude-1", None)),
    ] {
        // Exhaustiveness: the compiler is the enumeration guard.
        let _: () = match &s {
            LatchScoping::Anonymous | LatchScoping::Unknown(_) | LatchScoping::Scoped(_) => (),
        };
        assert_ne!(
            s.attribution(),
            Attribution::Unattributed,
            "a route that resolved a scoping DOES know which of the three this was"
        );
    }
}

/// The case the whole finding is about: `Anonymous` and `Unknown` are ONE
/// `None` to the latch — correctly, both fail open — and must be TWO states
/// on the row.
#[test]
fn an_unrecognized_tab_id_is_never_reported_as_headless() {
    use crate::activity::Attribution;
    let ghost = || LatchScoping::Unknown("not-a-real-tab".to_string());
    // The collapse that is right for the latch…
    assert!(
        ghost().into_scope().is_none(),
        "#45's bound: an unrecognized id keys no registry entry"
    );
    assert!(LatchScoping::Anonymous.into_scope().is_none());
    // …and wrong for the row.
    assert_ne!(ghost().attribution(), Attribution::Headless);
    assert_ne!(
        ghost().attribution(),
        LatchScoping::Anonymous.attribution(),
        "F-20: these two were one `None` and must be two row states"
    );
}

/// …and an unrecognized id is never reported as a real tab either — the rule
/// `activity::tests::only_a_configured_tab_counts_as_a_tab` states, from the
/// producer side.
#[test]
fn an_unrecognized_tab_id_is_never_reported_as_a_tab() {
    let attr = LatchScoping::Unknown("not-a-real-tab".to_string()).attribution();
    assert!(
        !attr.is_tab(),
        "filtering by a tab id must never surface a row that merely quoted it"
    );
    assert_eq!(attr.id(), Some("not-a-real-tab"));
}

/// **#48 F-39 / locked decision 42 — an invented tab id cannot choose how
/// many bytes of a capped lane one row occupies, and truncating it is not a
/// way to become a real tab.**
///
/// Three halves, and the ORDER is the finding's subtle part.
///
/// 1. The bound itself, on the row the producer actually writes
///    (`attribution()`, which `/graph_run` and `/mcp/call` both call).
/// 2. **Classification sees the FULL string.** A body id that is a configured
///    tab id plus a suffix — so that a naive parse-boundary truncation would
///    hand `is_configured_tab` the configured id — must still resolve as
///    `Unknown`. This is the assertion that fails if a future "fix" moves
///    `bounded_id` earlier, closing the bloat hole by opening an
///    impersonation one.
/// 3. The truncated id is still not a configured tab, from the same
///    `is_configured_tab` the resolution used.
///
/// **What this would still pass if the implementation were wrong:** it would
/// pass a bound applied anywhere at or after `tab_identity` (the constructor
/// in `latch_scope`, say) — deliberately, because every such site is after
/// classification and any of them is correct. It would NOT pass a bound
/// applied to `body.tab` before the identity check, and it would not pass a
/// larger-than-`BEACON_TOOL_MAX` bound, an ellipsis-free truncation that
/// happened to equal a configured id, or no bound at all.
#[test]
fn an_invented_tab_id_is_bounded_before_it_reaches_a_row_and_after_it_is_classified() {
    use crate::activity::Attribution;
    // A configured id exactly as long as the bound, so a truncation applied
    // one step too early would produce this very string.
    let real = "t".repeat(BEACON_TOOL_MAX);
    let s = settings_with_tabs(&[real.as_str()]);
    let forged = format!("{real}-and-then-some{}", "x".repeat(4096));

    // (2) The classifier is handed the whole thing, so the suffix counts.
    assert!(
        matches!(
            tab_identity(&s, "claude", Some(forged.as_str())),
            TabIdentity::Unknown(_)
        ),
        "truncation must not run before `is_configured_tab`"
    );
    assert!(matches!(
        tab_identity(&s, "claude", Some(real.as_str())),
        TabIdentity::Configured(_)
    ));

    // (1) The row's attribution is bounded — chars, not bytes, and one
    // ellipsis says it was cut.
    let attr = LatchScoping::Unknown(forged.clone()).attribution();
    let Attribution::Unrecognized(id) = &attr else {
        panic!("a 4 KiB invented id is not a tab: {attr:?}");
    };
    assert!(
        id.chars().count() <= BEACON_TOOL_MAX + 1,
        "{} chars reached the row",
        id.chars().count()
    );
    assert!(id.ends_with('…'), "a cut id must say it was cut: {id}");

    // (3) …and it is still nobody's tab.
    assert!(
        !is_configured_tab(&s, "claude", id),
        "truncation is not a forgery"
    );
    assert_ne!(*id, real);
    assert!(!attr.is_tab());

    // A multi-byte id is cut on a codepoint, never mid-character.
    let wide = LatchScoping::Unknown("é".repeat(4096)).attribution();
    let Attribution::Unrecognized(id) = &wide else {
        panic!("not a tab: {wide:?}");
    };
    assert!(id.chars().count() <= BEACON_TOOL_MAX + 1);

    // An id that fits is untouched — the bound must not cost the honest case
    // anything, since a stale-but-real id is why this variant exists.
    assert_eq!(
        LatchScoping::Unknown("opencode-removed".to_string()).attribution(),
        Attribution::Unrecognized("opencode-removed".to_string())
    );
}

/// #48 F-16 — the unattributed-write row names the project it was about to
/// write into.
///
/// [`LatchRegistry::gate`] has no scope to take a root from — that is
/// [`unattributed_write`]'s whole premise — but the ROUTE does (`/graph_run`
/// holds `body.cwd`, resolved through `GraphService::graph_root_key`, the same
/// resolution the dispatch's own `kind:"graph"` row uses). Before this the row
/// carried `root: ""`, so a project-scoped review could not see it.
#[test]
fn an_unattributed_write_row_names_the_project_it_was_about_to_write_into() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    assert_eq!(
        reg.gate(
            None,
            LatchRoute::Native,
            "context_note",
            ON,
            CallProvenance::internal_in(TEST_ROOT),
        ),
        Ok(WriteTaint::Unattributed)
    );
    let rows = outbound::test_rows::drain();
    let held = outbound::test_rows::of_screen(&rows, outbound::Screen::MemoryQuarantine);
    assert_eq!(held.len(), 1, "one held note, one review-queue row");
    assert_eq!(held[0].entry.root, TEST_ROOT);
    assert!(!held[0].entry.root.is_empty());
}

/// #48 (2026-08-08 re-review), M-19 — the identity-less PERSISTENT-WRITE.
///
/// This case used to be the tail of the test above, asserting
/// `Ok(WriteTaint::Clean)` under the comment *"no tab identity ⇒ no scope to
/// latch and none to taint"*. The first half of that is locked (F-5/H-8) and
/// still holds; the second half was the defect — a note nobody could
/// attribute, stored as ordinary auto-injecting memory, while the headless
/// path refuses the very same call for the very same missing facts.
///
/// Three properties, and dropping any one of them re-opens something:
/// the write is HELD (not clean, and not refused — locked decision 10);
/// it is held as `Unattributed`, so the model gets the true reason rather
/// than a claim about external content; and it still creates no latch row,
/// so the fail-open the fix is *not* touching stays untouched.
#[test]
fn an_identityless_persistent_write_is_held_not_stored_clean() {
    let reg = LatchRegistry::default();
    assert_eq!(
        reg.gate(
            None,
            LatchRoute::Native,
            "context_note",
            ON,
            NATIVE_IN_PROJECT
        ),
        Ok(WriteTaint::Unattributed)
    );
    assert!(WriteTaint::Unattributed.is_quarantined());
    assert_eq!(
        WriteTaint::Unattributed.write_notice(),
        Some(toolclass::UNATTRIBUTED_WRITE_NOTICE),
        "and it is explained as itself, not as an external-content quarantine"
    );
    assert!(
        reg.snapshot().is_empty(),
        "an identityless call still creates no latch row"
    );

    // Locked decision 16: this is a QUARANTINE decision, so the quarantine
    // switch turns it off — and the latch switch does not. Without the
    // second assertion the feature switch could be wired to the wrong half
    // and nothing would notice.
    let latch_only = GatePolicy {
        latch: true,
        quarantine: false,
    };
    assert_eq!(
        reg.gate(
            None,
            LatchRoute::Native,
            "context_note",
            latch_only,
            NO_CONTENT
        ),
        Ok(WriteTaint::Clean)
    );
    let quarantine_only = GatePolicy {
        latch: false,
        quarantine: true,
    };
    assert_eq!(
        reg.gate(
            None,
            LatchRoute::Native,
            "context_note",
            quarantine_only,
            NATIVE_IN_PROJECT
        ),
        Ok(WriteTaint::Unattributed)
    );
}

// ── V32 Phase H — the OpenCode native-tool gate's backend half ─────────

/// An OpenCode scope for `tab`.
fn oc_scope(tab: &str, session: Option<&str>) -> LatchScope {
    LatchScope {
        agent: "opencode",
        tab: tab.to_string(),
        session: session.map(str::to_string),
        root: TEST_ROOT.to_string(),
    }
}

/// Settings carrying the builtin OpenCode tab, so a per-tab L3 cell has a
/// tab to attach to (`Settings::default()` ships an EMPTY tab list).
fn oc_settings() -> (crate::settings::Settings, String) {
    // All-`Inherit`, not the V39 shipping row: these tests move ONE level
    // at a time — see `settings::ai_tab_inheriting_injection`.
    let tab = match crate::settings::ai_tab_inheriting_injection(
        crate::settings::default_opencode_tab(),
    ) {
        crate::settings::TabConfig::AiTool(c) => c,
        _ => unreachable!("default_opencode_tab is an AI tool tab"),
    };
    let id = tab.id.clone();
    (
        crate::settings::Settings {
            tabs: vec![crate::settings::TabConfig::AiTool(tab)],
            ..Default::default()
        },
        id,
    )
}

/// The verdict the plugin is handed: it needs the Phase H feature AND the
/// taint latch to resolve on, and goes off the moment the master switch
/// does.
///
/// The **fixture** is an all-`Inherit` tab (`oc_settings`), so this reads
/// the app-wide levels. What a real, newly created tab answers is the V39
/// per-tab baseline — every cell `Off` — and that is pinned in
/// `settings::injection`, not restated here.
#[test]
fn the_native_gate_verdict_needs_its_feature_and_the_latch_too() {
    use crate::settings::injection::{Feature, Override};
    let (mut s, id) = oc_settings();
    let scope = oc_scope(&id, Some("ses"));
    // Stated rather than assumed: this L2 shipped `false` under locked
    // decision 17 and ships `true` since V39, and the properties below are
    // about the transitions, not about the shipping value.
    s.set_l2_for_test(Feature::HarnessNativeGate, false);
    assert!(!native_gate_verdict(&s, scope.injection()));

    // The app-wide L2.
    s.set_l2_for_test(Feature::HarnessNativeGate, true);
    assert!(native_gate_verdict(&s, scope.injection()));

    // The taint latch is what this gate enforces — with that feature off
    // there is no boundary to enforce, so the gate reports off LIVE (no tab
    // restart), even though its own flag stays baked in the plugin.
    s.set_l2_for_test(Feature::TaintLatch, false);
    assert!(!native_gate_verdict(&s, scope.injection()));
    s.set_l2_for_test(Feature::TaintLatch, true);

    // The usual way in: L2 off app-wide, one tab's L3 `On`.
    s.set_l2_for_test(Feature::HarnessNativeGate, false);
    s.set_tab_override_for_test(&id, Feature::HarnessNativeGate, Override::On)
        .expect("the OpenCode tab carries a native-gate cell");
    assert!(
        native_gate_verdict(&s, scope.injection()),
        "an L3 On enables one tab"
    );
    assert!(
        !native_gate_verdict(&s, oc_scope("some-other-tab", Some("ses")).injection()),
        "and only that tab"
    );

    // Nothing re-enables past the master.
    s.set_master_for_test(false);
    assert!(!native_gate_verdict(&s, scope.injection()));
}

/// **#48 (A2-1): a tab id the settings no longer carry is not a hard OFF.**
///
/// #45 folded "not a configured tab" into `latch_scope`'s `None`, and
/// `handle_latch_state` mapped that `None` to `(false, default)` — so the
/// Phase H gate reported OFF for an id that had simply gone stale. That is
/// the ordinary case, not an exotic one: the OpenCode plugin is written per
/// working *directory* with one tab id baked in (the unfixed H-2), so
/// removing or re-id'ing a tab leaves the file naming an id settings no
/// longer have — and "the user switched containment off" and "cImp could
/// not find your tab" then rendered identically to the plugin.
///
/// The verdict now follows the resolved scope, which is the unknown
/// caller's for both identity-less shapes. Asserted as the *equality* the
/// fix is about: an unknown id answers what an unattributed call answers,
/// whatever that is.
///
/// **Renamed with #48 F-35** (was
/// `…_resolves_the_app_wide_gate_verdict_…`): locked decision 36 split
/// `Scope::App` into `Scope::AppWide` and `Scope::UnknownCaller`, and this
/// test asserts the second one. "App-wide" stopped describing it — the
/// resolved answer here also carries any configured tab's L3 `On` (N-1),
/// which the app-wide baseline does not.
#[test]
fn an_unknown_tab_id_resolves_as_an_unknown_caller_not_a_hard_off() {
    use crate::settings::injection::{Feature, Scope};
    let (mut s, _id) = oc_settings();
    let stale = LatchScoping::Unknown("opencode-removed".to_string());
    let anon = LatchScoping::Anonymous;
    assert!(matches!(stale.injection(), Scope::UnknownCaller));
    assert!(matches!(anon.injection(), Scope::UnknownCaller));

    // Off app-wide ⇒ off for a stale id. (The regression was invisible in
    // this direction, which is why #45 shipped.) The `off` is written here
    // rather than inherited from a default: V39 ships this L2 on.
    s.set_l2_for_test(Feature::HarnessNativeGate, false);
    assert!(!native_gate_verdict(&s, stale.injection()));

    // ON app-wide ⇒ ON for a stale id. This is the assertion that fails if
    // the hard-off comes back.
    s.set_l2_for_test(Feature::HarnessNativeGate, true);
    assert!(
        native_gate_verdict(&s, stale.injection()),
        "a stale tab id must inherit the app-wide verdict, not report off"
    );
    assert_eq!(
        native_gate_verdict(&s, stale.injection()),
        native_gate_verdict(&s, Scope::UnknownCaller),
        "and it must be the SAME answer an unattributed call gives, by construction"
    );

    // Through the reply the plugin actually reads, which is where the
    // regression lived: a `match` arm mapping "no usable identity" to a
    // hard-off verdict. The `latch` stays `open` because an unknown id keys
    // no registry entry — that part is #45's bound and is deliberate.
    let reply = latch_state_reply(&s, &stale, LatchView::default());
    assert_eq!(reply["gate"], true, "{reply}");
    assert_eq!(reply["latch"], "open", "{reply}");
    assert_eq!(reply["contaminated"], false, "{reply}");
    assert_eq!(
        latch_state_reply(&s, &anon, LatchView::default())["gate"],
        true,
        "an identity-less body resolves the same app-wide verdict"
    );

    // #45's actual goal is untouched: an unusable id yields no scope, so
    // nothing can key a registry entry off it.
    assert!(stale.scope().is_none());
    assert!(anon.scope().is_none());
    assert!(stale.into_scope().is_none());

    // The latch still ANDs in, live — a stale id cannot resurrect a gate
    // whose boundary nobody is maintaining.
    s.set_l2_for_test(Feature::TaintLatch, false);
    assert!(!native_gate_verdict(
        &s,
        LatchScoping::Unknown("x".into()).injection()
    ));
}

/// #48 (A2-6): `/latch/beacon`'s `tool` is an arbitrary unbounded string
/// from a request body and it lands in an activity row, a `tracing` line
/// and (through the feed) the TTS surface. Bounded before any of them.
#[test]
fn a_beacon_tool_name_is_bounded_before_it_reaches_a_row() {
    assert_eq!(bounded_tool(Some("WebFetch")), "WebFetch");
    assert_eq!(bounded_tool(Some("  webfetch  ")), "webfetch");
    // Absent, empty and whitespace all take the same honest placeholder.
    for empty in [None, Some(""), Some("   ")] {
        assert_eq!(bounded_tool(empty), "(native web tool)", "{empty:?}");
    }
    let long = "A".repeat(5_000);
    let bounded = bounded_tool(Some(&long));
    assert_eq!(bounded.chars().count(), BEACON_TOOL_MAX + 1);
    assert!(bounded.ends_with('…'), "truncation is visible to a reader");
    // Truncated by CHARS: a multi-byte name cannot be cut mid-codepoint,
    // which would panic on a byte slice and produce mojibake in the feed.
    let wide = "→".repeat(200);
    let bounded = bounded_tool(Some(&wide));
    assert_eq!(bounded.chars().count(), BEACON_TOOL_MAX + 1);
    assert!(bounded.starts_with('→'));
    // Exactly at the bound: no ellipsis, nothing lost.
    let exact = "b".repeat(BEACON_TOOL_MAX);
    assert_eq!(bounded_tool(Some(&exact)), exact);
}

/// `view_for` is the gate's read path: it must answer for a tab the proxy
/// has never served WITHOUT creating a row (a poll is not a tool call), and
/// the answer must be the one that denies nothing.
#[test]
fn view_for_answers_open_for_an_unknown_tab_without_creating_a_row() {
    let reg = LatchRegistry::default();
    let view = reg.view_for(&oc_scope("never-served", Some("ses")));
    assert_eq!(view, LatchView::default());
    assert_eq!(view.latch, "open", "fail-open: nothing to deny against");
    assert!(
        reg.snapshot().is_empty(),
        "a state read must not materialize a latch row"
    );
}

/// The read path reports the live latch — including after the decision-15
/// override, which is what makes "switch to local" move the native gate with
/// it (locked decision 17's last sentence) — and it rotates a stale latch
/// with the session, so a fresh conversation is never denied `read`/`bash`
/// on the strength of the previous one's fetch.
#[test]
fn view_for_tracks_the_latch_including_overrides_and_session_rotation() {
    let reg = LatchRegistry::default();
    let s = oc_scope("opencode", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    // EXTERNAL ⇒ the plugin denies the local natives.
    let view = reg.view_for(&s);
    assert_eq!(view.latch, "external");
    assert!(view.contaminated);

    // Decision 15's workflow button flips the boundary; the gate follows,
    // because it reads live state rather than caching a verdict.
    reg.apply_override(&s, LatchOverride::FlipLocal).unwrap();
    let view = reg.view_for(&s);
    assert_eq!(view.latch, "local", "the web side is now the denied one");
    assert!(view.contaminated, "an override never un-reads a page");

    // A tab restart rotates the session, and the read path sees it — a
    // stale `external` here would deny the whole local surface for a fresh
    // conversation.
    let after = oc_scope("opencode", Some("sess-b"));
    assert_eq!(reg.view_for(&after).latch, "open");
}

/// Per-tab isolation: one contaminated tab must not disarm (or arm) any
/// other, and the same tab id under a different agent is a different tab.
#[test]
fn latches_are_isolated_per_tab_and_per_agent() {
    let reg = LatchRegistry::default();
    let a = scope("claude-1", Some("sess-a"));
    let b = scope("claude-2", Some("sess-b"));
    let opencode = LatchScope {
        agent: "opencode",
        tab: "claude-1".to_string(),
        session: Some("sess-c".to_string()),
        root: TEST_ROOT.to_string(),
    };

    assert!(reg
        .gate(Some(&a), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    assert_eq!(
        reg.gate(
            Some(&a),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        ),
        Err(REFUSAL_LOCAL_BLOCKED)
    );
    // Tab B is untouched, and may latch the OTHER way.
    assert!(reg
        .gate(
            Some(&b),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(
        reg.gate(Some(&b), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
        Err(REFUSAL_EXTERNAL_BLOCKED)
    );
    // Same tab STRING, different agent ⇒ its own scope.
    assert!(reg
        .gate(
            Some(&opencode),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());

    let rows = reg.snapshot();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .map(|r| (r.consumer, r.tab.as_str(), r.latch()))
            .collect::<Vec<_>>(),
        [
            ("claude", "claude-1", "external"),
            ("claude", "claude-2", "local"),
            ("opencode", "claude-1", "local"),
        ]
    );
}

/// Live-verify 5: a tab restart starts unlatched. The tab id is
/// config-derived and never rotates, so the reset rides the SESSION id the
/// V28 registry re-stamps when the new harness session comes up.
#[test]
fn a_new_session_for_the_same_tab_starts_unlatched() {
    let reg = LatchRegistry::default();
    let before = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&before),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(
        reg.gate(
            Some(&before),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        ),
        Err(REFUSAL_LOCAL_BLOCKED)
    );

    // Tab restarted: same tab id, new session.
    let after = scope("claude-1", Some("sess-b"));
    assert!(reg
        .gate(
            Some(&after),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    let rows = reg.snapshot();
    assert_eq!(rows.len(), 1, "the restart reuses the tab's row: {rows:?}");
    assert_eq!(rows[0].session.as_deref(), Some("sess-b"));
    assert_eq!(rows[0].latch(), "local");
}

/// A withheld session id is absence of evidence, not evidence of a
/// restart — otherwise an injected model could reset its own latch by
/// calling until the registry blinked (TTL staleness, the H1 same-root
/// ambiguity). The latch survives; a later real id adopts the same scope.
#[test]
fn a_withheld_session_neither_resets_nor_splits_the_latch() {
    let reg = LatchRegistry::default();
    // Latched before the registry knew any session at all.
    let unknown = scope("claude-1", None);
    assert!(reg
        .gate(
            Some(&unknown),
            LatchRoute::Proxied,
            "ddg__search",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(
        reg.gate(
            Some(&unknown),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        ),
        Err(REFUSAL_LOCAL_BLOCKED)
    );

    // The session becomes known: same conversation, so the latch carries.
    let known = scope("claude-1", Some("sess-a"));
    assert_eq!(
        reg.gate(
            Some(&known),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        ),
        Err(REFUSAL_LOCAL_BLOCKED)
    );
    assert_eq!(reg.snapshot()[0].session.as_deref(), Some("sess-a"));

    // The registry blinks again: still no reset.
    assert_eq!(
        reg.gate(
            Some(&unknown),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        ),
        Err(REFUSAL_LOCAL_BLOCKED)
    );
    assert_eq!(
        reg.snapshot()[0].session.as_deref(),
        Some("sess-a"),
        "a withheld id must not erase the known one"
    );
}

/// Locked fail-open rule: a call with no tab identity (a child spawned
/// before `--tab`) is never gated. It is deliberately NOT folded into a
/// global latch — one identityless call would then latch every consumer.
/// Its EXTERNAL results are still spotlight-wrapped (that needs no
/// identity; see `handle_mcp_call`).
///
/// #48 M-19 narrows this to what it always meant: never *refused*, and
/// never latching. The one PERSISTENT-WRITE is admitted too — and held, see
/// `an_identityless_persistent_write_is_held_not_stored_clean`. Asserted
/// per name rather than with `.is_ok()`, because `.is_ok()` is true of
/// every verdict this function can return and so says nothing about which
/// one each name got.
#[test]
fn an_identityless_call_is_never_gated() {
    let reg = LatchRegistry::default();
    for (route, name, taint) in [
        (LatchRoute::Proxied, "ddg__fetch_content", WriteTaint::Clean),
        (LatchRoute::Native, "graph_snippet", WriteTaint::Clean),
        (LatchRoute::Proxied, "ddg__search", WriteTaint::Clean),
        (LatchRoute::Native, "context_note", WriteTaint::Unattributed),
    ] {
        assert_eq!(
            reg.gate(None, route, name, ON, NATIVE_IN_PROJECT),
            Ok(taint),
            "{name}"
        );
    }
    assert!(
        reg.snapshot().is_empty(),
        "an identityless call must not create a latch row"
    );
    // And it does not leak into a tab that DOES have identity.
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
}

/// A refused call must never engage or flip the latch: otherwise a
/// hallucinated (or injected) call to the blocked side could redefine which
/// side of the boundary the session is on.
#[test]
fn a_refused_call_does_not_move_the_latch() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    for _ in 0..3 {
        assert_eq!(
            reg.gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            ),
            Err(REFUSAL_LOCAL_BLOCKED)
        );
        assert_eq!(reg.snapshot()[0].latch(), "external");
    }
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(reg.snapshot()[0].latch(), "external");
}

/// `/graph_run` cannot serve a proxied server's content, so a name that
/// classifies EXTERNAL there is a typo or a hallucination — `run_graph_tool`
/// answers "unknown tool". It must not latch the tab: one bad tool name
/// would otherwise cost the session its local graph tools until restart.
#[test]
fn an_unserveable_name_on_the_native_route_does_not_latch_the_tab() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    for junk in ["graph_", "graph_nosuchtool", "ddg__search", ""] {
        assert!(
            reg.gate(Some(&s), LatchRoute::Native, junk, ON, NO_CONTENT)
                .is_ok(),
            "{junk}"
        );
    }
    assert!(
        reg.snapshot().is_empty(),
        "an unserveable native name must leave the tab unlatched"
    );
    // The real local-capability call that follows still latches normally.
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(reg.snapshot()[0].latch(), "local");
}

/// `/status`'s Phase B shape: the `Latch::label()` vocabulary plus the
/// identity needed to tell whose latch it is. Asserted key-by-key (rather
/// than as a whole-object equality) so V32 Phase F's additions — which
/// flatten alongside these — cannot break the guarantee this test exists
/// for: `latch` stays a TOP-LEVEL key with the three-label vocabulary.
/// The full Phase F object is pinned by
/// `status_snapshot_carries_contamination_and_override_availability`.
#[test]
fn status_snapshot_serializes_the_latch_labels() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    let json = serde_json::to_value(reg.snapshot()).unwrap();
    let row = &json[0];
    assert_eq!(row["consumer"], "claude");
    assert_eq!(row["tab"], "claude-1");
    assert_eq!(row["session"], "sess-a");
    assert_eq!(row["latch"], "external");
}

// ── V32 Phase C — the proxy's per-session EXTERNAL fetch budget ─────────

const TEST_LIMITS: outbound::BudgetLimits = outbound::BudgetLimits {
    max_calls: 3,
    max_bytes: 1000,
};

/// The count half: three proxied calls, then every further one is refused
/// with the fixed string — and the fourth refusal is the same as the first
/// (a spent budget does not un-spend).
#[test]
fn the_session_budget_stops_a_fetch_loop() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    for _ in 0..3 {
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert!(reg
            .budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content")
            .is_ok());
        reg.charge(Some(&s), 10);
    }
    for _ in 0..3 {
        assert_eq!(
            reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content"),
            Err(outbound::REFUSAL_BUDGET)
        );
    }
}

/// The byte half, and the fact that it bites on the call AFTER the one
/// that crossed the cap (a response's size is unknowable beforehand).
#[test]
fn the_session_budget_also_counts_bytes() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert!(reg
        .budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content")
        .is_ok());
    reg.charge(Some(&s), 999);
    assert!(reg
        .budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content")
        .is_ok());
    reg.charge(Some(&s), 1);
    assert_eq!(
        reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content"),
        Err(outbound::REFUSAL_BUDGET)
    );
}

/// #48 (finding D-3) — **a FAILED proxied fetch advances the call
/// counter.** The charge sat on the `Ok` arm alone, so a loop of fetches
/// against a host that 500s advanced nothing and never exhausted the
/// budget: the one screen whose whole purpose is stopping a loop was blind
/// to the loop that costs least to run. The worker's copy of the same
/// contract charged both arms (an `Err` there becomes an `ERROR: …` tool
/// result with `executed = true`), so the two paths disagreed.
///
/// Driven through `charge_call` — the exact function the handler calls, in
/// one unconditional statement above the match it used to live inside.
#[test]
fn a_failed_proxy_fetch_still_advances_the_call_counter() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    let failure: Result<String, String> = Err("upstream 500".into());
    for _ in 0..3 {
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert!(reg
            .budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content")
            .is_ok());
        reg.charge_call(Some(&s), &failure);
    }
    assert_eq!(
        reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content"),
        Err(outbound::REFUSAL_BUDGET),
        "three failed fetches must spend the three-call budget"
    );
    // Zero bytes, though: nothing was ingested. The call cap is what stops
    // a loop; the byte cap is about content that arrived.
    let reg = LatchRegistry::default();
    let s = scope("claude-2", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    reg.charge_call(Some(&s), &failure);
    reg.charge_call::<String>(Some(&s), &Ok("x".repeat(999)));
    assert!(
        reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content")
            .is_ok(),
        "999 bytes is under the 1000-byte cap — the failure contributed none"
    );
}

/// #48 — the SSRF denial row is bounded per tab session, and the bound
/// resets on a proved session rotation.
///
/// Every denial used to write a row with no dedup at all, while the feed
/// was one 200-row window evicted oldest-first within a kind: a model
/// looping denied URLs destroyed the `Canary`, `LatchBeacon` and
/// `MemoryQuarantine` rows that are the only record of an attack that got
/// through. Finding H-9 closed the cross-screen half of that at the store
/// (`activity::Lane` — one window per screen, so a loop costs only its own
/// screen's history); this ledger is what keeps a loop from evicting the
/// SSRF screen's own first denials. A process-global set keyed on the scope
/// string was the wrong
/// shape — proxy scopes are stable `agent:tab`, so it would suppress a
/// tab's rows across every future session — which is why the ledger rides
/// the tab's `Budget`.
#[test]
fn ssrf_denial_rows_are_bounded_per_session_and_reset_on_rotation() {
    use outbound::DoublingRow;
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    // Drive the registry's own ledger the way `TabAudit` does.
    let claim = || {
        reg.claim(
            Some(&s),
            outbound::Budget::claim_ssrf_flag,
            || DoublingRow::Suppress,
        )
    };
    let written: Vec<u32> = (0..200)
        .filter_map(|_| match claim() {
            DoublingRow::Write { total, .. } => Some(total),
            DoublingRow::Suppress => None,
        })
        .collect();
    assert_eq!(
        written,
        vec![1, 2, 4, 8, 16, 32, 64, 128],
        "200 denials cost the capped feed 8 rows, not 200"
    );
    // The first denial still reports immediately — a single one behaves
    // exactly as it always did.
    let fresh = LatchRegistry::default();
    let f = scope("claude-2", Some("sess-a"));
    assert!(fresh
        .gate(Some(&f), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    assert!(matches!(
        fresh.claim(
            Some(&f),
            outbound::Budget::claim_ssrf_flag,
            || DoublingRow::Suppress
        ),
        DoublingRow::Write { total: 1, .. }
    ));

    // A new conversation is entitled to its own rows: the rotation that
    // resets the budget resets the ledger with it.
    let rotated = scope("claude-1", Some("sess-b"));
    assert!(reg
        .gate(
            Some(&rotated),
            LatchRoute::Proxied,
            "ddg__search",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert!(matches!(
        reg.claim(
            Some(&rotated),
            outbound::Budget::claim_ssrf_flag,
            || DoublingRow::Suppress
        ),
        DoublingRow::Write { total: 1, .. }
    ));

}

/// #48 finding F-40 — the identity-less scope is LEDGERED, and this is the
/// split half of the assertion that used to live at the end of
/// `ssrf_denial_rows_are_bounded_per_session_and_reset_on_rotation`.
///
/// **Do not merge it back.** The old assertion was
/// `matches!(TabAudit(None).claim_ssrf(), Write { .. })` under the comment
/// *"it reports — the same fail-open the latch and the budget take"*, and it
/// stayed green for exactly the behaviour F-40 measured in the field: a
/// caller with no tab wrote **one row per denial** (~72 denials → ~64 rows)
/// where an attributed scope wrote `log2(n)` (20 → 4). Both halves of that
/// old line are still asserted here — it still reports, and it is now
/// bounded — but as two claims, so loosening one cannot hide behind the other.
///
/// Assertions are stated as **bounds, not exact totals**, because the ledger
/// is process-global by design ([`outbound::UnscopedAudit`]): any other test
/// in this binary that claims unscoped shifts the starting point, and a test
/// that would fail when its neighbours run is worse than no test.
#[test]
fn an_identity_less_call_reports_but_is_still_ledgered() {
    use outbound::{DoublingRow, ScopeAudit};

    // `gate` has never run for this one, so it takes the no-entry path —
    // the same fallback an absent, unknown or shell `tab` reaches.
    let unscoped = TabAudit(None, "claude");

    // It still REPORTS: a lone denial behaves as it always did.
    let first = unscoped.claim_ssrf();
    assert!(
        matches!(first, DoublingRow::Write { .. }),
        "an identity-less denial must still be able to write a row: {first:?}"
    );

    // It is now LEDGERED. `total: 0` was the wire-visible signature of the
    // unledgered fallback this finding removed, and it is what
    // `ssrf_flag_detail` would have to render as "denial #0".
    for _ in 0..64 {
        if let DoublingRow::Write { total, .. } = unscoped.claim_ssrf() {
            assert!(total >= 1, "a written row must count itself");
        }
    }

    // And it is BOUNDED: 128 further denials cost the capped `Ssrf` lane at
    // most a handful of rows, not 128. `log2(128) + 1 = 8` is the ceiling
    // even from a counter starting at zero, so this holds wherever the
    // shared ledger happens to be.
    let written = (0..128)
        .filter(|_| matches!(unscoped.claim_ssrf(), DoublingRow::Write { .. }))
        .count();
    assert!(
        written <= 8,
        "128 identity-less denials wrote {written} rows; the doubling bounds it to 8"
    );

    // The unscreened bit is a hard one-per-scope claim, not a doubling, and
    // the identity-less scope is one scope — so across many calls it is
    // claimable at most once. (It may already be spent by an earlier test;
    // "never twice" is the property, and it is the one that matters.)
    let claims = (0..16).filter(|_| unscoped.claim_unscreened()).count();
    assert!(
        claims <= 1,
        "the one unscreened row per scope was claimed {claims} times"
    );
}

/// #48 (finding A-1, proxy side) — restated as the shared rule the worker
/// now uses too. A bare name that classifies EXTERNAL is a hallucination,
/// and every proxied id contains `__` by construction, so the restrictive
/// unknown-⇒-EXTERNAL default still governs every name that can carry
/// external content.
#[test]
fn the_route_rule_is_one_definition_shared_with_the_worker() {
    assert_eq!(LatchRoute::of_tool("graph_symbols"), LatchRoute::Native);
    assert_eq!(LatchRoute::of_tool("read_file"), LatchRoute::Native);
    assert_eq!(LatchRoute::of_tool("ddg__search"), LatchRoute::Proxied);
    assert_eq!(
        LatchRoute::of_tool("somenewserver__anything"),
        LatchRoute::Proxied
    );
    assert!(LatchRoute::Proxied.external_is_content());
    assert!(!LatchRoute::Native.external_is_content());
}

/// **#48 (finding M-2) — `can_execute`, the rule A-1 and M-2 share, and the
/// two ways it must NOT over-reach.**
///
/// The whole risk of widening the wave-through set is that it stops being
/// about names that cannot run. All three variants are asserted here, and
/// the `Hook` row is the one that matters most: the three hook names are
/// exactly the `unrouted` rows, and applying M-2's rule to their own route
/// would wave through the gate M-7 built.
#[test]
fn can_execute_covers_the_unroutable_names_without_reaching_the_hook_routes() {
    let cls = toolclass::classify;
    // Native: a real tool executes; a typo and an unroutable classified
    // name do not.
    for real in [
        "read_file",
        "graph_snippet",
        "context_note",
        "graph_outline",
    ] {
        assert!(
            LatchRoute::Native.can_execute(real, cls(real)),
            "{real} must still be gated"
        );
    }
    for dead in ["graph_symbols", "definitely_not_a_tool", ""] {
        assert!(!LatchRoute::Native.can_execute(dead, cls(dead)), "{dead}");
    }
    for unrouted in ["Bash", "Edit", "Write", "hook_post_edit", "hook_compaction"] {
        assert!(
            !LatchRoute::Native.can_execute(unrouted, cls(unrouted)),
            "{unrouted} reaches no native dispatcher, so it must not move a latch"
        );
    }
    // Hook: the name is cImp's own and IS the route, so M-2's rule must not
    // apply — otherwise `/context/post_edit` stops being refusable and
    // M-7's fix silently unwinds.
    for hook in [
        HOOK_TOOL_POST_EDIT,
        HOOK_TOOL_SHOULD_READ,
        HOOK_TOOL_COMPACTION,
    ] {
        assert!(
            LatchRoute::Hook.can_execute(hook, cls(hook)),
            "{hook} must still be gated on its own route (M-7)"
        );
    }
    // …asserted end-to-end and not just on the predicate: a contaminated
    // tab is still refused `/context/post_edit`.
    let reg = LatchRegistry::default();
    let s = scope("claude-hook", Some("ses"));
    assert!(reg
        .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Hook,
            HOOK_TOOL_POST_EDIT,
            ON,
            NO_CONTENT
        ),
        Err(toolclass::REFUSAL_LOCAL_BLOCKED),
        "M-7: a contaminated conversation must not run the project's checks"
    );
    // …while the same name arriving as a model's tool call is simply not a
    // tool: neither refused nor latching.
    let reg = LatchRegistry::default();
    let s = scope("claude-native", Some("ses"));
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Native,
            HOOK_TOOL_POST_EDIT,
            ON,
            NO_CONTENT
        ),
        Ok(WriteTaint::Clean)
    );
    assert!(
        reg.snapshot().is_empty(),
        "a name no dispatcher serves must leave the tab unlatched"
    );
    // Proxied: every id here is a real proxied id, so the rule never
    // applies — an unknown one is untrusted content, not a typo.
    for id in ["ddg__search", "somenewserver__anything"] {
        assert!(LatchRoute::Proxied.can_execute(id, cls(id)), "{id}");
    }
}

/// Budgets are scoped exactly like the latch: per tab, and reset when the
/// tab's SESSION rotates (a tab restart). A withheld session id is not a
/// rotation — otherwise a model could reset its budget by waiting for the
/// V28 registry to blink.
#[test]
fn the_session_budget_is_per_tab_and_resets_on_session_rotation() {
    let reg = LatchRegistry::default();
    let a = scope("claude-1", Some("sess-a"));
    let b = scope("claude-2", Some("sess-b"));
    for _ in 0..3 {
        assert!(reg
            .gate(Some(&a), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        reg.charge(Some(&a), 1);
    }
    assert_eq!(
        reg.budget_gate(Some(&a), TEST_LIMITS, "ddg__search"),
        Err(outbound::REFUSAL_BUDGET)
    );
    // A different tab is untouched.
    assert!(reg
        .gate(Some(&b), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    assert!(reg
        .budget_gate(Some(&b), TEST_LIMITS, "ddg__search")
        .is_ok());

    // The registry withholding a session must NOT reset the budget.
    let a_silent = scope("claude-1", None);
    assert!(reg
        .gate(
            Some(&a_silent),
            LatchRoute::Proxied,
            "ddg__search",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(
        reg.budget_gate(Some(&a_silent), TEST_LIMITS, "ddg__search"),
        Err(outbound::REFUSAL_BUDGET)
    );

    // A genuinely new session does.
    let a2 = scope("claude-1", Some("sess-a2"));
    assert!(reg
        .gate(
            Some(&a2),
            LatchRoute::Proxied,
            "ddg__search",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert!(reg
        .budget_gate(Some(&a2), TEST_LIMITS, "ddg__search")
        .is_ok());
}

// ── V32 Phase F — native-web beacons + the manual override ──────────────

/// Locked decision 14: a beacon does exactly what an admitted proxied
/// EXTERNAL call does — engages the tab's latch and contaminates the
/// conversation — so the harness's own web tool stops being invisible to
/// containment. The proxied local-capability side closes as a result.
#[test]
fn a_native_web_beacon_engages_the_external_latch_like_a_proxied_fetch() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
    let view = out.view;
    assert_eq!(view.latch, "external");
    assert!(view.contaminated);
    assert!(view.can_flip_local);
    assert!(view.can_unlatch);
    // #45: the transition is reported, so the handler can write exactly one
    // origin-marked activity row for it.
    assert!(out.engaged, "the beacon MOVED the latch and must say so");
    assert_eq!(reg.snapshot()[0].latch(), "external");
    // ...and the containment that follows is the ordinary Phase B one.
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        ),
        Err(REFUSAL_LOCAL_BLOCKED)
    );
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Quarantined)
    );
}

/// Fail-open on identity, like every other gate here: a beacon with no tab
/// id has nothing to engage and must not crash, latch anything globally, or
/// invent a row. A beacon for a tab the proxy has never served creates that
/// tab's row, exactly as its first gated call would have.
#[test]
fn a_beacon_without_tab_identity_is_a_no_op_and_an_unknown_tab_is_created() {
    let reg = LatchRegistry::default();
    let out = reg.beacon(None, "WebSearch", ON, BEACON_PROV);
    assert_eq!(out, BeaconOutcome::inert());
    assert!(
        reg.snapshot().is_empty(),
        "an identityless beacon must not create a row"
    );
    // First contact for this tab is the beacon itself.
    let fresh = scope("claude-9", Some("sess-z"));
    assert_eq!(
        reg.beacon(Some(&fresh), "WebFetch", ON, BEACON_PROV)
            .view
            .latch,
        "external"
    );
    let rows = reg.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tab, "claude-9");
}

/// A beacon arriving while the tab is LOCAL-latched cannot refuse the fetch
/// — the harness already ran it — so it records the contamination and
/// leaves the latch where it is. That is the honest reading: this
/// conversation has now seen external content, and its proxied external
/// side stays closed.
#[test]
fn a_beacon_under_a_local_latch_contaminates_without_flipping() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
    assert_eq!(
        out.view.latch, "local",
        "sticky: a beacon never flips a latch"
    );
    assert!(out.view.contaminated);
    // #45: no transition ⇒ no activity row. The contamination is real, but
    // the latch did not move, and a row per beacon would let a caller flood
    // the feed.
    assert!(!out.engaged);
    // The contamination is what bites: the memory write is quarantined even
    // though the latch says `local`.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Quarantined)
    );
}

/// Locked decision 15's state machine. `flip_local` applies ONLY from
/// External (there is nothing to flip from Open, and from Local it would be
/// a no-op that reads like an action); `unlatch` applies from either
/// latched state and not from Open.
#[test]
fn flip_local_applies_only_from_external_and_unlatch_from_any_latch() {
    // Open: neither move applies.
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_outline",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert!(reg
        .apply_override(&s, LatchOverride::FlipLocal)
        .is_err_and(|e| e.contains("EXTERNAL-latched")));
    assert!(reg
        .apply_override(&s, LatchOverride::Unlatch)
        .is_err_and(|e| e.contains("not latched")));

    // Local: flip is refused (it is already there), unlatch works.
    let reg = LatchRegistry::default();
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert!(reg.apply_override(&s, LatchOverride::FlipLocal).is_err());
    let out = reg
        .apply_override(&s, LatchOverride::Unlatch)
        .expect("unlatch applies from local");
    assert_eq!(out.prior, Latch::Local);
    assert_eq!(out.view.latch, "open");

    // External: the flip is the workflow button.
    let reg = LatchRegistry::default();
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    let out = reg
        .apply_override(&s, LatchOverride::FlipLocal)
        .expect("flip applies from external");
    assert_eq!(out.prior, Latch::External);
    assert_eq!(out.view.latch, "local");
    assert!(out.view.contaminated);
    assert!(!out.view.can_flip_local, "no second flip to offer");
    assert!(out.view.can_unlatch);

    // A tab the proxy has never served has no latch to override at all.
    let reg = LatchRegistry::default();
    assert!(reg
        .apply_override(&s, LatchOverride::Unlatch)
        .is_err_and(|e| e.contains("nothing to override")));
}

/// The flip is the decision-15 workflow: research done, now apply it. It
/// restores the proxied local-capability tools and CLOSES the external side
/// in the same move — at no instant does the session hold both.
///
/// **#48 (F-34) SPLIT this test rather than loosening it.** It used to assert
/// `Err(REFUSAL_EXTERNAL_BLOCKED)` for the closed side, which pinned the
/// defect's *shape* — the string that says *"this task has already used a
/// local-capability tool"* about a latch no tool call moved. What it is FOR
/// is containment: after the flip the external side is closed, exactly and
/// only. That is what stays here, still asserted against an exact constant.
/// Which sentence each cause gets is
/// [`the_proxied_external_refusal_names_the_user_flip_only_when_a_user_flipped_it`],
/// and the old constant is still pinned there for the case where it is true.
#[test]
fn flip_local_reopens_local_tools_and_closes_the_external_side() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        ),
        Err(REFUSAL_LOCAL_BLOCKED)
    );
    reg.apply_override(&s, LatchOverride::FlipLocal)
        .expect("flip");
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
        Err(REFUSAL_EXTERNAL_USER_LOCAL),
        "the external side is closed — and by the flip, which is what closed it"
    );
}

/// **#48 (F-34): the proxied external refusal states the cause the gate
/// checked — F-23's twin, on the route that ships ON.**
///
/// The defect: after the user clicked "Switch to local", a proxied external
/// call was refused with `REFUSAL_EXTERNAL_BLOCKED` — *"this task has already
/// used a local-capability tool"*. False. No tool call latched that tab; the
/// user's own IPC flip did. Observed live, a tab's model believed the string
/// and told its user that `graph_snippet` had caused the latch: a confident,
/// wrong causal story about a security event.
///
/// Both halves are asserted, because the fix is a *split*, not a rename — the
/// old constant is the TRUE statement for a latch a tool call earned, and a
/// fix written as "local ⇒ the user did it" fails the first case below.
///
/// It also proves the invariant locked decision 34 inherits from F-23: **the
/// flag cannot outlive the latch it explains.** Both exits from `Local` are
/// walked on this route — the rotation reset and the unlatch — and after each
/// one a latch re-earned by a *tool call* is refused with the old constant
/// again. A stale `true` would be F-34 with the operands swapped: a tool
/// call's latch reported to the model as the user's decision.
#[test]
fn the_proxied_external_refusal_names_the_user_flip_only_when_a_user_flipped_it() {
    // 1. EARNED by a local-capability tool call. The pre-F-34 sentence is the
    //    true one here and must survive untouched.
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(reg.view_for(&s).latch, "local");
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
        Err(REFUSAL_EXTERNAL_BLOCKED),
        "a tool call really did close this side"
    );

    // 2. The finding's own path: fetch → EXTERNAL → the user's workflow flip.
    let reg = LatchRegistry::default();
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    reg.apply_override(&s, LatchOverride::FlipLocal)
        .expect("flip");
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
        Err(REFUSAL_EXTERNAL_USER_LOCAL),
        "no tool call closed this side, and the refusal must not say one did"
    );
    // Containment is byte-identical: only the sentence moved. The local side
    // is open (that is what the flip is FOR) and the write path is unchanged.
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    // …and the row a reviewer reads carries the same corrected sentence,
    // rather than the feed and the model being told different stories.
    outbound::test_rows::reset();
    let s2 = scope("claude-2", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s2),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    reg.apply_override(&s2, LatchOverride::FlipLocal)
        .expect("flip");
    let _ = outbound::test_rows::drain();
    assert_eq!(
        reg.gate(Some(&s2), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
        Err(REFUSAL_EXTERNAL_USER_LOCAL)
    );
    let rows = outbound::test_rows::drain();
    let refusals = outbound::test_rows::of_screen(&rows, outbound::Screen::LatchRefusal);
    assert_eq!(refusals.len(), 1);
    assert_eq!(
        refusals[0].response, REFUSAL_EXTERNAL_USER_LOCAL,
        "the incident row quotes what the model was told, verbatim"
    );

    // 3. The flag cannot outlive its latch — exit A, the rotation reset. The
    //    NEXT conversation's own `graph_snippet` is what closes its external
    //    side, and it must be told so.
    let rotated = scope("claude-1", Some("sess-b"));
    assert_eq!(reg.view_for(&rotated).latch, "open", "the rotation reopened");
    assert!(reg
        .gate(
            Some(&rotated),
            LatchRoute::Proxied,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(
        reg.gate(
            Some(&rotated),
            LatchRoute::Proxied,
            "ddg__search",
            ON,
            NO_CONTENT
        ),
        Err(REFUSAL_EXTERNAL_BLOCKED),
        "F-34 with the operands swapped: this one really was a tool call"
    );

    // 4. Exit B, the unlatch. Both sides open again, so there is nothing to
    //    explain; and a latch re-earned afterwards reports the tool call.
    let reg = LatchRegistry::default();
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    reg.apply_override(&s, LatchOverride::FlipLocal)
        .expect("flip");
    reg.apply_override(&s, LatchOverride::Unlatch)
        .expect("unlatch");
    assert!(reg
        .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Proxied,
            "graph_snippet",
            ON,
            NO_CONTENT
        ),
        Err(REFUSAL_LOCAL_BLOCKED),
        "the unlatch is not a free pass: the tab re-latched EXTERNAL"
    );

    // 5. The user flip is the ONLY thing that selects the new sentence. A
    //    tab whose contamination the user cleared, and one the user armed for
    //    a session clear, are user actions too — neither leaves the latch
    //    `local` by decision, and neither may borrow the flip's sentence.
    let (reg, s) = contaminated_local_registry();
    for action in [
        // Ordered so each precondition holds: the arm needs the bit set, the
        // clear consumes it.
        LatchOverride::AwaitSessionClear,
        LatchOverride::ClearContamination,
    ] {
        reg.apply_override(&s, action).expect("applies");
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_BLOCKED),
            "{action:?}: the latch is still the one graph_snippet earned"
        );
    }
}

/// **#48 (F-23): a `local` latch carries the reason it is `local`,** because
/// the two reasons are different statements and the native-web refusal has to
/// make the one it checked.
///
/// The defect: after the user's flip the OpenCode gate served
/// `REFUSAL_NATIVE_WEB_BLOCKED` — *"this session has already used a
/// local-capability tool"* — and a live tab's model believed it and told its
/// user that `graph_snippet` had latched the session. No such call happened;
/// a human clicked. The fix records WHY at the one site that knows and
/// publishes it on the wire the gate reads.
///
/// Every assertion here is about the FACT, not about the message: the message
/// is a fixed constant selected on this boolean, and which sentence the
/// generated plugin serves for it is pinned in `tabs::config`.
#[test]
fn a_user_flipped_local_latch_records_that_no_tool_call_closed_the_web_side() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));

    // A `local` latch EARNED by a local-capability tool: the pre-F-23
    // sentence is the true one here, so the flag must stay false. This is the
    // assertion that fails if the fix is written as "local ⇒ the user did it".
    assert!(reg
        .gate(Some(&s), LatchRoute::Native, "graph_snippet", ON, NO_CONTENT)
        .is_ok());
    let earned = reg.view_for(&s);
    assert_eq!(earned.latch, "local");
    assert!(!earned.local_by_user_flip, "a tool call latched this tab");

    // The finding's own path: fetch → EXTERNAL → the user's workflow flip.
    let reg = LatchRegistry::default();
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert!(!reg.view_for(&s).local_by_user_flip, "external, not flipped");
    let out = reg
        .apply_override(&s, LatchOverride::FlipLocal)
        .expect("flip");
    assert_eq!(out.view.latch, "local");
    assert!(
        out.view.local_by_user_flip,
        "the flip is the cause, and it is the app that knows it"
    );

    // It reaches the plugin on the wire it already reads — the whole point,
    // since the refusal is thrown inside the harness's own process.
    let (settings, _id) = oc_settings();
    let reply = latch_state_reply(
        &settings,
        &LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
        reg.view_for(&s),
    );
    assert_eq!(reply["latch"], "local", "{reply}");
    assert_eq!(reply["local_by_user_flip"], true, "{reply}");
    // A tab the proxy never served says `false` rather than nothing: the
    // plugin reads the field defensively, and "absent" must not be able to
    // mean "the user flipped it".
    assert_eq!(
        latch_state_reply(&settings, &LatchScoping::Anonymous, LatchView::default())
            ["local_by_user_flip"],
        false
    );

    // It cannot outlive the latch it explains — in either direction out of
    // `local`. An unlatch reopens both sides…
    reg.apply_override(&s, LatchOverride::Unlatch)
        .expect("unlatch");
    assert!(!reg.view_for(&s).local_by_user_flip, "web is open again");

    // …and a session rotation reopens the latch, after which the NEXT
    // conversation's own file read is what closed its web side. Left set, that
    // tab would be told a user flipped a latch nobody touched.
    let reg = LatchRegistry::default();
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    reg.apply_override(&s, LatchOverride::FlipLocal)
        .expect("flip");
    assert!(reg.view_for(&s).local_by_user_flip);
    let rotated = scope("claude-1", Some("sess-b"));
    let after = reg.view_for(&rotated);
    assert_eq!(after.latch, "open", "the rotation reopened the latch");
    assert!(!after.local_by_user_flip, "and the reason went with it");
    assert!(reg
        .gate(
            Some(&rotated),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    let relatched = reg.view_for(&rotated);
    assert_eq!(relatched.latch, "local");
    assert!(
        !relatched.local_by_user_flip,
        "F-23 with the operands swapped: this one really was a tool call"
    );
}

/// **The core Phase F invariant, as decision 15's 2026-08-10 amendment
/// leaves it.** Contamination is a property of the CONVERSATION, not of the
/// latch position: a note written after the *flip* was still composed by a
/// model that read an attacker's page, so persistence stays quarantined
/// through it.
///
/// H-2 extends it past the session boundary: this test used to end by
/// rotating the session and asserting a clean scope ("a tab restart, the one
/// clean exit the UI names"). It now asserts the opposite, because the
/// rotation signal comes from a file the model's own Bash can create — see
/// [`TabLatch::contaminated`]. The latch still reopens; the bit does not.
///
/// **Why the name narrowed** (it was
/// `contamination_survives_every_override_and_every_session_rotation`): the
/// user's 2026-08-10 decision moved `unlatch` out of this rule — *"if the
/// user restores full access then the tab should be cleared, it's the user's
/// decision."* The flip is a workflow step and keeps the bit; the unlatch is
/// a verdict and releases it, which is
/// `a_full_unlatch_clears_contamination_and_records_it` next door. Of the
/// four actions only `clear_contamination` and `unlatch` clear; the flip
/// never does, and the arm defers.
#[test]
fn contamination_survives_the_flip_and_every_session_rotation() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    let out = reg
        .apply_override(&s, LatchOverride::FlipLocal)
        .expect("the flip applies from external");
    assert!(
        out.view.contaminated,
        "the flip is a workflow step, not a verdict"
    );
    assert!(
        out.prior_taint.is_none(),
        "and it releases nothing, so it has no prior taint to record"
    );
    // The latch moved; the quarantine did not.
    assert_ne!(out.view.latch, "external");
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Quarantined),
        "a post-flip write must still be quarantined"
    );
    assert!(reg.snapshot()[0].view.contaminated);

    // H-2: a new session id reopens the latch — but the write is STILL
    // quarantined, because "the session rotated" is a claim sourced from
    // an attacker-writable transcript directory.
    let after = scope("claude-1", Some("sess-b"));
    assert_eq!(
        reg.gate(
            Some(&after),
            LatchRoute::Native,
            "context_note",
            ON,
            NO_CONTENT
        ),
        Ok(WriteTaint::Quarantined),
        "a rotation must not re-open the persistence channel"
    );
    let rows = reg.snapshot();
    assert!(rows[0].view.contaminated);
    assert_eq!(rows[0].latch(), "open");

    // And the fourth action likewise leaves the bit alone — one assertion,
    // because the full behaviour is `a_restore_arms_the_wait_and_clears_
    // nothing_now` and duplicating it here would give the rule two homes.
    let (armed_reg, armed_scope) = contaminated_registry();
    let armed = armed_reg
        .apply_override(&armed_scope, LatchOverride::AwaitSessionClear)
        .expect("arm");
    assert!(
        armed.view.contaminated,
        "the restore arm defers the clear; it does not perform one"
    );
}

/// **Decision 15's 2026-08-10 amendment** (user: *"if the user restores full
/// access then the tab should be cleared, it's the user's decision."*). One
/// invariant with several faces, so one test: the state, the payload the user
/// is buying, the prior state the audit rows quote, the two rows themselves,
/// and the two cases where nothing is released.
///
/// The trust root is the one that closed H-2 — **authority, not evidence**.
/// An attacker cannot click this; the click already hands back the strictly
/// more dangerous capability, so leaving persistent memory quarantined
/// afterwards overruled a judgement the product had just asked for.
#[test]
fn a_full_unlatch_clears_contamination_and_records_it() {
    outbound::test_rows::reset();
    let (reg, s) = contaminated_registry();

    // 1. The state. `can_clear` goes with the bit — there is nothing left to
    //    clear, so the popover must stop offering it.
    let out = reg
        .apply_override(&s, LatchOverride::Unlatch)
        .expect("a contaminated latched tab can be unlatched");
    assert!(!out.view.contaminated, "the flag went with the access");
    assert!(!out.view.can_clear, "and nothing is left to clear");
    assert!(!out.view.awaiting_session_clear);
    assert_eq!(out.view.latch, "open");
    assert!(!reg.snapshot()[0].view.contaminated);

    // 2. Prior state, captured BEFORE the latch moved. `external`, not
    //    `open`: this is the assertion that goes red if someone moves the
    //    clear after `entry.latch = Latch::Open`, which would make the audit
    //    row quote the state the click produced instead of the one it
    //    replaced.
    let prior = out.prior_taint.as_ref().expect("the clear happened here");
    assert_eq!(prior.latch, "external");
    assert_eq!(prior.session.as_deref(), Some("sess-a"));

    // 3. Two rows, right lanes, right words. Neither is written by the
    //    registry — both are composed here and filed by
    //    `apply_latch_override`, the IPC entry point, from one stated origin.
    let orow = override_row(outbound::Origin::Ipc, LatchOverride::Unlatch, &out);
    assert_eq!(orow.screen, outbound::Screen::LatchOverride);
    assert_eq!(orow.tool, "unlatch");
    let d = &orow.detail;
    assert!(d.contains("FULL access restored"), "{d}");
    assert!(d.contains("contaminated=true"), "the PRIOR state: {d}");
    assert!(d.contains("latch=external"), "the PRIOR latch: {d}");
    assert!(d.contains("STAY quarantined"), "decision 10 stated: {d}");

    let cleared = unlatch_clear_row(outbound::Origin::Ipc, LatchOverride::Unlatch, &s, &out)
        .expect("a release owes the contamination_cleared lane a row");
    assert_eq!(cleared.basis.tool(), "unlatch");
    assert_eq!(
        cleared.session.as_deref(),
        Some("sess-a"),
        "filed under the CONTAMINATED conversation, so it joins the row that opened it"
    );
    assert_eq!(cleared.root, TEST_ROOT, "an empty root defeats the row");
    let cd = &cleared.detail;
    assert!(cd.contains("basis: unlatch"), "{cd}");
    assert!(cd.contains("origin: ipc"), "{cd}");
    assert!(cd.contains("contaminated=true"), "the PRIOR state: {cd}");
    assert!(cd.contains("STAY quarantined"), "decision 10 stated: {cd}");

    // 4. Empty is not absent: the clear releases STATE, never EVIDENCE. The
    //    `contamination` row that set the bit is untouched by the override —
    //    what makes "cleared" distinguishable from "never contaminated" is
    //    that pair of rows, not the live view, which is now identical.
    let rows = outbound::test_rows::drain();
    assert_eq!(
        contamination_rows(&rows).len(),
        1,
        "the row that SET the bit is still in the feed after the release"
    );
    assert!(
        cleared_rows(&rows).is_empty(),
        "the release's own row is the IPC entry point's to write, exactly as the \
         resume's is — see `unlatch_clear_row`"
    );

    // 5. The payload the user is actually buying, and the reason this action
    //    and not the flip: BOTH holds are released, so the next memory write
    //    is stored clean.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Clean),
        "decision 15's 2026-08-10 amendment: restoring full access releases the \
         flag too — the user's decision"
    );

    // 6. The honest `None`: an unlatch on a tab that was never contaminated
    //    is still legal, releases nothing, and must not write a row claiming
    //    a bit was released.
    let clean = LatchRegistry::default();
    let cs = scope("claude-2", Some("sess-c"));
    assert!(clean
        .gate(
            Some(&cs),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    let cout = clean
        .apply_override(&cs, LatchOverride::Unlatch)
        .expect("an uncontaminated latched tab unlatches too");
    assert_eq!(cout.prior, Latch::Local);
    assert!(cout.prior_taint.is_none(), "there was nothing to release");
    assert!(
        unlatch_clear_row(outbound::Origin::Ipc, LatchOverride::Unlatch, &cs, &cout).is_none(),
        "a `contamination_cleared` row here would claim a release that never happened"
    );
    let cleanrow = override_row(outbound::Origin::Ipc, LatchOverride::Unlatch, &cout);
    assert!(
        cleanrow.detail.contains("nothing to clear"),
        "{}",
        cleanrow.detail
    );

    // 7. An arm is superseded, not stranded: `clear_contamination` drops it,
    //    because once the bit is gone there is nothing left to wait for.
    outbound::test_rows::reset();
    let (armed, arm_s) = contaminated_registry();
    armed
        .apply_override(&arm_s, LatchOverride::AwaitSessionClear)
        .expect("arm");
    let _ = outbound::test_rows::drain();
    let aout = armed
        .apply_override(&arm_s, LatchOverride::Unlatch)
        .expect("unlatch supersedes the arm");
    assert!(!aout.view.contaminated);
    assert!(
        !aout.view.awaiting_session_clear,
        "an arm outliving its bit is a trap waiting for the next rotation"
    );
    assert!(
        aout.prior_taint.as_ref().is_some_and(|p| p.armed),
        "and the row records that the tab had been armed"
    );
    assert!(
        cleared_rows(&outbound::test_rows::drain()).is_empty(),
        "still exactly one release row, and it is the builder's"
    );
    assert!(unlatch_clear_row(
        outbound::Origin::Ipc,
        LatchOverride::Unlatch,
        &arm_s,
        &aout
    )
    .is_some());

    // 8. And the collision case the ordering inside `apply_override` decides:
    //    an armed one-shot that fires on the SAME click (the user restored,
    //    ran `/clear`, then clicked). `observe` runs first, so it clears the
    //    bit and writes the rotation's row; the unlatch then finds a latch
    //    the rotation already reopened and is refused — while the clear that
    //    really happened is still recorded. Exactly one release row.
    outbound::test_rows::reset();
    let (raced, rs) = contaminated_registry();
    raced
        .apply_override(&rs, LatchOverride::AwaitSessionClear)
        .expect("arm");
    let _ = outbound::test_rows::drain();
    let rotated = scope("claude-1", Some("sess-b"));
    let err = raced
        .apply_override(&rotated, LatchOverride::Unlatch)
        .expect_err("the rotation reopened the latch, so there is nothing to unlatch");
    assert!(err.contains("not latched"), "{err}");
    assert_eq!(
        cleared_rows(&outbound::test_rows::drain()).len(),
        1,
        "a refused action must not swallow a clear that already happened"
    );
    assert!(!raced.snapshot()[0].view.contaminated);

    // 9. And from `Open`, contaminated: the H-2 state (a rotation reopened
    //    the latch and kept the bit). `unlatch` does not apply there and
    //    clears nothing — `clear_contamination` is that state's action, which
    //    is why `can_unlatch` is deliberately not widened.
    let (open_reg, _os) = contaminated_registry();
    let orotated = scope("claude-1", Some("sess-b"));
    let v = open_reg.view_for(&orotated);
    assert_eq!(v.latch, "open");
    assert!(v.contaminated, "unarmed: the bit is sticky (H-2)");
    let oerr = open_reg
        .apply_override(&orotated, LatchOverride::Unlatch)
        .expect_err("nothing to unlatch");
    assert!(oerr.contains("not latched"), "{oerr}");
    assert!(
        open_reg.snapshot()[0].view.contaminated,
        "a refused unlatch releases nothing"
    );
}

/// Full unlatch restores both sides — the at-own-risk move — **and, since
/// decision 15's 2026-08-10 amendment, persistence with them**: the user
/// restored full access, and the flag goes with it. Both facts matter: the
/// button must actually work, and it must release exactly what the
/// confirmation says it releases.
///
/// This test asserted the inverse until 2026-08-11 (it was
/// `full_unlatch_restores_both_sides_but_not_persistence`, ending in
/// *"unlatching must not un-contaminate the conversation"*). The flip keeps
/// that rule — see
/// `contamination_survives_the_flip_and_every_session_rotation`.
///
/// **It also needed splitting into two legs, and that is a finding of its
/// own.** The old single-registry version probed the external side first
/// (`ddg__search`) and *then* asserted the write was quarantined — but that
/// probe is an admitted EXTERNAL call: it re-latches the tab EXTERNAL and
/// re-contaminates the conversation. The quarantine it observed was the NEW
/// latch's, on `Latch::proxy_gate`'s own authority, so the assertion never
/// depended on the unlatch's treatment of the flag at all. Two registries,
/// because one call cannot both exercise the web side and leave the tab in
/// the state the other half is about.
#[test]
fn full_unlatch_restores_both_sides_including_persistence() {
    // Leg 1: the web side answers again — the button does what it says.
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    reg.apply_override(&s, LatchOverride::Unlatch)
        .expect("unlatch");
    assert!(reg
        .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    // …and that call has re-latched EXTERNAL and re-contaminated the tab,
    // which is correct: a new page really was read.
    assert!(reg.snapshot()[0].view.contaminated);

    // Leg 2: and so does persistence, which is what the amendment added.
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    reg.apply_override(&s, LatchOverride::Unlatch)
        .expect("unlatch");
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Clean),
        "decision 15's 2026-08-10 amendment: restoring full access also releases \
         the flag — the user's decision"
    );
    // The local-capability side answers too (that write re-latched Local).
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
}

/// The wire vocabulary. An unrecognized action is an ERROR, never resolved
/// to a default — the moves differ in exactly how much capability they hand
/// back, so a typo must not pick one.
///
/// The literal list below is the *assertion*, not the input (the same shape
/// `screen_labels_are_the_distinct_wire_values` takes): a fifth action fails
/// here until someone gives it a wire value and names it, because the
/// frontend's `LatchAction` union is a hand-kept mirror of exactly this set.
#[test]
fn latch_override_parses_exactly_the_declared_actions() {
    const ACTIONS: [(LatchOverride, &str); 4] = [
        (LatchOverride::FlipLocal, "flip_local"),
        (LatchOverride::Unlatch, "unlatch"),
        (LatchOverride::ClearContamination, "clear_contamination"),
        (LatchOverride::AwaitSessionClear, "await_session_clear"),
    ];
    for (action, wire) in ACTIONS {
        assert_eq!(action.as_str(), wire);
        assert_eq!(LatchOverride::parse(wire), Ok(action), "{wire}");
        // Trimmed, exactly as `unlatch` always was.
        assert_eq!(LatchOverride::parse(&format!(" {wire} ")), Ok(action));
    }
    for junk in [
        "",
        "unlatch_all",
        "flip",
        "FLIP_LOCAL",
        "open",
        // Near-misses of the two new ones. An action that CLEARS containment
        // is the last place a lenient parse belongs.
        "clear",
        "clear_contamination_now",
        "await_session",
        "session_clear_observed",
    ] {
        assert!(LatchOverride::parse(junk).is_err(), "{junk}");
    }
}

// ── #45 — the registry's bound, and the audit row's provenance ──────────

/// Settings carrying `ids` as AI tabs, plus one reserved Shell tab (which
/// hosts no harness and must therefore never be a valid latch scope).
fn settings_with_tabs(ids: &[&str]) -> crate::settings::Settings {
    settings_with_consumer_tabs(&ids.iter().map(|id| ("claude", *id)).collect::<Vec<_>>())
}

/// V33 C5: settings carrying `(consumer, id)` AI tabs — `"claude"` builds a
/// Claude tab, anything else an OpenCode one — plus the same reserved Shell
/// tab. The consumer of a tab is its COMMAND (`tabs::tab_consumer`), so
/// these are built from the real defaults rather than by stamping a field.
fn settings_with_consumer_tabs(tabs_in: &[(&str, &str)]) -> crate::settings::Settings {
    use crate::settings::{default_ai_tab, default_graph_monitor_tab, TabConfig};
    let mut tabs = vec![default_graph_monitor_tab()];
    for (consumer, id) in tabs_in {
        let mut t = crate::settings::ai_tab_inheriting_injection(default_ai_tab(
            if *consumer == "claude" {
                crate::settings::ai_tab_id("claude")
            } else {
                crate::settings::ai_tab_id("opencode")
            },
        ));
        if let TabConfig::AiTool(c) = &mut t {
            c.id = (*id).to_string();
        }
        tabs.push(t);
    }
    crate::settings::Settings {
        tabs,
        ..Default::default()
    }
}

/// V33: `/context/retrieve` accepts an optional `tab`, and only a
/// **configured** one becomes the checkpoint's identity.
///
/// Covers the three cases that must stay apart at this boundary: a real tab
/// (recorded), a forged/stale one (dropped — never written as a fabricated
/// attribution), and a body from a shim old enough not to send the field at
/// all (parses fine, records no tab, exactly the pre-V33 row).
///
/// **What it would still pass with if the change regressed:** a handler
/// that recorded `body.tab` verbatim would fail the forged case; a handler
/// that dropped the tab entirely would fail the configured case; a
/// `#[serde(default)]` removed from `tab` would fail the old-shim case with
/// a parse error, which is what turns "no identity" into "no context
/// injection for that user at all".
#[test]
fn context_retrieve_records_only_a_configured_tab_as_checkpoint_identity() {
    let s = settings_with_tabs(&["claude", "claude-2"]);
    let parse = |json: &str| -> ContextRetrieveBody {
        serde_json::from_str(json).expect("body parses")
    };

    // A real tab: recorded, alongside the session and agent.
    let body = parse(
        r#"{"cwd":"P:/p","prompt":"hi","session_id":"sess-1","agent":"claude","tab":"claude-2"}"#,
    );
    let origin = checkpoint_origin(&s, &body);
    assert_eq!(origin.tab.as_deref(), Some("claude-2"));
    assert_eq!(origin.session.as_deref(), Some("sess-1"));
    assert_eq!(origin.agent.as_deref(), Some("claude"));

    // A forged / stale id: dropped, not recorded as fact. The session still
    // is — it widens nothing and still improves the join materially.
    let body = parse(
        r#"{"cwd":"P:/p","prompt":"hi","session_id":"sess-1","agent":"claude","tab":"claude-99"}"#,
    );
    let origin = checkpoint_origin(&s, &body);
    assert_eq!(origin.tab, None);
    assert_eq!(origin.session.as_deref(), Some("sess-1"));

    // A pre-V33 shim: no `tab` field at all. Must parse, and must record
    // the pre-V33 shape rather than failing the prompt.
    let body = parse(r#"{"cwd":"P:/p","prompt":"hi","session_id":"sess-1","agent":"claude"}"#);
    let origin = checkpoint_origin(&s, &body);
    assert_eq!(origin.tab, None);
    assert_eq!(origin.agent.as_deref(), Some("claude"));

    // Blank spellings of "no identity" never read as one.
    let body = parse(r#"{"prompt":"hi","session_id":"  ","agent":"","tab":"   "}"#);
    let origin = checkpoint_origin(&s, &body);
    assert_eq!(origin, crate::workbench::shadow::Origin::default());
}

/// **V33 Phase F: `/workbench/tool_checkpoint` narrows the tab exactly as
/// the prompt tap does, and reads the tool name in the CALLER's
/// vocabulary.**
///
/// The vocabulary half is the subtle one. `CLAUDE_NATIVE_TABLE` and
/// `OPENCODE_NATIVE_TABLE` are two closed sets with no member in common:
/// `edit` is unknown in the first and `Edit` in the second. Crossing them
/// would not fail loudly — it would silently disable one harness's entire
/// seam while every test that only exercised the other stayed green. Both
/// directions are asserted.
///
/// **V40 Phase A changed what "unknown" answers here, and this is the one
/// behaviour change locked decision 16 makes.** The lookup used to be a
/// `match` with `"opencode"` in one arm and Claude's table in the `_` arm,
/// so an id the addressed harness does not declare — and an id from a
/// harness cImp has never heard of — answered `false`: *no checkpoint*. It
/// now answers `true`. The asymmetry is the argument: a checkpoint nobody
/// needed is one commit into cImp's own shadow repo, while a missed one is a
/// destructive tool call with no way back, and "not in Claude's table,
/// therefore safe" is exactly what made a third harness's whole mutation
/// surface invisible. The rows below that flipped are marked.
#[test]
fn the_tool_checkpoint_route_narrows_the_tab_and_reads_the_right_vocabulary() {
    let s = settings_with_tabs(&["claude", "claude-2"]);

    // Identity: same funnel, same answers as the prompt tap.
    let origin = checkpoint_identity(&s, Some("claude"), Some("sess-1"), Some("claude-2"));
    assert_eq!(origin.tab.as_deref(), Some("claude-2"));
    assert_eq!(origin.session.as_deref(), Some("sess-1"));
    assert_eq!(
        checkpoint_identity(&s, Some("claude"), Some("sess-1"), Some("claude-99")).tab,
        None,
        "a forged or stale tab id must degrade to `cannot attribute`"
    );
    // The route composes `source` itself and never takes one from the wire,
    // so `Origin::with_source` is the only way a checkpoint gets one.
    assert_eq!(origin.source, None);

    // Vocabulary: Claude's capitalized natives.
    for tool in ["Edit", "Write", "MultiEdit", "Bash"] {
        assert!(tool_checkpoint_is_mutating("claude", tool), "{tool}");
    }
    // Declared by Claude and declared NON-mutating — the answer that keeps a
    // read or a web fetch from minting a checkpoint. It is a declaration,
    // not a default: that is the whole difference from the row below.
    for tool in ["Read", "Grep", "WebFetch"] {
        assert!(!tool_checkpoint_is_mutating("claude", tool), "{tool}");
    }
    // …and OpenCode's lowercase ids, which are a DIFFERENT table.
    for tool in ["edit", "write", "patch", "apply_patch", "bash"] {
        assert!(tool_checkpoint_is_mutating("opencode", tool), "{tool}");
    }
    for tool in ["read", "grep", "glob", "webfetch"] {
        assert!(!tool_checkpoint_is_mutating("opencode", tool), "{tool}");
    }
    // **The V40 flip.** A name the addressed harness does not declare now
    // fails CLOSED. `edit` is OpenCode's id and Claude does not serve it;
    // `Edit` is Claude's and OpenCode does not; `task` is an OpenCode id
    // cImp reviewed and deliberately left ungated, so it has no row either.
    // All three used to answer `false` out of whichever table the `match`
    // happened to reach.
    assert!(tool_checkpoint_is_mutating("claude", "edit"));
    assert!(tool_checkpoint_is_mutating("opencode", "Edit"));
    assert!(tool_checkpoint_is_mutating("opencode", "task"));
    // A harness with no `agent` on the wire is Claude (`hook_agent`'s
    // documented pre-CHP default), so it reads Claude's table.
    assert!(tool_checkpoint_is_mutating(hook_agent(None), "Bash"));
    // An unrecognised token resolves to no harness at all — and reads no
    // harness's table. Before Phase A it fell through to Claude's; before
    // this change, falling through to Claude's is what made it answer at
    // all. Now it fails closed, for `Bash` and for anything else.
    assert!(tool_checkpoint_is_mutating(hook_agent(Some("nonsense")), "Bash"));
    assert!(tool_checkpoint_is_mutating(hook_agent(Some("nonsense")), "Read"));
}

/// **An unidentified source is REFUSED at the checkpoint route, not treated
/// as mutating** (V40 review finding M-6, parity lens).
///
/// `mutates_fs` fails closed for a harness with no vocabulary, which is the
/// right answer to "is this NAME mutating" and the wrong answer to "may this
/// CALLER mint a checkpoint": it made every tool name from a forged POST
/// mutating, and each one staged a snapshot attributed to
/// `unknown:<whatever>`. Bounded by the throttle and the tree-sha dedup, but
/// a checkpoint is the record a restore is judged against, and the route's
/// own doc claimed a POST naming a harness cImp does not know could not get
/// through it.
#[test]
fn an_unidentified_checkpoint_source_is_refused() {
    // Every registered harness is admitted, under its own token.
    for h in crate::harness::registry::all() {
        let id = h.id().expect("registered");
        assert_eq!(checkpoint_source_admits(Some(id)).as_deref(), Ok(id));
    }
    // ABSENT is the pre-CHP shim, and still resolves to the wire default.
    assert_eq!(
        checkpoint_source_admits(None).as_deref(),
        Ok(crate::harness::DEFAULT_HARNESS.token())
    );
    assert!(checkpoint_source_admits(Some("")).is_ok(), "empty is absent (M-4)");

    // …and everything else is refused, with the registered list in the
    // message. `offload` and `audit` included: they are cImp's own in-app
    // consumers, neither runs tools in a harness's vocabulary, and neither
    // has any business staging a pre-tool checkpoint.
    for token in ["codex", "unknown", "offload", "audit", "claude-code", " claude "] {
        let err = checkpoint_source_admits(Some(token))
            .expect_err(&format!("{token:?} must be refused"));
        for h in crate::harness::registry::all() {
            assert!(err.contains(h.id().expect("registered")), "{err}");
        }
    }
}

/// **The registry's bound, made real.** `latches()`'s doc claimed the map
/// was "bounded by construction — tab ids are config-derived"; they are
/// request-derived, and the claim was asserted only in that comment. The
/// key space is now the user's configured AI tabs, so the map cannot exceed
/// one entry per tab per agent no matter what a caller POSTs — which
/// matters because every entry is serialized into every `/status` response
/// and every 4 s `latch_status` poll, with no TTL, cap or eviction.
/// **#48 rewrote this test too.** It named a registry bound and exercised
/// [`is_configured_tab`] directly — a predicate *beside* the enforcement
/// point, not through it. Deleting the `is_configured_tab` call from
/// `latch_scope` left it green, so the one thing the issue actually changed
/// was untested. It now asserts through [`tab_identity`], which is the
/// decision `latch_scope` delegates to (its remaining work is the session
/// lookup, which needs an `AppHandle` this crate cannot mock), and then
/// through the registry itself.
#[test]
fn only_configured_ai_tab_ids_can_ever_key_a_latch() {
    let s = settings_with_tabs(&["claude", "claude-2"]);
    assert_eq!(
        tab_identity(&s, "claude", Some("claude")),
        TabIdentity::Configured("claude")
    );
    assert_eq!(
        tab_identity(&s, "claude", Some(" claude-2 ")),
        TabIdentity::Configured("claude-2"),
        "surrounding whitespace is trimmed, not treated as a different tab"
    );

    for forged in ["claude-1", "Claude", "../claude", "graph-monitor"] {
        assert_eq!(
            tab_identity(&s, "claude", Some(forged)),
            TabIdentity::Unknown(forged),
            "{forged:?} is not a configured AI tab and must not key a latch"
        );
    }
    // The two identity-less shapes are distinct (#48): "no tab id" is not
    // "an id I do not recognize", and `handle_latch_state` reads them apart.
    for anon in [None, Some(""), Some("   ")] {
        assert_eq!(
            tab_identity(&s, "claude", anon),
            TabIdentity::Anonymous,
            "{anon:?}"
        );
    }

    // The bound stated as a bound: whatever a caller sends, the set of ids
    // that get through is a subset of the configured AI tabs.
    let attempts = [
        "claude",
        "claude-2",
        "claude-1",
        "claude-3",
        "tab-9999",
        "graph-monitor",
    ];
    let admitted: Vec<&str> = attempts
        .iter()
        .copied()
        .filter(|t| matches!(tab_identity(&s, "claude", Some(t)), TabIdentity::Configured(_)))
        .collect();
    assert_eq!(admitted, ["claude", "claude-2"]);

    // And the bound where it is actually load-bearing: the registry. A
    // forged id resolves to no scope, and the two methods that insert are
    // the only ones that ever receive one — so `/status` and the 4 s
    // `latch_status` poll cannot be grown by a caller inventing ids.
    let reg = LatchRegistry::default();
    for forged in attempts.iter().copied().filter(|t| {
        !matches!(
            tab_identity(&s, "claude", Some(t)),
            TabIdentity::Configured(_)
        )
    }) {
        let scope = match tab_identity(&s, "claude", Some(forged)) {
            TabIdentity::Configured(t) => Some(LatchScope {
                agent: "claude",
                tab: t.to_string(),
                session: None,
                root: TEST_ROOT.to_string(),
            }),
            _ => None,
        };
        assert!(reg
            .gate(
                scope.as_ref(),
                LatchRoute::Proxied,
                "ddg__search",
                ON,
                NO_CONTENT
            )
            .is_ok());
        let _ = reg.beacon(scope.as_ref(), "WebFetch", ON, BEACON_PROV);
    }
    assert!(
        reg.snapshot().is_empty(),
        "forged tab ids keyed {} registry entries: {:?}",
        reg.snapshot().len(),
        reg.snapshot()
            .iter()
            .map(|r| r.tab.clone())
            .collect::<Vec<_>>()
    );
}

/// The availability floor, stated as a test so it is a decision rather than
/// an accident: with no AI tab in the snapshot the predicate accepts
/// everything, because `live_settings` falls back to `Settings::default()`
/// (empty `tabs`) before managed state is up, and a request in that window
/// must not be rejected on the strength of a list we could not read.
///
/// **V33 C5 keeps its trigger on the WHOLE list.** The plan's wording was
/// "narrow the floor to the asserted consumer"; doing that literally would
/// have *widened* it — on the ordinary install that runs only Claude tabs,
/// "opencode has zero tabs" would be true forever and every forged id
/// asserting `consumer: opencode` would get a scope, i.e. the unbounded key
/// space #45 closed. The condition the floor encodes is "settings are
/// unreadable", which is global, so only the positive test is
/// consumer-scoped. The last assertion is the one that would fail if a
/// future edit moved the floor into `ai_tab_ids_for`.
#[test]
fn an_unreadable_tab_list_accepts_rather_than_rejects() {
    let empty = crate::settings::Settings::default();
    assert!(empty.tabs.is_empty(), "the fallback snapshot has no tabs");
    assert!(is_configured_tab(&empty, "claude", "claude-1"));
    assert!(is_configured_tab(&empty, "opencode", "anything"));
    // A snapshot with only reserved Shell tabs is the same case: no AI tab
    // means no list to validate against.
    assert!(is_configured_tab(&settings_with_tabs(&[]), "claude", "anything"));

    // …and a snapshot that HAS tabs is a readable list, for every consumer —
    // including the ones that own none of them.
    let claude_only = settings_with_tabs(&["claude"]);
    assert!(
        !is_configured_tab(&claude_only, "opencode", "claude"),
        "a per-consumer floor would hand every forged id a scope here"
    );
    assert!(
        !is_configured_tab(&claude_only, "opencode", "invented"),
        "a per-consumer floor would hand every forged id a scope here"
    );
}

/// **V33 C5 (finding F-4): the `(consumer, tab)` pair is verified.**
///
/// The registry key is the pair ([`LatchScope::key`]) and `agent` is
/// caller-asserted on every route that has one, but until V33
/// [`is_configured_tab`] asked only "is this *some* configured AI tab id".
/// A caller could therefore key a latch under `("claude", <an OpenCode
/// tab's id>)`, and the pair was checked on no route in the system.
///
/// The review rated the cross-keyed case harmless on `/audit/run` as it
/// stands — the resulting latch is freshly open and engages a scope nobody
/// reads — so this pins a restored invariant, not a live exploit.
///
/// **What this would still pass with:** a check that compared the asserted
/// consumer against a field on the tab config would pass the first two
/// assertions and fail the third, because there is no such field: the
/// consumer of a tab is its COMMAND, which is what the launch path splits on
/// when it decides what to inject (`tabs::tab_consumer`). And a check that
/// merely rejected mismatches would pass without the `Configured` cases,
/// which is why both directions are asserted for both consumers.
#[test]
fn a_tab_of_one_consumer_cannot_key_a_latch_under_the_other() {
    let s = settings_with_consumer_tabs(&[("claude", "claude"), ("opencode", "opencode")]);

    // Each consumer's own tab resolves.
    assert_eq!(
        tab_identity(&s, "claude", Some("claude")),
        TabIdentity::Configured("claude")
    );
    assert_eq!(
        tab_identity(&s, "opencode", Some("opencode")),
        TabIdentity::Configured("opencode")
    );

    // Cross-keyed, both directions: a real tab id of the OTHER harness is
    // exactly as unrecognized as an invented string, and keys nothing.
    assert_eq!(
        tab_identity(&s, "claude", Some("opencode")),
        TabIdentity::Unknown("opencode"),
        "a caller asserting `claude` must not key a latch under an OpenCode tab"
    );
    assert_eq!(
        tab_identity(&s, "opencode", Some("claude")),
        TabIdentity::Unknown("claude"),
        "…and the reverse"
    );

    // The consumer of a tab is its command, not a stored label — a Claude
    // tab renamed to an OpenCode-looking id is still a Claude tab.
    let renamed = settings_with_consumer_tabs(&[("claude", "opencode-7")]);
    assert_eq!(
        tab_identity(&renamed, "claude", Some("opencode-7")),
        TabIdentity::Configured("opencode-7")
    );
    assert_eq!(
        tab_identity(&renamed, "opencode", Some("opencode-7")),
        TabIdentity::Unknown("opencode-7")
    );

    // And the bound where it is load-bearing: neither cross-keyed attempt
    // reaches the registry, so `/status` cannot be grown by asserting the
    // other consumer's name over a real tab id.
    let reg = LatchRegistry::default();
    for (agent, tab) in [("claude", "opencode"), ("opencode", "claude")] {
        let scope = match tab_identity(&s, agent, Some(tab)) {
            TabIdentity::Configured(t) => Some(LatchScope {
                agent,
                tab: t.to_string(),
                session: None,
                root: TEST_ROOT.to_string(),
            }),
            _ => None,
        };
        let _ = reg.beacon(scope.as_ref(), "WebFetch", ON, BEACON_PROV);
    }
    assert!(
        reg.snapshot().is_empty(),
        "a cross-keyed beacon created {:?}",
        reg.snapshot()
    );
}

// ── V32 C-2 / H-2 — a session rotation must not clear contamination ─────

/// A tab that has read a page: EXTERNAL-latched, contaminated, session
/// `real-session`.
fn contaminated_tab() -> TabLatch {
    let mut t = TabLatch::fresh();
    // A first sighting is not a rotation, so it can never clear anything.
    assert_eq!(t.observe(Some("real-session")), None);
    let scope = LatchScope {
        agent: "claude",
        tab: "claude".to_string(),
        session: Some("real-session".to_string()),
        root: TEST_ROOT.to_string(),
    };
    let reg = LatchRegistry::default();
    assert!(reg
        .gate(
            Some(&scope),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    // Mirror that admitted EXTERNAL call onto the standalone entry, so the
    // test's subject is built by the same two facts the gate sets.
    t.latch.engage(ToolClass::External);
    t.contaminated = true;
    t
}

/// **The seam the whole finding lives on, inverted by H-2.**
///
/// This test used to assert the opposite — that a rotation reaching
/// [`TabLatch::observe`] CLEARS `contaminated` — on the reading that only a
/// new conversation has a clean context. C-2 then tried to make the
/// rotation signal trustworthy, and H-2 showed it cannot be: the signal is
/// the newest `*.jsonl` under a directory the model's own Bash can write
/// (decision 3), so every bar over it is a bar over the attacker's own file.
///
/// The rotation still resets everything **permissive** — latch, budget, the
/// one-row-per-scope report bits — because those are re-earned by the next
/// real call and a stale one would falsely deny a fresh conversation. It no
/// longer resets the one bit an attacker would want reset.
#[test]
fn a_session_rotation_resets_the_latch_but_never_the_contamination_bit() {
    const ONE_CALL: outbound::BudgetLimits = outbound::BudgetLimits {
        max_calls: 1,
        max_bytes: 0,
    };
    let mut t = contaminated_tab();
    t.latch_flagged = true;
    t.beacon_flagged = true;
    t.budget.charge(4096);
    assert!(t.contaminated && t.latch == Latch::External);
    assert!(t.budget.exhausted(ONE_CALL), "the spend is on the books");

    // Step 4: the return value IS the "did this clear anything" answer, so
    // an unarmed tab must answer `None` — asserted here rather than only
    // through `t.contaminated` below, because a future `observe` that
    // cleared the bit and forgot the row would still leave `contaminated`
    // false and could not be caught by reading the field alone.
    assert_eq!(
        t.observe(Some("aaaa")),
        None,
        "an UNARMED tab clears nothing on a rotation, and reports nothing"
    );
    assert_eq!(t.session.as_deref(), Some("aaaa"), "the id itself rotates");
    assert_eq!(t.latch, Latch::Open, "a rotation reopens the latch");
    assert!(
        !t.latch_flagged,
        "and re-arms the one-row-per-scope reports"
    );
    assert!(!t.beacon_flagged);
    assert!(
        !t.budget.exhausted(ONE_CALL),
        "and refills the fetch budget"
    );
    assert!(
        t.contaminated,
        "H-2: a rotation is a claim about an attacker-writable file, so it may \
         not un-taint the context window — only a user's own click does (step 4)"
    );
    assert!(
        !t.awaiting_session_clear,
        "and nothing about a rotation may ARM the one-shot either"
    );

    // …and the same call with NO id, or the same id, changes nothing. This
    // is the "keep calling until the registry blinks" attack `observe`
    // already defended against; C-2 and H-2 are its harder siblings.
    let mut t = contaminated_tab();
    assert_eq!(t.observe(None), None);
    assert_eq!(t.observe(Some("real-session")), None);
    assert!(t.contaminated && t.latch == Latch::External);
}

/// **C-2/H-2, filesystem variant.** A Claude tab's session id is the stem of
/// the newest `*.jsonl` in its project dir, ranked purely by mtime, and the
/// tap used to mark a post-attach file live *immediately*
/// (`live_confirmed = !first_attach`). So `type nul > …/aaaa.jsonl` from
/// Bash — a zero-byte file — reported session `aaaa` within one 200 ms poll.
///
/// C-2's fix put a growth bar in the tap, and **H-2 walked straight over it
/// with `echo {} > …/aaaa.jsonl`**: `read_complete_lines` advances the
/// offset for any newline-terminated bytes, so a trailing `\n` was the whole
/// bar. The old version of this test asserted `gate.observed(0, 0)` — the
/// zero-byte PoC's exact shape — which is why one byte of content defeated a
/// green suite.
///
/// Two independent guards now, and this test states both:
/// 1. the gate takes a DECODE proof, so bytes alone confirm nothing; and
/// 2. **even a confirmed rotation cannot clear `contaminated`**, because the
///    file the proof is read from is one the attacker writes.
///
/// Asserted **through** `harness::claude::read::LiveSessionGate` rather than beside
/// it, so weakening the gate fails this test.
#[test]
fn a_forged_rotation_neither_confirms_a_session_nor_clears_contamination() {
    use crate::harness::claude::read::LiveSessionGate;
    let mut tab = contaminated_tab();
    let mut gate = LiveSessionGate::default();
    // The tap is running on a confirmed session.
    assert!(gate.observed(true));

    // The forged file wins `newest_jsonl` on mtime. The tap rotates onto it
    // and drains. Whatever the attacker wrote — nothing (`type nul`), or
    // bytes that decode to no record of this session (`echo {}`) — the drain
    // reports no evidence, however far the offset moved.
    gate.rotated();
    let live = gate.observed(false);
    assert!(
        !live,
        "a transcript that yields no record naming this session is not live"
    );
    // Ten more polls of the same nothing.
    for _ in 0..10 {
        assert!(!gate.observed(false));
    }
    // So no rotation ever reaches the registry, and the latch keeps the
    // session it was engaged for.
    if live {
        assert_eq!(tab.observe(Some("aaaa")), None);
    }
    assert_eq!(tab.session.as_deref(), Some("real-session"));
    assert_eq!(tab.latch, Latch::External);
    assert!(
        tab.contaminated,
        "contamination survives a transcript file the harness never wrote"
    );

    // H-2's belt-and-braces half: suppose the forger goes one better and
    // writes `{"sessionId":"aaaa"}`, clearing the decode bar. The rotation
    // now DOES reach `observe` — and still cannot un-taint the tab.
    let mut gate = LiveSessionGate::default();
    gate.rotated();
    assert!(gate.observed(true), "a decoded record confirms the session");
    assert_eq!(
        tab.observe(Some("aaaa")),
        None,
        "step 4 must not have widened this: the rotation is admitted, and on an \
         UNARMED tab it still clears nothing"
    );
    assert_eq!(tab.latch, Latch::Open, "the permissive state does reset");
    assert!(
        tab.contaminated,
        "H-2: no filesystem-derived rotation may clear the contamination bit"
    );
}

/// The other half of the same rule: a **real** new session — a file the
/// harness is actually writing into — still rotates the LATCH's scope. The
/// fix must not buy containment by freezing every tab's latch at its first
/// session. (What it deliberately does NOT rotate is `contaminated`; that is
/// the test above.)
#[test]
fn a_rotation_with_decoded_evidence_does_reopen_the_latch() {
    use crate::harness::claude::read::LiveSessionGate;
    let mut tab = contaminated_tab();
    let mut gate = LiveSessionGate::default();
    assert!(gate.observed(true));

    gate.rotated();
    // First poll after the rotation: the harness has created the file but
    // the first line has not landed yet. Still not proof.
    assert!(!gate.observed(false));
    // A line lands that carries no `sessionId` at all (a real shape —
    // `{"type":"file-history-snapshot",…}`). Not evidence either: it neither
    // confirms nor vetoes.
    assert!(!gate.observed(false));
    // Next poll: a decoded record naming this session.
    let live = gate.observed(true);
    assert!(live, "a transcript writing THIS session's records is live");
    // Confirmation is sticky until the next rotation — a quiet turn must
    // not un-confirm a session the tap already proved.
    assert!(gate.observed(false));

    assert_eq!(tab.observe(Some("new-session")), None);
    assert_eq!(tab.latch, Latch::Open);
    assert_eq!(tab.session.as_deref(), Some("new-session"));
    assert!(
        tab.contaminated,
        "a GENUINE rotation into an unarmed tab clears no more than a forged one"
    );
}

/// **C-2, token variant — closed by construction since V40 Phase D.**
///
/// `/memory/event`'s three registry writes key on body-supplied strings,
/// with `agent` defaulting and no validation. That used to matter because
/// the live-session registry was ONE map holding two key spaces: a
/// tab-keyed harness's reader wrote the tab id, this route wrote a session
/// id. A POST naming a configured tab id therefore repointed that tab's
/// session and flapped the latch clear in a loop — and the real tap
/// re-stamping the true id within 200 ms produced a *second* rotation, so
/// the race helped the attacker. It was closed by refusing any key that
/// named a configured tab.
///
/// The spaces are separate now (locked decision 20), so the collision
/// cannot be expressed and there is no list to keep in step. Asserted
/// **through** [`mark_live_session_from_body`] by observing what it would
/// write: deleting the key-space decision from that function fails this
/// test.
#[test]
fn a_memory_event_can_only_key_the_session_space() {
    let written = |agent: &str, key: &str| {
        let mut out: Option<(crate::harness::plugin::SessionKey, String)> = None;
        mark_live_session_from_body(
            |space, k| out = Some((space, k.to_string())),
            agent,
            key,
        );
        out
    };
    // A session-keyed harness writes — including for a string that names a
    // configured tab, which is now harmless: it lands in the session space,
    // and every tab-keyed reader looks in the other one.
    for key in [
        "ses_01JQ8Z2W6R3K4M5N6P7Q8R9S",
        "b3f1c2d4-5e6f-4708-8910-1112131415",
        "claude",
        "opencode-2",
        "",
    ] {
        assert_eq!(
            written("opencode", key),
            Some((crate::harness::plugin::SessionKey::Session, key.to_string())),
            "{key:?} must key the SESSION space and nothing else"
        );
    }
    // A tab-keyed harness's live session is bound by cImp's own reader, so
    // a request body may not claim it at all.
    assert_eq!(
        written("claude", "ses_whatever"),
        None,
        "a tab-keyed harness's binding is its reader's, never a POST body's"
    );
    // An unregistered agent writes nothing — fail closed.
    assert_eq!(written("not-a-harness", "ses_x"), None);
    assert_eq!(written("", "ses_x"), None);
}

#[test]
fn an_override_row_records_the_action_the_prior_latch_and_the_surviving_taint() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    let out = reg
        .apply_override(&s, LatchOverride::FlipLocal)
        .expect("flip applies");
    let row = override_row(outbound::Origin::Ipc, LatchOverride::FlipLocal, &out);
    let detail = &row.detail;
    assert!(detail.contains("USER OVERRIDE (flip_local"), "{detail}");
    assert!(detail.contains("external → local"), "{detail}");
    assert!(detail.contains("contaminated=true"), "{detail}");
    // The row must name the reset that actually works, and step 4 changed
    // what that is. H-2 left "restart cImp" as the only one and the row said
    // so; there are now two user actions, and a row still sending an
    // incident reviewer to a restart would misdirect them.
    assert!(detail.contains("clear_contamination"), "{detail}");
    assert!(detail.contains("await_session_clear"), "{detail}");
    assert!(
        !detail.to_lowercase().contains("restarting cimp"),
        "the restart is no longer the only clean reset: {detail}"
    );
    assert!(!detail.contains("Restarting the tab"), "{detail}");
    assert_eq!(
        row.tool, "flip_local",
        "the action is the row's tool column"
    );
    assert_eq!(
        row.screen,
        outbound::Screen::LatchOverride,
        "a latch move is filed as a latch move"
    );

    // A row that granted capability back must not be painted as a denial.
    assert!(!outbound::Screen::LatchOverride.is_denial());
}

/// #45's whole point: the row says WHO asked. An override can now only
/// arrive over IPC (the HTTP route is gone), and a beacon can only arrive
/// over HTTP — so the two rows must carry different origins, and the beacon
/// row must not imply a user acted.
///
/// **#48 rewrote this test, because it could not fail.** It asserted
/// `detail.contains("origin: ipc")` against a function that spelled
/// `Origin::Ipc` into its own format string — swapping `Flag.origin` at
/// both call sites left it green, so the one thing it named (the two rows
/// are told apart) was untested. The property is that the prose and the
/// `origin` key have a single source, so it is asserted over EVERY origin
/// the enum has: whatever a call site states, both halves of the row say
/// it, and a row whose two halves could disagree fails here.
#[test]
fn a_flag_rows_prose_and_its_origin_key_have_one_source() {
    for origin in outbound::Origin::ALL.iter().copied() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));

        let beacon = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        assert!(beacon.engaged);
        let brow = beacon_row(origin, "WebFetch", &beacon);
        assert_eq!(brow.origin, origin);
        assert!(
            brow.detail
                .contains(&format!("origin: {}", origin.as_str())),
            "{:?}: {}",
            origin,
            brow.detail
        );
        // Independent of the origin: a beacon row never implies a human.
        assert!(
            brow.detail.contains("NOT evidence of a user action"),
            "{}",
            brow.detail
        );

        let out = reg
            .apply_override(&s, LatchOverride::Unlatch)
            .expect("unlatch applies");
        let orow = override_row(origin, LatchOverride::Unlatch, &out);
        assert_eq!(orow.origin, origin);
        assert!(
            orow.detail
                .contains(&format!("origin: {}", origin.as_str())),
            "{:?}: {}",
            origin,
            orow.detail
        );

        // And the machine-readable half agrees with the prose, because it
        // is the same field: this is the assertion that fails if a future
        // call site ever sets `Flag.origin` from anything but `row.origin`.
        for row in [&brow, &orow] {
            let request = outbound::flag_request(&outbound::Flag {
                screen: outbound::Screen::LatchBeacon,
                origin: row.origin,
                consumer: s.agent,
                scope: &s.label(),
                attribution: s.attribution(),
                session: None,
                tool: &row.tool,
                host: None,
                url: None,
                resolved_ip: None,
                canary: false,
                root: String::new(),
                detail: &row.detail,
            });
            assert_eq!(request["origin"], origin.as_str());
            assert_eq!(request["scope"], "claude:claude-1");
        }
    }

    // The two live call sites still differ, which is the fact #45 bought:
    // an override can only arrive over IPC (the HTTP route is gone) and a
    // beacon only over HTTP.
    assert_ne!(outbound::Origin::Ipc, outbound::Origin::Http);
}

/// #48 (A2-2): a beacon that contaminates a conversation **without** moving
/// the latch writes a row too.
///
/// #45 keyed the row on `engaged` — the latch transition — while
/// `LatchRegistry::beacon` set `contaminated` unconditionally. A tab already
/// latched `Local` (Phase A's other direction: a local-capability call came
/// first) therefore took the contamination bit and left NO trace: no row, no
/// `warn!`, no `info!`. From that point every `context_note` is quarantined
/// and every external result enveloped, and the accepted-residuals entry
/// #45 wrote called the beacon "bounded, audited … and recoverable".
#[test]
fn a_beacon_that_only_contaminates_is_recorded_too() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    // A local-capability call first: the tab latches LOCAL, uncontaminated.
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(reg.snapshot()[0].latch(), "local");
    assert!(!reg.snapshot()[0].view.contaminated);

    let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
    assert!(!out.engaged, "the beacon cannot move a LOCAL latch");
    assert!(out.contaminated_now, "but it did contaminate the session");
    assert!(out.report, "and that is a reportable transition");
    assert_eq!(out.view.latch, "local", "decision 15: the latch is unmoved");
    assert!(out.view.contaminated);

    // The row's prose must not claim the latch moved.
    let row = beacon_row(outbound::Origin::Http, "WebFetch", &out);
    assert!(row.detail.contains("CONTAMINATED"), "{}", row.detail);
    assert!(
        !row.detail.contains("now EXTERNAL-latched"),
        "the row must not assert an engagement that did not happen: {}",
        row.detail
    );

    // Still one row per tab-session: a caller in a loop produces no more.
    for _ in 0..5 {
        let again = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        assert!(!again.report, "the feed must not be floodable");
        assert!(!again.contaminated_now, "and the bit is set only once");
    }
    // …and it is the SESSION that bounds it: a rotation re-arms the report,
    // because a new conversation's contamination is a new fact.
    let rotated = scope("claude-1", Some("sess-b"));
    let after = reg.beacon(Some(&rotated), "WebFetch", ON, BEACON_PROV);
    assert!(after.report, "a rotated session reports again");
}

/// The engagement case keeps its single row, and the two transitions do not
/// double-report: an engaging beacon contaminates and latches at once.
#[test]
fn an_engaging_beacon_reports_exactly_once_per_tab_session() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    let first = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
    assert!(first.engaged && first.contaminated_now && first.report);
    for _ in 0..5 {
        assert!(!reg.beacon(Some(&s), "WebSearch", ON, BEACON_PROV).report);
    }
}

/// `/status`'s Phase F shape: the Phase B keys are unchanged (`latch` stays
/// a top-level key — the flattened view provides it) and the three new
/// facts sit beside them, so the badge and the override popover read one
/// row per tab.
#[test]
fn status_snapshot_carries_contamination_and_override_availability() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    assert_eq!(
        serde_json::to_value(reg.snapshot()).unwrap(),
        serde_json::json!([{
            "consumer": "claude",
            "tab": "claude-1",
            "session": "sess-a",
            "latch": "external",
            "contaminated": true,
            "can_flip_local": true,
            "can_unlatch": true,
            // Step 4: both contamination moves are on offer, and nothing is
            // waiting. Asserted as an exact object rather than by key, so a
            // field added to the wire without a decision fails here.
            "can_clear": true,
            "awaiting_session_clear": false,
            // #48 (F-23): why the latch is where it is, for the one position
            // with two causes. `external` is not that position.
            "local_by_user_flip": false,
        }])
    );
    // After the flip: still contaminated, no further flip on offer.
    reg.apply_override(&s, LatchOverride::FlipLocal)
        .expect("flip");
    assert_eq!(
        serde_json::to_value(reg.snapshot()).unwrap(),
        serde_json::json!([{
            "consumer": "claude",
            "tab": "claude-1",
            "session": "sess-a",
            "latch": "local",
            "contaminated": true,
            "can_flip_local": false,
            "can_unlatch": true,
            "can_clear": true,
            "awaiting_session_clear": false,
            // #48 (F-23): the flip that just happened is on the row, so the
            // native-web refusal can name the cause it checked instead of
            // blaming a tool call that never ran.
            "local_by_user_flip": true,
        }])
    );
    // After the restore arm: the bit is still set (that is the whole
    // decision) and the tab now says what it is waiting for.
    reg.apply_override(&s, LatchOverride::AwaitSessionClear)
        .expect("arm");
    assert_eq!(
        serde_json::to_value(reg.snapshot()).unwrap(),
        serde_json::json!([{
            "consumer": "claude",
            "tab": "claude-1",
            "session": "sess-a",
            "latch": "local",
            "contaminated": true,
            "can_flip_local": false,
            "can_unlatch": true,
            "can_clear": true,
            "awaiting_session_clear": true,
            // Unchanged by the arm: it waits on the contamination bit and
            // never touches the latch.
            "local_by_user_flip": true,
        }])
    );
}

/// A LOCAL-only session is never contaminated: only *external* content can
/// contaminate, and a clean session must not be dragged into quarantine by
/// the Phase F bit.
#[test]
fn a_purely_local_session_is_never_contaminated() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    for name in ["graph_snippet", "graph_outline", "context_recall"] {
        assert!(
            reg.gate(Some(&s), LatchRoute::Native, name, ON, NO_CONTENT)
                .is_ok(),
            "{name}"
        );
    }
    assert!(!reg.snapshot()[0].view.contaminated);
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Clean)
    );
    // A REFUSED external call must not contaminate either — otherwise a
    // hallucinated (or injected) call to the blocked side could quarantine
    // a clean session's memory writes.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
        Err(REFUSAL_EXTERNAL_BLOCKED)
    );
    assert!(!reg.snapshot()[0].view.contaminated);
}

/// The two Phase F request bodies parse the shapes their senders actually
/// send — the OpenCode plugin, and (until 2026-08-17) the Claude beacon shim,
/// which a tab open across that upgrade may still be running — and fail open
/// on a missing tab exactly like `/graph_run` and `/mcp/call` do.
#[test]
fn phase_f_bodies_parse_the_shapes_the_reporters_send() {
    let claude: LatchBeaconBody = serde_json::from_slice(
        br#"{"tab":"claude-2","consumer":"claude","tool":"WebFetch","cwd":"P:\\proj","session_id":"s"}"#,
    )
    .expect("claude shim body parses");
    assert_eq!(claude.tab.as_deref(), Some("claude-2"));
    assert_eq!(claude.tool.as_deref(), Some("WebFetch"));

    let bare: LatchBeaconBody =
        serde_json::from_slice(br#"{"consumer":"opencode"}"#).expect("bare body parses");
    assert!(bare.tab.is_none(), "no tab ⇒ fail open, not a 400");

    // There is deliberately no override body type to parse (#45): the
    // override has no wire form, because it has no HTTP route. Its only
    // caller is the `latch_override` IPC command, whose arguments Tauri
    // deserializes into typed parameters.
}

/// Fail-open, exactly like the latch: a call with no tab identity has no
/// scope to charge, so it is never budget-refused.
#[test]
fn a_call_without_tab_identity_is_not_budgeted() {
    let reg = LatchRegistry::default();
    for _ in 0..50 {
        assert!(reg.budget_gate(None, TEST_LIMITS, "ddg__search").is_ok());
        reg.charge(None, 100_000);
    }
}

// ── #48 finding F-3 — the contamination TRANSITION row ─────────────────
//
// Every case below asserts on the rows `record_flag` actually received
// (`outbound::test_rows`), not on the registry's own return values. That is
// deliberate: `BeaconOutcome::contaminated_now` and `LatchStatus.contaminated`
// were already true before this work and F-3 was still open, because
// "the bit flipped" and "something recorded that the bit flipped" are
// different facts and only the second one survives the call.

/// Contamination rows in the order they were written.
fn contamination_rows(
    rows: &[crate::activity::ActivityRecord],
) -> Vec<&crate::activity::ActivityRecord> {
    outbound::test_rows::of_screen(rows, outbound::Screen::Contamination)
}

/// One row's request payload, parsed.
fn payload(row: &crate::activity::ActivityRecord) -> serde_json::Value {
    serde_json::from_str(&row.request).expect("the row's request payload is JSON")
}

/// The quarantine-only posture: the switch combination that made the
/// proxied path contaminate in complete silence.
const QUARANTINE_ONLY: GatePolicy = GatePolicy {
    latch: false,
    quarantine: true,
};

/// The primary path, which before this wrote **nothing at all**: an
/// admitted proxied EXTERNAL call. One row, carrying when / which tool /
/// which page / which project / which conversation.
///
/// The "exactly once" half is the other half of the finding: the row must
/// name the moment the conversation stopped being clean, so a second
/// EXTERNAL call — which restates a fact this row already carries, and
/// writes its own ordinary MCP activity row — must not write another.
#[test]
fn the_proxied_intake_records_the_contamination_transition_exactly_once() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(Some("https://evil.example/page"), Some("evil.example")),
        )
        .is_ok());
    // A second EXTERNAL call, in the same conversation, from a different
    // page: the conversation is already contaminated.
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__search",
            ON,
            CallProvenance::intake(Some("https://other.example/q"), Some("other.example")),
        )
        .is_ok());

    let rows = outbound::test_rows::drain();
    let hits = contamination_rows(&rows);
    assert_eq!(
        hits.len(),
        1,
        "a contaminated conversation must produce exactly one transition row, got {:?}",
        hits.iter().map(|r| &r.entry.tool).collect::<Vec<_>>()
    );
    let row = hits[0];
    // WHEN — the standard stamp, not a field the writer invented.
    assert!(row.entry.ts_ms > 0, "the row has no timestamp");
    // WHICH TOOL — the call that caused the transition, not the later one.
    assert_eq!(row.entry.tool, "ddg__fetch_content");
    // WHICH PROJECT — the field F-3 calls load-bearing. An empty root here
    // makes the row invisible to every per-project surface.
    assert_eq!(row.entry.root, TEST_ROOT);
    assert!(!row.entry.root.is_empty());
    // Nothing was refused: the call was admitted, so the feed must not
    // paint this as a failure.
    assert!(row.entry.ok, "a contamination row is not a denial");
    let req = payload(row);
    assert_eq!(req["screen"], "contamination");
    assert_eq!(req["origin"], "internal");
    assert_eq!(
        req["scope"], "claude:claude-1",
        "the LatchScope::label form"
    );
    // WHICH CONVERSATION — what step 3 will join a checkpoint against.
    assert_eq!(req["session"], "sess-a");
    // FROM WHICH PAGE.
    assert_eq!(req["host"], "evil.example");
    assert_eq!(req["url"], "https://evil.example/page");
    assert_eq!(row.entry.target, "evil.example (claude:claude-1)");
    assert!(
        row.response.contains("CONTAMINATED"),
        "the detail must say what happened: {}",
        row.response
    );
    // The latch the call LEAVES the tab in, not the one it found. A row
    // written before `engage` would say `open` about a tab that is
    // EXTERNAL-latched from this very call — the reader would then look for
    // a second event that never happened.
    assert!(
        row.response.contains("latch=external"),
        "the row quotes the pre-engagement latch: {}",
        row.response
    );
}

/// The beacon path records the transition **as well as** its own
/// `latch_beacon` row. The two are different statements — "this
/// conversation stopped being clean" and "a harness-native web tool was
/// detected" — and a build that collapsed them into one would still pass
/// every count-shaped assertion about "a beacon writes a row".
#[test]
fn a_beacon_writes_the_contamination_row_and_its_own_beacon_row() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
    report_beacon(Some(&s), outbound::Origin::Http, "WebFetch", &out);

    let rows = outbound::test_rows::drain();
    assert_eq!(contamination_rows(&rows).len(), 1, "no contamination row");
    assert_eq!(
        outbound::test_rows::of_screen(&rows, outbound::Screen::LatchBeacon).len(),
        1,
        "the beacon row this work must not have displaced"
    );
    let row = contamination_rows(&rows)[0];
    assert_eq!(row.entry.tool, "WebFetch");
    assert_eq!(row.entry.root, TEST_ROOT);
    let req = payload(row);
    // A beacon is a local process POSTing the loopback, never evidence a
    // human acted — the row has to say so (#45).
    assert_eq!(req["origin"], "http");
    assert_eq!(req["scope"], "claude:claude-1");
    assert_eq!(req["session"], "sess-a");
    // Nothing was fetched *through* cImp, so there is no page to name —
    // absent rather than invented.
    assert_eq!(req["host"], serde_json::Value::Null);

    // And a caller in a loop writes neither row again.
    for _ in 0..5 {
        let again = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        report_beacon(Some(&s), outbound::Origin::Http, "WebFetch", &again);
    }
    assert!(
        outbound::test_rows::drain().is_empty(),
        "the transition is over; a loop must not be able to flood the feed"
    );
}

/// **The two silent cases F-3 is about.** Both contaminate without moving
/// any latch, so a fix keyed on the latch transition — or a test that only
/// exercised the happy path — leaves exactly the bug being fixed.
#[test]
fn contamination_is_recorded_even_when_no_latch_moves() {
    // (a) A tab already latched LOCAL. The beacon cannot flip it (the fetch
    //     already happened), so nothing about the latch changes — while
    //     every `context_note` from here on is quarantined.
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(reg.snapshot()[0].latch(), "local");
    let _ = outbound::test_rows::drain();

    let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
    assert!(!out.engaged, "a beacon never flips a LOCAL latch");
    let rows = outbound::test_rows::drain();
    let hits = contamination_rows(&rows);
    assert_eq!(hits.len(), 1, "the LOCAL-latched case recorded nothing");
    assert_eq!(hits[0].entry.root, TEST_ROOT);
    assert_eq!(payload(hits[0])["scope"], "claude:claude-1");

    // (b) The taint latch feature OFF, the memory quarantine ON. The
    //     contamination bit is still tracked (it is the quarantine's input),
    //     the latch never engages, and this is the posture under which the
    //     proxied path was silent even for a brand-new conversation.
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let t = scope("claude-2", Some("sess-b"));
    assert!(reg
        .gate(
            Some(&t),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            QUARANTINE_ONLY,
            CallProvenance::intake(Some("https://p.example/x"), Some("p.example")),
        )
        .is_ok());
    assert_eq!(
        reg.snapshot()[0].latch(),
        "open",
        "the latch feature is off, so nothing engaged"
    );
    let rows = outbound::test_rows::drain();
    let hits = contamination_rows(&rows);
    assert_eq!(hits.len(), 1, "the latch-off case recorded nothing");
    assert_eq!(hits[0].entry.root, TEST_ROOT);
    assert_eq!(payload(hits[0])["host"], "p.example");
    assert_eq!(payload(hits[0])["session"], "sess-b");
    assert!(
        hits[0].response.contains("latch=open"),
        "with the latch feature off the row must not claim a latch: {}",
        hits[0].response
    );
    // The quarantine that follows is the fact the row explains.
    assert_eq!(
        reg.gate(
            Some(&t),
            LatchRoute::Native,
            "context_note",
            QUARANTINE_ONLY,
            NO_CONTENT
        ),
        Ok(WriteTaint::Quarantined)
    );
}

/// The row follows the BIT, so everything that does not set the bit writes
/// nothing: a purely local conversation, a REFUSED external call (which
/// must never contaminate — that is what keeps a hallucinated call to the
/// blocked side from quarantining a clean session), a native route's
/// EXTERNAL-classified name (a typo, not a page), and an inert policy.
#[test]
fn nothing_that_leaves_the_conversation_clean_writes_a_contamination_row() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    for name in ["graph_snippet", "graph_outline", "context_recall"] {
        assert!(reg
            .gate(Some(&s), LatchRoute::Native, name, ON, NO_CONTENT)
            .is_ok());
    }
    // EXTERNAL on a NATIVE route: a misspelled native tool, not content.
    assert!(reg
        .gate(Some(&s), LatchRoute::Native, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    // The tab is LOCAL-latched now, so a proxied external call is refused.
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__search",
            ON,
            CallProvenance::intake(Some("https://evil.example/"), Some("evil.example")),
        ),
        Err(REFUSAL_EXTERNAL_BLOCKED)
    );
    // Both controls off: a disabled control leaves no trace at all.
    const OFF: GatePolicy = GatePolicy {
        latch: false,
        quarantine: false,
    };
    let inert = scope("claude-3", Some("sess-c"));
    assert!(reg
        .gate(
            Some(&inert),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            OFF,
            CallProvenance::intake(Some("https://evil.example/"), Some("evil.example")),
        )
        .is_ok());
    assert!(
        !reg.beacon(Some(&inert), "WebFetch", OFF, BEACON_PROV)
            .report
    );

    let rows = outbound::test_rows::drain();
    assert!(
        contamination_rows(&rows).is_empty(),
        "a clean conversation was reported as contaminated: {:?}",
        contamination_rows(&rows)
            .iter()
            .map(|r| &r.entry.tool)
            .collect::<Vec<_>>()
    );
    assert!(!reg.snapshot()[0].view.contaminated);
}

/// A tab with no identity keys nothing and reports nothing — the fail-open
/// reading every gate here takes. Stated as a test because the row's whole
/// value is per-tab attribution, and a row scoped to "(no tab identity)"
/// would be a row no per-project surface could use.
#[test]
fn an_identityless_call_records_no_contamination() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    assert!(reg
        .gate(
            None,
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(Some("https://evil.example/"), Some("evil.example")),
        )
        .is_ok());
    let _ = reg.beacon(None, "WebFetch", ON, BEACON_PROV);
    let rows = outbound::test_rows::drain();
    assert!(contamination_rows(&rows).is_empty());
}

/// **One transition per TAB, not per conversation** — and the row's
/// `session` therefore names the conversation contamination *started* in.
///
/// This follows H-2 rather than the beacon's own reporting rule, and the
/// difference is deliberate on both sides. `observe` re-arms
/// `beacon_flagged` on a proved session rotation (a new conversation may
/// report a native web tool again) but does **not** clear `contaminated`,
/// because the rotation signal is a file the model's own shell can write.
/// So a `/clear` in a contaminated tab keeps the taint, keeps quarantining
/// its memory writes — and writes no second row, because nothing
/// transitioned.
///
/// Pinned as a test because a consumer that joins these rows to
/// conversation-scoped state has to know it: the anchor is the tab's first
/// contamination, not "the contamination of the session you are looking
/// at". If the contamination bit ever regains a clear path, this is the
/// test that has to be revisited with it.
#[test]
fn contamination_is_recorded_once_per_tab_across_session_rotations() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let first = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&first),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(None, Some("a.example")),
        )
        .is_ok());
    let rotated = scope("claude-1", Some("sess-b"));
    assert!(reg
        .gate(
            Some(&rotated),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(None, Some("b.example")),
        )
        .is_ok());
    // The rotation did happen — the latch reopened and the budget refilled…
    assert_eq!(reg.snapshot()[0].session.as_deref(), Some("sess-b"));
    // …and the tab stayed contaminated across it, so there was no second
    // transition to record.
    assert!(reg.snapshot()[0].view.contaminated);
    let rows = outbound::test_rows::drain();
    let hits = contamination_rows(&rows);
    assert_eq!(
        hits.len(),
        1,
        "the sticky bit transitioned once, so exactly one row may exist"
    );
    assert_eq!(
        payload(hits[0])["session"],
        "sess-a",
        "the row names the conversation contamination STARTED in"
    );
    assert_eq!(payload(hits[0])["host"], "a.example");
}

/// The two paths produce ONE shape of row, because they share
/// [`note_contamination`]. Asserted over the payload KEYS rather than by
/// eye: a second writer that drifted (a missing `session`, a different
/// `scope` spelling) would give the Timeline two shapes to understand.
#[test]
fn both_contamination_paths_write_the_same_row_shape() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let a = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&a),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(Some("https://x.example/"), Some("x.example")),
        )
        .is_ok());
    let b = scope("claude-2", Some("sess-b"));
    let out = reg.beacon(Some(&b), "WebFetch", ON, BEACON_PROV);
    report_beacon(Some(&b), outbound::Origin::Http, "WebFetch", &out);

    let rows = outbound::test_rows::drain();
    let hits = contamination_rows(&rows);
    assert_eq!(hits.len(), 2);
    let keys = |r: &crate::activity::ActivityRecord| {
        let mut k: Vec<String> = payload(r)
            .as_object()
            .expect("object payload")
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    };
    assert_eq!(keys(hits[0]), keys(hits[1]));
    for row in &hits {
        assert_eq!(row.entry.source, "contamination");
        assert_eq!(row.entry.kind, "injection_flag");
        assert!(!row.entry.root.is_empty(), "an empty root defeats the row");
        assert!(row.entry.ok);
    }
}

// ── Step 4 — the two user-driven contamination clears ──────────────────
//
// The governing risk in this area, three findings running: the code is
// right against the proof-of-concept and wrong against the invariant, and
// the test pins the PoC's shape. So the cases below are written against the
// *observable consequence* wherever one exists — a re-contamination row
// rather than a boolean, a `WriteTaint` rather than a bit — and the two
// that guard H-2 assert what must NOT happen on a tab nobody armed.

/// A contaminated, EXTERNAL-latched tab in session `sess-a`, with a page
/// already fetched (so its budget carries real spend) and both
/// one-row-per-scope report bits used up.
fn contaminated_registry() -> (LatchRegistry, LatchScope) {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(Some("https://evil.example/p"), Some("evil.example")),
        )
        .is_ok());
    assert!(reg.snapshot()[0].view.contaminated);
    (reg, s)
}

/// A contaminated tab whose latch is **LOCAL** — the #48 beacon case: the
/// tab used a local tool first, then the harness's own `WebFetch` reported
/// in, which contaminates the conversation without moving the latch.
///
/// It exists because of a seam that is easy to test past. Under an EXTERNAL
/// latch a `context_note` is quarantined by the **latch**
/// (`Latch::proxy_gate`), whatever the contamination bit says — so a test
/// that cleared the bit on an EXTERNAL-latched tab and asserted the write
/// was still held would be asserting the latch's behaviour and calling it
/// the bit's. On a LOCAL-latched tab the bit is the only thing deciding, so
/// every assertion about what clearing changes is made here.
fn contaminated_local_registry() -> (LatchRegistry, LatchScope) {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
    assert!(!out.engaged, "the latch stays LOCAL");
    assert_eq!(out.view.latch, "local");
    assert!(out.view.contaminated);
    (reg, s)
}

/// The rows the clear wrote, in order.
fn cleared_rows(
    rows: &[crate::activity::ActivityRecord],
) -> Vec<&crate::activity::ActivityRecord> {
    outbound::test_rows::of_screen(rows, outbound::Screen::ContaminationCleared)
}

/// **A: false-positive resume.** The user judged the flagged content
/// harmless, so the bit goes now — and *nothing else moves*. The latch keeps
/// its position, the session keeps its id, the budget keeps its spend.
///
/// Asserting those three is the point rather than padding: "clear the
/// contamination flag" is a one-line change to a boolean, and the tempting
/// wrong version of it is `*entry = TabLatch::fresh()`, which would pass any
/// test that only looked at `contaminated`.
#[test]
fn a_false_positive_resume_clears_the_bit_and_touches_nothing_else() {
    let (reg, s) = contaminated_registry();
    // Spend the budget down to its limit so a reset would be visible.
    reg.charge(Some(&s), 100_000);
    assert_eq!(
        reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__search"),
        Err(outbound::REFUSAL_BUDGET)
    );

    let out = reg
        .apply_override(&s, LatchOverride::ClearContamination)
        .expect("a contaminated tab can be resumed");
    assert!(!out.view.contaminated, "the bit is gone");
    assert!(!out.view.can_clear, "and there is nothing left to clear");
    assert!(!out.view.awaiting_session_clear);

    let row = &reg.snapshot()[0];
    assert_eq!(
        row.session.as_deref(),
        Some("sess-a"),
        "the SESSION is untouched — a resume is not a restart"
    );
    assert_eq!(
        row.latch(),
        "external",
        "and so is the latch: it has its own two buttons"
    );
    assert_eq!(
        reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__search"),
        Err(outbound::REFUSAL_BUDGET),
        "and the fetch budget keeps its spend — a click that refilled it \
         would make the budget advisory"
    );
    // The consequence of leaving the latch alone, stated so nobody reads
    // this feature as more than it is: an EXTERNAL latch quarantines memory
    // writes on its OWN authority (`Latch::proxy_gate`), so clearing the bit
    // does not reopen persistence while the tab is still latched. Reopening
    // it is `unlatch`, which is a separate decision with a separate button.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Quarantined),
        "the LATCH still holds writes; clearing the bit is not an unlatch"
    );
    reg.apply_override(&s, LatchOverride::Unlatch)
        .expect("unlatch");
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Clean),
        "…and with both released, writes are clean again"
    );
}

/// **B: restore.** The user rolled files back. That cannot un-read a page,
/// so the bit **stays set** — this action only arms the wait.
///
/// The locked decision is the assertion: a build that "helpfully" cleared on
/// restore is the exact regression this test exists to catch, and it would
/// pass any test that merely checked the command succeeded.
#[test]
fn a_restore_arms_the_wait_and_clears_nothing_now() {
    // LOCAL-latched, so the quarantine assertion below is about the
    // contamination bit rather than about the latch — see
    // `contaminated_local_registry`.
    let (reg, s) = contaminated_local_registry();
    let out = reg
        .apply_override(&s, LatchOverride::AwaitSessionClear)
        .expect("a contaminated tab can be armed");
    assert!(
        out.view.contaminated,
        "restoring FILES cannot remove injected text from a context window"
    );
    assert!(out.view.awaiting_session_clear, "it arms the one-shot");
    // And the quarantine it gates is still in force for this conversation.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Quarantined),
        "a note written after the restore is still held for review"
    );

    // Arming twice is answered, not silently repeated: a second click that
    // reported success would imply something new happened.
    let again = reg
        .apply_override(&s, LatchOverride::AwaitSessionClear)
        .expect_err("a second arm is refused");
    assert!(again.contains("already waiting"), "{again}");
    // …and neither refusal nor repetition may clear anything.
    assert!(reg.snapshot()[0].view.contaminated);
}

/// **The critical case: the arm is what decides, not the rotation.**
///
/// Same registry, same decode-proven rotation, two tabs — one armed by a
/// user, one not. The armed tab clears; the unarmed one does not. If step 4
/// silently reverted H-2, the second half fails.
///
/// The rotation is driven **through** `harness::claude::read::LiveSessionGate` rather
/// than beside it, so a build that weakened the decode proof (H-2's own
/// guard) fails here too rather than quietly clearing on a forged file.
#[test]
fn only_an_armed_tab_clears_on_a_proved_rotation() {
    use crate::harness::claude::read::LiveSessionGate;

    for armed in [true, false] {
        outbound::test_rows::reset();
        let (reg, s) = contaminated_registry();
        if armed {
            reg.apply_override(&s, LatchOverride::AwaitSessionClear)
                .expect("arm");
        }
        let _ = outbound::test_rows::drain();

        // The tap proves the new transcript really is this tab's session:
        // a decoded record naming it, which is the ONLY thing that lets a
        // new id reach the live-session registry (H-2).
        let mut live = LiveSessionGate::default();
        live.rotated();
        assert!(!live.observed(false), "no evidence yet, no rotation");
        assert!(live.observed(true), "a decoded record IS the proof");

        // …and only now does a rotated scope reach the registry.
        let rotated = scope("claude-1", Some("sess-b"));
        let view = reg.view_for(&rotated);

        assert_eq!(
            view.contaminated, !armed,
            "armed={armed}: the ARM decides, not the rotation"
        );
        assert!(
            !view.awaiting_session_clear,
            "armed={armed}: a one-shot fires once"
        );
        let rows = outbound::test_rows::drain();
        assert_eq!(
            cleared_rows(&rows).len(),
            usize::from(armed),
            "armed={armed}: a clear writes exactly one row, a non-clear none"
        );

        // The consequence, not the boolean: whether the next memory write is
        // held for review.
        assert_eq!(
            reg.gate(
                Some(&rotated),
                LatchRoute::Native,
                "context_note",
                ON,
                NO_CONTENT
            ),
            if armed {
                Ok(WriteTaint::Clean)
            } else {
                Ok(WriteTaint::Quarantined)
            },
            "armed={armed}"
        );
    }
}

/// **A forged rotation on an unarmed tab still clears nothing** — H-2's own
/// case, re-run against step 4's code rather than against the code H-2 left.
///
/// Two forgeries, because they fail at two different bars:
///
/// 1. `type nul` / `echo {}` — the transcript yields no record naming the
///    session, so `LiveSessionGate` never confirms and no new id ever
///    reaches the registry at all.
/// 2. `echo '{"sessionId":"…"}'` — the decode bar is cleared (decision 3
///    puts the model's Bash outside every cImp latch, so it always can be),
///    the rotation DOES reach `observe`… and the unarmed tab is still
///    contaminated afterwards.
///
/// The deliberate counter-case is in the test above: on an **armed** tab a
/// forged rotation does clear, and that is the design. The arm is the
/// authority — an attacker cannot click restore — so a forgery only helps in
/// the case where the user has already decided the bit should go, and its
/// worst effect is lifting it slightly earlier than their own `/clear`.
#[test]
fn a_forged_rotation_cannot_clear_an_unarmed_tab() {
    use crate::harness::claude::read::LiveSessionGate;
    let (reg, _s) = contaminated_registry();

    // Forgery 1: bytes, but no record naming this session.
    let mut live = LiveSessionGate::default();
    live.rotated();
    for _ in 0..10 {
        assert!(
            !live.observed(false),
            "newline-terminated bytes are not evidence of a harness"
        );
    }
    // So the registry is never told about `sess-forged`, and the tab keeps
    // the session it was contaminated in.
    assert_eq!(reg.snapshot()[0].session.as_deref(), Some("sess-a"));

    // Forgery 2: the attacker writes a record naming the session, clearing
    // the decode bar. The rotation reaches `observe`.
    let forged = scope("claude-1", Some("sess-forged"));
    let view = reg.view_for(&forged);
    assert_eq!(
        view.latch, "open",
        "the permissive state does reset — the fix must not freeze latches"
    );
    assert!(
        view.contaminated,
        "…and the contamination bit does not: no rotation clears an unarmed tab"
    );
    assert_eq!(
        reg.gate(
            Some(&forged),
            LatchRoute::Native,
            "context_note",
            ON,
            NO_CONTENT
        ),
        Ok(WriteTaint::Quarantined),
        "the persistence channel stays closed"
    );
    // Nor can a rotation ARM one — the only writer of the arm is a click.
    assert!(!reg.snapshot()[0].view.awaiting_session_clear);
}

/// **Clearing re-arms the transition report — proved by the consequence.**
///
/// `latch_flagged` / `beacon_flagged` are one-row-per-scope claim bits, and
/// the `contamination` row is self-limiting through `note_contamination`'s
/// `mem::replace`. Leave any of them set across a clear and a tab that gets
/// re-contaminated writes **no new row**: the feed says the tab is clean, the
/// registry says it is not, and the only trace is the quarantine rows of
/// later writes. That is the same class of bug #48 fixed for the
/// `Local`-latched beacon.
///
/// Asserted as "a re-contamination writes a new row", not as
/// `assert!(!entry.beacon_flagged)`: the boolean is the mechanism, the row is
/// the invariant, and a mechanism swapped for another one must not fail this
/// test while a lost row must.
///
/// **And both claim bits are actually SPENT first.** The obvious version of
/// this test starts from a proxied fetch, which sets neither bit — so the
/// clear's resets are no-ops and deleting them leaves the test green. (That
/// was the first draft, and reverting the resets did not turn it red. It is
/// exactly the failure mode this whole area keeps producing: a test that
/// pins the happy path's shape rather than the invariant.) So the tab here
/// is LOCAL-latched and it spends both: a beacon that contaminates without
/// moving the latch, and a refused proxied call.
#[test]
fn a_re_contamination_after_a_clear_writes_a_new_row() {
    outbound::test_rows::reset();
    let (reg, s) = contaminated_local_registry();
    let rows = outbound::test_rows::drain();
    assert_eq!(
        contamination_rows(&rows).len(),
        1,
        "the first contamination is recorded"
    );
    // Spend `beacon_flagged`: this beacon reported, so the next one in the
    // same tab-session must not.
    for _ in 0..3 {
        assert!(!reg.beacon(Some(&s), "WebSearch", ON, BEACON_PROV).report);
    }
    // Spend `latch_flagged`: the first refusal writes a row, later ones do
    // not — that bound is what makes leaving the bit set invisible.
    for i in 0..3 {
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_BLOCKED)
        );
        let rows = outbound::test_rows::drain();
        let refusals = outbound::test_rows::of_screen(&rows, outbound::Screen::LatchRefusal);
        assert_eq!(refusals.len(), usize::from(i == 0), "refusal {i}");
    }

    reg.apply_override(&s, LatchOverride::ClearContamination)
        .expect("resume");
    // (No `contamination_cleared` row is expected from the registry here:
    // the resume's row is composed by `override_row` and written by
    // `apply_latch_override`, the IPC entry point, exactly as the two latch
    // moves' rows always have been. The same is true of the unlatch's
    // release row (decision 15's 2026-08-10 amendment), which `unlatch_clear_row`
    // composes for that same entry point — so the `Unlatch` below likewise
    // adds nothing to this feed. Both are asserted in
    // `every_clear_records_its_basis_and_the_state_it_replaced`.)
    assert!(cleared_rows(&outbound::test_rows::drain()).is_empty());

    // 1. The harness reads a page again. The conversation was clean a moment
    //    ago, so this is a NEW transition and must be reported as one — both
    //    as a contamination row and as the beacon's own row.
    let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
    assert!(out.contaminated_now, "the tab is contaminated again");
    assert!(
        out.report,
        "a beacon after a clear is a new fact — a stale `beacon_flagged` makes \
         the whole event silent, which is the #48 bug one clear later"
    );
    report_beacon(Some(&s), outbound::Origin::Http, "WebFetch", &out);
    let rows = outbound::test_rows::drain();
    assert_eq!(
        contamination_rows(&rows).len(),
        1,
        "the re-contamination writes its own transition row"
    );
    assert_eq!(
        outbound::test_rows::of_screen(&rows, outbound::Screen::LatchBeacon).len(),
        1,
        "…and the beacon row beside it"
    );

    // 2. The next refusal in the re-contaminated tab is likewise a fact the
    //    feed has not carried since the clear.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
        Err(REFUSAL_EXTERNAL_BLOCKED)
    );
    let rows = outbound::test_rows::drain();
    assert_eq!(
        outbound::test_rows::of_screen(&rows, outbound::Screen::LatchRefusal).len(),
        1,
        "a refusal after a clear must be reportable again"
    );

    // 3. And the proxied intake path, which flips the bit through a
    //    different door, reports its own re-contamination too.
    reg.apply_override(&s, LatchOverride::ClearContamination)
        .expect("resume again");
    let _ = outbound::test_rows::drain();
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(Some("https://evil2.example/p"), Some("evil2.example")),
        ),
        Err(REFUSAL_EXTERNAL_BLOCKED),
        "the LOCAL latch still refuses it — the clear is not an unlatch"
    );
    reg.apply_override(&s, LatchOverride::Unlatch)
        .expect("unlatch");
    let _ = outbound::test_rows::drain();
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(Some("https://evil2.example/p"), Some("evil2.example")),
        )
        .is_ok());
    let rows = outbound::test_rows::drain();
    let hits = contamination_rows(&rows);
    assert_eq!(hits.len(), 1, "the proxied path reports it too");
    assert_eq!(payload(hits[0])["host"], "evil2.example");
}

/// **Decision 10 is not touched by any of this.** Clearing the tab bit stops
/// FUTURE writes being held; notes already quarantined stay quarantined, and
/// promote-or-discard remains the Memory view's own review — a separate
/// consent surface with a separate click.
///
/// Two halves, because the interesting failure is a well-meaning one:
/// someone wiring "and release this tab's held notes" into the clear.
#[test]
fn clearing_the_bit_does_not_promote_anything_already_quarantined() {
    // LOCAL-latched: the bit is what decides here, not the latch.
    let (reg, s) = contaminated_local_registry();
    // A note written while contaminated is held for review.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Quarantined)
    );
    reg.apply_override(&s, LatchOverride::ClearContamination)
        .expect("resume");
    // Only the NEXT write changes.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Clean),
        "future writes are stored clean again — that is the whole effect"
    );

    // And the structural half: nothing on the clear path can reach a stored
    // note. The note store's release/delete API is named here so that wiring
    // it into this module fails the build's own test rather than a review.
    // `concat!` throughout: a needle written whole would match its own text
    // in the file it scans.
    // V42 R2 (#114): this module was one file when the scan was written, so it
    // read one. Every file the split produced is read, or the needle could
    // simply move next door. V42 R4 (#115) split the routes themselves, so
    // the route surface arrives as [`ROUTE_SOURCES`] rather than as a row.
    for (file, src) in ROUTE_SOURCES.iter().copied().chain([
        ("offload/discovery.rs", include_str!("../discovery.rs")),
        ("offload/latch.rs", include_str!("../latch.rs")),
    ]) {
        for promotion in [
            concat!("mem_", "promote_note"),
            concat!("mem_", "delete_note"),
            concat!("mem_", "quarantined_notes"),
        ] {
            assert!(
                !src.contains(promotion),
                "`{promotion}` appeared in {file} — promoting a quarantined note is \
                 the Memory view's own review (locked decision 10), not a side effect of \
                 clearing a tab's contamination flag"
            );
        }
    }
}

/// **The audit row: basis, prior state, and who acted** — for both clears,
/// because they are the same state change reached two ways and a reviewer
/// must be able to tell them apart.
#[test]
fn every_clear_records_its_basis_and_the_state_it_replaced() {
    // Half 1: the immediate resume. Origin `ipc` — a human, right now.
    outbound::test_rows::reset();
    let (reg, s) = contaminated_registry();
    let out = reg
        .apply_override(&s, LatchOverride::ClearContamination)
        .expect("resume");
    let row = override_row(
        outbound::Origin::Ipc,
        LatchOverride::ClearContamination,
        &out,
    );
    assert_eq!(
        row.screen,
        outbound::Screen::ContaminationCleared,
        "a clear is filed beside the row that SET the bit, not among latch moves"
    );
    assert_eq!(row.tool, "clear_contamination");
    assert_eq!(row.origin, outbound::Origin::Ipc);
    let d = &row.detail;
    assert!(d.contains("basis: clear_contamination"), "{d}");
    assert!(d.contains("origin: ipc"), "{d}");
    assert!(d.contains("contaminated=true"), "the PRIOR state: {d}");
    assert!(d.contains("latch=external"), "the PRIOR latch: {d}");
    assert!(d.contains("session=sess-a"), "the PRIOR session: {d}");
    assert!(d.contains("STAY quarantined"), "decision 10 stated: {d}");

    // Half 2: the armed rotation. The row is written by the registry itself
    // (nothing else observes the rotation), so it is asserted through the
    // feed rather than through a builder.
    outbound::test_rows::reset();
    let (reg, s) = contaminated_registry();
    reg.apply_override(&s, LatchOverride::AwaitSessionClear)
        .expect("arm");
    let armrows = outbound::test_rows::drain();
    let arm = outbound::test_rows::of_screen(&armrows, outbound::Screen::LatchOverride);
    assert_eq!(
        arm.len(),
        0,
        "the arm row is written by the IPC entry point"
    );
    assert!(
        cleared_rows(&armrows).is_empty(),
        "arming clears nothing, so it writes no clear row"
    );

    let rotated = scope("claude-1", Some("sess-b"));
    assert!(!reg.view_for(&rotated).contaminated);
    let rows = outbound::test_rows::drain();
    let hits = cleared_rows(&rows);
    assert_eq!(hits.len(), 1, "the armed clear writes exactly one row");
    let hit = hits[0];
    assert_eq!(hit.entry.tool, "session_clear_observed");
    assert_eq!(hit.entry.root, TEST_ROOT, "an empty root defeats the row");
    assert!(hit.entry.ok, "nothing was denied");
    let req = payload(hit);
    assert_eq!(req["screen"], "contamination_cleared");
    assert_eq!(
        req["origin"], "internal",
        "the trigger is cImp's own observation; `ipc` means a human acted NOW"
    );
    assert_eq!(req["scope"], "claude:claude-1");
    assert_eq!(
        req["session"], "sess-a",
        "filed under the CONTAMINATED conversation, so it joins the row that opened it"
    );
    let d = &hit.response;
    assert!(d.contains("basis: session_clear_observed"), "{d}");
    assert!(d.contains("ONE-SHOT"), "{d}");
    assert!(d.contains("session=sess-a"), "the PRIOR session: {d}");
    assert!(d.contains("(sess-b)"), "and the one that replaced it: {d}");
    assert!(d.contains("latch=external"), "the PRIOR latch: {d}");

    // Half 3: the full unlatch (decision 15's 2026-08-10 amendment). Same
    // shape as half 1 — asserted through the builder, because this row is
    // likewise composed for `apply_latch_override` to file — and it must be
    // a DIFFERENT basis from the resume: "that content was harmless" and "I
    // am taking the whole risk knowingly" are different claims, and a
    // reviewer who cannot tell them apart cannot reconstruct the decision.
    outbound::test_rows::reset();
    let (reg, s) = contaminated_registry();
    let out = reg
        .apply_override(&s, LatchOverride::Unlatch)
        .expect("unlatch");
    let row = unlatch_clear_row(outbound::Origin::Ipc, LatchOverride::Unlatch, &s, &out)
        .expect("the release owes its own row");
    assert_eq!(row.origin, outbound::Origin::Ipc, "a human acted NOW");
    assert_eq!(row.basis.tool(), "unlatch");
    assert_ne!(row.basis, ClearBasis::Resume);
    assert_eq!(row.scope, "claude:claude-1");
    assert_eq!(row.session.as_deref(), Some("sess-a"));
    let d = &row.detail;
    assert!(d.contains("basis: unlatch"), "{d}");
    assert!(d.contains("origin: ipc"), "{d}");
    assert!(d.contains("contaminated=true"), "the PRIOR state: {d}");
    assert!(d.contains("latch=external"), "the PRIOR latch: {d}");
    assert!(d.contains("session=sess-a"), "the PRIOR session: {d}");
    assert!(d.contains("STAY quarantined"), "decision 10 stated: {d}");
    assert!(
        d.contains("moved the latch to `open`"),
        "the one sentence the three bases cannot share: {d}"
    );
}

/// **The arm's own row.** It is not a clear, so it is filed as a latch
/// override — and it has to say, in words, that the flag is still set, or a
/// reader who sees "restore" and no later `contamination_cleared` row cannot
/// tell "still waiting" from "lost".
#[test]
fn the_restore_arm_writes_a_row_that_says_the_flag_is_still_set() {
    let (reg, s) = contaminated_registry();
    let out = reg
        .apply_override(&s, LatchOverride::AwaitSessionClear)
        .expect("arm");
    let row = override_row(
        outbound::Origin::Ipc,
        LatchOverride::AwaitSessionClear,
        &out,
    );
    assert_eq!(row.screen, outbound::Screen::LatchOverride);
    assert_eq!(row.tool, "await_session_clear");
    let d = &row.detail;
    assert!(d.contains("NOT cleared"), "{d}");
    assert!(d.contains("contaminated=true"), "{d}");
    assert!(d.contains("`/clear`"), "the user is told what to do: {d}");
}

// ── #48, finding M-7 — the route enumeration, by containment property ──
//
// The finding's third clause was that the three `/context/*` hook routes
// "appear in no route enumeration". They did appear in the pinned path list
// below — what they appeared in was an enumeration of STRINGS, which cannot
// tell a route that gates from a route that walks straight into
// `GraphService`. So the enumeration now records, per route, what it does
// about the taint latch, and the test checks that claim against the
// handler's own source rather than restating it.

/// What one HTTP route does about the V32 session taint latch.
#[derive(Debug)]
enum Containment {
    /// Gates, on a tool name the REQUEST supplies (`/run`, `/graph_run`,
    /// `/audit/run`, `/mcp/call`). Which class the call lands in is the
    /// caller's tool's business; that the registry is consulted at all is
    /// this route's.
    GatesRequestTool,
    /// Gates, on a FIXED [`toolclass::TABLE`] name — the three hook routes.
    /// `refused_under_external` states the security-relevant consequence:
    /// whether a conversation that has ingested untrusted content is
    /// REFUSED here. It is checked against `toolclass`, not restated, so a
    /// demotion of the row shows up as a failure of this test.
    GatesFixedTool {
        tool: &'static str,
        refused_under_external: bool,
    },
    /// Touches the registry for something that is not a capability gate —
    /// a state read, or the beacon (which can only ever tighten). The
    /// string says which.
    RegistryNoGate(&'static str),
    /// Never consults the registry. The string is the reason, and it is the
    /// claim a reviewer has to disagree with in order to add a route here.
    NoRegistry(&'static str),
}

struct RouteRow {
    path: &'static str,
    method: &'static str,
    /// The handler function the dispatch table routes to. `""` for the two
    /// routes answered inline in the dispatch arm itself.
    handler: &'static str,
    containment: Containment,
}

const fn route(
    path: &'static str,
    method: &'static str,
    handler: &'static str,
    containment: Containment,
) -> RouteRow {
    RouteRow {
        path,
        method,
        handler,
        containment,
    }
}

/// **Every route this listener serves, and what it does about containment.**
///
/// This is the single enumeration: `no_http_route_can_reach_a_contamination_clear`
/// pins the SURFACE from it and
/// `every_loopback_route_declares_what_it_does_about_the_latch` pins the
/// PROPERTY. A new route therefore cannot be added by editing one list.
const ROUTE_CONTAINMENT: &[RouteRow] = &[
    route("/run", "POST", "handle_run", Containment::GatesRequestTool),
    route(
        "/graph_run",
        "POST",
        "handle_graph_run",
        Containment::GatesRequestTool,
    ),
    route(
        "/audit/run",
        "POST",
        "handle_audit_run",
        Containment::GatesRequestTool,
    ),
    // RECORDED RESIDUAL, not a clean case. The auto-injection channel: its
    // V32 containment is the spotlighting / memory-quarantine envelope
    // (locked decisions 10/12), not refusal, and it fires on every prompt
    // rather than on a model's election. But its digest carries exported
    // SIGNATURES — the same content H-1 demoted `graph_repo_map` for — so
    // "ungated" here is a standing question, not a settled one. M-7 named
    // three routes and this is not one of them; it is written down so the
    // next reviewer inherits the question instead of rediscovering it.
    route(
        "/context/retrieve",
        "POST",
        "handle_context_retrieve",
        Containment::NoRegistry(
            "auto-injection; contained by the spotlight/quarantine envelope, not by refusal",
        ),
    ),
    // V33 Phase F. Takes a Workbench checkpoint before a mutating tool
    // call. Ungated on purpose, and the reason is not "nobody got round to
    // it": it returns two booleans, grants no capability and hands back no
    // project data, so a latch would have nothing to refuse — while a
    // refusal would remove checkpoints from a tab that had touched external
    // content, i.e. exactly the tab most likely to need a rewind. It is
    // still token-gated, tab-narrowed, throttled per `(root, tab)` and
    // re-checked against the class table for `mutates_fs`.
    route(
        "/workbench/tool_checkpoint",
        "POST",
        "handle_tool_checkpoint",
        Containment::NoRegistry(
            "snapshots the work tree into cImp's own shadow repo; returns no local data \
             and grants no capability",
        ),
    ),
    route(
        "/context/compaction",
        "POST",
        "handle_context_compaction",
        Containment::GatesFixedTool {
            tool: HOOK_TOOL_COMPACTION,
            // TRUSTED: paths, symbol names and memory-note text, no source
            // text. Gated so the class table stays the one place this can
            // change.
            refused_under_external: false,
        },
    ),
    route(
        "/context/should_read",
        "POST",
        "handle_should_read",
        Containment::GatesFixedTool {
            tool: HOOK_TOOL_SHOULD_READ,
            refused_under_external: true,
        },
    ),
    route(
        "/context/post_edit",
        "POST",
        "handle_post_edit",
        Containment::GatesFixedTool {
            tool: HOOK_TOOL_POST_EDIT,
            refused_under_external: true,
        },
    ),
    route(
        "/memory/event",
        "POST",
        "handle_memory_event",
        // Ingress, not egress: it records the caller's OWN tool/usage events
        // and returns no project data. Nothing to refuse — a latch here
        // would only lose the record of what the tab did.
        Containment::NoRegistry("records the caller's own events; returns no local data"),
    ),
    route(
        "/activity/contract_drift",
        "POST",
        "handle_contract_drift",
        Containment::NoRegistry("a shim reporting its own broken payload; returns nothing"),
    ),
    route(
        // The constant the CHILD sends, so the two ends cannot drift: the
        // dispatch arm is scanned from the source and compared against this
        // list, which is built from `DISCOVERY_SKIPPED_PATH`.
        DISCOVERY_SKIPPED_PATH,
        "POST",
        "handle_discovery_skipped",
        Containment::NoRegistry(
            "a child reporting a discovery entry it skipped; answers a fixed `ok` on every \
             path and touches no registry",
        ),
    ),
    route(
        "/permission/event",
        "POST",
        "handle_permission_event",
        Containment::NoRegistry("a hook reporting a permission prompt; returns nothing"),
    ),
    route(
        "/latch/beacon",
        "POST",
        "handle_latch_beacon",
        Containment::RegistryNoGate(
            "engages the EXTERNAL latch for a harness-native web tool; it can only tighten",
        ),
    ),
    route(
        "/latch/state",
        "POST",
        "handle_latch_state",
        Containment::RegistryNoGate(
            "reads this tab's view for the plugin gate; creates nothing",
        ),
    ),
    // V35 Phase I (CHP). A generated harness artifact declaring its protocol
    // version and what it will serve. Ungated on purpose and the reason is
    // not "nobody got round to it": it hands back no project data and grants
    // no capability, so a latch would have nothing to refuse — while a
    // refusal would deny cImp the one message that says a tab is running a
    // STALE plugin, i.e. exactly the tab whose containment behaviour is in
    // question. It is still token-gated, and its tab id is validated against
    // the user's configured AI tabs before anything is recorded.
    route(
        "/session/hello",
        "POST",
        "handle_session_hello",
        Containment::NoRegistry(
            "a harness artifact declaring its protocol version and capabilities; returns no \
             local data and grants nothing",
        ),
    ),
    // ── V35 Phase J: the Claude-native ingress ───────────────────────────
    //
    // Six routes carrying Claude Code's own hook payloads, replacing five
    // shim binaries. Each declares the SAME containment as the CHP route it
    // shares a core with, and the test below checks that against the
    // handler's own source — so the two transports cannot come to gate
    // differently, which is the only way this phase could have introduced a
    // containment hole.
    route(
        "/claude/hook/user_prompt_submit",
        "POST",
        "handle_claude_user_prompt_submit",
        // Same recorded residual as `/context/retrieve`, whose core this
        // shares: the auto-injection channel is contained by the
        // spotlight/quarantine envelope, not by refusal.
        Containment::NoRegistry(
            "auto-injection; contained by the spotlight/quarantine envelope, not by refusal",
        ),
    ),
    route(
        "/claude/hook/pre_compact",
        "POST",
        "handle_claude_pre_compact",
        Containment::GatesFixedTool {
            tool: HOOK_TOOL_COMPACTION,
            refused_under_external: false,
        },
    ),
    route(
        "/claude/hook/pre_tool_use",
        "POST",
        "handle_claude_pre_tool_use",
        Containment::GatesFixedTool {
            tool: HOOK_TOOL_SHOULD_READ,
            refused_under_external: true,
        },
    ),
    route(
        "/claude/hook/post_tool_use",
        "POST",
        "handle_claude_post_tool_use",
        Containment::GatesFixedTool {
            tool: HOOK_TOOL_POST_EDIT,
            refused_under_external: true,
        },
    ),
    route(
        "/claude/hook/notification",
        "POST",
        "handle_claude_notification",
        Containment::NoRegistry("a hook reporting a permission prompt; returns nothing"),
    ),
    route(
        "/claude/hook/session_start",
        "POST",
        "handle_claude_session_start",
        // The Claude twin of `/session/hello`, and ungated for its reasons:
        // it hands back no project data and grants no capability, while a
        // refusal would deny cImp the one message that says a tab is running
        // a STALE overlay.
        Containment::NoRegistry(
            "a harness artifact declaring its protocol version and capabilities; returns no \
             local data and grants nothing",
        ),
    ),
    // ── V35 Phase L: the read path, pushed ───────────────────────────────
    //
    // Six routes, one containment answer: none of them consults the latch,
    // and the reason is the same for all six and is worth stating rather
    // than assuming. Each one moves data FROM the harness INTO cImp and
    // hands nothing back — no project content, no tool, no capability. The
    // taint latch exists to stop a conversation that has ingested untrusted
    // content from reaching cImp's own capabilities; a route that only
    // receives has none to reach.
    //
    // The one that came closest to needing a gate is `assistant_text`,
    // because its effect is audible: a compromised artifact can make cImp
    // SAY something. That is a social-engineering surface, not code
    // execution (design § 5.2), and it is contained by three things that are
    // not the latch — the per-tab `tts_injection.enabled` toggle, the
    // app-side escape-sequence strip, and app-side segmentation of prose the
    // sender cannot control.
    route(
        "/session/assistant_text",
        "POST",
        "handle_session_assistant_text",
        Containment::NoRegistry(
            "receives assistant prose for TTS; returns no local data and grants no capability \
             — contained by the per-tab TTS toggle and app-side escape stripping, not by the \
             latch",
        ),
    ),
    // ── V39 Phase B: cross-harness delegation ───────────────────────────
    //
    // **Closed, not declared.** The first cut of this phase left the route
    // ungated and said so here, because the generated `delegate_task_<id>`
    // names cannot be classified by a static table and a gate keyed on them
    // would have been a silent no-op. That framing was the mistake: the
    // name the GATE needs was never the one the model types. The child
    // resolves the harness id and forwards it; the route states its own
    // class-table identity (`DELEGATE_TOOL`), exactly as the three
    // `/context/*` hooks do — which makes this a fixed-tool route, and the
    // per-harness naming a non-problem.
    //
    // It gates as LOCAL-CAPABILITY, so a contaminated conversation is
    // refused here for the same reason V32 C-1c refuses it `offload_task`:
    // both hand a task to a fresh, permissive executor, and this one's is a
    // whole peer harness with its own tools. "The user asked for it" does
    // not launder the request — the task text is model-authored.
    route(
        "/delegate",
        "POST",
        "handle_delegate",
        Containment::GatesFixedTool {
            tool: DELEGATE_TOOL,
            // Computed, not restated: LOCAL-CAPABILITY under an EXTERNAL
            // latch is blocked, so a demotion of the row fails this test.
            refused_under_external: true,
        },
    ),
    route(
        "/session/tool_result",
        "POST",
        "handle_session_tool_result",
        Containment::NoRegistry("receives one tool result's SIZE; returns nothing"),
    ),
    route(
        "/session/output_started",
        "POST",
        "handle_harness_output",
        Containment::NoRegistry(
            "receives a turn boundary; returns nothing and grants no capability — the tab \
             must have declared the event in its hello for it to reach the avatar at all",
        ),
    ),
    route(
        "/session/output_stopped",
        "POST",
        "handle_harness_output",
        Containment::NoRegistry(
            "receives a turn boundary; returns nothing and grants no capability — the tab \
             must have declared the event in its hello for it to reach the avatar at all",
        ),
    ),
    route(
        "/session/subagents_active",
        "POST",
        "handle_subagents_active",
        Containment::NoRegistry("receives a sub-agent COUNT edge; returns nothing"),
    ),
    route(
        "/session/subagent",
        "POST",
        "handle_session_subagent",
        Containment::NoRegistry("receives a sub-agent lifecycle edge; returns nothing"),
    ),
    route(
        "/claude/hook/stop",
        "POST",
        "handle_claude_stop",
        Containment::NoRegistry(
            "the Claude ingress for /session/assistant_text; same core, same answer",
        ),
    ),
    route(
        "/claude/hook/post_tool_use_result",
        "POST",
        "handle_claude_tool_result",
        Containment::NoRegistry(
            "the Claude ingress for /session/tool_result; sizes a result, runs nothing",
        ),
    ),
    route(
        "/claude/hook/subagent",
        "POST",
        "handle_claude_subagent",
        Containment::NoRegistry(
            "the Claude ingress for /session/subagent; same core, same answer",
        ),
    ),
    route(
        "/claude/hook/post_tool_use_failure",
        "POST",
        "handle_claude_tool_failure",
        Containment::NoRegistry(
            "the errored half of /session/tool_result; sizes an error string, runs nothing",
        ),
    ),
    // ── 2026-08-17: the two migrated beacons ─────────────────────────────
    //
    // Each declares the SAME containment as the harness-neutral route it
    // shares a core with, and the test below checks that against the
    // handler's own source — so the two transports cannot come to gate
    // differently, which is the only way a migration like this could
    // introduce a containment hole.
    route(
        "/claude/hook/pre_tool_use_taint",
        "POST",
        "handle_claude_taint_beacon",
        Containment::RegistryNoGate(
            "engages the EXTERNAL latch for a harness-native web tool; it can only tighten",
        ),
    ),
    route(
        "/claude/hook/pre_tool_use_checkpoint",
        "POST",
        "handle_claude_checkpoint",
        // `/workbench/tool_checkpoint`'s answer, for its reasons: it
        // snapshots the work tree into cImp's own shadow repo, returns no
        // local data and grants no capability, so a latch would have nothing
        // to refuse — while a refusal would remove checkpoints from the tab
        // most likely to need a rewind.
        Containment::NoRegistry(
            "snapshots the work tree into cImp's own shadow repo; returns no local data \
             and grants no capability",
        ),
    ),
    route(
        "/mcp/list",
        "POST",
        "handle_mcp_list",
        // Advertisement only. Consumers cache `tools/list` at connect, which
        // is exactly why the proxy enforces by REFUSAL at `/mcp/call`
        // instead of by removing defs here (decision 3).
        Containment::NoRegistry("tool advertisement; enforcement is by refusal at /mcp/call"),
    ),
    route(
        "/mcp/call",
        "POST",
        "handle_mcp_call",
        Containment::GatesRequestTool,
    ),
    route(
        "/describe",
        "GET",
        "",
        Containment::NoRegistry("the proxy's own tool list as text; no project data"),
    ),
    route(
        "/events",
        "GET",
        "handle_events",
        Containment::NoRegistry("the offload service's own event stream"),
    ),
    route(
        "/health",
        "GET",
        "",
        Containment::NoRegistry("a fixed `ok`"),
    ),
    route(
        "/status",
        "GET",
        "handle_status",
        // Reads latch state through `latch_snapshot`, not through the
        // registry handle — a debug view over cImp's own identifiers, with
        // no capability behind it.
        Containment::NoRegistry("a debug view of cImp's own identifiers"),
    ),
];

/// Every path the dispatch table routes, sorted and deduped — scanned from
/// the source because the `match` is not reachable from a test.
///
/// V42 R4 (#115): every file of the route surface is scanned, not just the
/// one the `match` is in. Core's arms all live in `loopback/mod.rs` today,
/// and a route arm is not something a family file may grow — but a scan that
/// looked in one place could not tell the difference between "there are none"
/// and "they moved", and this list is what the containment enumeration is
/// checked against in both directions.
fn dispatched_routes(files: &[(&'static str, &'static str)]) -> Vec<&'static str> {
    let mut routes: Vec<&'static str> = Vec::new();
    for marker in ["(\"POST\", \"", "(\"GET\", \""] {
        for (_file, src) in files {
            for part in src.split(marker).skip(1) {
                routes.push(part.split('"').next().expect("a closing quote"));
            }
        }
    }
    // V40 Phase C, locked decision 15: core's `match` is no longer the whole
    // surface — every registered plugin's `routes()` is appended after it.
    // A route this listener serves is a route this listener serves,
    // whichever file declares it, so the containment enumeration has to see
    // all of them or a plugin could add an ungated door by declaring it.
    for h in crate::harness::registry::all() {
        let Some(p) = h.plugin() else { continue };
        routes.extend(p.routes().iter().map(|r| r.path));
    }
    routes.sort_unstable();
    routes.dedup();
    routes
}

/// The `pub const NAME: &str = "<path>";` a plugin source declares for
/// `path`, by name.
///
/// The join that lets the containment enumeration check a plugin's route
/// table without restating its constants: the table is written in terms of
/// the constants, the enumeration in terms of the paths, and this is what
/// pins the two spellings equal.
fn route_const_named(src: &str, path: &str) -> Option<String> {
    let needle = format!(": &str = \"{path}\";");
    src.lines().find_map(|l| {
        let l = l.trim();
        let rest = l.strip_prefix("pub const ")?;
        let (name, tail) = rest.split_once(':')?;
        (format!(":{tail}") == needle).then(|| name.trim().to_string())
    })
}

/// **Every 400 body a route sends for an unparseable request, pinned.**
///
/// V42 R22 (#115) folded the decode-body-or-400 preamble into [`decode`],
/// whose `refusal` parameter exists because these replies are NOT one shape:
/// the pushed `/session/*` routes send no parse detail at all, `/delegate`
/// sends its own result type, and the hook routes build a bare object where
/// the task-shaped routes build a [`RunResult`]. The children and shims that
/// read them (`offload::mcp`, `audit::mcp::run_via_loopback`, the generated
/// OpenCode plugin, the Claude hook shims) parse what they are sent, and
/// nothing pinned these bytes before — a route's 400 path needs a `TcpStream`
/// to reach — so they are pinned here, at the builders.
///
/// **Why the first two coincide today, and why they are still two functions.**
/// `serde_json` is built with `preserve_order` in this tree (it is in the lock
/// file's dependency list, pulled in transitively), so `json!` emits its keys
/// in insertion order and the bare object happens to agree with the struct.
/// Without that feature a `Map` is a `BTreeMap` and the same object would come
/// out `error` first. That is a transitive build detail, not something either
/// route decided — so each keeps building the body it always built, and this
/// test is what would notice if the resolution changed underneath them.
///
/// The serde wording is deliberately not pinned; what is pinned is the
/// envelope: which fields, in which order, with which prefix.
#[test]
fn every_bad_body_reply_keeps_its_own_bytes() {
    let parse_error = || {
        serde_json::from_slice::<serde_json::Value>(b"{").expect_err("an unparseable body")
    };
    let detail = serde_json::to_string(&format!("bad request body: {}", parse_error()))
        .expect("a JSON string");
    let with_detail = format!("{{\"ok\":false,\"error\":{detail}}}");

    // 1. The task-shaped routes: `/run`, `/graph_run`, `/audit/run`,
    //    `/mcp/call`, `/latch/beacon`, `/latch/state`, `/session/hello`.
    assert_eq!(
        serde_json::to_string(&bad_body_result(parse_error())).expect("serializes"),
        with_detail
    );

    // 2. The hook routes: `/context/*`, `/workbench/tool_checkpoint`,
    //    `/activity/contract_drift`.
    assert_eq!(
        serde_json::to_string(&bad_body_json(parse_error())).expect("serializes"),
        with_detail
    );

    // 3. The pushed `/session/*` routes: no parse detail reaches the caller.
    assert_eq!(
        serde_json::to_string(&bad_request("bad request body")).expect("serializes"),
        r#"{"ok":false,"error":"bad request body"}"#
    );

    // 4. `/delegate`, which answers in its own result type — the model reads
    //    this one as a tool result, so every absent field stays absent.
    assert_eq!(
        serde_json::to_string(&DelegateResult::failed(format!(
            "bad request body: {}",
            parse_error()
        )))
        .expect("serializes"),
        with_detail
    );
}

/// **V33 C4's allowlist, RUN rather than read** (V42 Phase A2).
///
/// The scan below
/// ([`post_edit_takes_its_working_directory_from_the_app_not_from_the_body`])
/// asserts that `hook_exec_roots` takes the context and the settings and NOT
/// the request. What it could never assert is what the function ANSWERS,
/// because it took an `AppHandle` and this crate has no `tauri::test` mock.
/// With the handles injected it can be called, so the property `POST
/// /context/post_edit` rests on — the directories it may run the project's
/// configured check commands in are the served root and its configured tabs'
/// directories, and nothing a caller names — is now a behavioural test:
///
/// * the launch directory is always admitted, and is the answer when the body
///   names no cwd at all;
/// * a configured tab's own `cwd` joins the list;
/// * a sibling directory outside every root is REFUSED, which is the whole
///   point — a hook payload's `cwd` is attacker-influenced (#104), and a miss
///   must deny rather than fall back to "run it wherever you asked".
#[test]
fn the_post_edit_allowlist_is_the_served_root_and_its_tabs_and_nothing_else() {
    use crate::offload::host::testing::{route_ctx, FakeRouteServices};
    use crate::service::host::testing::core_host;
    use crate::settings::{AiToolTabConfig, Settings, SettingsHandle, TabConfig};

    let scratch = root_tree("exec-roots");
    let served = scratch.join("served");
    let tab_dir = scratch.join("worker");
    let outside = scratch.join("elsewhere");
    for d in [&served, &tab_dir, &outside] {
        std::fs::create_dir_all(d).unwrap();
    }

    // `AiToolTabConfig` has private fields (the injection overrides), so the
    // seed is built by assignment — `service::delegation`'s fixture note.
    #[allow(clippy::field_reassign_with_default)]
    let mut cfg = AiToolTabConfig::default();
    cfg.id = "ai-worker".to_string();
    cfg.name = "worker".to_string();
    cfg.cwd = Some(tab_dir.clone());
    let settings = Settings {
        tabs: vec![TabConfig::AiTool(cfg)],
        ..Default::default()
    };

    let mut core = core_host(SettingsHandle::new(
        settings.clone(),
        settings.clone(),
        scratch.clone(),
    ))
    .host;
    core.launch_cwd = served.clone();
    let ctx = route_ctx(FakeRouteServices {
        core: Some(core),
        ..Default::default()
    });

    let roots = super::hook_exec_roots(&ctx, &settings);
    assert!(
        roots.contains(&served),
        "the served root must always be admitted: {roots:?}"
    );
    assert!(
        roots.contains(&tab_dir),
        "a configured tab's own directory must be admitted: {roots:?}"
    );

    assert_eq!(
        super::admitted_hook_root(&roots, None),
        Some(served.clone()),
        "a body naming no cwd runs in the served root, not wherever the process happens to be"
    );
    assert_eq!(
        super::admitted_hook_root(&roots, Some(&tab_dir.to_string_lossy())),
        Some(tab_dir.clone()),
    );
    assert_eq!(
        super::admitted_hook_root(&roots, Some(&outside.to_string_lossy())),
        None,
        "a directory outside every root must be REFUSED, not fallen back on"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

/// **No route file reaches into managed state** (V42 Phase A2's tripwire).
///
/// Twenty-one `AppHandle::try_state::<T>()` calls used to sit in these files,
/// resolving the settings, the code graph, the Workbench service and the audit
/// runner. Each was a service-locator call inside a security boundary — code
/// whose whole job is to answer "what may this request have?", also answering
/// "and is that subsystem up?". They are one injected
/// [`RouteCtx`](crate::offload::host::RouteCtx) now, resolved in
/// `offload/host.rs`, which is deliberately NOT on the route surface.
///
/// This is the guard that cleanup earns, and it is structural rather than
/// stylistic: a handler that can reach the managed-state table can reach ANY
/// managed service, including ones no reviewer of this directory expects it to
/// touch, and it can do so without changing a signature — so nothing else here
/// would notice. Two layers, because either alone is one edit from useless:
///
/// 1. no lookup call, and
/// 2. no `tauri::Manager` import — the trait those methods live on. Without it
///    in scope a lookup does not compile, so a future edit has to defeat this
///    test twice.
///
/// The scan reads CODE, not prose ([`files_containing`]), so the paragraphs
/// above can name the thing they forbid.
#[test]
fn no_route_file_reaches_into_managed_state() {
    for needle in ["try_state::<", ".state::<"] {
        let offenders = files_containing(ROUTE_SOURCES, needle);
        assert!(
            offenders.is_empty(),
            "`{needle}` is back on the route surface, in {offenders:?}. A route handler asks \
             its `RouteCtx` for what it needs by name (`ctx.settings()`, `ctx.graph()`, \
             `ctx.workbench()`, `ctx.audit()`, `ctx.core()`); the lookups behind those live in \
             `offload/host.rs`, off the route surface, so that adding a NEW reach is a change \
             to the declared context rather than an invisible line in a handler."
        );
    }
    for needle in ["tauri::Manager", "Manager,", "Manager}", " Manager;"] {
        let offenders = files_containing(ROUTE_SOURCES, needle);
        assert!(
            offenders.is_empty(),
            "`{needle}` — the trait `state`/`try_state` live on — is imported by {offenders:?}. \
             Keeping it out is layer 2 of this tripwire: a route file that cannot name the \
             trait cannot call the lookup, whatever it calls the variable."
        );
    }
    // The control: this scan is only worth anything if it CAN see a lookup, so
    // assert it against a synthetic file rather than trusting that production
    // text happens to be clean.
    let planted = "async fn handle_probe(app: &AppHandle) {\n    app.try_state::<Thing>();\n}\n";
    assert_eq!(
        files_containing(&[("planted.rs", planted)], "try_state::<"),
        vec!["planted.rs"],
        "the scan stopped seeing a lookup, so its green above means nothing"
    );
}

/// **The scanners see every file the routes are in.**
///
/// [`ROUTE_SOURCES`] is what every source-scanning test below reads, and it is
/// hand-kept — so the way it goes wrong is a family file added to `mod.rs` and
/// not to the list: the new routes' handlers would then be scanned by nobody,
/// with every test green. Joined here at the two real sources, the `mod`
/// declarations and the list itself, in both directions.
#[test]
fn the_source_scanners_read_every_route_file() {
    // V42 review RV-7: the scrape reads THROUGH the visibility modifier. It
    // used to be `strip_prefix("mod ")`, which sees `mod x;` and nothing else
    // — and the file it could not see would be missing from `ROUTE_SOURCES`
    // too, so the join below would compare two equally-short lists and pass.
    // These are the spellings a family file can legitimately be declared with;
    // each must be seen.
    for spelling in [
        "mod probe;",
        "pub mod probe;",
        "pub(crate) mod probe;",
        "pub(super) mod probe;",
        "pub(in crate::offload) mod probe;",
    ] {
        assert_eq!(
            mod_name(spelling),
            Some("probe"),
            "`{spelling}` is invisible to the scrape — a route file declared that way \
             would be scanned by nobody with this test green"
        );
    }
    // …and it stays a TOP-LEVEL declaration scrape: prose, a nested `mod`, an
    // inline module and a lookalike identifier are all not one.
    for not_a_route_file in [
        "// mod probe;",
        "    mod probe;",
        "mod probe {",
        "use foo::mod_probe;",
    ] {
        assert_eq!(
            mod_name(not_a_route_file),
            None,
            "`{not_a_route_file}` was read as a route-file declaration"
        );
    }

    let dispatch = include_str!("mod.rs");
    let mut declared: Vec<String> = dispatch
        .lines()
        .filter_map(mod_name)
        .filter(|m| *m != "tests")
        .map(|m| format!("offload/loopback/{m}.rs"))
        .collect();
    // Vacuity guard: an empty scrape would make the comparison below trivially
    // satisfiable by an empty list, which is the failure this test is about.
    assert!(
        declared.len() > 5,
        "the `mod` scrape found {declared:?} — it is not seeing the declarations"
    );
    declared.push("offload/loopback/mod.rs".to_string());
    declared.sort();

    let mut listed: Vec<String> = ROUTE_SOURCES.iter().map(|(f, _)| f.to_string()).collect();
    listed.sort();
    assert_eq!(
        listed, declared,
        "a route file is declared but unscanned (or the reverse) — every source \
         scan below would silently stop covering it"
    );

    // …and no two rows are the same text: a list of twelve copies of one
    // `include_str!` would satisfy the join above and scan one file twelve
    // times.
    for (a, (file, src)) in ROUTE_SOURCES.iter().enumerate() {
        for (other, second) in ROUTE_SOURCES.iter().skip(a + 1) {
            assert_ne!(
                src, second,
                "{file} and {other} scan the same text — one of the rows names the \
                 wrong file"
            );
        }
    }
}

/// Whether `path` is served by a plugin rather than by core's own `match`.
fn is_plugin_route(path: &str) -> bool {
    crate::harness::registry::all()
        .filter_map(|h| h.plugin())
        .any(|p| p.routes().iter().any(|r| r.path == path))
}

/// The source text of one top-level `async fn`, signature to closing brace,
/// with line endings normalised to `\n` (this file is checked out CRLF on
/// Windows, and a needle written with `\n` would silently match nothing —
/// which for a security assertion means silently passing).
///
/// Starts at the SIGNATURE, so a handler's doc comment is deliberately not
/// part of it: a route must not be able to claim a gate in prose.
fn handler_body(name: &str) -> String {
    // V40 Phase C: the twelve `handle_claude_*` bodies and the legacy
    // `--notify-hook` route live in `harness/claude/hook.rs` now, and V42 R4
    // (#115) spread the rest across `loopback/*.rs`. These scans are about
    // the PROPERTY (one core per capability, the gate in the route's own
    // body), which neither move changed — so the scanner takes the whole
    // surface and follows the code, rather than the tests being deleted with
    // it or left reading the file it used to be in.
    fn_body_in(&route_surface(), &format!("async fn {name}("))
}

/// [`handler_body`] for any top-level item, given its exact opening text —
/// so a non-`async` helper (or a shared core, V35 Phase J) can be scanned by
/// the same rules.
///
/// V42 review (dropped-at-cap): there used to be a second, byte-identical
/// copy of this called `top_level_fn`, described as "the non-`async` twin".
/// It never was one — the signature is a parameter, so `async` is just part
/// of the text — and two copies of a scanner primitive is one copy that can be
/// hardened while the other quietly is not. There is one.
fn fn_body(src: &str, sig: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in src.lines() {
        if !inside {
            // V40 Phase C: a moved item is often `pub(crate)` now, because
            // its one remaining caller is the plugin that used to sit
            // beside it; V42 R4 (#115) made a family file's items
            // `pub(super)`. The item is still top-level, which is the
            // property the column-0 `}` terminator depends on — see
            // [`declares`].
            if !declares(line, sig) {
                continue;
            }
            inside = true;
        }
        out.push_str(line);
        out.push('\n');
        // The closing brace of a top-level item is the only `}` in column 0.
        if line == "}" {
            break;
        }
    }
    assert!(!out.is_empty(), "no top-level `{sig}`");
    assert!(
        out.ends_with("}\n"),
        "`{sig}` was not terminated — the scan would read past it"
    );
    out
}

/// **M-7's third clause.** Every route the listener serves declares what it
/// does about the taint latch, and the declaration is checked against the
/// handler rather than believed.
///
/// The four checks, and what each one catches:
///
/// 1. Every dispatched path is declared, and every declared path is
///    dispatched — so a new route cannot slip in unclassified.
/// 2. A route that claims to gate must actually reach `latches()`. **This
///    is the check that would have failed before this commit** for the
///    three `/context/*` hooks, and it is what stops the classic failure of
///    a gate tested through its helper while the call site is deleted.
/// 3. A route that claims NOT to touch the registry must not — so a gate
///    added without a review of what it means to that route also fails.
/// 4. A fixed-tool route names a real class-table row, uses that constant
///    in its own body, and the declared "refused under EXTERNAL" answer is
///    computed from [`toolclass`], not restated. Demoting
///    `hook_post_edit` to TRUSTED therefore fails here.
#[test]
fn every_loopback_route_declares_what_it_does_about_the_latch() {
    // V42 R4 (#115): the dispatch `match` is core's and stays in
    // `loopback/mod.rs`, but the handlers it names are spread across the
    // family files — so the two halves of this test read different things:
    // the ARM from the dispatch, the BODY from whichever family declares it.
    let dispatch = include_str!("mod.rs");

    // 1. Surface ↔ declaration, both directions.
    let mut declared: Vec<&str> = ROUTE_CONTAINMENT.iter().map(|r| r.path).collect();
    declared.sort_unstable();
    assert_eq!(
        dispatched_routes(ROUTE_SOURCES),
        declared,
        "a route is dispatched but undeclared (or the reverse)"
    );

    for row in ROUTE_CONTAINMENT {
        // The declared handler really is the one the route is served by.
        // For core's own arms that is the dispatch `match`; for a plugin
        // route it is the `route!` entry in the plugin's table, resolved
        // through the path constant so the two spellings cannot part.
        if is_plugin_route(row.path) {
            let konst = route_const_named(HOOK_SRC, row.path).unwrap_or_else(|| {
                panic!("{} is served by a plugin but named by no constant", row.path)
            });
            assert!(
                HOOK_SRC.contains(&format!(
                    "route!(\"{}\", {konst}, {})",
                    row.method, row.handler
                )),
                "{} does not register `{}` in the plugin's route table",
                row.path,
                row.handler
            );
        } else {
            let arm = format!("(\"{}\", \"{}\") =>", row.method, row.path);
            let arm_at = dispatch
                .find(&arm)
                .unwrap_or_else(|| panic!("no dispatch arm for {}", row.path));
            if !row.handler.is_empty() {
                assert!(
                    dispatch[arm_at..].starts_with(&format!("{arm} {}(", row.handler)),
                    "{} does not dispatch to `{}`",
                    row.path,
                    row.handler
                );
            }
        }
        // The two inline arms have no handler to scan; nothing behind them
        // can gate, which is why they are the only rows allowed to omit one.
        if row.handler.is_empty() {
            assert!(
                matches!(row.containment, Containment::NoRegistry(_)),
                "{} is answered inline, so it cannot be gating anything",
                row.path
            );
            continue;
        }
        let body = handler_body(row.handler);
        // V40 Phase C: a plugin route reaches the registry through the
        // narrow facades (`hook_gate_admits`, `latch_beacon_for`), because
        // `LatchRegistry` is private to this module and a harness may not
        // hold it. Same funnels, one indirection further out.
        let reaches_registry = body.contains("latches()")
            || body.contains("hook_gate_admits(")
            || body.contains("latch_beacon_for(");
        let gates = body.contains("latches().gate(")
            // V40 Phase C: a plugin route cannot hold `LatchRegistry` (it is
            // private to this module), so its gate call is the narrow
            // facade. Same funnel, same decision, same ledger.
            || body.contains("if !hook_gate_admits(")
            || body.contains("hook_admit(\n        latches(),")
            || body.contains("audit_admit(\n        latches(),")
            // V39 Phase B: `/delegate`'s own admit funnel, same shape and same
            // reason as the two above — the decision is a function so it can be
            // unit-tested without a `TcpStream`, which means the handler body
            // names the funnel rather than `latches().gate(`.
            || body.contains("delegate_admit(\n        latches(),");

        match row.containment {
            Containment::GatesRequestTool => assert!(
                gates,
                "{} claims to gate but its handler never reaches the latch registry",
                row.path
            ),
            Containment::GatesFixedTool {
                tool,
                refused_under_external,
            } => {
                assert!(
                    gates,
                    "{} claims to gate but its handler never reaches the latch registry",
                    row.path
                );
                assert!(
                    body.contains(tool_const(tool)),
                    "{} must gate on `{tool}`'s constant in its own body",
                    row.path
                );
                // The security-relevant property, computed rather than
                // restated: is a contaminated conversation refused here?
                assert_eq!(
                    Latch::External.blocks(toolclass::classify(tool)),
                    refused_under_external,
                    "`{tool}`'s class no longer matches what {} declares",
                    row.path
                );
            }
            Containment::RegistryNoGate(why) => {
                assert!(
                    reaches_registry,
                    "{} claims to reach the registry ({why}) and does not",
                    row.path
                );
                assert!(
                    !gates,
                    "{} now gates capability — declare it, don't leave it as a state read",
                    row.path
                );
            }
            Containment::NoRegistry(why) => assert!(
                !reaches_registry,
                "{} is declared ungated ({why}) but now reaches the latch registry",
                row.path
            ),
        }
    }
}

/// The identifier a hook tool-name constant is written as at the call site.
/// The handler bodies use the CONSTANT, not the string, so the check above
/// has to look for the same thing a reader would.
fn tool_const(tool: &str) -> &'static str {
    match tool {
        "delegate_task" => "DELEGATE_TOOL",
        "hook_post_edit" => "HOOK_TOOL_POST_EDIT",
        "hook_should_read" => "HOOK_TOOL_SHOULD_READ",
        "hook_compaction" => "HOOK_TOOL_COMPACTION",
        other => panic!("no constant known for `{other}`"),
    }
}

/// **M-7's first clause: an EXTERNAL-latched tab reaches local capability
/// through these routes.** Now it does not.
///
/// `post_edit` executes the project's configured check commands and
/// `should_read` hands back repo source text, so a conversation that has
/// ingested untrusted content is refused both. The compaction carry-over is
/// admitted, and that is stated here rather than left to be inferred from a
/// missing assertion — it is TRUSTED content (paths, symbol names, note
/// text) and refusing it would also skip the route's dedup-clear side
/// effects.
#[test]
fn a_contaminated_conversation_is_refused_the_executing_hook_routes() {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    // One proxied fetch contaminates the conversation.
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(reg.snapshot()[0].latch(), "external");

    let admit = |tool: &'static str| {
        hook_admit(
            &reg,
            tool,
            "claude",
            Some("claude-1"),
            |_, _| LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
            |_| ON,
        )
    };
    assert_eq!(
        admit(HOOK_TOOL_POST_EDIT),
        Err(REFUSAL_LOCAL_BLOCKED),
        "a contaminated conversation must not have the project's checks executed for it"
    );
    assert_eq!(
        admit(HOOK_TOOL_SHOULD_READ),
        Err(REFUSAL_LOCAL_BLOCKED),
        "…nor be handed repo source text by the read advisor"
    );
    assert_eq!(
        admit(HOOK_TOOL_COMPACTION),
        Ok(()),
        "the carry-over is TRUSTED content and stays admitted"
    );
    // A refused hook never redefines which side of the boundary the
    // conversation is on.
    assert_eq!(reg.snapshot()[0].latch(), "external");
}

/// **A hook may be refused by a latch but must never move one.**
///
/// This is what [`LatchRoute::Hook`] exists for, and getting it wrong would
/// have been worse than the hole: `post_edit`/`should_read` classify
/// LOCAL-CAPABILITY, so gating them on `LatchRoute::Native` would latch
/// every tab with the read advisor or auto-check on to `Local` at its first
/// read or edit — silently refusing every proxied web/MCP tool for the rest
/// of the session, for a choice the model never made.
///
/// The `Native` half of the assertion is the control, so this test cannot
/// pass by the gate having done nothing at all. **It changed with #48's
/// M-2 fix and the change is the finding**: the control used to be the SAME
/// NAME on `LatchRoute::Native`, which latched — and M-7's own review
/// recorded that as a residual, because `hook_post_edit` is not a tool a
/// model can call, so a model that emits it has hallucinated and used to
/// cost its tab every proxied tool for the session. The control is now a
/// name that really is elective and really dispatches, and the old case is
/// asserted the other way round beside it.
#[test]
fn a_hook_route_reads_the_latch_and_never_engages_it() {
    let reg = LatchRegistry::default();
    for tool in [HOOK_TOOL_POST_EDIT, HOOK_TOOL_SHOULD_READ] {
        assert_eq!(
            hook_admit(
                &reg,
                tool,
                "claude",
                Some("claude-1"),
                |_, _| LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
                |_| ON,
            ),
            Ok(())
        );
    }
    assert_eq!(
        reg.snapshot()[0].latch(),
        "open",
        "the hooks fired on cImp's own automation — the conversation elected nothing"
    );
    // …and the proxied web side is therefore still available, which is the
    // user-visible fact the previous assertion protects.
    assert!(reg
        .gate(
            Some(&scope("claude-1", Some("sess-a"))),
            LatchRoute::Proxied,
            "ddg__search",
            ON,
            NO_CONTENT
        )
        .is_ok());

    // The control: a name that IS elective and IS dispatchable latches on
    // the same route, with the same registry and scope shape.
    let elective = LatchRegistry::default();
    assert!(elective
        .gate(
            Some(&scope("claude-2", Some("sess-b"))),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(elective.snapshot()[0].latch(), "local");

    // …and the case that used to be the control, now asserted the other way
    // round (#48, M-2): `hook_post_edit` arriving as a MODEL's tool call is
    // a hallucination — no dispatcher serves that name — so it neither
    // latches nor is refused, and the tab keeps its tools.
    let hallucinated = LatchRegistry::default();
    assert_eq!(
        hallucinated.gate(
            Some(&scope("claude-3", Some("sess-c"))),
            LatchRoute::Native,
            HOOK_TOOL_POST_EDIT,
            ON,
            NO_CONTENT
        ),
        Ok(WriteTaint::Clean)
    );
    assert!(
        hallucinated.snapshot().is_empty(),
        "one hallucinated name must not cost a tab its web tools (A-1's harm, M-2's half)"
    );
}

/// The residual, pinned so it is a decision and not an accident: a hook POST
/// with no usable tab identity resolves no scope and is ADMITTED.
///
/// That is the locked fail-open posture of `latch_scope` (a shim from a
/// build before `--tab` was baked in must not lose the feature), and it is
/// what finding F-5/H-8 tracks. Pinned here so that a future change to it is
/// a deliberate edit to this test, and so the residual cannot be read as
/// "someone forgot".
#[test]
fn a_hook_post_without_a_tab_is_admitted_and_keys_nothing() {
    for scoping in [
        LatchScoping::Anonymous,
        LatchScoping::Unknown("ghost".into()),
    ] {
        let reg = LatchRegistry::default();
        // Contaminate a real tab first: the point is that the ungated call
        // is ungated because it has no identity, not because nothing was
        // latched anywhere.
        assert!(reg
            .gate(
                Some(&scope("claude-1", Some("sess-a"))),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(
            hook_admit(
                &reg,
                HOOK_TOOL_POST_EDIT,
                "claude",
                None,
                |_, _| scoping,
                |_| ON,
            ),
            Ok(())
        );
        // #45's bound: no identity ⇒ no registry row of its own.
        assert_eq!(
            reg.snapshot().len(),
            1,
            "only the contaminated tab is keyed"
        );
    }
}

/// **V33 C4 (finding F-5's directory half): `/context/post_edit` executes
/// the project's configured check commands, and it will only do so in a
/// directory this instance serves.**
///
/// The `cwd` used to come straight out of the request body (defaulting to
/// `"."`) with no ancestor check and no allowlist, so a token-holder could
/// have the operator's own vetted commands run in a directory it named.
///
/// This exercises the decision function, which is pure so the property is
/// assertable without a `TcpStream` or an `AppHandle` — the [`audit_admit`]
/// shape, and the same two path helpers that route's step 3 uses.
///
/// **What this would still pass with, and the guards:** a check that only
/// compared string prefixes (`P:\projx` is asserted to be refused — it is a
/// prefix of neither root component-wise, which is the trap
/// [`is_ancestor_or_equal`] exists for); a check that canonicalized and
/// therefore silently re-bucketed every existing caller (the admitted path
/// is asserted to come back byte-for-byte as written, because it keys the
/// single-flight runner downstream); and a component walk that trusted
/// [`canon`] to resolve `..` (it cannot for a path that does not exist, so
/// `..` is refused outright and that case is asserted).
#[test]
fn post_edit_runs_only_in_a_directory_this_instance_serves() {
    // Built with `join` rather than written with separators so the component
    // walk means the same thing on both platforms.
    let served = PathBuf::from("P:\\proj");
    let worktree = PathBuf::from("P:\\worktrees").join("feature-a");
    let roots = vec![served.clone(), worktree.clone()];
    let s = |p: &Path| p.to_string_lossy().into_owned();

    // 1. No `cwd` on the wire ⇒ the served root, never the process cwd.
    for absent in [None, Some(""), Some("   "), Some("\t")] {
        assert_eq!(
            admitted_hook_root(&roots, absent),
            Some(served.clone()),
            "{absent:?}"
        );
    }

    // 2. Inside a served root — the root itself, a subdirectory, and the
    //    same for a tab that lives in a worktree outside the launch root.
    for ok in [
        served.clone(),
        served.join("src"),
        served.join("src").join("deep"),
        worktree.clone(),
        worktree.join("src"),
    ] {
        let asked = s(&ok);
        assert_eq!(
            admitted_hook_root(&roots, Some(asked.as_str())),
            Some(PathBuf::from(&asked)),
            "{asked} is served and must come back exactly as written"
        );
    }

    // 3. Outside every root — including the string-prefix near miss, and a
    //    traversal that `canon` cannot resolve because the path does not
    //    exist (which is precisely when a component walk would be fooled).
    for bad in [
        PathBuf::from("Q:\\evil"),
        PathBuf::from("P:\\projx"),
        PathBuf::from("P:\\projx").join("src"),
        PathBuf::from("P:\\worktrees"),
        served.join("..").join("..").join("evil"),
        PathBuf::from("..").join("evil"),
    ] {
        let asked = s(&bad);
        assert_eq!(
            admitted_hook_root(&roots, Some(asked.as_str())),
            None,
            "{asked} is not served and the checks must not run there"
        );
    }

    // 4. No resolvable root at all ⇒ deny, including the absent-`cwd` case.
    //    A root that cannot be resolved reads as absent, never as "allow".
    assert_eq!(admitted_hook_root(&[], None), None);
    assert_eq!(admitted_hook_root(&[], Some(&s(&served))), None);

    // 5. Windows only: on-disk casing and an agent-reported cwd routinely
    //    disagree, which is why `is_ancestor_or_equal` folds case there.
    if cfg!(windows) {
        let shouty = s(&served).to_uppercase();
        assert_eq!(
            admitted_hook_root(&roots, Some(shouty.as_str())),
            Some(PathBuf::from(&shouty))
        );
    }
}

/// **V33 C4's other half, checked against the source rather than believed:
/// the roots cannot come from the request.**
///
/// The allowlist above is only as good as what feeds it, and "roots derive
/// from configured tabs and the served root, never from the request" is the
/// kind of claim that survives its own violation if it lives only in prose.
/// Two structural assertions:
///
/// 1. The work resolves its working directory through the pair, so the
///    check cannot be deleted while the route keeps running commands.
/// 2. [`hook_exec_roots`] takes the route context and the settings snapshot
///    and NOTHING ELSE — the request is not in scope, so no future edit can
///    let a body widen the allowlist without changing this signature. (V42
///    Phase A2 replaced the `AppHandle` with a
///    [`RouteCtx`](crate::offload::host::RouteCtx); the property is the same
///    one — what is NOT in the signature is the request — and this scan is red
///    until the needle follows the spelling.)
///
/// **V35 Phase J moved the scan one frame down.** The admission now lives in
/// [`post_edit_diagnostics`], the core BOTH post-edit transports call, and
/// each transport's handler is asserted to go through it — which makes the
/// C4 guarantee stronger, not weaker: it is now impossible for the http
/// route to grow its own directory resolution without failing here.
#[test]
fn post_edit_takes_its_working_directory_from_the_app_not_from_the_body() {
    let body = fn_body_in(ROUTE_SOURCES, "async fn post_edit_diagnostics(");
    assert!(
        body.contains("admitted_hook_root(&hook_exec_roots(ctx, settings), body.cwd.as_deref())"),
        "the route must resolve its cwd through the C4 allowlist: {body}"
    );
    assert!(
        !body.contains("PathBuf::from(\".\")"),
        "the pre-V33 caller-supplied default is back: {body}"
    );
    for handler in ["handle_post_edit", "handle_claude_post_tool_use"] {
        assert!(
            handler_body(handler).contains("post_edit_diagnostics("),
            "{handler} must run the checks through the one admitted-root core"
        );
    }
    // The signature is looked for across the whole route surface: V42 R4
    // (#115) moved it to `loopback/context.rs`, and a scan pinned to one file
    // would have started asserting nothing the moment it did.
    assert_eq!(
        files_containing(
            ROUTE_SOURCES,
            "fn hook_exec_roots(ctx: &RouteCtx, settings: &crate::settings::Settings) -> Vec<PathBuf>"
        )
        .len(),
        1,
        "the roots must derive from the app and the settings, never from a request body"
    );
}

// ── #104: a cwd is never a project root by itself ──────────────────────

/// A throwaway directory tree for the root-resolution tests.
///
/// Canonicalized (so it matches what `canon` resolves the cwd to on
/// Windows) and then put back in the PLAIN spelling, which is the form
/// `resolve_external_root` answers in — see `fsutil::plain_path`.
fn root_tree(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("cimp-root-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    let canon = p.canonicalize().unwrap();
    PathBuf::from(crate::fsutil::plain_path(&canon.to_string_lossy()))
}

/// Whether any `.cimp` directory exists anywhere under `dir`.
fn any_state_dir_under(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if p.file_name().map(|n| n == ".cimp").unwrap_or(false) || any_state_dir_under(&p) {
            return true;
        }
    }
    false
}

/// **The defect.** A sub-agent's Bash keeps its cwd across calls, so after
/// one `cd src-tauri/src/harness` every hook it fires reports that
/// directory. It is not a project, it is not the tab's directory, and the
/// sub-agent is `headless` so there is no tab to ask — resolution must walk
/// UP to the repo that contains it rather than treat it as a root.
#[test]
fn resolve_external_root_walks_up_from_a_sub_agents_cwd() {
    let root = root_tree("subagent-cwd");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let cwd = root.join("src-tauri").join("src").join("harness");
    std::fs::create_dir_all(&cwd).unwrap();

    let got = resolve_external_root(None, Some(&cwd.to_string_lossy()), ".cimp");
    assert_eq!(
        got.as_deref().map(crate::fsutil::norm_dir_key_path),
        Some(crate::fsutil::norm_dir_key_path(&root)),
        "a sub-directory cwd must resolve to the repo that contains it"
    );
    // And nothing was minted on the way — the whole point of the issue.
    assert!(!any_state_dir_under(&root));
    std::fs::remove_dir_all(&root).ok();
}

/// The tab's own configured directory is the one legitimate root source, so
/// it beats the walk: a sub-project opened as its own tab inside a larger
/// repo must not have its rows (or its state) filed under the outer repo.
#[test]
fn resolve_external_root_prefers_the_tabs_own_directory() {
    let root = root_tree("tab-root");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let tab_root = root.join("frontend");
    let cwd = tab_root.join("src").join("lib");
    std::fs::create_dir_all(&cwd).unwrap();

    let got =
        resolve_external_root(Some(tab_root.clone()), Some(&cwd.to_string_lossy()), ".cimp");
    assert_eq!(got, Some(tab_root));
    std::fs::remove_dir_all(&root).ok();
}

/// A cwd outside the tab's directory still gets the walk — the tab root is
/// a preference, not a clamp, and this is the sub-agent that ran in a
/// different checkout.
#[test]
fn resolve_external_root_walks_when_the_cwd_is_outside_the_tab() {
    let a = root_tree("tab-a");
    let b = root_tree("tab-b");
    std::fs::create_dir_all(b.join(".git")).unwrap();
    let cwd = b.join("deep").join("deeper");
    std::fs::create_dir_all(&cwd).unwrap();

    let got = resolve_external_root(Some(a.clone()), Some(&cwd.to_string_lossy()), ".cimp");
    assert_eq!(
        got.as_deref().map(crate::fsutil::norm_dir_key_path),
        Some(crate::fsutil::norm_dir_key_path(&b))
    );
    std::fs::remove_dir_all(&a).ok();
    std::fs::remove_dir_all(&b).ok();
}

/// No marker anywhere and no tab ⇒ REFUSED. The caller records the row with
/// an empty root and creates nothing; inventing a root here is what minted
/// the ten stray directories.
#[test]
fn resolve_external_root_refuses_an_unmarked_cwd_with_no_tab() {
    let root = root_tree("unmarked");
    let cwd = root.join("scratch");
    std::fs::create_dir_all(&cwd).unwrap();

    assert_eq!(
        resolve_external_root(None, Some(&cwd.to_string_lossy()), ".cimp"),
        None
    );
    assert!(!any_state_dir_under(&root));
    std::fs::remove_dir_all(&root).ok();
}

/// A genuinely new, un-versioned folder OPENED AS A TAB still works: the
/// tab's directory answers, so first-time indexing of such a project is
/// unchanged.
#[test]
fn resolve_external_root_falls_back_to_the_tab_for_an_unmarked_cwd() {
    let root = root_tree("unmarked-tab");
    let cwd = root.join("scratch");
    std::fs::create_dir_all(&cwd).unwrap();

    assert_eq!(
        resolve_external_root(Some(root.clone()), Some(&cwd.to_string_lossy()), ".cimp"),
        Some(root.clone())
    );
    // An absent cwd is the same question with less information.
    assert_eq!(
        resolve_external_root(Some(root.clone()), None, ".cimp"),
        Some(root.clone())
    );
    assert_eq!(resolve_external_root(None, Some("   "), ".cimp"), None);
    std::fs::remove_dir_all(&root).ok();
}

/// **End to end from the payload.** A real `PostToolUse`-shaped hook body
/// whose `cwd` is a sub-directory, mapped through the same
/// parse → cwd → body → root chain the handlers take. The advisor row this
/// produces is attributed to the REAL root, and the sub-directory gains
/// nothing.
///
/// The handlers themselves need an `AppHandle` (unconstructible in a unit
/// test), so the tab lookup is supplied directly — which is also the
/// `headless` sub-agent's real situation: no tab at all.
#[test]
fn a_post_tool_use_payload_from_a_sub_dir_attributes_to_the_real_root() {
    let root = root_tree("hook-e2e");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let sub = root.join("src-tauri").join("src").join("harness");
    std::fs::create_dir_all(&sub).unwrap();
    let dir = root.join("src").join("lib").join("settings");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("types.ts");
    std::fs::write(&file, "export type A = 1;\n").unwrap();

    // The payload the sub-agent's hook actually posts: the shell's cwd,
    // which is NOT the project, and an absolute file_path elsewhere in the
    // tree — exactly the row in the issue.
    let payload = serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "s-104",
        "cwd": sub.to_string_lossy(),
        "tool_name": "Read",
        "tool_input": { "file_path": file.to_string_lossy() },
    });
    let input: claude_hook::HookInput = serde_json::from_value(payload).unwrap();
    // `claude_hook_cwd` keeps the payload's cwd verbatim (it also feeds
    // relative-path joins), so this is what reaches the body.
    let cwd = Some(input.cwd.clone());
    let reqst =
        claude_hook::plan_request(input.tool_name.as_deref(), &input.tool_input, &input.cwd)
            .expect("a Read is a read request");
    let body = claude_hook::should_read_body_from_hook(&input, &reqst, None, cwd);
    assert_eq!(body.tab, None, "a sub-agent hook names no tab");

    let resolved = resolve_external_root(None, body.cwd.as_deref(), ".cimp")
        .expect("the payload's cwd resolves to the repo above it");
    assert_eq!(
        crate::fsutil::norm_dir_key_path(&resolved),
        crate::fsutil::norm_dir_key_path(&root),
        "the advisor row must be attributed to the project, not to the shell's cwd"
    );
    // The row's own key is the project's, in one spelling.
    assert!(crate::activity::root_key_eq(
        &crate::activity::root_key(&resolved),
        &crate::activity::root_key(&root)
    ));
    // Nothing was created under the sub-directory.
    assert!(!any_state_dir_under(&root));
    std::fs::remove_dir_all(&root).ok();
}

/// State the defect already minted does not get to keep capturing the cwds
/// below it: the `.git` root wins and the stray is named so the user can
/// remove it — and it is still on disk afterwards, because cImp does not
/// delete the user's data.
#[test]
fn a_stray_state_dir_below_a_root_is_reported_and_left_alone() {
    let root = root_tree("stray");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let sub = root.join("src-tauri").join("src").join("harness");
    std::fs::create_dir_all(sub.join(".cimp")).unwrap();

    let got = resolve_external_root(None, Some(&sub.to_string_lossy()), ".cimp");
    assert_eq!(
        got.as_deref().map(crate::fsutil::norm_dir_key_path),
        Some(crate::fsutil::norm_dir_key_path(&root))
    );
    assert!(
        sub.join(".cimp").is_dir(),
        "the stray is reported, not swept"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// `agent` is caller-asserted and absent on a pre-#48 shim. Absent ⇒
/// `claude`, because all three Claude hooks are installed from Claude's own
/// settings overlay; `opencode` is the only other answer, and it is the one
/// the generated plugin's `post_edit` POST sends.
#[test]
fn a_hook_bodys_agent_narrows_to_the_two_that_exist() {
    assert_eq!(hook_agent(None), "claude");
    assert_eq!(hook_agent(Some("claude")), "claude");
    assert_eq!(hook_agent(Some("opencode")), "opencode");
    assert_eq!(hook_agent(Some("OpenCode")), "opencode");
    // cImp's own in-app consumer keeps its own name — it is a real source
    // in the activity feed, not an invented one.
    assert_eq!(hook_agent(Some("offload")), "offload");
    // **V40 Phase A: anything INVENTED is `unknown`, not `claude`.** It used
    // to fall through to Claude, so a forged or hand-run caller asserting
    // any token at all got Claude's activity badge and Claude's memory
    // scope — a misattribution in the view whose whole job is attribution.
    // `agent` is caller-asserted either way (F-4 still holds; the (agent,
    // tab) pair is verified on no route), so this is about honesty of the
    // row, and `unknown` scopes to no sessions rather than to another
    // agent's.
    assert_eq!(hook_agent(Some("codex")), crate::graph::UNKNOWN_SOURCE);
    // Padding is still NOT trimmed — that narrowing lives in
    // `audit_consumer`, whose route requires identity. No shim sends any.
    assert_eq!(
        hook_agent(Some(" opencode ")),
        crate::graph::UNKNOWN_SOURCE
    );
    // **V40 review M-4: EMPTY is ABSENT, not unknown.** Both identity
    // readers answer `""` rather than `None` for a body with no
    // discriminator — `identity_of_request` is `unwrap_or_default()`,
    // `chp::Envelope::agent_token` is `unwrap_or("")` — so an artifact from
    // before the field existed arrives as `Some("")`. On develop
    // `source_for_consumer("")` was `"claude"` and this never mattered;
    // resolving it to `unknown` switched CHP stale-artifact recording and
    // the quiet-hook detector off for exactly the pre-upgrade artifacts they
    // exist to catch.
    assert_eq!(hook_agent(Some("")), "claude");
    assert_eq!(hook_agent(Some("   ")), "claude");
}

/// The same rule on the two routes whose identity-less default is the
/// ROUTE's rather than the app's, and on the CHP observer that reads the
/// same bytes (V40 review M-4).
///
/// The disagreement this closes: on `/memory/event` an identity-less body
/// was `opencode` to the handler and `unknown` to the observer, on ONE
/// request. Both go through `wire_agent` now.
#[test]
fn an_identity_less_body_resolves_to_its_routes_declared_default() {
    for route in [MEMORY_EVENT_ROUTE, LATCH_STATE_ROUTE] {
        let declared = crate::harness::ingress::wire_default(route).token();
        assert_eq!(wire_agent(route, None), declared, "{route}");
        assert_eq!(wire_agent(route, Some("")), declared, "{route}: empty");
        assert_eq!(wire_agent(route, Some(" ")), declared, "{route}: blank");
    }
    // A route nobody claims takes the app default…
    assert_eq!(wire_agent("/context/compaction", Some("")), "claude");
    // …and a token with content is still resolved, or refused, on its own
    // merits: V40's `unknown` narrowing is not what this funnel is about.
    assert_eq!(wire_agent(MEMORY_EVENT_ROUTE, Some("claude")), "claude");
    assert_eq!(
        wire_agent(MEMORY_EVENT_ROUTE, Some("codex")),
        crate::graph::UNKNOWN_SOURCE
    );

    // The actual pre-upgrade artifact, end to end: a `/context/compaction`
    // body with a tab and a session and NO `agent`. Both identity readers
    // hand `note_chp` an empty token, and it has to land on a real harness
    // or the tab's stale-artifact report and quiet-hook detection are off.
    let body = br#"{"tab":"claude","session_id":"s","chp":1}"#;
    let (env, tab) = crate::harness::chp::envelope("/context/compaction", body)
        .expect("a body with a tab is observable");
    assert_eq!(env.agent_token(), "", "precondition: the reader answers empty");
    assert_eq!(tab, "claude");
    assert_eq!(
        wire_agent("/context/compaction", Some(env.agent_token())),
        "claude"
    );

    let req = request_for_test("POST", "/claude/hook/pre_compact", Some("claude"), Some(1));
    let id = crate::harness::ingress::identity_of_request("/claude/hook/pre_compact", &req)
        .expect("the Claude plugin claims its own hook route");
    assert_eq!(id.agent, "", "precondition: the reader answers empty");
    assert_eq!(
        wire_agent("/claude/hook/pre_compact", Some(id.agent.as_str())),
        "claude"
    );
}

/// All three hook bodies still parse without the two new fields — a shim or
/// plugin file from an older build must not start failing at the parse
/// boundary and lose the feature outright.
#[test]
fn pre_48_hook_bodies_still_parse_without_tab_or_agent() {
    let compaction: ContextCompactionBody =
        serde_json::from_slice(br#"{"cwd":"P:\\p","session_id":"s","trigger":"auto"}"#)
            .expect("pre-#48 compaction body");
    assert!(compaction.tab.is_none() && compaction.agent.is_none());

    let read: ShouldReadBody =
        serde_json::from_slice(br#"{"cwd":"P:\\p","session_id":"s","file_path":"a.rs"}"#)
            .expect("pre-#48 should_read body");
    assert!(read.tab.is_none() && read.agent.is_none());

    let edit: ContextPostEditBody = serde_json::from_slice(
        br#"{"cwd":"P:\\p","session_id":"s","file_path":"a.rs","tool_name":"Edit"}"#,
    )
    .expect("pre-#48 post_edit body");
    assert!(edit.tab.is_none() && edit.agent.is_none());

    // …and the new fields do arrive when sent.
    let edit: ContextPostEditBody = serde_json::from_slice(
        br#"{"session_id":"s","file_path":"a.rs","tab":"claude-1","agent":"opencode"}"#,
    )
    .expect("post-#48 post_edit body");
    assert_eq!(edit.tab.as_deref(), Some("claude-1"));
    assert_eq!(edit.agent.as_deref(), Some("opencode"));
}

// ── V35 Phase J: the two transports meet at one body ────────────────────

/// Both transports of each capability run through the SAME core function —
/// scanned from the source, because a shared core that only one side calls
/// is how two paths silently diverge while every unit test stays green.
#[test]
fn both_transports_of_a_capability_call_one_core() {
    for (core, handlers) in [
        (
            "context_retrieve_core(",
            ["handle_context_retrieve", "handle_claude_user_prompt_submit"],
        ),
        (
            "compaction_block(",
            ["handle_context_compaction", "handle_claude_pre_compact"],
        ),
        (
            "should_read_verdict(",
            ["handle_should_read", "handle_claude_pre_tool_use"],
        ),
        (
            "post_edit_diagnostics(",
            ["handle_post_edit", "handle_claude_post_tool_use"],
        ),
        (
            "permission_signal(",
            ["handle_permission_event", "handle_claude_notification"],
        ),
        // V40 Phase C: both permission transports still meet at ONE core,
        // and that core is now core's — `send_permission_edge`, the neutral
        // half. The classifier above them is the harness's.
        (
            "send_permission_edge(",
            ["permission_signal", "permission_signal"],
        ),
        // 2026-08-17: the two migrated beacons. Their cores were extracted
        // from the routes' own handlers in the same change, which is what
        // makes the migration a relocation — the `mutates_fs` re-check, the
        // #45 narrowing, the deadline and the row each engagement writes are
        // one implementation with two envelopes.
        (
            // The plugin route reaches the same core through the narrow
            // facade `latch_beacon_for`, whose only body is that call — so
            // scanning for the core's own name would miss it by one hop.
            "latch_beacon_",
            ["handle_latch_beacon", "handle_claude_taint_beacon"],
        ),
        (
            "tool_checkpoint_core(",
            ["handle_tool_checkpoint", "handle_claude_checkpoint"],
        ),
        // …and the two halves of the tool-result push: success and failure
        // are ONE capability, so they must not grow two accountings.
        (
            "tool_result_core(",
            ["handle_claude_tool_result", "handle_claude_tool_failure"],
        ),
    ] {
        for h in handlers {
            assert!(
                handler_body(h).contains(core),
                "`{h}` must reach `{core}` — the two transports of one capability may not \
                 grow separate implementations"
            );
        }
    }
    // …and the three gated Claude routes carry their own `hook_admit` call
    // rather than inheriting one, which is what keeps the route-enumeration
    // test above able to see the gate at the route.
    for h in [
        "handle_claude_pre_compact",
        "handle_claude_pre_tool_use",
        "handle_claude_post_tool_use",
    ] {
        assert!(
            handler_body(h).contains("if !hook_gate_admits("),
            "`{h}` must gate in its own body"
        );
    }
}

/// **The two-timer relationship the pre-tool checkpoint rests on**, restated
/// after the 2026-08-17 migration moved the outer timer.
///
/// It used to be `checkpoint_beacon::REPLY_TIMEOUT > TOOL_CHECKPOINT_BUDGET`:
/// the shim had to keep listening for longer than the app took to give up, or
/// Claude would start the tool while the app was still staging into it. The
/// shim is gone and the outer timer is now the harness's own — the hook
/// entry's pinned `timeout` — so the same ordering has to hold against
/// that number instead. Nothing but this assertion keeps the two constants
/// (different files, different layers) in the right order.
///
/// The second half is the other side of the argument: every OTHER route keeps
/// the 1 s budget, so this exception cannot quietly widen into "hooks may
/// take five seconds".
#[test]
fn the_checkpoint_hooks_ceiling_sits_above_the_apps_own_budget() {
    let ceiling = Duration::from_secs(claude_hook::TIMEOUT_CHECKPOINT_SECS);
    assert!(
        ceiling > tool_checkpoint_budget(),
        "the harness must not stop waiting before the app answers, or an abandoned \
         snapshot and a still-running one become indistinguishable to Claude: \
         {ceiling:?} vs {:?}",
        tool_checkpoint_budget()
    );
    assert_eq!(
        claude_hook::timeout_secs(claude_hook::ROUTE_PRE_TOOL_USE_CHECKPOINT),
        claude_hook::TIMEOUT_CHECKPOINT_SECS
    );
    assert_eq!(
        claude_hook::timeout_secs(claude_hook::ROUTE_PRE_TOOL_USE_TAINT),
        claude_hook::TIMEOUT_SECS,
        "the sensor has nothing to wait for and must not inherit the checkpoint's ceiling"
    );
}

/// **The prompt hook's OTHER two-timer relationship** (2026-08-17 fix).
///
/// `UserPromptSubmit` keeps the 1 s budget, and the harness DISCARDS a
/// reply that arrives after it — silently, so a handler that overruns looks
/// exactly like a handler that had nothing to say while having already
/// spent the session's once-per-session greeting, its dedup ledger and its
/// parked auto-check block. [`RETRIEVE_BUDGET_MS`] is the app's own,
/// smaller bound: past it the handler answers with what it has and parks
/// the digest for the next prompt.
///
/// Nothing but this assertion keeps the two constants (different files,
/// different layers) in the right order — the same reason the checkpoint
/// pin above exists.
#[test]
fn the_retrieve_budget_sits_under_the_prompt_hooks_ceiling() {
    let ceiling = Duration::from_secs(claude_hook::TIMEOUT_SECS);
    let budget = Duration::from_millis(RETRIEVE_BUDGET_MS);
    assert!(
        budget < ceiling,
        "a digest composed after the harness stopped listening is state spent for \
         nothing: {budget:?} vs {ceiling:?}"
    );
    assert_eq!(
        claude_hook::timeout_secs(claude_hook::ROUTE_USER_PROMPT_SUBMIT),
        claude_hook::TIMEOUT_SECS,
        "the prompt hook is not the documented exception — the checkpoint route is"
    );

    // The OpenCode transport's ceiling is CLIENT-side: the plugin aborts
    // its `/context/retrieve` fetch on its own timer, and a reply that
    // leaves after that abort is lost WITH the parked backlog it drained.
    // Read the number out of the template rather than repeating it here,
    // and demand real margin (compose + write + the plugin's own fetch
    // overhead), not mere ordering — a budget equal to the abort loses
    // the race on every timeout path.
    let plugin = include_str!("../../harness/opencode/templates/plugin.js");
    let at = plugin
        .find("/context/retrieve")
        .expect("the plugin template posts /context/retrieve");
    let tail = &plugin[at..];
    let marker = "AbortSignal.timeout(";
    let t = tail
        .find(marker)
        .expect("the retrieve fetch carries an AbortSignal.timeout");
    let digits: String = tail[t + marker.len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let client_abort_ms: u64 = digits.parse().expect("a literal millisecond count");
    assert!(
        RETRIEVE_BUDGET_MS + 100 <= client_abort_ms,
        "the retrieval budget must leave the OpenCode plugin's client abort at least \
         100 ms of reply margin: {RETRIEVE_BUDGET_MS} ms vs {client_abort_ms} ms"
    );
}

/// The injected reply's composition: locked ORDER, empties skipped, files
/// parked-then-fresh with no duplicates.
///
/// The order is the contract with the model: the project map first, then
/// anything retrieved for an EARLIER prompt (marked as such by the store),
/// then this prompt's own digest, then the auto-check block — so what is
/// most likely to answer the prompt is never buried under a late arrival.
#[test]
fn an_injection_reply_keeps_its_locked_order_and_skips_empties() {
    assert_eq!(
        merge_injection_blocks(&["greeting", "parked", "fresh", "check"]),
        "greeting\n\nparked\n\nfresh\n\ncheck"
    );
    // Empties (and whitespace-only parts) never contribute a blank gap.
    assert_eq!(
        merge_injection_blocks(&["", "parked", "   \n", "check"]),
        "parked\n\ncheck"
    );
    assert_eq!(merge_injection_blocks(&["", "", "", ""]), "");
    // Blocks are joined verbatim — nothing here rewrites a block's content.
    assert_eq!(
        merge_injection_blocks(&["a\n\nb", "", "c"]),
        "a\n\nb\n\nc",
        "internal structure of a block is not normalised"
    );

    assert_eq!(
        merge_files_used(
            vec!["a.rs".into(), "b.rs".into()],
            vec!["b.rs".into(), "c.rs".into()]
        ),
        vec!["a.rs".to_string(), "b.rs".into(), "c.rs".into()],
        "parked first, fresh after, each file named once"
    );
    assert!(merge_files_used(Vec::new(), Vec::new()).is_empty());
}

/// **A failed tool result is sized, counted, and never mined.**
///
/// The transcript reader keeps two readers over one `tool_result` block:
/// `extract_tool_results` sizes every result including failures and never
/// looks at `is_error`, while `tool_result_is_error` exists solely to keep a
/// failed result out of the session→commit provenance tap. The push path has
/// to mirror both, and the second one it mirrors *structurally* — it carries
/// a `u32` and never the text — which is exactly the kind of claim that rots
/// silently if a later change starts forwarding the error string.
///
/// Asserted on the handler's source because the property is an ABSENCE, and
/// an absence has no call to observe.
#[test]
fn a_failed_tool_result_is_counted_but_never_reaches_provenance() {
    let body = handler_body("handle_claude_tool_failure");
    assert!(
        body.contains("tool_result_core("),
        "the failure half must feed the same accounting as the success half"
    );
    for forbidden in ["record_commit", "session_commit", "parse_commit_hashes"] {
        assert!(
            !body.contains(forbidden),
            "the failure handler reaches `{forbidden}` — a failed tool's output must \
             never be mined for commit hashes (`tool_result_is_error`'s whole purpose)"
        );
    }
    // …and what it sizes is the `error` field, through the transcript
    // reader's own sizing function rather than a second implementation.
    assert!(
        body.contains("tool_result_chars(&input.error)"),
        "the error must be sized by the function the reader sizes a failed \
         result's content with, or the two paths report different numbers"
    );
}


/// The `X-CIMP-*` headers are read under exactly the names the overlay
/// emits. `read_request` lowercases keys and matches lowercase literals, so
/// a rename on either side is a silent loss of identity — the hook would
/// still 200 and simply stop being attributed to a tab.
#[test]
fn the_cimp_headers_are_read_under_the_names_the_overlay_emits() {
    for name in [
        claude_hook::HEADER_TAB,
        claude_hook::HEADER_AGENT,
        claude_hook::HEADER_CHP,
        claude_hook::HEADER_HELLO,
    ] {
        let lower = name.to_ascii_lowercase();
        // `read_request` is in `loopback/mod.rs`, but the property is that
        // SOMETHING in the surface reads the header — asked of the whole
        // list so the answer cannot become "nothing does" by relocation.
        assert!(
            !files_containing(ROUTE_SOURCES, &format!("\"{lower}\" =>")).is_empty(),
            "`{name}` is emitted but `read_request` never matches `{lower}`"
        );
    }
}

/// **Neither clear is reachable over HTTP**, which is the invariant the whole
/// design rests on: a model with a shell that could POST its way to a clear
/// would defeat every part of this.
///
/// Three independent halves, because each closes a different door.
#[test]
fn no_http_route_can_reach_a_contamination_clear() {
    // 1. The HTTP surface, pinned. Every route this listener serves is
    //    listed here; a new one fails this test until someone names it, and
    //    the point of naming it is to notice if it is an override door.
    //    (#45 removed `POST /latch/override` for exactly this reason.)
    //
    //    #48 (M-7): the list is no longer a literal here — it is
    //    [`ROUTE_CONTAINMENT`], which is the same enumeration answering one
    //    more question per route (does it gate?). ONE list, so a new route
    //    cannot satisfy one enumeration and be missing from the other.
    let routes = dispatched_routes(ROUTE_SOURCES);
    let declared: Vec<&str> = {
        let mut v: Vec<&str> = ROUTE_CONTAINMENT.iter().map(|r| r.path).collect();
        v.sort_unstable();
        v
    };
    assert_eq!(
        routes, declared,
        "the loopback's HTTP surface changed — is the new route a door onto the \
         latch override or the contamination clear?"
    );

    // 2. The only entry point that can clear is not an HTTP handler. Its
    //    doc says so; this asserts the shape the doc describes — the three
    //    clearing actions (`clear_contamination`, `unlatch` since decision
    //    15's 2026-08-10 amendment, and the deferred `await_session_clear`)
    //    exist solely as `LatchOverride` values, and the only function that
    //    turns a string into one is called from the IPC command.
    let ipc = include_str!("../../ipc/commands.rs");
    assert!(
        ipc.contains("apply_latch_override(&app, &consumer, &tab, &action)"),
        "the IPC command is the caller of record"
    );
    // `concat!` so this needle does not match itself in the source it
    // scans — the first version of this assertion counted 2 and was
    // "failing" on nothing but its own text.
    //
    // V42 R3 (#114) moved the override entry point and the registry to
    // `offload/latch.rs`, so the count is taken over BOTH files: one entry
    // point in the module that has it, and none in the module that answers
    // HTTP. The door-shaped needles are scanned over both for the same reason
    // — a parse-from-body added on either side is the same door.
    //
    // V42 R4 (#115) split the routes across `loopback/*.rs`; both counts are
    // taken over EVERY file of the surface, or the door could be cut into a
    // family file with this test still counting one.
    let latch_src = include_str!("../latch.rs");
    let surface: Vec<(&str, &str)> = ROUTE_SOURCES
        .iter()
        .copied()
        .chain([("offload/latch.rs", latch_src)])
        .collect();
    assert_eq!(
        surface
            .iter()
            .map(|(_, text)| text.matches(concat!("pub fn ", "apply_latch_override")).count())
            .sum::<usize>(),
        1,
        "one entry point, or the doc's claim is unverifiable"
    );
    for (file, text) in surface {
        assert!(
            !text.contains(concat!("LatchOverride::", "parse(&body"))
                && !text.contains(concat!("LatchOverride::", "parse(body")),
            "{file}: an override action parsed from a request body is an HTTP door"
        );
    }

    // 3. Behaviourally: the two registry entry points that ARE HTTP-reachable
    //    (`/latch/beacon` → `beacon`, `/latch/state` → `view_for`) can
    //    neither clear an unarmed tab nor arm one. The beacon only ever
    //    tightens, and that must not have widened.
    let (reg, s) = contaminated_registry();
    for _ in 0..5 {
        let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        assert!(out.view.contaminated, "a beacon cannot clear");
        assert!(!out.view.awaiting_session_clear, "a beacon cannot arm");
    }
    // …including across a rotation, which is the one moment an arm would
    // matter. Nothing an HTTP caller can send sets it.
    let rotated = scope("claude-1", Some("sess-b"));
    assert!(reg.view_for(&rotated).contaminated);
    assert!(
        reg.beacon(Some(&rotated), "WebFetch", ON, BEACON_PROV)
            .view
            .contaminated
    );
}

/// The registry's read path folds live sessions in, so an armed one-shot
/// fires on the UI's existing 4 s poll rather than waiting for the model to
/// call a cImp tool.
///
/// `latch_snapshot` itself needs an `AppHandle` (it resolves the live-session
/// registry), which this crate cannot mock — so what is asserted here is the
/// half that has the logic: given resolved scopes, `observe_all` applies the
/// same rotation rule to every entry and hands back the rows to record.
#[test]
fn the_read_path_observes_rotations_for_every_tab_it_reports() {
    let (reg, s) = contaminated_registry();
    reg.apply_override(&s, LatchOverride::AwaitSessionClear)
        .expect("arm");

    // A second, unarmed tab in the same registry: it must be observed too
    // (its latch reopens) and cleared not at all.
    let other = scope("claude-2", Some("sess-x"));
    assert!(reg
        .gate(
            Some(&other),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            NO_CONTENT
        )
        .is_ok());

    let keys = reg.keys();
    assert_eq!(keys.len(), 2, "both tabs are in the registry");
    let rotated = [
        scope("claude-1", Some("sess-b")),
        scope("claude-2", Some("sess-y")),
    ];
    let cleared = reg.observe_all(&rotated);
    assert_eq!(cleared.len(), 1, "exactly the armed tab clears");

    let rows = reg.snapshot();
    let armed = rows.iter().find(|r| r.tab == "claude-1").expect("claude-1");
    let unarmed = rows.iter().find(|r| r.tab == "claude-2").expect("claude-2");
    assert!(!armed.view.contaminated);
    assert!(
        unarmed.view.contaminated,
        "the read path must not have become a second way to un-taint a tab"
    );
    assert_eq!(unarmed.latch(), "open", "…while still resetting the latch");
    assert_eq!(unarmed.session.as_deref(), Some("sess-y"));

    // An entry the caller resolved no scope for is simply skipped; it is not
    // an error and it changes nothing.
    assert!(reg.observe_all(&[]).is_empty());
}
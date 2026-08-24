//! The wire itself: bearer auth, the identity a request resolves to, the SSE
//! frame shape, the token's entropy — and the usage/memory ingest that reads a
//! turn off a body.

use super::*;

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

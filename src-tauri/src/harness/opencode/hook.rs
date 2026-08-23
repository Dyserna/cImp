//! V40 Phase I — **this harness's memory ingress**, as its own wire struct
//! (issue #107 item 2).
//!
//! `POST /memory/event` is the generated plugin's tool hook
//! (`templates/plugin.js`), and since V24 Phase F it carries usage bodies as
//! well: this harness's SSE stream reports no tokens (see the C3 spike note
//! atop [`super::read`]), so the hook that already fires after every tool call
//! is the only place a completed turn's real totals can be recorded from.
//!
//! Until Phase I the body struct lived in `offload/loopback.rs`, and core read
//! every one of its fields. That named no harness *id*, so both layering
//! allowlists stayed clean — but the row SHAPE was this plugin's, and a second
//! harness with a different one would have had nowhere to put it but core. The
//! struct is here now; what crosses back is
//! [`crate::harness::plugin::MemoryEvent`], which says what the body MEANS.
//!
//! **The split is deliberate.** The route stays in core's router and the
//! recording stays in core: the graph writes, the live-session registry and the
//! `cwd` resolution are cImp's own, and moving them here would put
//! `crate::graph` inside `harness/` — the dependency direction
//! `harness_modules_do_not_import_capabilities` exists to refuse. What moved is
//! exactly the part that is this harness's claim: the field names, and which
//! declared lane a turn lands in.

use serde::Deserialize;

use crate::harness::plugin::{MemoryEvent, MemoryEventKind};

/// A `POST /memory/event` request body — this harness's tool hook, and (V14
/// Phase C / V24 Phase F) its only *usage* ingress for the same reason.
///
/// `session_id` is the only required field: the hook fires on paths where the
/// rest legitimately differ, and a body that cannot name its session is the one
/// thing nothing downstream can key.
#[derive(Deserialize)]
struct MemoryEventBody {
    #[serde(default)]
    cwd: Option<String>,
    session_id: String,
    // Tool-event shape (V10): present on `tool.execute.after` POSTs. Optional
    // now that the same route also carries usage bodies (V24 Phase F), which
    // have no `tool`.
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    args: serde_json::Value,
    // V24 Phase F usage shape: `kind == "usage"`, emitted by the plugin's
    // `event` hook on a completed assistant turn.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    parent_session_id: Option<String>,
    #[serde(default)]
    msg_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    in_tok: u32,
    #[serde(default)]
    out_tok: u32,
    #[serde(default)]
    cache_read: u32,
    #[serde(default)]
    cache_make: u32,
}

/// The `kind` value that marks a usage body rather than a tool body.
const KIND_USAGE: &str = "usage";

/// Read one body into the neutral [`MemoryEvent`] core records.
///
/// `Err` is the serde message, which core answers 400 with — the producer is
/// a generated plugin, so a body it cannot form is a defect worth surfacing to
/// whoever is looking at the console rather than a silent 200.
pub(in crate::harness) fn memory_event(body: &[u8]) -> Result<MemoryEvent, String> {
    let body: MemoryEventBody =
        serde_json::from_slice(body).map_err(|e| format!("bad request body: {e}"))?;
    Ok(MemoryEvent {
        cwd: body.cwd.clone(),
        session_id: body.session_id.clone(),
        kind: kind_of(&body),
    })
}

/// Which of the four things this body is.
///
/// The order is the handler's original order and is load-bearing: the usage
/// shape short-circuits the tool path (it carries no `tool`), and a sub-agent's
/// tool call is dropped before the classification that would record it.
fn kind_of(body: &MemoryEventBody) -> MemoryEventKind {
    if body.kind.as_deref() == Some(KIND_USAGE) {
        return turn_kind(body).unwrap_or(MemoryEventKind::Nothing);
    }
    // Tool-event path (V10): requires a `tool` name. A body without one and not
    // a usage event has nothing to record.
    let Some(tool) = body.tool.clone() else {
        return MemoryEventKind::Nothing;
    };
    // V24 Phase F: a task-tool CHILD (sub-agent) session's tool events are the
    // sub-agent's own working set, not the parent's — this mirrors the Claude
    // sidechain contract (`harness/claude/read.rs`'s `record_tool_events`
    // early-returns on `isSidechain`) and drops them entirely: no mem event, no
    // tool-result chars against the child, and the child is never marked live.
    // The child's real token spend still reaches the parent through the usage
    // arm above; the PARENT is marked live so the sub-agent's activity keeps the
    // parent's row active.
    match parent_session(body) {
        Some(parent) => MemoryEventKind::SubagentTool { parent },
        None => MemoryEventKind::Tool {
            tool,
            args: body.args.clone(),
        },
    }
}

/// V24 Phase F: from a usage body, the target session id and the declared lane
/// its spend lands in — or `None` when the body has no usable data
/// (missing/empty `msg_id`, or all four token totals are zero).
///
/// When `parent_session_id` is present the spend rolls up to the PARENT session
/// in this harness's **declared sub-agent lane** (sub-agent spend is the
/// parent's spend); otherwise it is the reporting session in the declared main
/// lane. A model id that is absent/empty maps to `None` (unknown model),
/// matching the Claude tap. Pure, so the mapping is unit-tested without a live
/// handler.
///
/// **V40 Phase G (locked decision 19)** removed the hard-coded
/// `UsageOrigin::Agent` / `::Session` here — core writing one harness's two lane
/// names into another harness's rows. Phase I finishes the move: the lanes come
/// from THIS harness's own [`TurnUsageShape`](crate::harness::plugin::TurnUsageShape),
/// read directly rather than looked up by an asserted `agent` string. A shape
/// that declares no main (or no sub-agent) lane records **nothing**: guessing a
/// lane would put real tokens in a fabricated bucket, and a dropped row is
/// recoverable where a mis-attributed one is not.
fn turn_kind(body: &MemoryEventBody) -> Option<MemoryEventKind> {
    let msg_id = body.msg_id.clone().filter(|m| !m.is_empty())?;
    // The plugin only forwards COMPLETED turns, so an all-zero body is a
    // degenerate/creation emit — skip it rather than plant an empty turn row.
    // `est_only` is unaffected either way (it's derived from the summed token
    // totals, which a zero row doesn't move), so skipping only keeps the turn
    // series free of noise; it never resurrects real data.
    if body.in_tok == 0 && body.out_tok == 0 && body.cache_read == 0 && body.cache_make == 0 {
        return None;
    }
    let shape = &super::harness_plugin::TURN_SHAPE;
    let (target, origin) = match parent_session(body) {
        Some(p) => (p, shape.subagent_origin()?),
        None => (body.session_id.clone(), shape.main_origin()?),
    };
    Some(MemoryEventKind::Turn {
        target,
        origin,
        msg_id,
        model: body.model.clone().filter(|m| !m.is_empty()),
        in_tok: body.in_tok,
        out_tok: body.out_tok,
        cache_read: body.cache_read,
        cache_make: body.cache_make,
    })
}

/// The parent session id when the reporting session is a task-tool CHILD
/// (sub-agent), else `None`. An empty string is absent — the plugin emits the
/// field unconditionally.
fn parent_session(body: &MemoryEventBody) -> Option<String> {
    body.parent_session_id
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The lane ids this harness declares, so the expectations below read as
    /// the declaration rather than as two more literals.
    fn lanes() -> (&'static str, &'static str) {
        let shape = &super::super::harness_plugin::TURN_SHAPE;
        (
            shape.main_origin().expect("a main lane is declared"),
            shape.subagent_origin().expect("a sub-agent lane is declared"),
        )
    }

    fn parse(v: serde_json::Value) -> MemoryEvent {
        memory_event(v.to_string().as_bytes()).expect("the fixture parses")
    }

    fn usage_body(mut v: serde_json::Value) -> serde_json::Value {
        v["kind"] = json!("usage");
        v
    }

    /// Moved from `offload::loopback::tests::usage_body_well_formed_records_session_turn`
    /// with the type it is about (V40 Phase I). Same fixture, same numbers; the
    /// assertion is against the neutral `MemoryEventKind::Turn` core now
    /// receives instead of against `graph::UsageEvent::Turn`, which core builds
    /// from it.
    #[test]
    fn a_well_formed_usage_body_records_a_main_lane_turn() {
        let (main, _) = lanes();
        let ev = parse(usage_body(json!({
            "session_id": "ses_a",
            "msg_id": "m1",
            "model": "anthropic/claude-sonnet-4",
            "in_tok": 10u32,
            "out_tok": 20u32,
            "cache_read": 30u32,
            "cache_make": 40u32,
        })));
        assert_eq!(ev.session_id, "ses_a");
        assert_eq!(
            ev.kind,
            MemoryEventKind::Turn {
                target: "ses_a".to_string(),
                origin: main,
                msg_id: "m1".to_string(),
                model: Some("anthropic/claude-sonnet-4".to_string()),
                in_tok: 10,
                out_tok: 20,
                cache_read: 30,
                cache_make: 40,
            }
        );
    }

    /// Moved from `usage_body_with_parent_rolls_up_as_agent`. A child session's
    /// spend is the PARENT's spend, in the declared sub-agent lane.
    #[test]
    fn a_child_sessions_usage_rolls_up_to_the_parent_in_the_subagent_lane() {
        let (_, sub) = lanes();
        let ev = parse(usage_body(json!({
            "session_id": "ses_child",
            "parent_session_id": "ses_parent",
            "msg_id": "m2",
            "out_tok": 5u32,
        })));
        // The REPORTING session is still the child — only the attribution moves.
        assert_eq!(ev.session_id, "ses_child");
        let MemoryEventKind::Turn { target, origin, .. } = ev.kind else {
            panic!("expected a turn");
        };
        assert_eq!(target, "ses_parent");
        assert_eq!(origin, sub);
    }

    /// Moved from `usage_body_malformed_or_empty_is_ignored`. Two ways a usage
    /// body carries nothing worth a row, and both answer `Nothing` rather than
    /// planting an empty turn.
    #[test]
    fn a_usage_body_with_no_msg_id_or_no_tokens_records_nothing() {
        // No `msg_id` — nothing to upsert by.
        let ev = parse(usage_body(json!({ "session_id": "s", "in_tok": 9u32 })));
        assert_eq!(ev.kind, MemoryEventKind::Nothing);
        // Empty `msg_id` is absent.
        let ev = parse(usage_body(
            json!({ "session_id": "s", "msg_id": "", "in_tok": 9u32 }),
        ));
        assert_eq!(ev.kind, MemoryEventKind::Nothing);
        // All four totals zero — the plugin's degenerate/creation emit.
        let ev = parse(usage_body(json!({ "session_id": "s", "msg_id": "m" })));
        assert_eq!(ev.kind, MemoryEventKind::Nothing);
    }

    /// Moved from `tool_event_parent_flags_child_sessions_only`. A first-party
    /// tool call is recorded; a sub-agent's is dropped with the parent kept
    /// live, and an EMPTY `parent_session_id` is absent (the plugin emits the
    /// field unconditionally).
    #[test]
    fn a_tool_body_is_a_child_s_only_when_it_names_a_parent() {
        let ev = parse(json!({ "session_id": "s", "tool": "read", "args": { "path": "a.rs" } }));
        assert_eq!(
            ev.kind,
            MemoryEventKind::Tool {
                tool: "read".to_string(),
                args: json!({ "path": "a.rs" }),
            }
        );
        let ev = parse(json!({
            "session_id": "s",
            "parent_session_id": "p",
            "tool": "read",
        }));
        assert_eq!(
            ev.kind,
            MemoryEventKind::SubagentTool {
                parent: "p".to_string()
            }
        );
        let ev = parse(json!({
            "session_id": "s",
            "parent_session_id": "",
            "tool": "read",
        }));
        assert!(matches!(ev.kind, MemoryEventKind::Tool { .. }));
    }

    /// A body with neither a `tool` nor the usage shape is a 200 with nothing
    /// recorded — the hook fires on paths where that is ordinary.
    #[test]
    fn a_body_with_no_tool_and_no_usage_shape_records_nothing() {
        let ev = parse(json!({ "session_id": "s", "cwd": "C:/proj" }));
        assert_eq!(ev.kind, MemoryEventKind::Nothing);
        assert_eq!(ev.cwd.as_deref(), Some("C:/proj"));
    }

    /// A body that cannot name its session is a 400, not a silent 200: nothing
    /// downstream has a key to file it under.
    #[test]
    fn a_body_with_no_session_id_is_a_refusal() {
        let err = memory_event(br#"{"tool":"read"}"#).expect_err("no session id");
        assert!(err.starts_with("bad request body: "), "{err}");
        assert!(memory_event(b"not json").is_err());
    }
}

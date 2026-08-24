//! `POST /memory/event` — a harness's memory/usage ingress.
//!
//! One of the route families V42 R4 (#115) split out of `loopback.rs`. The
//! body shape is the harness plugin's (V40 Phase I); what is here is the
//! recording, which is cImp's.

use super::*;

/// **The live-session write a `/memory/event` body asks for** (V40 Phase D,
/// locked decision 20).
///
/// The body names a SESSION, so the write lands in the session key space —
/// where it cannot name a cImp tab. That is the whole of the C-2 fix now: it
/// used to be `mark_live_session_from_event`, which refused any key that
/// exactly matched a configured AI tab id, because one map held both key spaces
/// and a POST could therefore repoint a running tab's session (flapping the
/// taint latch clear in a loop, with the real tap's re-stamp producing a second
/// rotation that helped the attacker). A check beside the write has to keep a
/// list in step; separate spaces make the collision unrepresentable.
///
/// A harness whose identity is TAB-keyed ([`SessionKey::Tab`] — its session is
/// bound by cImp's own reader) gets **no registry write from a request body at
/// all**: its live session is not something a wire value may claim. An
/// unregistered `agent` likewise writes nothing — fail closed.
///
/// `mark` is the registry write, taken as a parameter rather than reached
/// through a whole `GraphService` — the same
/// reasoning #48 gave for the function this replaces: a bound asserted *beside*
/// its enforcement point survives deleting the call, so the test drives this
/// function and observes whether (and into which space) the write happened.
pub(super) fn mark_live_session_from_body(
    mark: impl FnOnce(crate::harness::plugin::SessionKey, &str),
    agent: &str,
    session: &str,
) {
    let space = crate::harness::HarnessId::from_id(agent)
        .and_then(|h| h.plugin())
        .map(|p| p.session_key_space());
    match space {
        Some(crate::harness::plugin::SessionKey::Session) => {
            mark(crate::harness::plugin::SessionKey::Session, session);
        }
        Some(crate::harness::plugin::SessionKey::Tab) => debug!(
            target: "offload",
            agent,
            "loopback: /memory/event named a session for a tab-keyed harness; its reader owns \
             that binding, so nothing is written"
        ),
        None => warn!(
            target: "offload",
            agent,
            "loopback: /memory/event from an unregistered harness; no live-session write"
        ),
    }
}

/// `POST /memory/event`: record what a harness's memory-ingress body reports —
/// its tool events, and (V14 Phase C) the usage the same hook is the only
/// source of. Best-effort — an unclassifiable tool or a missing graph service is
/// a silent no-op (200 with the recording skipped), never an error the plugin
/// has to handle.
///
/// **V40 Phase I (issue #107 item 2): the body shape is the harness's.** This
/// function used to declare the wire struct itself and read every field of it —
/// `msg_id`, `in_tok`, `parent_session_id`, `tool`, `args`. It named no harness
/// *id*, so the layering allowlists stayed clean, but the row shape was one
/// plugin's, and a second harness's would have had nowhere to live but here.
/// [`crate::harness::plugin::HarnessPlugin::memory_event`] reads it now and
/// answers a neutral [`crate::harness::plugin::MemoryEvent`]; what stays is the
/// recording, which is cImp's: the `cwd` resolution, the graph writes and the
/// live-session registry.
pub(super) async fn handle_memory_event(
    stream: &mut TcpStream,
    ctx: &RouteCtx,
    req: &Request,
) -> AppResult<()> {
    use crate::harness::plugin::MemoryEventKind;

    // Which harness is speaking. The `agent` discriminator is CHP's, not any
    // harness's, so core reads that one field itself; everything else in the
    // body belongs to whoever sent it. An identity-less body resolves through
    // `wire_default`, which is this route's compatibility promise to plugins
    // generated before the field existed.
    let asserted = serde_json::from_slice::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v.get("agent").and_then(|a| a.as_str()).map(str::to_string));
    let agent = wire_agent(MEMORY_EVENT_ROUTE, asserted.as_deref());
    let ok = serde_json::json!({ "ok": true });
    // A harness with no memory ingress — or an `agent` naming no registered
    // harness at all — records nothing. Locked decision 2: `None` is a
    // first-class answer here, not a reason to fall back to whichever harness
    // core happens to know the body shape of.
    let Some(parsed) = crate::harness::HarnessId::from_id(agent)
        .and_then(|h| h.plugin())
        .and_then(|p| p.memory_event(&req.body))
    else {
        return write_json(stream, 200, &ok).await;
    };
    let event = match parsed {
        Ok(e) => e,
        Err(why) => {
            return write_json(stream, 400, &serde_json::json!({ "ok": false, "error": why })).await;
        }
    };

    let Some(graph) = ctx.graph() else {
        return write_json(stream, 200, &ok).await;
    };
    // #104: every arm below opens the project's store (memory rows, usage
    // totals), so the plugin-supplied `cwd` is resolved to a real root first.
    // This body carries no `tab` — the memory POST never had one — so an
    // unresolvable cwd has nothing to fall back to and the event is dropped
    // rather than filed against a directory that is not a project.
    let Some(cwd) = external_project_root(ctx.app(), &ctx.settings(), None, event.cwd.as_deref())
    else {
        return write_json(stream, 200, &ok).await;
    };
    // C-2 (2026-08-07 review) used to read settings here, once for the whole
    // request, so the three live-session writes below could refuse a key that
    // named a configured tab. V40 Phase D removed the read with the check: the
    // registry has two key spaces now and `mark_live_session_from_body` decides
    // which one a body-supplied id lands in, which needs no settings at all.
    let mark_live = |target: &str| {
        mark_live_session_from_body(
            |space, k| graph.mark_live_session(space, k, agent, k),
            agent,
            target,
        )
    };

    match event.kind {
        // V24 Phase F: a completed assistant turn's real token totals. The
        // roll-up target and the declared lane are the sending harness's choice
        // (locked decision 19); `record_usage` upserts by `msg_id`, so the
        // plugin's duplicate final emit is harmless.
        MemoryEventKind::Turn {
            target,
            origin,
            msg_id,
            model,
            in_tok,
            out_tok,
            cache_read,
            cache_make,
        } => {
            graph.record_usage(
                &cwd,
                &target,
                agent,
                crate::graph::UsageEvent::Turn {
                    msg_id,
                    model,
                    in_tok,
                    out_tok,
                    cache_read,
                    cache_make,
                    origin: origin.to_string(),
                },
            );
            // Mark the SAME id live: the target is the session row that exists
            // / gets the spend attributed (the parent when a child reports), so
            // that's the row the Sessions list should flag active.
            mark_live(&target);
        }
        // A sub-agent's tool call: recorded against nobody, but the PARENT
        // stays live — the child's activity is the parent still working.
        MemoryEventKind::SubagentTool { parent } => mark_live(&parent),
        MemoryEventKind::Tool { tool, args } => {
            // V40 Phase A, locked decision 16: the memory classification is the
            // SENDING harness's, resolved through the registry. A body whose
            // `agent` names no registered harness records nothing — where the
            // old single `match` would have answered it out of whichever
            // vocabulary happened to contain the name.
            let source = crate::harness::HarnessId::from_id(agent);
            if let Some((kind, arg)) = crate::harness::native::memory_kind(source, &tool) {
                // V40 Phase C, locked decision 16: which KEY carries the target
                // is the sending harness's vocabulary, not core's. This was a
                // chain of four `or_else`s mixing one harness's snake_case with
                // another's camelCase in one lookup — see
                // `HarnessPlugin::memory_arg_keys`.
                let value = crate::harness::native::memory_arg(source, arg, &args);
                let (path, detail) = match arg {
                    crate::harness::plugin::MemArg::Path
                    | crate::harness::plugin::MemArg::Pattern => (value.unwrap_or_default(), None),
                    crate::harness::plugin::MemArg::Command => (
                        String::new(),
                        value.map(|c| c.chars().take(200).collect::<String>()),
                    ),
                };
                // Skip an event with no usable target: an empty path
                // (Path/Pattern) or a Command whose `command` arg was absent
                // (detail is None) — recording it would just evict useful
                // events from the ring.
                let recordable = match arg {
                    crate::harness::plugin::MemArg::Command => detail.is_some(),
                    _ => !path.is_empty(),
                };
                if recordable {
                    graph.record_mem_event(
                        &cwd,
                        &event.session_id,
                        agent,
                        kind,
                        &path,
                        None,
                        None,
                        detail.as_deref(),
                    );
                }
            }

            // V14 Phase C: the usage tap. Unlike the memory recording above,
            // this runs for EVERY tool call, not just ones the native table
            // maps to a filesystem/query target — usage wants the full picture.
            // `chars` is estimated from the tool's serialized INPUT args (its
            // actual output isn't visible to this hook). This path records only
            // tool-result chars, never Turn tokens, so a session that never got
            // a real usage event stays est-only in the X-ray (V24 Phase E
            // derives `est_only` from zero token totals — see
            // `usage_row_for_session`).
            let chars = serde_json::to_string(&args)
                .map(|s| s.chars().count())
                .unwrap_or(0) as u32;
            graph.record_usage(
                &cwd,
                &event.session_id,
                agent,
                crate::graph::UsageEvent::ToolResult {
                    tool: Some(tool),
                    chars,
                },
            );

            // V24 Phase B: this harness has no tab binding on this path, so the
            // live-session registry is keyed by the reporting session id itself;
            // the entry expires by TTL (there is no cancel signal to clear it).
            // C-2: which is exactly why the key must not be allowed to name a
            // TAB — the other half of the same map.
            mark_live(&event.session_id);
        }
        MemoryEventKind::Nothing => {}
    }

    write_json(stream, 200, &ok).await
}

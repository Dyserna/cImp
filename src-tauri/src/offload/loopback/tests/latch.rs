//! The taint latch: the delegation gate, per-tab/per-agent scoping, the
//! external/local sides and the flips between them, the session budget, and
//! what every one of those records.

use super::*;

/// #48 F-16: what `/graph_run` actually states — a native route that knows
/// which project the call runs against. Any test that drives a
/// PERSISTENT-WRITE through `gate` must use this rather than [`NO_CONTENT`],
/// because that path writes a `MemoryQuarantine` row and `record_flag`'s
/// tripwire refuses to let one be filed under no project.
const NATIVE_IN_PROJECT: CallProvenance<'static> = CallProvenance::internal_in(TEST_ROOT);

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

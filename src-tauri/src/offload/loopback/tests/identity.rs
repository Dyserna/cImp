//! Who may key a latch: only a configured AI tab of the right consumer, only
//! for a session this instance can prove, and never an id it invented. Session
//! rotation, override rows and the status snapshot are the same question from
//! the other end.

use super::*;

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

/// **The latch's identity funnel, RUN rather than read** (V42 Phase A2).
///
/// `latch_scope` is *the* funnel every gated route resolves identity through —
/// `/graph_run` and `/mcp/call` via `gate`, `/latch/beacon` via `beacon`, the
/// three `/context/*` hooks and `/delegate` via `admit` — and the rule it
/// enforces (V33 C5, #45) is that a scope comes from the user's own tab
/// CONFIGURATION and never from the caller's assertion. It took an `AppHandle`
/// (the V28 live-session lookup), so until A2 injected that reach the rule
/// could only be read; now it can be driven:
///
/// * a configured tab of the asserted consumer resolves to a scope, whose
///   `root` is derived from settings — never from anything on the wire;
/// * an id that names no configured tab resolves to `Unknown`, which keys NO
///   registry entry (#45's bound) and is deliberately not the same answer as
///   "no id at all";
/// * no id at all is `Anonymous`, the fail-open case a pre-`--tab` child hits.
#[test]
fn a_latch_scope_comes_from_the_configuration_and_never_from_the_caller() {
    use crate::offload::host::testing::{route_ctx, FakeRouteServices};
    use crate::offload::latch::LatchScoping;
    use crate::service::host::testing::core_host;
    use crate::settings::{AiToolTabConfig, Settings, SettingsHandle, TabConfig};

    let scratch = root_tree("latch-scope");

    #[allow(clippy::field_reassign_with_default)]
    let mut cfg = AiToolTabConfig::default();
    cfg.id = "ai-one".to_string();
    cfg.name = "one".to_string();
    cfg.command = crate::harness::DEFAULT_HARNESS.token().to_string();
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
    core.launch_cwd = scratch.clone();
    // No graph service: the live-session lookup withholds, which is the
    // `None` "absence of evidence" case `LatchScope::session` documents.
    let ctx = route_ctx(FakeRouteServices {
        core: Some(core),
        ..Default::default()
    });
    let agent = crate::harness::DEFAULT_HARNESS.token();

    match latch_scope(&ctx, &settings, agent, Some("ai-one")) {
        LatchScoping::Scoped(s) => {
            assert_eq!(s.tab, "ai-one");
            assert_eq!(s.agent, agent);
            assert_eq!(
                s.session, None,
                "no live-session registry means no session, never an invented one"
            );
            assert_eq!(
                s.root,
                tab_root_key(&ctx, &settings, "ai-one"),
                "the root rides the scope and is derived from settings"
            );
            assert!(!s.root.is_empty(), "a resolvable root must not read as absent");
        }
        other => panic!("a configured tab must resolve to a scope, got {other:?}"),
    }

    assert!(
        matches!(
            latch_scope(&ctx, &settings, agent, Some("ai-forged")),
            LatchScoping::Unknown(ref t) if t == "ai-forged"
        ),
        "an id naming no configured tab must be Unknown — it keys no registry entry"
    );
    assert!(
        matches!(
            latch_scope(&ctx, &settings, agent, None),
            LatchScoping::Anonymous
        ),
        "no id at all is Anonymous, which is a different answer from Unknown"
    );

    let _ = std::fs::remove_dir_all(&scratch);
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

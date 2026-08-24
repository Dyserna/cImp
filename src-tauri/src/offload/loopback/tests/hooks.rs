//! The hook routes: what a Claude/OpenCode hook POST may key, the working
//! directory a post-edit check runs in, the header names the overlay emits,
//! and the budgets the injection replies sit under.

use super::*;

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

/// **V33 C4's allowlist, RUN rather than read** (V42 Phase A2).
///
/// The scan above
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

    let roots = hook_exec_roots(&ctx, &settings);
    assert!(
        roots.contains(&served),
        "the served root must always be admitted: {roots:?}"
    );
    assert!(
        roots.contains(&tab_dir),
        "a configured tab's own directory must be admitted: {roots:?}"
    );

    assert_eq!(
        admitted_hook_root(&roots, None),
        Some(served.clone()),
        "a body naming no cwd runs in the served root, not wherever the process happens to be"
    );
    assert_eq!(
        admitted_hook_root(&roots, Some(&tab_dir.to_string_lossy())),
        Some(tab_dir.clone()),
    );
    assert_eq!(
        admitted_hook_root(&roots, Some(&outside.to_string_lossy())),
        None,
        "a directory outside every root must be REFUSED, not fallen back on"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

// ── #104: a cwd is never a project root by itself ──────────────────────

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
    let plugin = include_str!("../../../harness/opencode/templates/plugin.js");
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
    let ipc = include_str!("../../../ipc/commands.rs");
    assert!(
        ipc.contains("apply_latch_override(&ctx, &consumer, &tab, &action)"),
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
    let latch_src = include_str!("../../latch.rs");
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

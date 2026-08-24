//! Request bodies and the path guard in front of them: what each route parses,
//! what it refuses, and the ancestor check that keeps a cwd inside the tree
//! this instance serves.

use super::*;

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

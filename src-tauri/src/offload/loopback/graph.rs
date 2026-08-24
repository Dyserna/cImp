//! `POST /graph_run` — the warm code-graph query route.
//!
//! One of the route families V42 R4 (#115) split out of `loopback.rs`; see
//! [`super`] for the dispatch and the shared wire helpers.

use super::*;

/// A `POST /graph_run` request body (the warm code-graph query path).
#[derive(Deserialize)]
pub(super) struct GraphRunBody {
    /// The calling session's working directory; the project root is resolved
    /// from it (same ancestor-walk the MCP child uses).
    ///
    /// **Absent ⇒ `"."`, which resolves to nothing.** The field is optional for
    /// back-compat with children that predate it, and the `"."` default is a
    /// placeholder, NOT a working directory: it would resolve against *cImp's*
    /// process cwd (its install directory), which is never the caller's
    /// project. Every consumer therefore refuses it rather than guessing —
    /// graph tools with "no code graph found from .", `run_check` with an
    /// explicit "not an absolute project root", and `sandbox::plan` with an
    /// `Unavailable` skip. rc.9 live: a `/graph_run` post that omitted this
    /// field reached the AppContainer engine, which mapped a drive letter to
    /// `\??\.` and failed the spawn with a bare `CreateProcessW failed (267)`.
    #[serde(default)]
    pub(super) cwd: Option<String>,
    /// The `graph_*` / `context_*` tool name.
    pub(super) name: String,
    /// The tool arguments.
    #[serde(default)]
    pub(super) args: Value,
    /// The requesting consumer (`"claude"` / `"opencode"`); selects the activity
    /// source and the `context_*` tools' per-agent session scope. Defaults to
    /// Claude when absent.
    #[serde(default)]
    pub(super) consumer: Option<String>,
    /// V28 (issue #13): the cImp TAB id the calling MCP child was spawned for
    /// (`cimp --offload-mcp --tab <id>`), used to resolve *which* session of
    /// this agent the `context_*` memory tools should scope to. Optional by
    /// design — a child spawned before the upgrade sends no `tab`, and an
    /// unknown/stale one resolves to `None`; both fall back to the pre-V28
    /// most-recent-session behavior rather than erroring the call.
    #[serde(default)]
    pub(super) tab: Option<String>,
}

/// `POST /graph_run`: run one `graph_*` tool against the app's WARM graph index
/// (single shared connection — no second cross-process open of the SQLite store)
/// and return its text. The `GraphService` is resolved from managed state at
/// request time, so this is robust against the graph-vs-loopback startup order.
pub(super) async fn handle_graph_run(stream: &mut TcpStream, app: &AppHandle, req: &Request) -> AppResult<()> {
    let Some(body) = decode::<GraphRunBody, _>(stream, req, bad_body_result).await? else {
        return Ok(());
    };
    let graph = match app.try_state::<Arc<crate::graph::GraphService>>() {
        Some(g) => g.inner().clone(),
        None => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some("graph service not ready".into()),
            };
            return write_json(stream, 200, &r).await;
        }
    };
    // V40 review H-1: ONE identity for this call. `proxy_identity` folds an
    // in-app consumer onto `Consumer::conservative_grant` and refuses a token
    // nobody declared — before this, `?consumer=offload` reached the
    // `LatchRoute::Native` gate with an activity source that names no
    // configured tab (so no latch, no attribution) and `?consumer=<garbage>`
    // did the same with nothing refusing it. The FOLDED token goes downstream,
    // so `run_command`'s exposure switch, the memory agent scope, the activity
    // source and the latch all answer about the same harness.
    let Some((resolved, consumer_source)) = proxy_identity(body.consumer.as_deref()) else {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(unknown_consumer_message()),
        };
        return write_json(stream, 400, &r).await;
    };
    let consumer = resolved.token();
    // V28: resolve the calling TAB to the session it currently reports, so the
    // `context_*` memory tools scope to this tab's own session instead of "the
    // most recent session for this agent" (two same-agent tabs used to share —
    // and steal — one memory scope). Fail-open: no `tab`, an unknown key, a
    // different agent's entry, or a TTL-stale one all yield `None`, which is
    // exactly the pre-V28 behavior.
    //
    // V32 Phase B folds that same resolution into the latch scope, so the
    // registry is consulted once and the memory scope and the taint scope can
    // never disagree about which session this call belongs to.
    //
    // V32 Phase G: ONE settings read for the whole call (see the sibling note
    // on `/mcp/call`). #45 pulls it above the scope resolution, because the tab
    // id is now validated against the configured tab list and that check must
    // use the same snapshot as the policy it feeds.
    let settings = live_settings(app);
    // #104: the tools this route serves take a project root — `run_command`
    // creates its marker directory under it and `run_check` runs the project's
    // configured commands from it — so the body's `cwd` is resolved to a real
    // root rather than used as one. A refusal is a tool-level error the model
    // can read and act on, not a silently different project.
    let Some(cwd) = external_project_root(app, &settings, body.tab.as_deref(), body.cwd.as_deref())
    else {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(format!(
                "no project root for {} — a working directory is not a project by \
                 itself; open this project as a cImp tab, or run it from inside a \
                 git repository",
                bounded_id(body.cwd.as_deref().unwrap_or("(none)"))
            )),
        };
        return write_json(stream, 200, &r).await;
    };
    let scoping = latch_scope(app, &settings, consumer_source, body.tab.as_deref());
    // #48 F-20: resolved BEFORE `into_scope()` collapses `Anonymous` and
    // `Unknown` into one `None`. That collapse is right for the latch (both fail
    // open) and wrong for the row, which has to say which of the three this call
    // was. See `LatchScoping::attribution`.
    let tab_attr = scoping.attribution();
    let scope = scoping.into_scope();
    let session = scope.as_ref().and_then(|s| s.session.clone());

    // V32 Phase B: the session taint latch over the tools THIS route serves —
    // the content-bearing graph tools (LOCAL-CAPABILITY), the structural ones
    // and the memory reads (TRUSTED, never gated), and `context_note`
    // (PERSISTENT-WRITE).
    //
    // V32 Phase C2 (locked decision 10): a `context_note` under an EXTERNAL
    // latch is no longer refused. The gate returns `Quarantined` and the write
    // proceeds with that verdict threaded into it — the note is stored with a
    // `tainted` flag, kept out of `context_recall`/`context_notes`/the
    // compaction carry-over/the fact distiller (and so out of auto-injection),
    // and held for explicit user promote-or-discard. That preserves the
    // legitimate research conclusion the Phase B refusal dropped.
    //
    // V32 Phase G: both halves resolve through the three-level hierarchy at this
    // tab's scope, from ONE settings read — so a tab with the latch overridden
    // off still quarantines, and a tab with the master switch off does neither.
    // #48 F-16: resolved here, from the service, so the quarantine row this gate
    // may write and the activity row the dispatch will write name the SAME
    // project. `graph_root_key` is `run_graph_tool`'s own resolution, exposed.
    let call_root = graph.graph_root_key(&cwd);
    let gate_policy = GatePolicy::resolve(&settings, scope.as_ref());
    let taint = match latches().gate(
        scope.as_ref(),
        LatchRoute::Native,
        &body.name,
        gate_policy,
        CallProvenance::internal_in(&call_root),
    ) {
        Ok(t) => t,
        Err(refusal) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(refusal.to_string()),
            };
            // 200, like every other tool-level error here: the child renders
            // `error` as the tool result, which is how the model reads it.
            return write_json(stream, 200, &r).await;
        }
    };

    let r = match graph
        .run_graph_tool(
            &cwd,
            &body.name,
            &body.args,
            consumer,
            session.as_deref(),
            // V32 Phase G: the second resolved verdict this route carries — the
            // memory-read tools it serves (`context_recall` / `context_notes`)
            // are the recall envelope's delivery point, and only this frame
            // knows the tab whose scope decides it.
            toolclass::CallGuards {
                taint,
                spotlight_recall: crate::settings::injection::effective(
                    crate::settings::injection::Feature::Spotlighting,
                    crate::settings::injection::Scope::for_tab(
                        consumer_source,
                        body.tab.as_deref(),
                    ),
                    &settings,
                ),
            },
            tab_attr,
        )
        .await
    {
        Ok(text) => RunResult {
            ok: true,
            text: Some(text),
            error: None,
        },
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(e),
        },
    };
    // 200 even on a tool-level error: the child renders `error` as a tool result.
    write_json(stream, 200, &r).await
}

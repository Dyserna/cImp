//! `POST /mcp/list` and `POST /mcp/call` — the proxied MCP surface.
//!
//! One of the route families V42 R4 (#115) split out of `loopback.rs`; the
//! consumer→identity funnel it gates on is [`super::proxy_identity`].

use super::*;

/// A `POST /mcp/call` request body (a Claude-exposed MCP tool invocation).
#[derive(Deserialize)]
pub(super) struct McpCallBody {
    /// The namespaced `<server>__<tool>` name.
    pub(super) name: String,
    /// The tool arguments.
    #[serde(default)]
    pub(super) arguments: Value,
    /// The calling session's working directory (the child's cwd), used to
    /// attribute the Tool Activity row to a project. Optional by design — a
    /// child from before this field sends none, and the row just gets an
    /// empty root.
    #[serde(default)]
    pub(super) cwd: Option<String>,
    /// V32 Phase B: the cImp TAB id the calling MCP child was spawned for.
    /// V28 sent `tab` on `/graph_run` only — external servers hold no cImp
    /// memory scope — but the taint latch needs the *same* identity on both
    /// tool-serving routes or a tab could launder an external fetch past its
    /// own latch. Optional on exactly the same fail-open terms as
    /// [`GraphRunBody::tab`].
    #[serde(default)]
    pub(super) tab: Option<String>,
}

/// `POST /mcp/list`: the proxied MCP tool descriptors for the requesting
/// consumer (servers with that consumer's access flag), for the per-session
/// child to merge into its `tools/list`. The consumer is taken from the
/// `?consumer=` query (Claude when absent). Returns
/// `{ "tools": [ {name, description, inputSchema}, … ] }`.
pub(super) async fn handle_mcp_list(
    stream: &mut TcpStream,
    service: &Arc<OffloadService>,
    req: &Request,
) -> AppResult<()> {
    // An unrecognised consumer advertises NOTHING rather than inheriting
    // another's grants (locked decision 2). An empty list, not an error: a
    // `tools/list` that 400s would break the child's handshake, while an empty
    // one is the honest answer to "what may this caller reach".
    // V40 review H-1: through the same funnel `/mcp/call` resolves its grant
    // with, so a token that is callable is exactly a token that is listed.
    let tools = match proxy_identity(query_param(&req.path, "consumer")) {
        Some((c, _)) => service.mcp_tool_descriptors(c).await,
        None => Vec::new(),
    };
    write_json(stream, 200, &serde_json::json!({ "tools": tools })).await
}

/// `POST /mcp/call`: run one proxied MCP tool for the requesting consumer.
/// Body `{name, arguments}`; consumer from `?consumer=` (Claude when absent).
/// The service guards the call against any tool not offered by a server
/// exposed to that consumer. 200 even on a tool-level error (the child renders
/// `error` as a tool result).
///
/// V32 Phase B adds the two halves of the consumer-side containment: the tab's
/// session taint latch in front of the call, and the spotlighting envelope
/// around its result. This is the tab's untrusted-content intake — the one
/// route through which a fetched page's bytes reach a Claude/OpenCode session.
pub(super) async fn handle_mcp_call(
    stream: &mut TcpStream,
    service: &Arc<OffloadService>,
    ctx: &RouteCtx,
    req: &Request,
) -> AppResult<()> {
    let Some(body) = decode::<McpCallBody, _>(stream, req, bad_body_result).await? else {
        return Ok(());
    };
    // V40 review H-1: the GRANT and the LATCH KEY are resolved together, here,
    // before anything is charged or gated — `proxy_identity` folds an in-app
    // consumer onto `Consumer::conservative_grant` and derives `agent` from the
    // FOLDED consumer, so a request served Claude's server set is judged under
    // Claude's latch. Refused (not degraded) for a token nobody declared:
    // locked decision 2, and refusing here means an unattributable caller
    // cannot spend a tab's budget either.
    let Some((consumer, agent)) = proxy_identity(query_param(&req.path, "consumer")) else {
        return write_json(
            stream,
            400,
            &serde_json::json!({ "error": unknown_consumer_message() }),
        )
        .await;
    };
    // V32 Phase G: ONE settings read here, so the tab-id check, the latch, the
    // budget, detection and the envelope all resolve under the same snapshot —
    // a mid-call settings save must not leave a result screened by one posture
    // and wrapped by another. #45 adds the tab-id check inside `latch_scope` to
    // that list, which is why the read precedes it.
    //
    // Corrected 2026-08-08 (#48, review finding G-4): this comment also named
    // "the SSRF screen", and that one is NOT resolved here. `ServiceHandle::mcp_call`
    // builds `outbound::Policy` from its own independent `self.settings.current()`,
    // so the SSRF guard can be a snapshot behind or ahead of everything above.
    // Benign in practice (sub-millisecond, both postures the user's own) and
    // recorded as an accepted residual in the V32 spec rather than fixed here;
    // the fix is to thread `settings` into `mcp_call`. Do not restore the old
    // wording without making it true.
    let settings = ctx.settings();
    let scoping = latch_scope(ctx, &settings, agent, body.tab.as_deref());
    // #48 F-20 — see `handle_graph_run`: resolved before the collapse. This is
    // the row that answers "which tab fetched that page", and it is the one the
    // finding says could not.
    let tab_attr = scoping.attribution();
    let scope = scoping.into_scope();
    let inj_scope = crate::settings::injection::Scope::for_tab(agent, body.tab.as_deref());
    let gate_policy = GatePolicy::resolve(&settings, scope.as_ref());
    // V32 Phase C: the flagged row's provenance, read from the arguments before
    // they are moved into the call — the result alone cannot say which page it
    // came from, and that is the first thing a user reads off the row.
    //
    // #48 (F-3): read BEFORE the gate rather than after the budget, because the
    // gate is now also a row writer — this is the route whose admitted call
    // contaminates the conversation, and "from which page" is one of the three
    // facts the contamination row exists to carry. Moving the read up changes
    // nothing else: `origin_of` only inspects the arguments.
    let (flag_url, flag_host) = detection::origin_of(&body.arguments);
    // The gate's V32 Phase C2 `WriteTaint` is discarded here, and can only ever
    // be `Clean`: this route serves proxied `<server>__<tool>` ids, every one of
    // which classifies EXTERNAL by the unknown-⇒-EXTERNAL invariant, so no
    // PERSISTENT-WRITE can arrive on it. Memory writes reach cImp through
    // `/graph_run` alone.
    if let Err(refusal) = latches().gate(
        scope.as_ref(),
        LatchRoute::Proxied,
        &body.name,
        gate_policy,
        CallProvenance::intake(flag_url.as_deref(), flag_host.as_deref()),
    ) {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(refusal.to_string()),
        };
        return write_json(stream, 200, &r).await;
    }
    // V32 Phase C: the session's EXTERNAL budget, checked after the latch (a
    // latched-out call was never going to run, and must not consume the one
    // budget report) and before the call leaves the process.
    if let Err(refusal) = latches().budget_gate(
        scope.as_ref(),
        crate::settings::injection::budget_limits(&settings, inj_scope),
        &body.name,
    ) {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(refusal.to_string()),
        };
        return write_json(stream, 200, &r).await;
    }

    let cwd = body.cwd.map(PathBuf::from);
    // The scope label the SSRF screen's `injection_flag` row carries. Without
    // tab identity we are already fail-open on the latch and the budget; the
    // SSRF screen still runs (it needs no identity), so name the scope honestly
    // rather than inventing one.
    let scope_label = scope
        .as_ref()
        .map(LatchScope::label)
        .unwrap_or_else(|| format!("{agent}:{}", outbound::NO_TAB_IDENTITY));
    let detection_cfg = detection::Config::from_settings(&settings, inj_scope);
    let spotlight_on = crate::settings::injection::effective(
        crate::settings::injection::Feature::Spotlighting,
        inj_scope,
        &settings,
    );
    let root_key = cwd
        .as_deref()
        .map(crate::activity::root_key)
        .unwrap_or_default();
    // #48: the tab session's audit-row claim ledger, threaded to the SSRF
    // chokepoint and to the detection boundary so neither can flood a capped
    // feed on a loop. See `TabAudit`.
    let audit = TabAudit(scope.as_ref(), agent);
    let called = service
        .mcp_call(
            consumer,
            &body.name,
            body.arguments,
            cwd.as_deref(),
            &scope_label,
            body.tab.as_deref(),
            tab_attr,
            &audit,
        )
        .await;
    // V32 Phase C, corrected in #48 (D-3): charge the session's EXTERNAL budget
    // for the call that was just ATTEMPTED — before the match, so it cannot
    // again end up on one arm only. See `LatchRegistry::charge_call`.
    latches().charge_call(scope.as_ref(), &called);
    let r = match called {
        // Locked decisions 5 + 6: detection, the envelope and the warning
        // header all compose here, at the proxy's tool-result boundary, so
        // EVERY consumer gets them identically — and they apply whether or not
        // the call carried tab identity, since none of the three needs it. The
        // same `wrap_external_result` the worker's boundary calls, so the
        // external-only rule and the composition order have one definition.
        //
        // #48 M-17 corrects the sentence that used to end this comment — "Errors
        // are cImp-composed strings, not fetched content, and are never screened
        // or wrapped." The diagnostic half is cImp's; the server's own
        // `error.message` never was, and it reached the model here with no bound,
        // no envelope and no screen. `HostError` keeps the two halves apart and
        // `wrap_remote_error` treats the remote half as what it is.
        Ok(text) => {
            let wrapped = detection::wrap_external_result(
                &body.name,
                text,
                detection::ResultCtx {
                    consumer: agent,
                    scope: &scope_label,
                    root: root_key,
                    url: flag_url,
                    host: flag_host,
                    cfg: detection_cfg,
                    spotlight: spotlight_on,
                    audit: &audit,
                    // #48/M-5: the proxy truncates NOTHING after this point — the
                    // consumer (a Claude/OpenCode tab) receives the whole result.
                    // This is the boundary where the unscreened notice is
                    // load-bearing, and the reason it is derived rather than
                    // deleted.
                    delivered_bytes: usize::MAX,
                },
            )
            .await;
            RunResult {
                ok: true,
                text: Some(wrapped),
                error: None,
            }
        }
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(
                detection::wrap_remote_error(
                    &body.name,
                    e.diagnostic(),
                    e.remote(),
                    detection::ResultCtx {
                        consumer: agent,
                        scope: &scope_label,
                        root: root_key,
                        url: flag_url,
                        host: flag_host,
                        cfg: detection_cfg,
                        spotlight: spotlight_on,
                        audit: &audit,
                        // As above: nothing truncates a proxied error either.
                        delivered_bytes: usize::MAX,
                    },
                )
                .await,
            ),
        },
    };
    write_json(stream, 200, &r).await
}

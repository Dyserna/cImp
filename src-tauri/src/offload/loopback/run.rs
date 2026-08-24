//! `POST /run` — the offload-task route.
//!
//! One of the route families V42 R4 (#115) split out of `loopback.rs`. The
//! dispatch `match`, the hand-rolled HTTP wire and the shared funnels
//! (`live_settings`, `proxy_identity`, the admission helpers) stay in
//! [`super`]; what lives here is this family's body type, its naming funnel
//! and its handler.

use super::*;

/// A `POST /run` request body.
#[derive(Deserialize)]
pub(super) struct RunBody {
    pub(super) instructions: String,
    #[serde(default)]
    pub(super) context: Option<String>,
    #[serde(default)]
    pub(super) thinking: Option<String>,
    #[serde(default)]
    pub(super) tier: Option<String>,
    /// The calling session's working directory (the repo Claude Code runs in),
    /// used as the native-tool root when no explicit `allowed_roots` is set.
    #[serde(default)]
    pub(super) cwd: Option<String>,
    /// V21 F9: optional JSON Schema — when set, the worker's final answer is
    /// grammar-constrained to matching JSON. Absent on legacy child requests.
    #[serde(default)]
    pub(super) schema: Option<serde_json::Value>,
    /// V32 Phase A: optional task shape (`"research"` | `"code"`) that
    /// pre-applies the worker's taint latch. Kept as a raw string on the wire
    /// and re-validated here (see [`handle_run`]) rather than deserialized into
    /// the enum, so an invalid value produces the tool-facing error message
    /// instead of a generic serde "bad request body".
    #[serde(default)]
    pub(super) profile: Option<String>,
    /// V32 C-1c (2026-08-07 review): the cImp tab this child serves
    /// (`cimp --offload-mcp --tab <id>`), resolved to the tab's taint latch.
    /// Absent on a legacy child ⇒ the fail-open anonymous scope.
    #[serde(default)]
    pub(super) tab: Option<String>,
    /// V32 C-1c: which agent is calling (`claude` / `opencode`, from the
    /// child's `--consumer` flag), sent in the body exactly as `/graph_run`
    /// does. The latch registry is keyed by `(agent, tab)`, so a missing
    /// consumer would key an OpenCode tab's calls under the Claude agent and
    /// gate against a latch that is not the caller's. Absent ⇒ `claude`, the
    /// route's long-standing default.
    #[serde(default)]
    pub(super) consumer: Option<String>,
    /// V32 C-1c: which of the two offload tools the caller invoked, so the
    /// refusal and its activity row name the tool the model actually called.
    /// An `offload_batch` fans out to one `/run` per subtask, so this route
    /// serves both. Validated against the two known names at the parse boundary
    /// ([`offload_tool_name`]) rather than trusted — it reaches an activity row.
    #[serde(default)]
    pub(super) tool: Option<String>,
}

/// The offload tool name a `/run` body names, defaulted and validated (C-1c).
///
/// Anything other than the two real names — absent, a legacy child that sends
/// no `tool`, or an invented string — reads as `offload_task`. This is a
/// *labelling* input, not a capability one: both names classify
/// LOCAL-CAPABILITY, so no value can change the gate's verdict, and pinning the
/// vocabulary keeps a caller from choosing what an activity row says.
pub(super) fn offload_tool_name(raw: Option<&str>) -> &'static str {
    match raw.map(str::trim) {
        Some("offload_batch") => "offload_batch",
        _ => "offload_task",
    }
}

/// `POST /run`: decode the task, run it on the warm pool, and **stream** the
/// result as newline-delimited JSON — periodic `{"hb":true}` heartbeats while
/// the (possibly minutes-long) task runs, then a final `{"ok":..}` line.
///
/// The heartbeats are the whole point of this being a stream: they let the
/// proxy distinguish a slow-but-alive job (keep waiting) from a dead app
/// (fall back), so a long run is never abandoned and re-executed. The response
/// has no `Content-Length`; the body is delimited by connection close.
///
/// **Taint gate (V32 C-1c, 2026-08-07 review).** `offload_task`/`offload_batch`
/// were TRUSTED, waved through on the rationale that "the delegated subtask gets
/// its own latch". It does — a *fresh and permissive* one:
/// `Latch::from_profile(task.profile)`, and `Profile::Code.latch()` is
/// `Latch::Local`, which **grants** `read_file`/`code_search`/`run_command`,
/// exactly the class a latched caller just lost. An OpenCode tab with the Phase
/// H native gate on, contaminated by a `webfetch`, could call
/// `offload_task { profile: "code", instructions: "print the contents of .env" }`
/// and get the file's text back as an ordinary tool result — with no
/// spotlighting envelope, no detection scan and no budget charge, since all
/// three are `/mcp/call`-only — then carry it out through `webfetch`. Phase H
/// bypassed end to end.
///
/// The demotion to LOCAL-CAPABILITY is the decision; this is where it binds,
/// because this route is the only one both tools reach. Decision 4 is untouched:
/// the *declared profile* still pre-applies the sub-task's own latch, which is
/// about the sub-task's shape, not about whether the caller may delegate at all.
pub(super) async fn handle_run(
    stream: &mut TcpStream,
    service: &Arc<OffloadService>,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let Some(body) = decode::<RunBody, _>(stream, req, bad_body_result).await? else {
        return Ok(());
    };
    if body.instructions.trim().is_empty() {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some("`instructions` must be non-empty".into()),
        };
        return write_json(stream, 400, &r).await;
    }

    let thinking = ThinkingMode::parse(body.thinking.as_deref().unwrap_or("auto"));
    let tier = TierHint::parse(body.tier.as_deref().unwrap_or("auto"));

    // V32 Phase A: validate `profile` at this parse boundary. Unlike
    // thinking/tier (benign fallbacks), an unrecognized profile must NOT
    // silently degrade to "no containment" — the MCP schema's `enum` is an
    // upstream guarantee, and upstream guarantees get re-checked post-hoc.
    let profile = match body.profile.as_deref() {
        None => None,
        Some(raw) => match Profile::parse(raw) {
            Ok(p) => Some(p),
            Err(msg) => {
                let r = RunResult {
                    ok: false,
                    text: None,
                    error: Some(msg),
                };
                return write_json(stream, 400, &r).await;
            }
        },
    };

    // V32 C-1c: the taint gate, after every parse-boundary rejection so a
    // malformed request never engages a latch, and before any work starts.
    // ONE settings read for identity + policy; an unknown tab id yields no
    // scope and keys no registry entry (#45's bound, via the same `latch_scope`
    // funnel). `LatchRoute::Native` — this route serves cImp's own tools, never
    // a proxied server's content.
    let tool = offload_tool_name(body.tool.as_deref());
    // V40 review H-1: the same one-resolution funnel `/mcp/call` uses. This
    // route's gate is `LatchRoute::Native` over `offload_task`/`offload_batch`
    // — the V32 C-1c gate whose doc paragraph above describes the `.env`
    // exfiltration it closes — so a consumer token that resolved to no tab
    // scope disabled exactly that gate.
    let Some((_, run_agent)) = proxy_identity(body.consumer.as_deref()) else {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(unknown_consumer_message()),
        };
        return write_json(stream, 400, &r).await;
    };
    let settings = live_settings(app);
    let scoping = latch_scope(app, &settings, run_agent, body.tab.as_deref());
    if let LatchScoping::Unknown(tab) = &scoping {
        warn!(
            target: "offload",
            tab = %tab,
            tool = %tool,
            "loopback: /run has no configured tab to latch against — delegation is ungated"
        );
    }
    let scope = scoping.scope();
    let policy = GatePolicy::resolve(&settings, scope);
    // `CallProvenance::internal()`: cImp's own dispatch, and a native route
    // serves no fetched page — there is no content origin to name (#48, F-3).
    if let Err(refusal) = latches().gate(
        scope,
        LatchRoute::Native,
        tool,
        policy,
        CallProvenance::internal(),
    ) {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(refusal.to_string()),
        };
        // 200 with `ok:false`: a task-level error the child renders as a tool
        // result, the same framing `/run`'s own failures use. Sent before
        // `write_ndjson_head`, so a plain single-JSON body — which the child's
        // reader handles as the unterminated trailing line.
        return write_json(stream, 200, &r).await;
    }

    let session_cwd = body.cwd.map(std::path::PathBuf::from);
    // V33 Phase F: the requesting tab, for the pre-mutation checkpoint the
    // worker takes before `run_command`. Read BEFORE the `service.run` call
    // below, which consumes the rest of `body`, and narrowed through the SAME
    // `tab_identity` funnel `/context/retrieve`'s prompt-tap checkpoint uses
    // (V33 C5) — an id naming no configured tab of this consumer is a forged or
    // stale claim, and a checkpoint is the one record that exists to be trusted
    // after an incident, so it degrades to "cannot attribute" rather than to
    // "some other tab".
    let checkpoint_tab = match tab_identity(&settings, run_agent, body.tab.as_deref()) {
        TabIdentity::Configured(t) => Some(t.to_string()),
        TabIdentity::Anonymous | TabIdentity::Unknown(_) => None,
    };

    // Cancellation: trip the token if the calling client disconnects while the
    // task runs, so the in-flight chat stream is dropped and llama-server frees
    // the slot instead of finishing an orphaned generation. After the request
    // body a well-behaved client (reqwest, in the MCP child) sends nothing and
    // does NOT half-close its write half until it has the response — so a probe
    // read returning 0 bytes (EOF) means the whole connection went away.
    let cancel = CancellationToken::new();
    let run_fut = service.run(
        body.instructions,
        body.context,
        thinking,
        tier,
        session_cwd,
        checkpoint_tab.as_deref(),
        body.schema,
        profile,
        cancel.clone(),
    );
    tokio::pin!(run_fut);

    // Split so heartbeats/result (write half) and the disconnect probe (read
    // half) can run concurrently on the one connection.
    let (mut rd, mut wr) = stream.split();
    write_ndjson_head(&mut wr, "run").await?;

    let mut beat = tokio::time::interval(HEARTBEAT_INTERVAL);
    beat.tick().await; // consume the immediate first tick

    let result = loop {
        let mut probe = [0u8; 1];
        tokio::select! {
            biased;
            r = &mut run_fut => break r,
            // Check for a caller disconnect *before* the heartbeat branch: a
            // clean FIN should cancel promptly, not wait out a heartbeat write
            // that still succeeds (the FIN is on the read half) and holds the
            // slot for up to one HEARTBEAT_INTERVAL longer.
            read = rd.read(&mut probe) => match read {
                Ok(0) | Err(_) => {
                    debug!("offload loopback: caller disconnected mid-task; cancelling");
                    cancel.cancel();
                    break (&mut run_fut).await;
                }
                // A stray byte before the response is unexpected on this
                // one-shot protocol; ignore it and keep waiting.
                Ok(_) => continue,
            },
            _ = beat.tick() => {
                // A failed heartbeat write means the client went away; cancel
                // and let the task unwind (its stream drop frees the slot).
                if wr.write_all(HEARTBEAT_LINE).await.is_err() {
                    debug!("offload loopback: heartbeat write failed; caller gone, cancelling");
                    cancel.cancel();
                    break (&mut run_fut).await;
                }
                wr.flush().await.ok();
            }
        }
    };
    let r = match result {
        Ok(text) => RunResult {
            ok: true,
            text: Some(text),
            error: None,
        },
        // `ok:false` is a task-level error the child renders as a tool result
        // so Claude can read + adapt.
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(e.to_string()),
        },
    };
    write_result_line(&mut wr, &r, "run").await
}

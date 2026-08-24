//! `POST /audit/run` — the V26 code-audit MCP surface's app-side half.
//!
//! One of the route families V42 R4 (#115) split out of `loopback.rs`. The
//! four refusals this route may take, in the one order they may be taken,
//! are [`audit_admit`]'s; see [`super`] for the dispatch and the wire.

use super::*;

/// A `POST /audit/run` request body (V26 code-audit MCP surface).
///
/// Deliberately tiny: `category` reuses
/// [`Category`](crate::audit::adapters::Category)'s own lowercase serde (so
/// `"security"` / `"quality"` deserialize directly — a bad word is a clean parse
/// error → 400). Everything else is validated *after* the parse, by
/// [`audit_admit`], so a bad value becomes a readable tool error rather than a
/// bare 400 the model cannot act on.
#[derive(Deserialize)]
pub(super) struct AuditRunBody {
    pub(super) category: crate::audit::adapters::Category,
    /// The agent that triggered the scan, from the child's `--consumer` flag.
    /// It selects which `expose_*` toggle the route re-enforces at run time, so
    /// it is a *capability selector*, not a label — H-8: narrowed to
    /// [`audit_consumers`] by [`audit_consumer`] before it reaches
    /// [`AuditState::consumer_exposed`](crate::audit::AuditState::consumer_exposed).
    #[serde(default)]
    pub(super) consumer: Option<String>,
    /// The child's working directory (the agent's project), sent for
    /// verification only — the scan always runs against this app's own
    /// launch root. `#[serde(default)]` keeps older children compatible.
    #[serde(default)]
    pub(super) cwd: Option<String>,
    /// V32 C-1b (2026-08-07 review): the cImp tab this child serves
    /// (`cimp --code-audit-mcp --tab <id>`), resolved to the tab's taint latch
    /// so a contaminated conversation cannot run a local scanner.
    ///
    /// H-8 (2026-08-08 re-review): **required in practice.** It stays
    /// `Option` on the wire — a missing field must produce the route's readable
    /// refusal, not serde's 400 — but [`audit_tab`] refuses a body without it.
    /// The fail-open anonymous scope the other identity-taking routes keep is
    /// not available here: this route's whole gate keys on tab identity, so
    /// "no tab" meant "no containment", silently.
    #[serde(default)]
    pub(super) tab: Option<String>,
}

/// The consumers that legitimately POST `/audit/run` (H-8).
///
/// Empirically the complete set — there are exactly two spawn sites for the
/// `cimp --code-audit-mcp` child, and no other caller exists:
///
/// - Claude: `tabs::config::build_pre_args` emits
///   `[--code-audit-mcp, --tab, <id>]` with **no** `--consumer`, so the child's
///   own default (`audit::mcp::CONSUMER`, `"claude"`) goes on the wire. Pinned
///   by `tabs::config::tests::the_code_audit_child_carries_its_own_tab_id`.
/// - OpenCode: `tabs::config::build_opencode_config` emits
///   `[<exe>, --code-audit-mcp, --consumer, opencode, --tab, <id>]`.
///
/// `"offload"` is deliberately **absent**, even though
/// [`CodeAuditSettings::expose_offload`](crate::settings::schema::CodeAuditSettings)
/// exists: the offload worker is an *in-process* consumer of the audit surface
/// and never speaks to this route. `offload::tools::audit_tools::execute` calls
/// [`audit::mcp::run_audit`](crate::audit::mcp::run_audit) directly through
/// `audit::global()`, gated by `OffloadService::run_on` (`enabled` AND
/// `expose_offload` AND a local backend) and re-gated by `HostRouter::call`.
/// `CodeAuditSettings::mcp_exposed` states the same split from the other side
/// ("`expose_offload` is deliberately absent: the offload worker runs
/// in-process"). So `expose_offload` — which defaults **true** — was reachable
/// over HTTP only by a caller that no legitimate component ever is.
/// The consumers `/audit/run` serves — **the registry**, not a literal pair
/// (V40 Phase A). A harness added without a line here used to be refused by a
/// route it is entitled to, with a message naming two products it isn't one of.
pub(super) fn audit_consumers() -> Vec<&'static str> {
    crate::harness::registry::harness_ids()
}

/// H-8: narrow `/audit/run`'s caller-asserted `consumer` to [`audit_consumers`]
/// at the parse boundary, returning the `&'static str` the rest of the route
/// uses. Same discipline `ada4bae` gave `/run`'s `tool` label
/// ([`offload_tool_name`]) and for a stronger reason: `tool` is only a label,
/// whereas this value **selects which `expose_*` toggle is checked**.
///
/// Absent/blank still means `"claude"`, which is the child's own default and
/// the pre-H-8 documented behaviour; only *unrecognized* values are refused.
/// (A child old enough to omit `consumer` predates `--tab` and is already
/// refused by [`audit_tab`], so the default is compatibility, not a hole.)
pub(super) fn audit_consumer(raw: Option<&str>) -> Result<&'static str, String> {
    let raw = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            crate::harness::DEFAULT_HARNESS
                .id()
                .expect("DEFAULT_HARNESS names a registered harness")
        });
    let lower = raw.to_ascii_lowercase();
    audit_consumers()
        .into_iter()
        .find(|c| *c == lower)
        .ok_or_else(|| {
            format!(
                "code audit does not serve the consumer {raw:?} - this route serves the \
                 cimp-code-audit MCP child only ({})",
                audit_consumers().join(", ")
            )
        })
}

/// H-8: `/audit/run` requires a tab identity — a request without one is
/// **refused**, never treated as clean.
///
/// Both spawn paths have sent `--tab` since V32 C-1b (see [`audit_consumers`]),
/// so the only bodies this rejects are a hand-run child, a forged request, and
/// a *stale* child left over from a pre-C-1b build — which is why the message
/// names the remedy (restart the tab) rather than the symptom.
///
/// Trimming/emptiness is checked here rather than left to [`tab_identity`]
/// because `""` and `"   "` are exactly the shapes a caller would use to opt
/// itself back out of the gate.
pub(super) fn audit_tab(raw: Option<&str>) -> Result<&str, String> {
    raw.map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| {
        "this code-audit MCP connection carries no cImp tab id, so the scan cannot be checked \
         against that tab's containment latch — restart this tab in cImp (its MCP child is from \
         an older build) and try again."
            .to_string()
    })
}

/// Everything `/audit/run` decides **before** the scan starts — all four
/// refusals, in the one order they may be taken — returning the validated
/// consumer on success. `Err(msg)` is the single `RunResult { ok: false, error }`
/// the route writes over HTTP 200, always *before* [`write_ndjson_head`].
///
/// It is one function, taking its dependencies as arguments, for two reasons:
/// the caller cannot reach [`LatchRegistry::gate`] without passing every check
/// first (so an added refusal cannot be inserted on the wrong side of the
/// gate), and the ordering below is testable without a `TcpStream` or an
/// `AppHandle`.
///
/// **Why this order** (each step's own note says what it decides):
///
/// 1. `consumer` — H-8. A *parse-boundary narrowing*: it must precede step 2
///    because it is what step 2 is keyed by. Engages nothing.
/// 2. `expose` — the per-consumer run-time re-gate. Kept ahead of the identity
///    and containment checks so a consumer the user has opted out still gets
///    the specific "not exposed" error rather than a containment refusal that
///    would not explain its situation. Engages nothing.
/// 3. `cwd` — the wrong-instance guard. Same reasoning: a misrouted request was
///    never going to run here, and its own error is the actionable one.
///    Engages nothing.
/// 4. `tab` — H-8. The identity half of the gate below, so it sits immediately
///    before it and shares its "a request that was never going to run does not
///    engage this tab's latch" property. Engages nothing: the refusal happens
///    before any [`LatchScope`] exists.
/// 5. the taint gate — the only step that may touch the registry, and therefore
///    last, exactly as V32 C-1b established.
pub(super) fn audit_admit(
    reg: &LatchRegistry,
    body: &AuditRunBody,
    served_root: &Path,
    exposed: impl FnOnce(&str) -> bool,
    scope_of: impl FnOnce(&'static str, &str) -> LatchScoping,
    policy_of: impl FnOnce(Option<&LatchScope>) -> GatePolicy,
) -> Result<&'static str, String> {
    // 1. H-8: the caller-asserted `consumer` is narrowed to one of two known
    //    values before anything reads it — see `audit_consumer`.
    let consumer = audit_consumer(body.consumer.as_deref())?;

    // 2. Re-enforce this consumer's expose toggle at run time (see the route's
    //    doc comment): a still-registered child whose consumer has since been
    //    opted out gets a clean tool error, not a scan.
    if !exposed(consumer) {
        return Err(format!(
            "code audit is not exposed to {consumer} — re-enable it in cImp Settings → Code Audit"
        ));
    }

    // 3. Wrong-instance guard: the scan always runs against THIS app's launch
    //    root, so a child whose cwd falls outside it was misrouted (stale or
    //    foreign discovery entry — possible with several cImp instances off one
    //    install). A clean error beats silently auditing the wrong project.
    //
    //    **This is a routing guard, not a boundary, and must not be read as
    //    one** (V33, recorded so the asymmetry with `/context/post_edit` is not
    //    re-raised): `cwd` is `#[serde(default)]` for older children, so step 3
    //    is skipped outright when the field is absent — a caller holding the
    //    loopback token opts out of it by saying nothing. Nothing is gained by
    //    doing so either: passing this check does not choose what gets scanned;
    //    `served_root` does, and it comes from the app. What the two routes DO
    //    share is the path comparison itself, and its `..` refusal now lives in
    //    `is_ancestor_or_equal` so neither route can drift from the other.
    if let Some(child_cwd) = body.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let served = canon(served_root);
        if !is_ancestor_or_equal(&served, &canon(Path::new(child_cwd))) {
            return Err(format!(
                "this cImp instance serves {} — launch cImp in {} (or close the other instance) to audit it",
                served.display(),
                child_cwd
            ));
        }
    }

    // 4. H-8: identity is REQUIRED here. Before this, `tab` was caller-supplied
    //    and optional, absence resolved to `LatchScoping::Anonymous`, and
    //    `gate(None, ..)` returned `Ok(Clean)` without classifying anything —
    //    i.e. the whole containment gate below was opt-in by the caller it was
    //    meant to contain, and opting out was silent.
    let tab = audit_tab(body.tab.as_deref())?;

    // 5. V32 C-1b: the taint gate — the last thing checked before the scan
    //    starts. Identity is resolved through the same `latch_scope` funnel
    //    every other gated route uses (`scope_of`) rather than a route-local
    //    check that can drift from it, and an unknown tab id still yields no
    //    scope and so keys no registry entry: #45's bound.
    let tool = crate::audit::mcp::tool_name_for(body.category);
    let scoping = scope_of(crate::graph::source_for_consumer(consumer), tab);
    let scope = scoping.scope();
    if scope.is_none() {
        // H-8: **a containment gate that does not apply is never silent.** The
        // surviving no-scope case is `Unknown` (an id naming no configured tab
        // — a re-id'd or removed tab, or a forged id); `Anonymous` is refused
        // at step 4 and can no longer arrive here, but this is written over
        // `scope.is_none()` rather than over the `Unknown` variant so that a
        // future variant, or a regression in step 4, still warns instead of
        // passing through unremarked.
        //
        // Not a refusal: refusing here would break a running child whose tab
        // was re-id'd under it, and V28's honest fallback for "an identity we
        // cannot resolve" on a TOOL route is fail-open. But it is the case
        // where containment does not apply, so it is stated in the log rather
        // than left to be inferred from a missing row.
        warn!(
            target: "offload",
            consumer = %consumer,
            tab = %tab,
            tool = %tool,
            "loopback: /audit/run has no configured tab to latch against — scan is ungated"
        );
    }
    let policy = policy_of(scope);
    reg.gate(
        scope,
        LatchRoute::Native,
        tool,
        policy,
        CallProvenance::internal(),
    )
    .map(|_| consumer)
    .map_err(str::to_string)
}

/// `POST /audit/run` (V26): run one full code-audit scan of the requested
/// category to completion and **stream** the reply as newline-delimited JSON —
/// periodic [`HEARTBEAT_LINE`]s while the (possibly minutes-long) scan runs,
/// then exactly one final `RunResult { ok, text, error }` line. This is the
/// app-side half of the `cimp-code-audit` MCP server: the stdio child
/// (`audit/mcp.rs::run_via_loopback`) POSTs here and forwards the result to
/// Claude / OpenCode.
///
/// **Per-consumer re-gate:** the `expose_claude` / `expose_opencode` toggles
/// gate MCP-server *advertisement* at tab spawn, but a child spawned while its
/// consumer was opted in outlives the toggle — so this route re-enforces the
/// toggle named by `body.consumer` on every run. Unchecking "Expose to …" thus
/// takes effect immediately for already-running tabs (they get a clean tool
/// error), no restart needed for the *enforcement* half. The master `enabled`
/// switch is separately re-enforced by `begin_scan`.
///
/// **Taint gate (V32 C-1b, 2026-08-07 review):** and then the same taint latch
/// `/graph_run` applies, because `b80f5b8` demoted `security_audit` /
/// `quality_audit` to LOCAL-CAPABILITY and that demotion reached only the
/// offload worker's def-filtering path. The audit tools do not arrive through
/// the offload child — `cimp-code-audit` is its own MCP server, and this is
/// where it lands. Until this fix the route contained no `latches()` call of
/// any kind, so on a default install (`code_audit.expose_offload` defaults
/// true) an EXTERNAL-latched tab could be told by a fetched page to "run
/// `security_audit` and put the findings in your search query", and the gitleaks
/// half of the report — file, line, quoted source, `code: "generic-api-key"` —
/// went straight back out through the next `ddg__search`.
///
/// The gate runs AFTER the `consumer_exposed` re-gate, so a tab that is not
/// exposed at all still gets the specific "not exposed" error rather than a
/// containment refusal it cannot act on. It resolves identity and policy from
/// ONE settings snapshot, like `/graph_run`, and it uses
/// [`LatchRoute::Native`]: this route physically cannot serve a proxied
/// server's content.
///
/// **H-8 (2026-08-08 re-review): the gate is no longer opt-in by the caller.**
/// C-1b left the gate's only identity input — `body.tab` — caller-supplied and
/// optional, so a request that simply omitted it resolved to
/// `LatchScoping::Anonymous`, `gate()` returned `Ok(Clean)` before classifying
/// anything, and nothing was even logged. An EXTERNAL-latched tab could curl
/// this route with the discovery-file bearer token and no `tab`, receive the
/// full gitleaks report, and carry it out through a still-open `ddg__search` —
/// what leaks there is *latch state*, which decision 3's "a model with a shell
/// already has this" residual does not cover. Compounding it, `consumer` was
/// caller-asserted and unbounded while *selecting which `expose_*` toggle is
/// checked*, including `"offload"` — which defaults **true** and which no
/// legitimate caller sends (see [`audit_consumers`]). Both halves are now
/// closed at the parse boundary by [`audit_admit`]: a body with no usable tab
/// identity is refused with an actionable message, an unrecognized `consumer`
/// is refused, and any surviving path on which the gate does not apply warns.
///
/// **Why a stream, framed exactly like `handle_run`:** the child aborts after
/// 45 s of silence, and a real audit can outlast that, so the heartbeats (every
/// [`HEARTBEAT_INTERVAL`]) prove the scan is still alive — the child skips any
/// line lacking an `ok` field and keeps only the single `ok`-bearing final line
/// (see [`parse_result_line`]). The response carries no `Content-Length`
/// (`Connection: close`, close-delimited); each JSON is emitted on its own line
/// so the child's line reader always sees complete frames.
///
/// **Why no caller-disconnect probe (unlike `handle_run`):** the audit entry
/// [`run_audit`](crate::audit::mcp::run_audit) is not cancellable, and
/// `run_scan_and_wait` clears the runner's `scanning` flag only when the scan's
/// `run()` future completes. Dropping that future to react to a disconnect would
/// wedge the runner in `scanning`, so instead a failed heartbeat write (the
/// caller-gone signal) drains the scan to completion off the wire and discards
/// the unsendable result — the runner ends clean either way. There is also no
/// llama-server slot to free promptly, which is the only reason `handle_run`
/// probes at all.
///
/// Tool-level failures (master switch off, no tools enabled, `"a scan is already
/// in progress"`) flow through as the final `{ok:false, error}` line over HTTP
/// 200 — the child renders them as a readable tool error, mirroring
/// [`handle_graph_run`]. Only a malformed body is a 400.
pub(super) async fn handle_audit_run(stream: &mut TcpStream, ctx: &RouteCtx, req: &Request) -> AppResult<()> {
    // Malformed body / unknown category → 400 (the child treats any
    // non-200 as a hard failure), mirroring `handle_graph_run`.
    let Some(body) = decode::<AuditRunBody, _>(stream, req, bad_body_result).await? else {
        return Ok(());
    };
    let category = body.category;

    // Resolve the runner from managed state at request time (robust to the
    // audit-vs-loopback startup order). `main.rs` manages it as `Arc<AuditState>`
    // (and publishes the same handle via `audit::set_global`). Not ready → a
    // single `ok:false` line over 200, same shape as `handle_graph_run`.
    let state = match ctx.audit() {
        Some(s) => s,
        None => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some("audit service not ready".into()),
            };
            return write_json(stream, 200, &r).await;
        }
    };

    // Every pre-scan check, in the one order they may be taken, over ONE
    // settings read for identity + policy (the `/mcp/call` discipline) — see
    // [`audit_admit`], which owns the ordering rationale and the four refusal
    // messages.
    let settings = ctx.settings();
    let consumer = match audit_admit(
        latches(),
        &body,
        &state.root(),
        |c| state.consumer_exposed(c),
        |agent, tab| latch_scope(ctx.app(), &settings, agent, Some(tab)),
        |scope| GatePolicy::resolve(&settings, scope),
    ) {
        Ok(c) => c,
        Err(msg) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(msg),
            };
            // 200 with `ok:false`, like every other tool-level error on this
            // route: the child renders `error` as the tool result. Sent BEFORE
            // `write_ndjson_head`, so this is a plain single-JSON body — which
            // the child's line reader already handles (`parse_result_line` over
            // the unterminated trailing line). A refusal written after the
            // ndjson head would corrupt the stream, so every refusal this route
            // can take is funnelled through this one arm.
            return write_json(stream, 200, &r).await;
        }
    };

    write_ndjson_head(stream, "audit").await?;

    // Run the scan concurrently with the heartbeat interval: whichever branch
    // fires, `run_audit` still owns clearing `scanning`.
    let run_fut = crate::audit::mcp::run_audit(&state, category, consumer, body.tab.as_deref());
    tokio::pin!(run_fut);

    let mut beat = tokio::time::interval(HEARTBEAT_INTERVAL);
    beat.tick().await; // consume the immediate first tick

    let result = loop {
        tokio::select! {
            biased;
            r = &mut run_fut => break r,
            _ = beat.tick() => {
                // A failed heartbeat write means the caller went away. Stop
                // beating, but drain the (uncancellable) scan to completion so
                // the runner leaves `scanning` — then drop the unsendable result.
                if stream.write_all(HEARTBEAT_LINE).await.is_err() {
                    debug!("audit loopback: heartbeat write failed; caller gone, draining scan");
                    let _ = (&mut run_fut).await;
                    return Ok(());
                }
                stream.flush().await.ok();
            }
        }
    };

    let r = match result {
        // #48 M-6: the report crosses the delivery boundary here — screened,
        // enveloped under the scanner preamble, headered if a layer fired. It is
        // a `RawReport`, not a `String`, so this call cannot be dropped by
        // omission (see `audit::mcp::RawReport`).
        //
        // Settings are re-read rather than reusing the `settings` snapshot
        // `audit_admit` gated on: a scan can run for minutes, and the envelope is
        // resolved **once per delivery** for exactly the reason
        // `spotlight::recall_envelope` is — the posture that applies is the one
        // in force when the text enters the conversation, not the one in force
        // when the scan was admitted.
        Ok(report) => RunResult {
            ok: true,
            text: Some(
                report
                    .deliver(crate::audit::mcp::Delivery {
                        settings: &ctx.settings(),
                        scope: crate::settings::injection::Scope::for_tab(
                            crate::graph::source_for_consumer(consumer),
                            body.tab.as_deref(),
                        ),
                    })
                    .await,
            ),
            error: None,
        },
        // Busy / disabled / no-tools errors intentionally arrive here as
        // `ok:false` — a tool result the child surfaces, not a protocol failure.
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(e),
        },
    };
    write_result_line(stream, &r, "audit").await
}

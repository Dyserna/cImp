//! V8-01 offload agent loop — the OpenAI-compatible conversation the
//! local model drives against `llama-server`.
//!
//! Posts `messages` + the aggregated `tools` with `tool_choice: "auto"`,
//! routes each model `tool_call` to its executor (native tools today;
//! MCP-server tools layer in via [`ToolRouter`] in Phase C), token-caps
//! every tool result, and loops until a final assistant message or a cap
//! is hit. `<think>` blocks are stripped from the returned text.
//!
//! Bounded by three caps — `max_steps`, a wall-clock `deadline` (which
//! the caller sets from `offload_timeout_secs`), and the per-slot token
//! budget. On any cap the loop forces a final-synthesis turn ("answer
//! from what you have now") rather than truncating mid-thought.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{debug, warn};

use crate::error::{AppError, AppResult};

use crate::settings::ToolScope;

use tokio_util::sync::CancellationToken;

use super::metrics::CallRecord;
use super::openai::{
    strip_think, ChatChunk, ChatMessage, ChatRequest, ChatResponse, StreamAccumulator, ToolDef,
};
use super::tools::{self, ToolCtx};

/// Accumulates a [`CallRecord`] per LLM call as the loop runs, for the Offload
/// Server tab's run log. The caller owns it and passes `&mut` in, so the calls
/// survive even when the run ends in an error; the service then finalizes a
/// `RunRecord` from it. `None` is passed by the headless child / self-test
/// paths that don't feed the dashboard.
#[derive(Default)]
pub struct RunTrace {
    pub calls: Vec<CallRecord>,
}

/// Classify a turn for the run log: step 0 is the plan, the forced-final
/// synthesis is `"final"`, everything else is tool `"ingestion"`.
fn call_kind(step: u32, is_final: bool) -> &'static str {
    if is_final {
        "final"
    } else if step == 0 {
        "planning"
    } else {
        "ingestion"
    }
}

/// Per-call thinking policy. Opus sets this per `offload_task` from the
/// task's shape; `Auto` lets the loop think only on the turns that
/// matter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkingMode {
    /// Think only on the planning (first) and final-synthesis turns;
    /// suppress on routine tool-ingestion turns.
    Auto,
    /// Never think (cheapest; for deterministic extract/list/lookup).
    Off,
    /// Always think (for genuine analysis).
    On,
}

impl ThinkingMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "off" => ThinkingMode::Off,
            "on" => ThinkingMode::On,
            _ => ThinkingMode::Auto,
        }
    }
}

/// Routes a model `tool_call` to its owner. The native baseline
/// implements this directly; Phase C's MCP host wraps it to also reach
/// the user's tool servers. Lets the loop stay agnostic to where a tool
/// lives.
#[async_trait]
pub trait ToolRouter: Send + Sync {
    /// The full tool surface advertised to the model this call.
    fn tool_defs(&self) -> Vec<ToolDef>;
    /// Execute `name(args)`; errors become a `role: tool` error message
    /// (the loop continues — a tool failure is not fatal).
    async fn call(&self, name: &str, args: serde_json::Value) -> Result<String, String>;
}

/// Native-only router: the `read_file`/`code_search`/`run_command`
/// baseline, **scoped to the chosen backend's allow-list** (V8-02). Only
/// tools the backend's [`ToolScope`] permits are advertised to the model,
/// and a disallowed call is refused even if the model asks for it — the
/// defense-in-depth half of the cloud-privacy guarantee (the router half
/// is in [`super::router`]).
pub struct NativeRouter {
    pub defs: Vec<ToolDef>,
    pub ctx: ToolCtx,
    /// The chosen backend's tool scope. `All` for local/LAN; a cloud
    /// backend's scope denies the local-data tools.
    pub scope: ToolScope,
}

impl NativeRouter {
    /// Build a native router whose advertised tools are filtered through
    /// `scope`. (Construct the `defs` from `tools::enabled_defs` first.)
    pub fn new(defs: Vec<ToolDef>, ctx: ToolCtx, scope: ToolScope) -> Self {
        Self { defs, ctx, scope }
    }
}

#[async_trait]
impl ToolRouter for NativeRouter {
    fn tool_defs(&self) -> Vec<ToolDef> {
        self.defs
            .iter()
            .filter(|d| self.scope.allows_namespaced(&d.function.name))
            .cloned()
            .collect()
    }
    async fn call(&self, name: &str, args: serde_json::Value) -> Result<String, String> {
        // Refuse a disallowed tool even if the model requests it (e.g. a
        // cloud backend that hallucinates a `read_file` call) — the local
        // file is never read for an out-of-scope backend.
        if !self.scope.allows_namespaced(name) {
            return Err(format!(
                "tool `{name}` is not available on this backend (denied by its tool scope)"
            ));
        }
        tools::dispatch(name, args, &self.ctx).await
    }
}

/// V8-03 host-aware router: the native baseline **plus** the warm MCP host's
/// namespaced tools, all filtered through the chosen backend's [`ToolScope`].
/// The merged tool surface is computed once at construction (the warm pool is
/// reconciled before the call, so the set is stable for the loop's duration);
/// `call` dispatches native names locally and namespaced `<server>__<tool>`
/// ids to the owning MCP server. The scope is re-checked on every call as the
/// defense-in-depth half of the cloud-privacy guarantee.
pub struct HostRouter {
    /// Merged native + MCP tool defs, already scope-filtered.
    defs: Vec<ToolDef>,
    ctx: ToolCtx,
    host: std::sync::Arc<super::mcp_host::McpHost>,
    scope: ToolScope,
    /// V9-01: whether the code-graph (`graph_*`) tools may run here. The caller
    /// computes this from the feature flag + the local/remote opt-in; we re-gate
    /// dispatch on it as defense-in-depth so a hallucinated `graph_*` call on a
    /// non-opted-in remote backend can't reach the local index.
    allow_graph: bool,
}

impl HostRouter {
    /// Build the merged, scope-filtered router. `native_defs` are the enabled
    /// native tool defs; `mcp_defs` are the host's namespaced read-class
    /// tools (`McpHost::tool_defs().await`). `allow_graph` gates the `graph_*`
    /// tools (already reflected in `native_defs` by the caller).
    pub fn new(
        native_defs: Vec<ToolDef>,
        mcp_defs: Vec<ToolDef>,
        ctx: ToolCtx,
        host: std::sync::Arc<super::mcp_host::McpHost>,
        scope: ToolScope,
        allow_graph: bool,
    ) -> Self {
        let defs: Vec<ToolDef> = native_defs
            .into_iter()
            .chain(mcp_defs)
            .filter(|d| scope.allows_namespaced(&d.function.name))
            .collect();
        Self { defs, ctx, host, scope, allow_graph }
    }
}

#[async_trait]
impl ToolRouter for HostRouter {
    fn tool_defs(&self) -> Vec<ToolDef> {
        self.defs.clone()
    }
    async fn call(&self, name: &str, args: serde_json::Value) -> Result<String, String> {
        if !self.scope.allows_namespaced(name) {
            return Err(format!(
                "tool `{name}` is not available on this backend (denied by its tool scope)"
            ));
        }
        // V9-01: re-gate the code-graph tools (defense-in-depth) — a remote
        // backend the user didn't opt in must never reach the local index.
        if name.starts_with("graph_") && !self.allow_graph {
            return Err(format!(
                "tool `{name}` is not available on this backend (code-graph access for a remote \
                 offload worker is off — enable it in ccImp Settings → Code Graph)"
            ));
        }
        // Namespaced ids (`<server>__<tool>`) belong to an MCP server; bare
        // names are the native baseline.
        if name.contains("__") {
            self.host.call(name, args).await
        } else {
            tools::dispatch(name, args, &self.ctx).await
        }
    }
}

/// Static loop configuration derived from `OffloadSettings` + the
/// discovered per-slot budget.
pub struct AgentConfig {
    /// HTTP origin of the server, e.g. `http://127.0.0.1:8080`.
    pub base_url: String,
    /// Optional model alias (llama-server ignores it, but some proxies
    /// require it).
    pub model: Option<String>,
    pub max_steps: u32,
    /// Per-slot working token budget `n_ctx * high_water`; the loop compacts
    /// when usage crosses it. `None` if undiscovered (no compaction, rely on
    /// `max_steps`/deadline).
    pub budget_tokens: Option<u32>,
    /// The backend's discovered per-slot context window. Used to reserve
    /// generation headroom (the effective compaction budget is held below this)
    /// and to bound each request's `max_tokens` so output can't run past the
    /// slot. `None` if undiscovered.
    pub n_ctx: Option<u32>,
    /// The backend's parallel slot count (`-np`). When >1, concurrent slots
    /// share the GPU and per-slot generation slows, so the output time-cap
    /// scales the measured rate down by [`slot_rate_factor`] to stay safe even
    /// if another slot becomes busy mid-request.
    pub slots: u32,
    /// Per-tool-result cap in tokens (approximated as bytes/4).
    pub per_tool_result_token_cap: u32,
    /// Optional bearer token for the backend (cloud APIs). `None` for a
    /// local/LAN llama-server.
    pub auth_token: Option<String>,
    /// Fixed wall-clock timeout granted to **each** llama-server request (a
    /// single step, or the forced-final synthesis). Set from
    /// `offload_timeout_secs`. Every call gets the same generous window — the
    /// loop no longer shrinks it as the job's overall `deadline` approaches.
    /// The total job is still bounded: `deadline` gates whether a *new* step
    /// starts, and the heartbeat-streamed loopback (see `loopback.rs` /
    /// `mcp.rs`) lets the proxy wait out a long-but-live job instead of
    /// abandoning + re-running it, so a fixed per-call timeout is safe.
    pub per_call_timeout: Duration,
}

/// The task to run.
pub struct OffloadTask {
    pub instructions: String,
    pub context: Option<String>,
    pub thinking: ThinkingMode,
}

const SYSTEM_PROMPT: &str = "You are a local offload worker. You are given a self-contained subtask \
by a more capable orchestrator. Use the available tools to gather what you need, then return a \
single concise, complete answer — the orchestrator sees ONLY your final message, not your \
intermediate tool calls or reasoning. Be specific and include concrete references (file paths, \
line numbers, names) when relevant. Do not ask clarifying questions; make reasonable assumptions \
and state them.";

/// Cap a tool result to `cap_tokens`, appending a truncation marker so
/// the model knows it was cut and narrows/paginates.
fn cap_result(result: String, cap_tokens: u32) -> String {
    let cap_bytes = (cap_tokens as usize).saturating_mul(4);
    if result.len() <= cap_bytes {
        return result;
    }
    let mut cut = cap_bytes.min(result.len());
    while cut > 0 && !result.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = result[..cut].to_string();
    out.push_str("\n[result truncated — refine your query or page through it]");
    out
}

/// Whether to think on a given turn under the policy.
fn think_on_turn(mode: ThinkingMode, is_planning: bool, is_final: bool) -> bool {
    match mode {
        ThinkingMode::On => true,
        ThinkingMode::Off => false,
        ThinkingMode::Auto => is_planning || is_final,
    }
}

/// Context a slot must keep free for the model's own output, so a
/// thinking-heavy turn can't push `prompt + generation` past `n_ctx` and drop
/// the stream. A quarter of the window (floored), which scales: tight on a
/// halved `-np 2` slot, generous at `-np 1`.
fn gen_reserve(n_ctx: u32) -> u32 {
    (n_ctx / 4).max(8192)
}

/// The effective prompt budget the loop compacts against: the configured
/// high-water budget, but never so high that it leaves no room for generation.
/// Capping it below `n_ctx` by [`gen_reserve`] is the "lower budget" guard —
/// it bounds the prompt regardless of the user's high-water percentage.
fn compaction_budget(cfg: &AgentConfig) -> Option<u32> {
    match (cfg.budget_tokens, cfg.n_ctx) {
        (Some(b), Some(n)) => Some(b.min(n.saturating_sub(gen_reserve(n)))),
        (b, _) => b,
    }
}

/// Conservative default generation rate (tokens/sec) for the first request,
/// before the server has reported its real throughput. Sized low (for a slow
/// dense model) so the opening turn can't overrun its budget; subsequent turns
/// use the measured rate.
const DEFAULT_GEN_TPS: f32 = 45.0;

/// Fraction of a request's time budget the output may consume; the remainder
/// covers prompt prefill and network/scheduling slack.
const GEN_TIME_FRACTION: f32 = 0.6;

/// Per-slot throughput falloff as slots are added to one GPU. Calibrated so two
/// slots run at ~0.71 of single-slot speed (measured: ~160 tok/s → ~115 tok/s),
/// i.e. `ALPHA ≈ 0.4`. Higher = steeper falloff.
const CONTENTION_ALPHA: f32 = 0.4;

/// Fraction of single-slot generation speed a slot achieves when `n_slots`
/// share the GPU. `1.0` for one slot; sublinear (not `1/N`) decay as slots are
/// added, because batched decoding shares the per-step overhead — so two slots
/// are far better than half-speed. Generalizes to any slot count and stays
/// conservative as `N` grows. This is a GPU/batching property; the model's
/// absolute speed is measured separately ([`output_token_cap`]'s `gen_tps`), so
/// scaling the measured rate by this factor sizes output for the worst case
/// (all slots busy) without assuming a specific model.
fn slot_rate_factor(n_slots: u32) -> f32 {
    let n = n_slots.max(1) as f32;
    1.0 / (1.0 + CONTENTION_ALPHA * (n - 1.0))
}

/// Per-request output ceiling (`max_tokens`/`n_predict`): the smaller of the
/// slot's free context and what fits in the request's *time* budget at the
/// model's measured generation rate.
///
/// The context bound stops generation running into the context wall. The time
/// bound is what stops a slow thinking turn from running past the deadline and
/// dropping the stream — and because it's derived from the server-reported
/// tokens/sec, it adapts to the model: a fast MoE (e.g. ~170 tok/s) gets a high
/// cap, a slow dense model (~50 tok/s) a low one, both sized to finish in time.
/// `req_timeout` is this request's share of the deadline; `gen_tps` is the last
/// measured rate (or [`DEFAULT_GEN_TPS`] on the first turn).
fn output_token_cap(cfg: &AgentConfig, req_timeout: Duration, gen_tps: f32) -> Option<u32> {
    // Scale the measured rate down for slot contention, so the cap stays safe
    // even if another slot becomes busy mid-request (single slot → no change).
    let tps = (gen_tps * slot_rate_factor(cfg.slots)).max(1.0);
    let time_cap = (req_timeout.as_secs_f32() * tps * GEN_TIME_FRACTION).max(256.0) as u32;
    match cfg.n_ctx {
        Some(n) => {
            // With no configured high-water budget, reserve generation headroom
            // directly (`gen_reserve`). Falling back to `n` here would make
            // `n - n = 0` and collapse the cap to the 512 floor, starving output.
            let prompt_budget =
                compaction_budget(cfg).unwrap_or_else(|| n.saturating_sub(gen_reserve(n)));
            let ctx_cap = n.saturating_sub(prompt_budget).saturating_sub(512).max(512);
            Some(ctx_cap.min(time_cap))
        }
        // Without a known slot size we still bound generation by time.
        None => Some(time_cap),
    }
}

/// A "final answer" that is actually a tool call the server failed to parse
/// (the chat template's `<tool_call><function=…>` XML leaked into `content`
/// instead of the structured `tool_calls` field). Returning it verbatim would
/// hand Opus raw markup; the loop instead forces a real final answer. Keyed on
/// a leading marker to avoid flagging prose that merely mentions the syntax.
fn looks_like_leaked_tool_call(answer: &str) -> bool {
    let t = answer.trim_start();
    t.starts_with("<tool_call>") || t.starts_with("<function=")
}

/// Run the agent loop and return the synthesized final answer (with
/// `<think>` stripped). `deadline` bounds the loop wall-clock; on expiry
/// (or `max_steps`/budget) it forces a final-synthesis turn.
pub async fn run(
    client: &reqwest::Client,
    cfg: &AgentConfig,
    router: &dyn ToolRouter,
    task: OffloadTask,
    deadline: Instant,
    mut trace: Option<&mut RunTrace>,
    cancel: &CancellationToken,
) -> AppResult<String> {
    let url = format!("{}/v1/chat/completions", cfg.base_url);
    let tools = router.tool_defs();

    let user = match &task.context {
        Some(c) if !c.is_empty() => format!("{}\n\n# Context\n{}", task.instructions, c),
        _ => task.instructions.clone(),
    };
    let mut convo = Convo::new(SYSTEM_PROMPT, user);
    // Measured generation rate (tokens/sec), refreshed from each response's
    // server `timings` and used to size the next request's output budget.
    let mut gen_tps = DEFAULT_GEN_TPS;

    for step in 0..cfg.max_steps {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            debug!("offload: deadline reached at step {step}; forcing final synthesis");
            if let Some(budget) = compaction_budget(cfg) {
                convo.compact(budget);
            }
            return force_final(
                client, &url, cfg, convo.flatten(), task.thinking, gen_tps, step,
                trace.as_deref_mut(), cancel,
            )
            .await;
        }
        let is_planning = step == 0;
        let enable_thinking = think_on_turn(task.thinking, is_planning, false);

        // Each call gets the full, fixed `per_call_timeout` — the loop no longer
        // shrinks it toward the deadline. `deadline` above gates whether a *new*
        // step starts; an in-flight call is allowed its whole window (a heavy
        // thinking turn must prefill the accumulated prompt before generating,
        // which a shrinking remainder would starve). The heartbeat-streamed
        // loopback waits out the longer total instead of abandoning the job, so
        // a fixed window is safe (see `loopback.rs` / `mcp.rs`).
        convo.mark_sent();
        let call_started = Instant::now();
        let resp = post_chat(
            client, &url, cfg, &convo.flatten(), &tools, enable_thinking, true,
            cfg.per_call_timeout, gen_tps, cancel,
        )
        .await?;
        let call_dur = call_started.elapsed();
        let usage = resp.usage;
        // Refresh the throughput estimate from what the server actually did, so
        // the next request's output cap tracks the real model speed.
        if let Some(t) = resp.gen_tps {
            if t > 1.0 {
                gen_tps = t;
            }
        }
        let choice = resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Offload("server returned no choices".into()))?;
        let msg = choice.message;

        // Record this call for the run log before the branches consume `msg`.
        let result_class = if !msg.tool_calls.is_empty() {
            format!("tool_calls({})", msg.tool_calls.len())
        } else {
            let a = strip_think(msg.content.as_deref().unwrap_or_default());
            if a.trim().is_empty() {
                "empty".to_string()
            } else if looks_like_leaked_tool_call(&a) {
                "leaked".to_string()
            } else {
                "answer".to_string()
            }
        };
        if let Some(t) = trace.as_deref_mut() {
            t.calls.push(CallRecord {
                step,
                kind: call_kind(step, false).into(),
                thinking: enable_thinking,
                prompt_tokens: usage.map(|u| u.prompt_tokens).unwrap_or(0),
                output_tokens: usage.map(|u| u.completion_tokens).unwrap_or(0),
                duration_ms: call_dur.as_millis() as u64,
                tps: gen_tps,
                result: result_class,
            });
        }

        // Final answer: no tool calls.
        if msg.tool_calls.is_empty() {
            let answer = strip_think(&msg.content.unwrap_or_default());
            if !answer.trim().is_empty() && !looks_like_leaked_tool_call(&answer) {
                return Ok(answer);
            }
            // The model ended its turn with no usable answer. Either it spent
            // the whole turn inside a <think> block that strip_think removed
            // (leaving ""), or it emitted a tool call in the chat template's
            // `<tool_call>` XML form that the server failed to parse into
            // structured `tool_calls`, leaving raw markup in `content`. Either
            // way, returning it as success is wrong. Instead make one
            // forced-final attempt (tools suppressed, "answer now"); that path
            // is guaranteed to return a non-empty string — a real answer, or an
            // explicit "(offload produced no answer …)" placeholder the caller
            // can see — never a bare empty string or leaked markup.
            warn!(
                target: "offload",
                leaked = looks_like_leaked_tool_call(&answer),
                "offload: unusable final answer (empty or leaked tool call); forcing a final answer"
            );
            if let Some(budget) = compaction_budget(cfg) {
                convo.compact(budget);
            }
            return force_final(
                client, &url, cfg, convo.flatten(), task.thinking, gen_tps, step,
                trace.as_deref_mut(), cancel,
            )
            .await;
        }

        // Append the assistant turn (carrying the tool_calls) plus each tool
        // result as one droppable turn.
        let tool_calls = msg.tool_calls.clone();
        let mut turn = vec![msg];
        for call in &tool_calls {
            let args: serde_json::Value = if call.function.arguments.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&call.function.arguments)
                    .unwrap_or_else(|_| serde_json::json!({ "_raw": call.function.arguments }))
            };
            let result = match router.call(&call.function.name, args).await {
                Ok(r) => r,
                Err(e) => format!("ERROR: {e}"),
            };
            let capped = cap_result(result, cfg.per_tool_result_token_cap);
            turn.push(ChatMessage::tool(&call.id, capped));
        }
        convo.push_turn(turn);

        // Budget policing — driven solely by what the server reports. This
        // response's `prompt_tokens` is the *real* token count of the prefix we
        // just sent; recording it attributes a true per-turn cost (the delta
        // from the previous report). Once the measured history reaches the
        // per-slot budget we compact precisely against those real costs. No
        // local character estimate is involved, so the policed size and the
        // slot's actual size can't drift apart.
        // Only feed the budget accounting a *real* measurement: a server that
        // reports `usage:{}` (prompt_tokens == 0) would otherwise record a bogus
        // zero-cost turn and corrupt every later delta. Skip it — the next
        // measured request re-establishes the true prefix cost.
        if let Some(u) = usage {
            if u.prompt_tokens > 0 {
                convo.record(u.prompt_tokens);
            }
        }
        if let Some(budget) = compaction_budget(cfg) {
            if convo.over_budget(budget) {
                warn!(
                    target: "offload",
                    measured = convo.known_total(),
                    budget,
                    "offload: measured prompt at/over budget; compacting"
                );
                convo.compact(budget);
            }
        }
    }

    // Ran out of steps — force a final answer.
    debug!("offload: max_steps reached; forcing final synthesis");
    if let Some(budget) = compaction_budget(cfg) {
        convo.compact(budget);
    }
    force_final(
        client, &url, cfg, convo.flatten(), task.thinking, gen_tps, cfg.max_steps,
        trace.as_deref_mut(), cancel,
    )
    .await
}

/// One streaming chat-completions POST. `with_tools` lets the forced-final
/// turn suppress further tool calls. The response is consumed as an SSE token
/// stream and reassembled into a `ChatResponse`, so callers are unchanged.
///
/// Streaming is what makes cancellation work: if `cancel` fires (or this
/// future is dropped) mid-generation, we stop reading and drop the request,
/// which closes the connection — llama-server detects the disconnect on its
/// next token and aborts, freeing the slot instead of running an orphan to
/// completion.
async fn post_chat(
    client: &reqwest::Client,
    url: &str,
    cfg: &AgentConfig,
    messages: &[ChatMessage],
    tools: &[ToolDef],
    enable_thinking: bool,
    with_tools: bool,
    req_timeout: Duration,
    gen_tps: f32,
    cancel: &CancellationToken,
) -> AppResult<ChatResponse> {
    let req = ChatRequest {
        messages: messages.to_vec(),
        tools: if with_tools { tools.to_vec() } else { Vec::new() },
        tool_choice: if with_tools { Some("auto".into()) } else { Some("none".into()) },
        model: cfg.model.clone(),
        temperature: Some(0.2),
        chat_template_kwargs: Some(serde_json::json!({ "enable_thinking": enable_thinking })),
        stream: Some(true),
        stream_options: Some(serde_json::json!({ "include_usage": true })),
        max_tokens: output_token_cap(cfg, req_timeout, gen_tps),
    };

    // Send and consume with one retry on a transient transport failure —
    // either a connect/send error (a stale pooled keep-alive socket the local
    // server closed between requests, surfacing as "error sending request for
    // url …") OR a mid-stream drop (the streamed body ends early —
    // "error decoding response body" — when a busy single-slot server resets a
    // connection that was contending for the slot). Both are the same transient
    // class the streaming path exposes, so both earn the one retry; without it
    // a single dropped stream fails the whole offload even though a fresh
    // attempt succeeds. NOT retried: timeouts (a timed-out request may still be
    // generating server-side), an HTTP error status (a real rejection such as a
    // context-overflow 4xx won't fix itself), and cancellation. The per-request
    // timeout (`req_timeout`, a fixed per-call window) bounds the whole stream.
    let mut attempt: u8 = 0;
    loop {
        let mut builder = client.post(url).timeout(req_timeout).json(&req);
        if let Some(token) = cfg.auth_token.as_deref().filter(|t| !t.is_empty()) {
            builder = builder.bearer_auth(token);
        }
        let sent = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AppError::Offload("offload cancelled".into())),
            r = builder.send() => r,
        };
        let mut resp = match sent {
            Ok(r) => r,
            Err(e) if attempt == 0 && !e.is_timeout() && (e.is_connect() || e.is_request()) => {
                attempt += 1;
                warn!(
                    target: "offload",
                    error = %e,
                    "chat send failed (transport); retrying once (likely stale pooled connection)"
                );
                continue;
            }
            Err(e) => return Err(AppError::Offload(format!("chat request failed: {e}"))),
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::Offload(format!(
                "server returned {status}: {}",
                body.chars().take(500).collect::<String>()
            )));
        }

        // Consume the SSE stream. On cancel we return and drop `resp`, closing
        // the socket so the server aborts. A mid-stream transport error is
        // captured (not returned) so it can be retried once like a send error;
        // any partial `acc` is discarded — the retry regenerates from scratch.
        let mut acc = StreamAccumulator::default();
        let mut buf: Vec<u8> = Vec::new();
        let stream_err = loop {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AppError::Offload("offload cancelled".into())),
                c = resp.chunk() => c,
            };
            match chunk {
                Ok(Some(bytes)) => {
                    buf.extend_from_slice(&bytes);
                    if drain_sse_lines(&mut buf, &mut acc) {
                        break None; // saw [DONE]
                    }
                }
                Ok(None) => break None, // stream closed without an explicit [DONE]
                Err(e) => break Some(e),
            }
        };
        match stream_err {
            None => return Ok(acc.into_response()),
            Some(e) if attempt == 0 && !e.is_timeout() => {
                attempt += 1;
                warn!(
                    target: "offload",
                    error = %e,
                    "chat stream dropped mid-generation; retrying once (transient slot/connection drop)"
                );
                continue;
            }
            Some(e) => return Err(AppError::Offload(format!("chat stream failed: {e}"))),
        }
    }
}

/// Drain complete `\n`-terminated lines from `buf`, feeding each `data:`
/// payload to `acc`. Leaves any trailing partial line in `buf` for the next
/// chunk. Returns true once the `[DONE]` sentinel is seen. Non-JSON lines
/// (SSE comments / keep-alives) are tolerated and skipped.
fn drain_sse_lines(buf: &mut Vec<u8>, acc: &mut StreamAccumulator) -> bool {
    let mut done = false;
    while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
        let raw: Vec<u8> = buf.drain(..=nl).collect();
        let text = String::from_utf8_lossy(&raw);
        let line = text.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue; // comment, event:, or blank separator line
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            done = true;
            continue;
        }
        if let Ok(chunk) = serde_json::from_str::<ChatChunk>(data) {
            acc.push_chunk(chunk);
        }
    }
    done
}

/// Force a final answer from the conversation so far (tools suppressed).
#[allow(clippy::too_many_arguments)]
async fn force_final(
    client: &reqwest::Client,
    url: &str,
    cfg: &AgentConfig,
    mut messages: Vec<ChatMessage>,
    thinking: ThinkingMode,
    gen_tps: f32,
    step: u32,
    mut trace: Option<&mut RunTrace>,
    cancel: &CancellationToken,
) -> AppResult<String> {
    messages.push(ChatMessage::user(
        "You are out of budget. Stop using tools and answer now, as completely as you can, \
         from what you already have. If your information is partial, say so explicitly.",
    ));
    let enable_thinking = think_on_turn(thinking, false, true);
    // The synthesis gets the same full per-call window as any other request.
    // It runs *after* the loop's `deadline` (often triggered early, by an
    // empty/all-thinking turn or `max_steps`) and must prefill the whole
    // accumulated prompt before generating — so it can't be squeezed into the
    // sliver of deadline that may be left. The heartbeat-streamed loopback
    // keeps the proxy waiting through it (see `loopback.rs` / `mcp.rs`).
    let req_timeout = cfg.per_call_timeout;
    let call_started = Instant::now();
    let resp = post_chat(
        client,
        url,
        cfg,
        &messages,
        &[],
        enable_thinking,
        false,
        req_timeout,
        gen_tps,
        cancel,
    )
    .await?;
    let call_dur = call_started.elapsed();
    let usage = resp.usage;
    let final_tps = resp.gen_tps.filter(|t| *t > 1.0).unwrap_or(gen_tps);
    let content = resp
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .unwrap_or_default();
    let stripped = strip_think(&content);
    let empty = stripped.is_empty();
    if let Some(t) = trace.as_deref_mut() {
        t.calls.push(CallRecord {
            step,
            kind: call_kind(step, true).into(),
            thinking: enable_thinking,
            prompt_tokens: usage.map(|u| u.prompt_tokens).unwrap_or(0),
            output_tokens: usage.map(|u| u.completion_tokens).unwrap_or(0),
            duration_ms: call_dur.as_millis() as u64,
            tps: final_tps,
            result: if empty { "empty".into() } else { "answer".into() },
        });
    }
    if empty {
        // A failed run: the synthesis produced nothing usable (e.g. a thinking
        // turn that ran out of output budget). Surface a typed error so the
        // service can retry a `thinking:on` run once with `auto`, then mark it
        // failed — never a fake-success placeholder.
        Err(AppError::OffloadNoAnswer(
            "offload produced no answer within its budget".into(),
        ))
    } else {
        Ok(stripped)
    }
}

/// One model turn: the assistant message that requested tools plus the tool
/// results answering it. This is the unit compaction drops, and the
/// granularity at which the server's reported prompt size is attributed.
struct Turn {
    msgs: Vec<ChatMessage>,
    /// Real token cost of this turn, derived purely from the difference between
    /// two consecutive server `prompt_tokens` reports. `None` until a request
    /// that *included* this turn has been measured — so the most-recently
    /// appended turn is always `None` until the next round.
    cost: Option<u32>,
    /// True for the synthetic "older context evicted" note, so compaction never
    /// counts it as droppable history and never stacks a second note on it.
    note: bool,
}

/// The agent's running conversation with **server-measured** token accounting.
///
/// Every size decision uses llama-server's reported `prompt_tokens` — the true
/// token count of the messages a request carried — never a local character
/// estimate. Differencing successive reports yields the real cost of each turn
/// appended between them, so compaction trims against ground truth and the
/// policed size can't drift from the slot's actual usage. The cost of the
/// budget is that policing is one step reactive: the turn whose appended tool
/// results first cross the slot is only measured on the *next* request, so a
/// single overshoot can still reach the server — which is why the streaming
/// path retries a dropped stream (see `post_chat`).
struct Convo {
    /// System prompt + the original user task. Never dropped.
    head: Vec<ChatMessage>,
    /// Real token cost of `head`, learned from the first request's report.
    head_cost: Option<u32>,
    turns: Vec<Turn>,
    /// Turn count carried by the most recent request, so the next report's
    /// delta is attributed to the right (newest-but-now-measured) turn.
    sent_turns: usize,
}

impl Convo {
    fn new(system: &str, user: String) -> Self {
        Self {
            head: vec![ChatMessage::system(system), ChatMessage::user(user)],
            head_cost: None,
            turns: Vec::new(),
            sent_turns: 0,
        }
    }

    /// The full wire conversation: head followed by every turn's messages. A
    /// turn always carries its assistant message before its tool replies, so
    /// the flattened list never starts a `tool` message without its owning
    /// assistant — no orphan-tool repair needed.
    fn flatten(&self) -> Vec<ChatMessage> {
        let mut out = self.head.clone();
        for t in &self.turns {
            out.extend(t.msgs.iter().cloned());
        }
        out
    }

    /// Note that the request about to be sent carries the turns present now, so
    /// the report it returns is attributed to this prefix.
    fn mark_sent(&mut self) {
        self.sent_turns = self.turns.len();
    }

    /// Append a model turn (assistant + its tool results) as one droppable unit.
    fn push_turn(&mut self, msgs: Vec<ChatMessage>) {
        self.turns.push(Turn { msgs, cost: None, note: false });
    }

    /// Attribute a server `prompt_tokens` report. `prompt_tokens` is the real
    /// size of `head + turns[0..sent_turns]`; the delta over what we already
    /// knew is the true cost of the turn that prefix newly included (normally
    /// exactly one — the turn appended last round).
    fn record(&mut self, prompt_tokens: u32) {
        if self.sent_turns == 0 {
            // The first request carried head alone, so its report *is* head's
            // real cost. (Only step 0 has no turns; every later request carries
            // at least one.)
            self.head_cost = Some(prompt_tokens);
            return;
        }
        let n = self.sent_turns.min(self.turns.len());
        let known: u32 =
            self.head_cost.unwrap_or(0) + self.turns[..n].iter().filter_map(|t| t.cost).sum::<u32>();
        let delta = prompt_tokens.saturating_sub(known);
        // Attribute to the newest still-unmeasured *real* turn. Skip the synthetic
        // eviction note (`note`): it carries no meaningful token cost of its own,
        // and letting it absorb the delta would both mismeasure the real turn and
        // inflate `known_total`, triggering premature compaction.
        if let Some(t) = self
            .turns[..n]
            .iter_mut()
            .rev()
            .find(|t| t.cost.is_none() && !t.note)
        {
            t.cost = Some(delta);
        }
    }

    /// Real token size of everything measured so far (`head` + every turn with
    /// a known cost). The most-recent, not-yet-measured turn is excluded — it
    /// only adds more, so this is a sound lower bound for the over-budget test.
    fn known_total(&self) -> u32 {
        self.head_cost.unwrap_or(0) + self.turns.iter().filter_map(|t| t.cost).sum::<u32>()
    }

    /// True once the measured history alone reaches `budget`: the next request
    /// (which also carries the unmeasured newest turn) is bound to exceed it.
    fn over_budget(&self, budget: u32) -> bool {
        self.known_total() >= budget
    }

    /// Drop the oldest turns until the measured history fits `budget`, always
    /// keeping `head` and the most recent turns. Uses only server-derived
    /// per-turn costs; a turn whose cost isn't known yet counts as 0 (it can't
    /// be the cause of an over-budget prefix, which is measured) but is still
    /// physically dropped, so progress is guaranteed and the next report
    /// re-measures the trimmed prefix. Leaves a note so the model knows earlier
    /// results were evicted.
    fn compact(&mut self, budget: u32) {
        const KEEP_RECENT_TURNS: usize = 3;
        const NOTE: &str = "[earlier tool results were evicted to stay within the context budget \
                            — re-fetch anything you still need]";
        // Phase 1 — drop the oldest measured turns until the budget fits, always
        // keeping `head` and the most recent turns.
        if self.turns.len() > KEEP_RECENT_TURNS {
            let max_drop = self.turns.len() - KEEP_RECENT_TURNS;
            let mut total = self.known_total();
            let mut n_drop = 0;
            while n_drop < max_drop && total > budget {
                total = total.saturating_sub(self.turns[n_drop].cost.unwrap_or(0));
                n_drop += 1;
            }
            if n_drop > 0 {
                self.turns.drain(..n_drop);
                // Prepend a single eviction note (don't stack one on an existing note).
                if !self.turns.first().is_some_and(|t| t.note) {
                    self.turns.insert(
                        0,
                        Turn { msgs: vec![ChatMessage::user(NOTE)], cost: None, note: true },
                    );
                }
            }
        }
        // Phase 2 (last resort) — the retained turns *still* measure over budget:
        // a single turn carrying several large tool results, which the keep-floor
        // forbids dropping. Hard-truncate the biggest message contents so the
        // prompt fits; without this the oversized prompt reaches the server and is
        // rejected with a context-overflow the loop can't recover from. The
        // server-measured rewrite dropped this fallback the original `compact` had.
        if self.known_total() > budget {
            self.truncate_to_budget(budget);
        }
    }

    /// Hard-truncate the costliest retained turns' message contents until the
    /// measured history fits `budget`. Only reached when dropping whole turns
    /// can't help (the keep-floor). Each pass halves the longest message of the
    /// highest-cost turn and halves that turn's recorded cost to mirror the cut,
    /// so `known_total` converges; the next server report re-measures the trimmed
    /// prefix exactly. A bounded loop guards against pathological inputs.
    fn truncate_to_budget(&mut self, budget: u32) {
        const TRUNC_MARK: &str = "\n…[truncated to fit the context budget]…";
        for _ in 0..64 {
            if self.known_total() <= budget {
                return;
            }
            // The costliest measured, non-note turn — cutting it is what moves
            // `known_total` toward the budget.
            let Some(ti) = self
                .turns
                .iter()
                .enumerate()
                .filter(|(_, t)| !t.note && t.cost.unwrap_or(0) > 1)
                .max_by_key(|(_, t)| t.cost.unwrap_or(0))
                .map(|(i, _)| i)
            else {
                return; // nothing measurable left to cut
            };
            // Halve the longest message content within that turn.
            let mi = self.turns[ti]
                .msgs
                .iter()
                .enumerate()
                .max_by_key(|(_, m)| m.content.as_ref().map_or(0, |c| c.len()))
                .map(|(i, _)| i);
            let mut cut_made = false;
            if let Some(mi) = mi {
                let m = &mut self.turns[ti].msgs[mi];
                let content = m.content.take().unwrap_or_default();
                if content.len() > TRUNC_MARK.len() + 16 {
                    let keep = char_boundary(&content, content.len() / 2);
                    let mut cut = content[..keep].to_string();
                    cut.push_str(TRUNC_MARK);
                    m.content = Some(cut);
                    cut_made = true;
                } else {
                    m.content = Some(content);
                }
            }
            if cut_made {
                if let Some(c) = self.turns[ti].cost.as_mut() {
                    *c = (*c / 2).max(1);
                }
            } else {
                return; // can't shrink further; avoid spinning
            }
        }
    }
}

/// Largest byte index ≤ `idx` that lands on a UTF-8 char boundary — a stable
/// stand-in for the nightly `str::floor_char_boundary`, so slicing never panics.
fn char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_policy_auto() {
        assert!(think_on_turn(ThinkingMode::Auto, true, false)); // planning
        assert!(think_on_turn(ThinkingMode::Auto, false, true)); // final
        assert!(!think_on_turn(ThinkingMode::Auto, false, false)); // ingestion
    }

    #[test]
    fn drain_sse_reassembles_across_partial_chunks_and_stops_on_done() {
        let mut acc = StreamAccumulator::default();
        let mut buf: Vec<u8> = Vec::new();

        // A data line split across two network chunks (no newline yet).
        buf.extend_from_slice(b"data: {\"choices\":[{\"delta\":{\"content\":\"He");
        assert!(!drain_sse_lines(&mut buf, &mut acc));
        buf.extend_from_slice(b"llo\"}}]}\n");
        assert!(!drain_sse_lines(&mut buf, &mut acc));

        // An SSE comment / keep-alive line is ignored; then [DONE] terminates.
        buf.extend_from_slice(b": keep-alive\n\ndata: [DONE]\n");
        assert!(drain_sse_lines(&mut buf, &mut acc));

        let resp = acc.into_response();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello"));
    }

    #[test]
    fn thinking_policy_overrides() {
        assert!(think_on_turn(ThinkingMode::On, false, false));
        assert!(!think_on_turn(ThinkingMode::Off, true, true));
    }

    #[test]
    fn cap_result_marks_truncation() {
        let big = "x".repeat(10_000);
        let capped = cap_result(big, 100); // 100 tokens ≈ 400 bytes
        assert!(capped.len() < 1000);
        assert!(capped.contains("truncated"));
    }

    #[test]
    fn cap_result_passes_short() {
        let small = "hello".to_string();
        assert_eq!(cap_result(small.clone(), 100), small);
    }

    fn turn(tag: &str, cost: Option<u32>) -> Turn {
        Turn { msgs: vec![ChatMessage::user(tag)], cost, note: false }
    }

    fn test_cfg(budget: Option<u32>, n_ctx: Option<u32>, cap: u32) -> AgentConfig {
        AgentConfig {
            base_url: "http://x".into(),
            model: None,
            max_steps: 16,
            budget_tokens: budget,
            n_ctx,
            slots: 1,
            per_tool_result_token_cap: cap,
            auth_token: None,
            per_call_timeout: Duration::from_secs(300),
        }
    }

    #[test]
    fn compaction_budget_reserves_generation_headroom() {
        // A high budget on a 90112 slot is capped to n_ctx - gen_reserve.
        let cfg = test_cfg(Some(72089), Some(90112), 8000);
        let reserve = gen_reserve(90112);
        assert_eq!(compaction_budget(&cfg), Some(90112 - reserve));
        // A budget already under the cap is left untouched.
        assert_eq!(compaction_budget(&test_cfg(Some(30_000), Some(90112), 8000)), Some(30_000));
        // No n_ctx → fall back to the raw budget (no headroom math possible).
        assert_eq!(compaction_budget(&test_cfg(Some(50_000), None, 8000)), Some(50_000));
    }

    #[test]
    fn output_token_cap_is_min_of_context_and_time() {
        let cfg = test_cfg(Some(72089), Some(90112), 8000);
        let ctx_cap = 90112 - compaction_budget(&cfg).unwrap() - 512;
        // Generous time budget at a high rate → the context bound dominates.
        assert_eq!(output_token_cap(&cfg, Duration::from_secs(10_000), 1000.0), Some(ctx_cap));
        // Tight time budget at a slow rate → the time bound dominates:
        // 30s * 50 tok/s * 0.6 = 900.
        assert_eq!(output_token_cap(&cfg, Duration::from_secs(30), 50.0), Some(900));
        // No n_ctx → still time-bounded (never unbounded now).
        assert_eq!(
            output_token_cap(&test_cfg(Some(50_000), None, 8000), Duration::from_secs(30), 50.0),
            Some(900)
        );
    }

    #[test]
    fn slot_rate_factor_is_sublinear_and_one_for_single_slot() {
        assert_eq!(slot_rate_factor(1), 1.0);
        // Two slots ≈ 0.71 of single-slot speed (matches the ~160→115 measurement).
        assert!((slot_rate_factor(2) - 0.714).abs() < 0.01);
        // Sublinear: a 4-slot factor is well above 1/4.
        assert!(slot_rate_factor(4) > 0.25);
        // Monotonically decreasing as slots are added.
        assert!(slot_rate_factor(2) > slot_rate_factor(3));
        assert!(slot_rate_factor(0) == 1.0); // floored at one slot
    }

    #[test]
    fn output_cap_shrinks_the_time_bound_under_slot_contention() {
        // No n_ctx so the time bound is the sole constraint. 30s * 50 tok/s * 0.6.
        let single = test_cfg(Some(50_000), None, 8000); // slots: 1
        let mut dual = test_cfg(Some(50_000), None, 8000);
        dual.slots = 2;
        let one = output_token_cap(&single, Duration::from_secs(30), 50.0).unwrap();
        let two = output_token_cap(&dual, Duration::from_secs(30), 50.0).unwrap();
        // Dual-slot cap is the single-slot cap scaled by the contention factor.
        assert!(two < one);
        assert!(((two as f32) - (one as f32) * slot_rate_factor(2)).abs() < 2.0);
    }

    #[test]
    fn detects_leaked_tool_call_markup() {
        assert!(looks_like_leaked_tool_call("<tool_call>\n<function=read_file>"));
        assert!(looks_like_leaked_tool_call("  <function=read_file>\n<parameter=path>"));
        // Prose that merely mentions the syntax is not flagged.
        assert!(!looks_like_leaked_tool_call("The template uses <tool_call> tags for calls."));
        assert!(!looks_like_leaked_tool_call("[{\"file\":\"x\",\"summary\":\"bug\"}]"));
    }

    #[test]
    fn convo_attributes_turn_costs_from_server_reports() {
        let mut c = Convo::new("sys", "task".into());
        // Step 0: request carried head alone; server says head = 100 tokens.
        c.mark_sent();
        c.push_turn(vec![ChatMessage::user("t0")]);
        c.record(100);
        assert_eq!(c.head_cost, Some(100));
        // Step 1: carried head + t0; server says 180 → t0 cost = 80.
        c.mark_sent();
        c.push_turn(vec![ChatMessage::user("t1")]);
        c.record(180);
        assert_eq!(c.turns[0].cost, Some(80));
        // Step 2: carried head + t0 + t1; server says 300 → t1 cost = 120.
        c.mark_sent();
        c.push_turn(vec![ChatMessage::user("t2")]);
        c.record(300);
        assert_eq!(c.turns[1].cost, Some(120));
        // Measured history = head + t0 + t1 = 300; t2 is not yet measured.
        assert_eq!(c.turns[2].cost, None);
        assert_eq!(c.known_total(), 300);
        assert!(c.over_budget(300));
        assert!(!c.over_budget(301));
    }

    #[test]
    fn convo_compacts_oldest_turns_by_server_cost() {
        let mut c = Convo::new("sys", "task".into());
        c.head_cost = Some(50);
        // Six turns of a known 100 tokens each → measured total 650.
        for i in 0..6 {
            c.turns.push(turn(&format!("t{i}"), Some(100)));
        }
        // Budget 400: drop oldest until 50 + kept ≤ 400, keeping the last 3.
        c.compact(400);
        assert!(c.turns[0].note, "an eviction note is prepended");
        assert_eq!(c.turns.len(), 4); // note + 3 kept
        assert_eq!(c.turns[1].msgs[0].content.as_deref(), Some("t3"));
        assert_eq!(c.turns[3].msgs[0].content.as_deref(), Some("t5"));
        assert!(c.known_total() <= 400);
        // A second compaction at the same budget is a no-op (already minimal).
        let before = c.turns.len();
        c.compact(400);
        assert_eq!(c.turns.len(), before);
    }

    #[test]
    fn compact_truncates_when_floor_turns_exceed_budget() {
        let mut c = Convo::new("sys", "task".into());
        c.head_cost = Some(10);
        // Three kept turns (the keep-floor — none can be dropped), each a large
        // measured tool result. The last-resort truncation must bring the
        // measured size within budget rather than send an oversized prompt.
        for i in 0..3 {
            let mut t = turn(&format!("t{i}"), Some(1_000));
            t.msgs[0].content = Some("x".repeat(4_000));
            c.turns.push(t);
        }
        assert!(c.known_total() > 500);
        c.compact(500);
        assert_eq!(c.turns.len(), 3, "no turn is dropped below the keep-floor");
        assert!(c.known_total() <= 500, "truncation fits the measured size to budget");
        assert!(
            c.turns
                .iter()
                .any(|t| t.msgs.iter().any(|m| m
                    .content
                    .as_deref()
                    .is_some_and(|s| s.contains("truncated")))),
            "a truncation marker is left behind"
        );
    }

    #[test]
    fn convo_compact_noop_when_few_turns() {
        let mut c = Convo::new("sys", "task".into());
        c.head_cost = Some(10);
        c.turns.push(turn("a", Some(1_000)));
        c.turns.push(turn("b", Some(1_000)));
        c.compact(1); // way over budget, but ≤ KEEP_RECENT_TURNS so nothing drops
        assert_eq!(c.turns.len(), 2);
        assert!(!c.turns[0].note);
    }

    #[test]
    fn parse_thinking_mode() {
        assert_eq!(ThinkingMode::parse("off"), ThinkingMode::Off);
        assert_eq!(ThinkingMode::parse("on"), ThinkingMode::On);
        assert_eq!(ThinkingMode::parse("auto"), ThinkingMode::Auto);
        assert_eq!(ThinkingMode::parse("garbage"), ThinkingMode::Auto);
    }
}

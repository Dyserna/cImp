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

use super::openai::{
    strip_think, ChatMessage, ChatRequest, ChatResponse, ToolDef,
};
use super::tools::{self, ToolCtx};

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
}

impl HostRouter {
    /// Build the merged, scope-filtered router. `native_defs` are the enabled
    /// native tool defs; `mcp_defs` are the host's namespaced read-class
    /// tools (`McpHost::tool_defs().await`).
    pub fn new(
        native_defs: Vec<ToolDef>,
        mcp_defs: Vec<ToolDef>,
        ctx: ToolCtx,
        host: std::sync::Arc<super::mcp_host::McpHost>,
        scope: ToolScope,
    ) -> Self {
        let defs: Vec<ToolDef> = native_defs
            .into_iter()
            .chain(mcp_defs)
            .filter(|d| scope.allows_namespaced(&d.function.name))
            .collect();
        Self { defs, ctx, host, scope }
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
    /// Per-slot working token budget `(n_ctx/np) * high_water`; the loop
    /// compacts when usage crosses it. `None` if undiscovered (no
    /// compaction, rely on `max_steps`/deadline).
    pub budget_tokens: Option<u32>,
    /// Per-tool-result cap in tokens (approximated as bytes/4).
    pub per_tool_result_token_cap: u32,
    /// Optional bearer token for the backend (cloud APIs). `None` for a
    /// local/LAN llama-server.
    pub auth_token: Option<String>,
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

/// Wall-clock budget granted to the forced-final synthesis turn, which runs
/// *after* the main `deadline` has (often) already passed. Bounding it keeps
/// the total run from reaching ~2× `offload_timeout_secs` (one full in-loop
/// request that finishes at the deadline, then an unbounded force-final) and
/// staying within the loopback proxy's `+30s` headroom (see `mcp.rs`), past
/// which the proxy abandons the streamed offload and re-runs it on the local
/// self-contained path — a wasteful double execution.
const FINAL_SYNTHESIS_GRACE: Duration = Duration::from_secs(25);

/// Run the agent loop and return the synthesized final answer (with
/// `<think>` stripped). `deadline` bounds the loop wall-clock; on expiry
/// (or `max_steps`/budget) it forces a final-synthesis turn.
pub async fn run(
    client: &reqwest::Client,
    cfg: &AgentConfig,
    router: &dyn ToolRouter,
    task: OffloadTask,
    deadline: Instant,
) -> AppResult<String> {
    let url = format!("{}/v1/chat/completions", cfg.base_url);
    let tools = router.tool_defs();

    let mut messages: Vec<ChatMessage> = Vec::new();
    messages.push(ChatMessage::system(SYSTEM_PROMPT));
    let user = match &task.context {
        Some(c) if !c.is_empty() => format!("{}\n\n# Context\n{}", task.instructions, c),
        _ => task.instructions.clone(),
    };
    messages.push(ChatMessage::user(user));

    for step in 0..cfg.max_steps {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            debug!("offload: deadline reached at step {step}; forcing final synthesis");
            return force_final(client, &url, cfg, messages, task.thinking).await;
        }
        let is_planning = step == 0;
        let enable_thinking = think_on_turn(task.thinking, is_planning, false);

        // Bound this request by the *remaining* deadline, not the client's
        // global `offload_timeout_secs`. Otherwise a step that starts just
        // before the deadline could still block for the full timeout, then
        // `force_final` would block for another — letting one offload run for
        // ~2× its budget and overrun the proxy headroom.
        let resp =
            post_chat(client, &url, cfg, &messages, &tools, enable_thinking, true, remaining).await?;
        let usage = resp.usage;
        let choice = resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Offload("server returned no choices".into()))?;
        let msg = choice.message;

        // Final answer: no tool calls.
        if msg.tool_calls.is_empty() {
            let content = msg.content.unwrap_or_default();
            return Ok(strip_think(&content));
        }

        // Append the assistant turn (carrying the tool_calls), then each
        // tool result.
        let tool_calls = msg.tool_calls.clone();
        let pre_len = messages.len();
        messages.push(msg);
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
            messages.push(ChatMessage::tool(&call.id, capped));
        }

        // Budget policing. `usage.prompt_tokens` measures the prompt we *sent
        // this step* — i.e. BEFORE the tool results appended just above. We add
        // a local estimate of those freshly appended results so a single
        // tool-heavy step that would overflow the backend's per-slot context
        // gets compacted NOW, rather than one round later after the server has
        // already rejected the oversized request with a hard 400/500.
        if let Some(budget) = cfg.budget_tokens {
            // Treat a missing OR zero `prompt_tokens` (some servers send
            // `usage:{}`) as "no usable count" and fall back to a local
            // estimate — a real prompt is never 0 tokens, and trusting the 0
            // would defeat the projection and let the next step overflow.
            let sent = match usage {
                Some(u) if u.prompt_tokens > 0 => u.prompt_tokens as usize,
                _ => estimate_tokens(&messages[..pre_len]),
            };
            let appended = estimate_tokens(&messages[pre_len..]);
            let projected = sent.saturating_add(appended);
            if projected >= budget as usize {
                warn!(
                    sent,
                    appended,
                    budget,
                    "offload: projected prompt over budget; compacting"
                );
                compact(&mut messages, budget);
            }
        }
    }

    // Ran out of steps — force a final answer.
    debug!("offload: max_steps reached; forcing final synthesis");
    force_final(client, &url, cfg, messages, task.thinking).await
}

/// One chat-completions POST. `with_tools` lets the forced-final turn
/// suppress further tool calls.
async fn post_chat(
    client: &reqwest::Client,
    url: &str,
    cfg: &AgentConfig,
    messages: &[ChatMessage],
    tools: &[ToolDef],
    enable_thinking: bool,
    with_tools: bool,
    req_timeout: Duration,
) -> AppResult<ChatResponse> {
    let req = ChatRequest {
        messages: messages.to_vec(),
        tools: if with_tools { tools.to_vec() } else { Vec::new() },
        tool_choice: if with_tools { Some("auto".into()) } else { Some("none".into()) },
        model: cfg.model.clone(),
        temperature: Some(0.2),
        chat_template_kwargs: Some(serde_json::json!({ "enable_thinking": enable_thinking })),
    };
    // Per-request timeout override: bounds this single POST (connect through
    // body read) to the caller's remaining budget, regardless of the client's
    // global `offload_timeout_secs` default.
    let mut builder = client.post(url).timeout(req_timeout).json(&req);
    if let Some(token) = cfg.auth_token.as_deref().filter(|t| !t.is_empty()) {
        builder = builder.bearer_auth(token);
    }
    let resp = builder
        .send()
        .await
        .map_err(|e| AppError::Offload(format!("chat request failed: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Offload(format!(
            "server returned {status}: {}",
            body.chars().take(500).collect::<String>()
        )));
    }
    resp.json::<ChatResponse>()
        .await
        .map_err(|e| AppError::Offload(format!("chat response parse failed: {e}")))
}

/// Force a final answer from the conversation so far (tools suppressed).
async fn force_final(
    client: &reqwest::Client,
    url: &str,
    cfg: &AgentConfig,
    mut messages: Vec<ChatMessage>,
    thinking: ThinkingMode,
) -> AppResult<String> {
    messages.push(ChatMessage::user(
        "You are out of budget. Stop using tools and answer now, as completely as you can, \
         from what you already have. If your information is partial, say so explicitly.",
    ));
    let enable_thinking = think_on_turn(thinking, false, true);
    // The forced-final turn runs after the main deadline; cap it to a fixed
    // grace so the whole offload can't reach ~2× its timeout (see
    // FINAL_SYNTHESIS_GRACE).
    let resp = post_chat(
        client,
        url,
        cfg,
        &messages,
        &[],
        enable_thinking,
        false,
        FINAL_SYNTHESIS_GRACE,
    )
    .await?;
    let content = resp
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .unwrap_or_default();
    let stripped = strip_think(&content);
    if stripped.is_empty() {
        Ok("(offload produced no answer within its budget)".into())
    } else {
        Ok(stripped)
    }
}

/// Rough local token estimate (~4 chars/token) for a slice of chat messages,
/// used to project prompt size *before* a POST so compaction can fire ahead of
/// a server-side context overflow. Deliberately cheap and slightly
/// conservative: counts message content plus tool-call name/argument JSON, with
/// a small per-message framing overhead.
fn estimate_tokens(messages: &[ChatMessage]) -> usize {
    let mut chars = 0usize;
    for m in messages {
        chars += 4; // role + message framing overhead
        if let Some(c) = &m.content {
            chars += c.len();
        }
        for tc in &m.tool_calls {
            chars += tc.function.name.len() + tc.function.arguments.len() + 8;
        }
        if let Some(id) = &m.tool_call_id {
            chars += id.len();
        }
    }
    chars / 4
}

/// Mini auto-compact: drop the oldest tool/assistant turns (keeping the
/// system + original user message and the most recent turns) and leave a
/// note so the model knows context was evicted.
///
/// Budget-aware: it keeps shrinking the retained tail until the rebuilt
/// conversation is estimated under `budget`, so a single oversized recent turn
/// can't leave the result still over budget — which would otherwise make
/// compaction a no-op that re-fires (and re-sends a near-identical oversized
/// prompt) every step until `max_steps`.
fn compact(messages: &mut Vec<ChatMessage>, budget: u32) {
    const KEEP_RECENT: usize = 6;
    const NOTE: &str = "[earlier tool results were summarized away to stay within the context \
                        budget — re-fetch anything you still need]";
    if messages.len() <= 2 + KEEP_RECENT {
        return;
    }
    let head: Vec<ChatMessage> = messages.iter().take(2).cloned().collect(); // system + user
    let budget = budget as usize;
    let mut tail_start = messages.len() - KEEP_RECENT;
    loop {
        // The tail must not begin with a `tool` message: its owning assistant
        // turn (which carries the matching `tool_calls` id) would have been
        // evicted, and OpenAI-compatible servers reject a `tool` message that
        // doesn't follow the assistant that requested it. Advance past any
        // leading orphan tool messages to the next real turn boundary.
        while tail_start < messages.len() && messages[tail_start].role == "tool" {
            tail_start += 1;
        }
        let mut rebuilt = head.clone();
        rebuilt.push(ChatMessage::user(NOTE));
        rebuilt.extend(messages.iter().skip(tail_start).cloned());

        // Done when under budget, or when we can't drop more without losing the
        // single most-recent message (always keep at least one real turn).
        if estimate_tokens(&rebuilt) < budget || tail_start >= messages.len().saturating_sub(1) {
            *messages = rebuilt;
            return;
        }
        // Still over budget: drop the next-oldest kept turn and retry.
        tail_start += 1;
    }
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

    #[test]
    fn compact_tail_never_starts_with_tool() {
        let assistant = || ChatMessage {
            role: "assistant".into(),
            content: Some("a".into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        };
        // Arrange so the naive cutoff (len - KEEP_RECENT) lands on a `tool`
        // message whose owning assistant turn is evicted.
        let mut messages = vec![
            ChatMessage::system("s"),
            ChatMessage::user("u"),
            assistant(),                       // 2: owns the tool calls below
            ChatMessage::tool("c1", "r1"),     // 3
            ChatMessage::tool("c2", "r2"),     // 4 <- naive tail_start (10-6)
            assistant(),                       // 5
            ChatMessage::tool("c3", "r3"),     // 6
            assistant(),                       // 7
            ChatMessage::tool("c4", "r4"),     // 8
            assistant(),                       // 9
        ];
        // Large budget: one pass, exercising only the orphan-tool skip.
        compact(&mut messages, u32::MAX);
        // First message after the system+user+note head must not be a tool.
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[2].role, "user"); // the eviction note
        assert_ne!(messages[3].role, "tool", "tail must not start with an orphan tool message");
    }

    #[test]
    fn parse_thinking_mode() {
        assert_eq!(ThinkingMode::parse("off"), ThinkingMode::Off);
        assert_eq!(ThinkingMode::parse("on"), ThinkingMode::On);
        assert_eq!(ThinkingMode::parse("auto"), ThinkingMode::Auto);
        assert_eq!(ThinkingMode::parse("garbage"), ThinkingMode::Auto);
    }
}

//! MCP server toward Claude — the `cimp --offload-mcp` subcommand.
//!
//! A minimal, hand-rolled stdio JSON-RPC 2.0 MCP server (newline-delimited
//! messages on stdin/stdout). Implements `initialize` (declaring
//! `tools.listChanged`), `tools/list`, and `tools/call`, exposing one tool:
//! `offload_task(instructions, context?, thinking?, tier?) -> string`.
//!
//! **V8-03: proxy + fallback.** The child is now a thin bridge. When the app
//! is running it discovers the app's [loopback endpoint](super::loopback)
//! (via the `{port, token, pid}` discovery file next to the exe) and
//! forwards everything — `tools/call` → `POST /run`, the tool *description* ←
//! `GET /describe` — so the heavy lifting runs on the app's warm pool +
//! global gate + MCP host. A background task subscribes to `GET /events` and
//! relays `notifications/tools/list_changed` to Claude over the same stdio
//! pipe. When the app is **unreachable** (not running, headless cron,
//! mid-restart) the child degrades to the self-contained V8-02 path below
//! (read settings → probe → route → native-tools loop), so offload still
//! works without the app — just without the warm pool, global concurrency,
//! or live MCP host. Both paths share the pure router and the agent loop.
//!
//! Dispatched in `main()` before Tauri init, exactly like `--statusline`,
//! so it stays GUI-free and fast to spawn.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::Semaphore;

use crate::error::AppError;
use crate::mcp_stdio::tool_error;

use super::loopback::{parse_result_line, proxy_base_for};

/// Root-aware endpoint resolution for this child: its cwd is the agent's
/// project directory (inherited at spawn — the injected mcp-config sets no
/// cwd), so with several cImp instances off one install the child connects
/// to the instance actually serving ITS project, not the last one launched.
fn proxy_base() -> Option<(String, String)> {
    proxy_base_for(std::env::current_dir().ok().as_deref())
}

use crate::offload::agent::{self, AgentConfig, NativeRouter, OffloadTask, ThinkingMode};
use crate::offload::router::{self, BackendView, RouteError, TierHint};
use crate::offload::server::{per_slot_n_ctx, ServerCommand};
use crate::offload::tools::{self, ToolCtx};
use crate::settings::{
    BackendTier, OffloadBackend, OffloadBackendKind, OffloadSettings, ToolScope,
};

const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "cimp-offload";

/// The consumer this child serves (from `--consumer <name>`, default
/// `"claude"`). Threaded onto the loopback `/mcp/*` queries so the app returns
/// the right per-consumer MCP-server tool set. Set once at startup.
static CONSUMER: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// V28 (issue #13): the cImp TAB this child was spawned for (from
/// `--tab <tab-id>`). One `--offload-mcp` child runs per tab and its argv is
/// composed entirely by cImp, so the tab identity can be baked in at spawn —
/// which is what lets the app resolve *which* session of this agent the
/// `context_*` memory tools should scope to. Unset for a child spawned by hand
/// (or by a pre-V28 cImp), which fails open to the old behavior.
static TAB: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The configured consumer name, lowercased; `"claude"` when unset.
fn consumer() -> &'static str {
    CONSUMER.get().map(String::as_str).unwrap_or("claude")
}

/// The tab id this child serves, or `None` when it was spawned without `--tab`.
fn tab() -> Option<&'static str> {
    TAB.get().map(String::as_str)
}

/// Entry point for `cimp --offload-mcp [--consumer <name>] [--tab <tab-id>]`.
/// Builds a current-thread tokio runtime and serves the stdio loop until stdin
/// closes. `consumer` selects which MCP-server set the app proxies to this child
/// (`claude` default, or `opencode`); `tab` is the cImp tab id this child was
/// spawned for (V28 — forwarded on `/graph_run` so the app can scope the
/// `context_*` memory tools to THIS tab's session). Never panics — a crash here
/// would garble the host agent's MCP session.
pub fn run(consumer: &str, tab: Option<&str>) {
    let _ = CONSUMER.set(consumer.trim().to_ascii_lowercase());
    // Tab ids are case-sensitive settings keys — store verbatim (trimmed), never
    // lowercased like the consumer name.
    if let Some(t) = tab.map(str::trim).filter(|t| !t.is_empty()) {
        let _ = TAB.set(t.to_string());
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return,
    };
    rt.block_on(serve());
}

async fn serve() {
    // stdout is shared between the request loop and the `/events` relay task
    // (which pushes `tools/list_changed` notifications), so guard it.
    let stdout = Arc::new(TokioMutex::new(tokio::io::stdout()));

    // Relay app-side capability changes to Claude as `tools/list_changed`,
    // whenever the app's loopback endpoint is reachable. Best-effort and
    // self-healing — it reconnects when the app comes/goes.
    {
        let stdout = stdout.clone();
        tokio::spawn(async move { events_relay(stdout).await });
    }

    // V30 Phase 0 channel spike — remove when #15 spike concludes.
    // Inert unless `CIMP_CHANNEL_SPIKE` names a trigger file: publishes the
    // shared stdout for `spike_slow_progress` and starts the channel pusher,
    // which holds its own clone exactly like the `/events` relay above.
    if let Some(trigger) = spike_trigger_path() {
        let _ = SPIKE_STDOUT.set(stdout.clone());
        let stdout = stdout.clone();
        tokio::spawn(async move { spike_push_task(stdout, trigger).await });
    }

    // The shared stdio JSON-RPC loop (`mcp_stdio`): spawns each request so
    // multiple in-flight tool calls run concurrently (two parallel
    // `offload_task`s must occupy both llama-server slots at once), captures
    // handler panics as JSON-RPC errors, and stops accepting work once a
    // response write fails (Claude closed the pipe).
    crate::mcp_stdio::serve(stdout, "offload", handle_owned).await;
}

/// Owned-argument wrapper around [`handle`] so it can run on its own
/// `tokio::spawn`ed task (which requires a `'static` future) for panic capture.
async fn handle_owned(method: String, params: Value) -> Result<Value, (i64, String)> {
    handle(&method, params).await
}

/// Dispatch one JSON-RPC method. Returns `Ok(result)` or
/// `Err((code, message))` for a JSON-RPC error object.
async fn handle(method: &str, params: Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => {
            // The client's own `capabilities` (and `clientInfo`) arrive in
            // `params` and are deliberately discarded — this child advertises a
            // fixed surface. Channel support is a SERVER declaration, so the
            // spike needs no client-capability parsing.
            let mut result = json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") }
            });
            // V30 Phase 0 channel spike — remove when #15 spike concludes.
            if spike_enabled() {
                spike_decorate_initialize(&mut result);
            }
            Ok(result)
        }
        "ping" => Ok(json!({})),
        "tools/list" => {
            let mut tools = Vec::new();
            // The offload tools only when offload is enabled — this same MCP
            // child also carries graph + Claude-exposed MCP tools, which work
            // with offload off, so don't advertise a dead `offload_task` then.
            if current_offload_settings().enabled {
                tools.push(offload_task_tool_live().await);
                tools.push(offload_batch_tool());
            }
            // V9-01 code-knowledge-graph tools (present only when the graph
            // feature is enabled for this project).
            tools.extend(crate::graph::mcp_tools());
            // Claude-Code-exposed MCP servers (those with `claude_access`),
            // proxied through the app's warm host. Empty when the app is down.
            tools.extend(proxy_mcp_list().await);
            // V30 Phase 0 channel spike — remove when #15 spike concludes.
            if spike_enabled() {
                tools.extend(spike_tools());
            }
            Ok(json!({ "tools": tools }))
        }
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            // V30 Phase 0 channel spike — remove when #15 spike concludes.
            // Dispatched here (not in `handle_tools_call`) because the progress
            // probe needs the REQUEST-level `params._meta.progressToken`, which
            // this is the last place to still hold intact.
            if spike_enabled() && name.starts_with("spike_") {
                handle_spike_tool(name, &params).await
            } else if name.starts_with("graph_")
                || name.starts_with("context_")
                || name == "run_check"
            {
                // Graph + session-memory tools, plus `run_check` (V12 Phase A —
                // independent of the graph, but shares this dispatch surface).
                // Warm path: let the app's single index serve it (no second
                // cross-process DB open; lets the app record it for the monitor
                // and scope memory to this consumer). Fall back to a direct
                // read-only open when the app isn't up.
                match proxy_graph(&params).await {
                    Some(r) => r,
                    None => crate::graph::handle_mcp_call(&params, consumer()).await,
                }
            } else if name == "offload_batch" {
                handle_batch_tool(params).await
            } else if name.contains("__") {
                // A namespaced `<server>__<tool>` from a Claude-exposed MCP
                // server — route to the app's warm host.
                proxy_mcp_call(&params).await
            } else {
                handle_tools_call(params).await
            }
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    }
}

/// The `offload_task` descriptor with a **live** description: the app's
/// `GET /describe` (health-accurate) when reachable, else the config-derived
/// fallback renderer.
async fn offload_task_tool_live() -> Value {
    let description = match proxy_describe().await {
        Some(d) if !d.trim().is_empty() => d,
        _ => offload_task_description(&current_offload_settings()),
    };
    let mut tool = offload_task_tool();
    tool["description"] = Value::String(description);
    tool
}

/// The `offload_task` tool descriptor, with its `description` rendered
/// from the current (config-derived) capability set.
fn offload_task_tool() -> Value {
    let settings = current_offload_settings();
    json!({
        "name": "offload_task",
        "description": offload_task_description(&settings),
        "inputSchema": {
            "type": "object",
            "properties": {
                "instructions": {
                    "type": "string",
                    "description": "A self-contained subtask. The local worker sees only this (plus optional context) and returns one synthesized answer."
                },
                "context": {
                    "type": "string",
                    "description": "Optional extra context (paths, prior findings) to seed the worker."
                },
                "thinking": {
                    "type": "string",
                    "enum": ["auto", "off", "on"],
                    "description": "Reasoning effort. 'off' is only for pure transforms of the PROVIDED context (summarize/extract/reformat from the `context` arg) — it skips reasoning. Any task that needs tool calls (file reads, searches, counting files, web) should use 'auto' (default) or 'on', or the worker may guess instead of verifying. 'on' forces genuine analysis; 'auto' lets the worker decide per step."
                },
                "tier": {
                    "type": "string",
                    "enum": ["auto", "fast", "quality"],
                    "description": "Backend bias when multiple offload backends are configured. 'fast' routes trivial single-pass work (summarize/extract/classify) to the small/fast backend; 'quality' forces the large/capable one for real reasoning or big context; 'auto' (default) lets the router decide by task size. Local-file tasks always run on a backend with file access (never a cloud backend)."
                },
                "schema": {
                    "type": "object",
                    "description": "Optional JSON Schema. When provided, the worker's final answer is grammar-constrained (llama.cpp sampler) to a single JSON value matching it — guaranteed-parseable, no prose, composable into scripts. The worker still uses tools normally; only its final message is constrained. Omit for a normal prose answer."
                }
            },
            "required": ["instructions"]
        }
    })
}

/// Render the capability description as a **union across enabled backends**
/// (V8-02). Each backend contributes a coarse label — its name, kind, tier,
/// context window, and a tool-scope summary — so Opus knows a fast tier
/// exists, which backends can read local files, and how to bias with
/// `tier`. Falls back to the native-tool list for a single-local pool.
///
/// Config-derived (the per-call child can't see live app-side health);
/// health-accurate re-rendering is the warm-pool followup noted in V8-01.
fn offload_task_description(settings: &OffloadSettings) -> String {
    let backends: Vec<OffloadBackend> = settings
        .effective_backends()
        .into_iter()
        .filter(|b| b.enabled)
        .collect();

    if backends.is_empty() {
        return "Delegate a token-heavy subtask to a local model to conserve this session's \
                context. (No offload backend is configured/enabled — set one up in cImp \
                Settings → Offload.)"
            .to_string();
    }

    let parts: Vec<String> = backends
        .iter()
        .map(|b| backend_label(b, settings))
        .collect();
    let any_local_file = backends
        .iter()
        .any(|b| !b.cloud_blocked() && b.tool_scope.allows("read_file"));
    let routing_note = if backends.len() > 1 {
        let local_hint = if any_local_file {
            " Local-file tasks run on a backend with file access (never a cloud backend)."
        } else {
            ""
        };
        format!(" Pass `tier` (fast|quality) to bias the choice.{local_hint}")
    } else {
        String::new()
    };

    format!(
        "Delegate a token-heavy subtask (broad codebase search, large-file/log summarization, web \
         research) to a local/remote model to conserve this session's context. Pass a \
         self-contained instruction; you get back only the synthesized result. Use `thinking: off` \
         only for pure transforms of context you provide; any task that needs tool calls (file \
         reads, searches, counting files) should use `auto` (default) or `on`. You can run \
         offloads in parallel — issue multiple offload_task calls at once to fan out independent \
         subtasks; they queue if all slots are busy. Backends: {}.{}",
        parts.join("; "),
        routing_note
    )
}

/// One backend's coarse capability label for the union description.
fn backend_label(b: &OffloadBackend, settings: &OffloadSettings) -> String {
    let tier = match b.tier {
        BackendTier::Fast => "fast",
        BackendTier::Quality => "quality",
    };
    let kind = match &b.kind {
        OffloadBackendKind::Local { .. } => "local",
        OffloadBackendKind::Remote { is_cloud: true, .. } => "cloud",
        OffloadBackendKind::Remote { .. } => "LAN",
    };
    let ctx = match b.declared_context {
        Some(n) => format!("~{}k ctx", (n / 1000).max(1)),
        None => "ctx discovered".to_string(),
    };
    let tools = tool_scope_summary(&b.tool_scope, settings);
    let consent = if b.cloud_blocked() {
        " — NEEDS CONSENT (disabled until granted)"
    } else {
        ""
    };
    format!("{} ({kind}, {tier}, {ctx}, {tools}{consent})", b.name)
}

/// Coarse, human-readable summary of a backend's tool scope.
fn tool_scope_summary(scope: &ToolScope, settings: &OffloadSettings) -> String {
    // Names the model could plausibly use: native toggles + MCP server names.
    let mut pool: Vec<String> = Vec::new();
    if settings.tools.read_file {
        pool.push("read_file".into());
    }
    if settings.tools.code_search {
        pool.push("code_search".into());
    }
    if settings.tools.run_command {
        pool.push("run_command".into());
    }
    for s in &settings.mcp_servers {
        if s.offload_access && !s.name.is_empty() {
            pool.push(s.name.clone());
        }
    }
    let allowed: Vec<&String> = pool.iter().filter(|t| scope.allows(t)).collect();
    match scope {
        ToolScope::All => "all tools".to_string(),
        _ if allowed.is_empty() => "no tools".to_string(),
        _ => {
            // Compact: if only web/docs survive, say so.
            let local_blocked = !scope.allows("read_file") && !scope.allows("code_search");
            if local_blocked {
                "web/docs only".to_string()
            } else {
                format!("{} tools", allowed.len())
            }
        }
    }
}

async fn handle_tools_call(params: Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    if name != "offload_task" {
        return Err((-32602, format!("unknown tool: {name}")));
    }
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let instructions = args
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if instructions.trim().is_empty() {
        return Ok(tool_error("offload_task requires non-empty `instructions`"));
    }
    let context = args
        .get("context")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let thinking_str = args
        .get("thinking")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let tier_str = args
        .get("tier")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let schema = schema_arg(&args);
    match run_one(instructions, context, thinking_str, tier_str, schema).await {
        Ok(text) => Ok(json!({ "content": [{ "type": "text", "text": text }] })),
        // A "not ready/busy" condition is returned as a tool result (not a
        // protocol error) so Opus can read it and retry/adapt.
        Err(msg) => Ok(tool_error(&msg)),
    }
}

/// Run one offload subtask: prefer the app's warm pool when reachable, degrade
/// to the self-contained path otherwise. `proxy_run` returns `None` only when
/// the app is unreachable (transport failure / no discovery file) — a
/// task-level error from the app comes back as `Some(Err(..))` and is surfaced
/// as-is, not retried locally. Shared by the single (`offload_task`) and batch
/// (`offload_batch`) tools.
async fn run_one(
    instructions: String,
    context: Option<String>,
    thinking_str: String,
    tier_str: String,
    // V21 F9: optional JSON Schema — forwarded to the warm app (`/run`) or run
    // in the self-contained fallback; constrains the worker's final output.
    schema: Option<serde_json::Value>,
) -> Result<String, String> {
    let thinking = ThinkingMode::parse(&thinking_str);
    let tier = TierHint::parse(&tier_str);
    match proxy_run(
        &instructions,
        context.as_deref(),
        &thinking_str,
        &tier_str,
        schema.as_ref(),
    )
    .await
    {
        Some(r) => r,
        None => run_offload(instructions, context, thinking, tier, schema).await,
    }
}

/// `offload_batch`: run several subtasks **in parallel** from a single tool
/// call. This is the way to get genuine concurrency from one session — the MCP
/// client serializes separate `offload_task` calls, but here the child fans the
/// subtasks out to the app at once (each is a normal `/run`, so the app's warm
/// pool + global gate bound real parallelism to the backend's slots; extras
/// queue). Results come back as one section per subtask, errors inline.
async fn handle_batch_tool(params: Value) -> Result<Value, (i64, String)> {
    /// Backstop on a single call's fan-out (the app's slot gate is the real
    /// throttle; this just bounds a runaway request).
    const MAX_TASKS: usize = 16;

    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let tasks = args
        .get("tasks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if tasks.is_empty() {
        return Ok(tool_error(
            "offload_batch requires a non-empty `tasks` array",
        ));
    }
    if tasks.len() > MAX_TASKS {
        return Ok(tool_error(&format!(
            "offload_batch accepts at most {MAX_TASKS} tasks per call (got {})",
            tasks.len()
        )));
    }

    // Spawn every subtask concurrently. The app gates real parallelism to the
    // slot count, so excess subtasks simply wait their turn on the gate.
    let handles: Vec<_> = tasks
        .iter()
        .map(|t| {
            let instructions = t
                .get("instructions")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let context = t
                .get("context")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let thinking_str = t
                .get("thinking")
                .and_then(|v| v.as_str())
                .unwrap_or("auto")
                .to_string();
            let tier_str = t
                .get("tier")
                .and_then(|v| v.as_str())
                .unwrap_or("auto")
                .to_string();
            // V21 F9: per-subtask JSON Schema (object only).
            let schema = schema_arg(t);
            tokio::spawn(async move {
                if instructions.trim().is_empty() {
                    return Err("subtask requires non-empty `instructions`".to_string());
                }
                run_one(instructions, context, thinking_str, tier_str, schema).await
            })
        })
        .collect();

    let mut sections = Vec::with_capacity(handles.len());
    let mut ok_count = 0usize;
    for (i, h) in handles.into_iter().enumerate() {
        let n = i + 1;
        match h.await {
            Ok(Ok(text)) => {
                ok_count += 1;
                sections.push(format!("## Subtask {n} — OK\n\n{text}"));
            }
            Ok(Err(msg)) => sections.push(format!("## Subtask {n} — ERROR\n\n{msg}")),
            Err(_) => sections.push(format!("## Subtask {n} — ERROR\n\n(subtask was cancelled)")),
        }
    }
    let combined = sections.join("\n\n---\n\n");
    // Surface partial success normally (so the orchestrator gets whatever
    // completed); only flag `isError` when *every* subtask failed.
    if ok_count == 0 {
        Ok(tool_error(&combined))
    } else {
        Ok(json!({ "content": [{ "type": "text", "text": combined }] }))
    }
}

/// V21 F9: extract an optional JSON-Schema `schema` argument from an
/// `offload_task` / `offload_batch`-subtask arguments object. Only a JSON object
/// is accepted (a schema must be an object); a string/array/null/absent value
/// yields `None`, so a malformed hint degrades to a normal prose run rather than
/// an error. Shared by the single-task and per-subtask parse paths so both
/// thread schemas identically.
fn schema_arg(args: &Value) -> Option<Value> {
    args.get("schema").filter(|v| v.is_object()).cloned()
}

/// The `offload_batch` tool descriptor.
fn offload_batch_tool() -> Value {
    json!({
        "name": "offload_batch",
        "description": "Run several offload subtasks IN PARALLEL across the local worker's slots in a single call. Prefer this over issuing multiple `offload_task` calls when you want real concurrency: separate tool calls are serialized by the MCP client, but this one call fans its subtasks out to the app at once (bounded by the backend's slot count; extras queue). Each subtask is independent and self-contained; you get back all results, one section per subtask, with per-subtask errors inline.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "The subtasks to run in parallel (1–16). Each is independent.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "instructions": {
                                "type": "string",
                                "description": "A self-contained subtask. The local worker sees only this (plus optional context) and returns one synthesized answer."
                            },
                            "context": {
                                "type": "string",
                                "description": "Optional extra context (paths, prior findings) for this subtask."
                            },
                            "thinking": {
                                "type": "string",
                                "enum": ["auto", "off", "on"],
                                "description": "Reasoning effort for this subtask. 'off' is only for pure transforms of the PROVIDED context (summarize/extract/reformat from `context`); any subtask needing tool calls (file reads, searches, counting files, web) should use 'auto' (default) or 'on', else the worker may guess instead of verifying. 'on' forces analysis; 'auto' lets the worker decide."
                            },
                            "tier": {
                                "type": "string",
                                "enum": ["auto", "fast", "quality"],
                                "description": "Backend bias for this subtask: 'fast', 'quality', or 'auto' (default)."
                            },
                            "schema": {
                                "type": "object",
                                "description": "Optional JSON Schema for THIS subtask. When set, the worker's final answer is grammar-constrained to a single JSON value matching it (guaranteed-parseable, no prose). Omit for a normal prose answer."
                            }
                        },
                        "required": ["instructions"]
                    }
                }
            },
            "required": ["tasks"]
        }
    })
}

// ── Proxy toward the app's loopback endpoint ────────────────────────────
//
// When the app is up, the child forwards to it (warm pool + global gate +
// MCP host). All three helpers fail soft to `None`/fallback when the app is
// unreachable, so offload still works headless.

/// Fetch the live capability description from `GET /describe`. `None` when
/// the app is unreachable (the caller renders the config-derived fallback).
async fn proxy_describe() -> Option<String> {
    let (base, token) = proxy_base()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let resp = client
        .get(format!("{base}/describe"))
        .bearer_auth(&token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().await.ok()
}

/// Forward an `offload_task` to `POST /run`. Returns:
/// - `None` → app unreachable; the caller runs the self-contained fallback.
/// - `Some(Ok(text))` → the synthesized answer from the warm pool.
/// - `Some(Err(msg))` → a task-level error the app already resolved (busy /
///   no backend / timeout) — surfaced to Claude as-is, not retried locally.
async fn proxy_run(
    instructions: &str,
    context: Option<&str>,
    thinking: &str,
    tier: &str,
    schema: Option<&serde_json::Value>,
) -> Option<Result<String, String>> {
    let (base, token) = proxy_base()?;
    // Short connect timeout to fast-detect "app not listening" (→ `None` →
    // fallback). Deliberately NO overall request timeout: once connected we read
    // the app's heartbeat-streamed `/run` body and rely on the per-chunk idle
    // window below to tell a slow-but-alive job from a wedged one. That's the
    // whole refactor — a long thinking job is waited out, never abandoned and
    // re-executed locally where it would only contend for the same slot.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .pool_max_idle_per_host(0)
        .build()
        .ok()?;

    // Forward this child's working directory (the session's project root) so
    // the app scopes native tools to the repo Claude is in, not the app's own
    // launch dir, when no explicit `allowed_roots` is configured.
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let body = json!({
        "instructions": instructions,
        "context": context,
        "thinking": thinking,
        "tier": tier,
        "cwd": cwd,
        // V21 F9: forward the JSON Schema so the warm app constrains the final
        // turn. Omitted (null, `serde(default)` on the app side) when unset.
        "schema": schema,
    });
    let mut resp = match client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        // Could not even get response headers → app unreachable → fallback.
        Err(_) => return None,
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        return Some(Err(format!(
            "offload app returned {status}: {}",
            txt.chars().take(300).collect::<String>()
        )));
    }

    // Read the NDJSON body chunk by chunk. As long as bytes (heartbeats or the
    // result) keep arriving within IDLE, the app is alive — wait however long
    // the job takes. A full IDLE gap means it wedged: surface an error rather
    // than fall back (a local re-run would just fight for the busy slot).
    const IDLE: Duration = Duration::from_secs(45);
    let mut buf: Vec<u8> = Vec::new();
    let mut result: Option<Result<String, String>> = None;
    loop {
        match tokio::time::timeout(IDLE, resp.chunk()).await {
            Err(_) => {
                return Some(Err(
                    "offload worker stopped responding (no heartbeat) — it may be stuck; \
                     not retried locally"
                        .into(),
                ))
            }
            Ok(Ok(Some(bytes))) => {
                buf.extend_from_slice(&bytes);
                while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = buf.drain(..=nl).collect();
                    if let Some(r) = parse_result_line(&raw, "offload failed") {
                        result = Some(r);
                    }
                }
            }
            Ok(Ok(None)) => break, // EOF (connection closed)
            Ok(Err(e)) => return Some(Err(format!("offload stream error: {e}"))),
        }
    }
    // A trailing unterminated line (e.g. a one-shot non-streamed error body
    // that carries no final newline).
    if result.is_none() && !buf.is_empty() {
        if let Some(r) = parse_result_line(&buf, "offload failed") {
            result = Some(r);
        }
    }
    Some(result.unwrap_or_else(|| Err("offload stream ended without a result".into())))
}

/// Forward a `graph_*` tool call to the app's warm path (`POST /graph_run`), so
/// the app's single warm index serves it instead of this child opening a second
/// (cross-process) handle on the SQLite-backed store — and so the app can record
/// the call for the monitor tab. Returns `None` when the app is unreachable (the
/// caller falls back to opening graph.db directly); on success returns the same
/// JSON-RPC tool result shape as [`crate::graph::handle_mcp_call`].
async fn proxy_graph(params: &Value) -> Option<Result<Value, (i64, String)>> {
    let (base, token) = proxy_base()?;
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .ok()?;
    // V28: `tab` rides along ONLY here (`/graph_run`) — `/mcp/call` proxies to
    // external servers, which hold no cImp memory scope. Omitted entirely when
    // this child has no tab identity, so the body stays byte-identical to the
    // pre-V28 shape on that path.
    let mut body = json!({ "cwd": cwd, "name": name, "args": args, "consumer": consumer() });
    if let (Some(t), Some(map)) = (tab(), body.as_object_mut()) {
        map.insert("tab".to_string(), Value::String(t.to_string()));
    }
    let resp = client
        .post(format!("{base}/graph_run"))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .ok()?; // transport failure → None → direct-open fallback
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    let ok = v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false);
    if ok {
        let text = v.get("text").and_then(|t| t.as_str()).unwrap_or_default();
        Some(Ok(json!({ "content": [{ "type": "text", "text": text }] })))
    } else {
        let err = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("graph query failed");
        Some(Ok(
            json!({ "content": [{ "type": "text", "text": err }], "isError": true }),
        ))
    }
}

/// Fetch the Claude-Code-exposed MCP tool descriptors from the app
/// (`POST /mcp/list`) so the child can advertise them in its `tools/list`.
/// Returns an empty vec when the app is unreachable or has no Claude-enabled
/// servers — the child still lists its own (offload + graph) tools.
async fn proxy_mcp_list() -> Vec<Value> {
    let Some((base, token)) = proxy_base() else {
        return Vec::new();
    };
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    else {
        return Vec::new();
    };
    let resp = match client
        .post(format!("{base}/mcp/list?consumer={}", consumer()))
        .bearer_auth(&token)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return Vec::new(),
    };
    let Ok(v) = resp.json::<Value>().await else {
        return Vec::new();
    };
    v.get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Forward a Claude-exposed MCP tool call (`<server>__<tool>`) to the app's
/// warm host via `POST /mcp/call`, returning the JSON-RPC tool-result shape.
/// There is no local fallback — these tools live behind the app's host — so an
/// unreachable app surfaces as an `isError` tool result, not a hard failure.
async fn proxy_mcp_call(params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let Some((base, token)) = proxy_base() else {
        return Ok(tool_error(
            "cImp app is not running — its MCP tools are only available while the app is up",
        ));
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| (-32603, format!("client build failed: {e}")))?;
    let body = json!({ "name": name, "arguments": args });
    let resp = match client
        .post(format!("{base}/mcp/call?consumer={}", consumer()))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Ok(tool_error(&format!("MCP tool call transport error: {e}"))),
    };
    if !resp.status().is_success() {
        return Ok(tool_error(&format!(
            "MCP tool call failed: HTTP {}",
            resp.status()
        )));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| (-32603, format!("bad MCP call response: {e}")))?;
    if v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
        let text = v.get("text").and_then(|t| t.as_str()).unwrap_or_default();
        Ok(json!({ "content": [{ "type": "text", "text": text }] }))
    } else {
        let err = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("MCP tool call failed");
        Ok(tool_error(err))
    }
}

/// Long-lived task: while the app's loopback endpoint is reachable, hold a
/// `GET /events` SSE connection and relay each `change` event to Claude as a
/// `notifications/tools/list_changed`. Reconnects when the app comes/goes.
async fn events_relay(stdout: Arc<TokioMutex<tokio::io::Stdout>>) {
    // Build the client once and reuse it across reconnects — the old code
    // rebuilt a fresh reqwest::Client on every 2s retry (~1800/hr while the app
    // is down). No client timeout: this is a long-lived streaming connection.
    let Ok(client) = reqwest::Client::builder().build() else {
        return;
    };
    loop {
        if let Some((base, token)) = proxy_base() {
            {
                if let Ok(mut resp) = client
                    .get(format!("{base}/events"))
                    .bearer_auth(&token)
                    .send()
                    .await
                {
                    // Read the SSE byte stream chunk by chunk; emit a
                    // notification each time a `change` event arrives. Keep a
                    // small carry of the previous chunk's tail so a marker
                    // split across a chunk boundary (TCP can break anywhere) is
                    // still detected — otherwise a capability change would be
                    // silently dropped.
                    // Bound each read: the app sends an SSE keep-alive every
                    // 20s, so a 60s gap means the connection went half-open
                    // (e.g. the app was hard-killed without a FIN). Break to
                    // reconnect rather than hang here forever — otherwise
                    // list_changed notifications would stop reaching Claude.
                    const READ_IDLE: Duration = Duration::from_secs(60);
                    let mut carry: Vec<u8> = Vec::new();
                    while let Ok(Ok(Some(chunk))) =
                        tokio::time::timeout(READ_IDLE, resp.chunk()).await
                    {
                        let mut buf = std::mem::take(&mut carry);
                        buf.extend_from_slice(&chunk);
                        if buf.windows(SSE_CHANGE.len()).any(|w| w == SSE_CHANGE) {
                            emit_list_changed(&stdout).await;
                        }
                        // Retain only the bytes that could be a marker prefix
                        // straddling into the next chunk (at most len-1).
                        let keep = SSE_CHANGE.len().saturating_sub(1).min(buf.len());
                        carry = buf[buf.len() - keep..].to_vec();
                    }
                }
            }
        }
        // App down or stream ended — back off and retry.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// The SSE marker the app emits for a capability change.
const SSE_CHANGE: &[u8] = b"event: change";

/// Build one JSON-RPC **notification** frame (no `id`, so no response is ever
/// expected). Pure — separated from the write so the exact wire shape is
/// unit-testable; `params` is omitted entirely when `None`, which is what a
/// bare `tools/list_changed` needs.
fn notification_frame(method: &str, params: Option<Value>) -> Value {
    let mut frame = json!({ "jsonrpc": "2.0", "method": method });
    if let (Some(p), Some(map)) = (params, frame.as_object_mut()) {
        map.insert("params".to_string(), p);
    }
    frame
}

/// Write one unsolicited JSON-RPC notification on the shared stdout pipe.
///
/// This is the child's single out-of-band write path (see the `mcp_stdio`
/// module docs): the caller holds its own clone of the shared stdout mutex, so
/// a notification can never interleave with a response frame. Best-effort — a
/// failed write means the host closed the pipe, which the request loop's own
/// shutdown guard detects.
async fn emit_notification(
    stdout: &Arc<TokioMutex<tokio::io::Stdout>>,
    method: &str,
    params: Option<Value>,
) {
    let frame = notification_frame(method, params).to_string();
    let mut out = stdout.lock().await;
    let _ = out.write_all(frame.as_bytes()).await;
    let _ = out.write_all(b"\n").await;
    let _ = out.flush().await;
}

/// Write a `tools/list_changed` notification on the shared stdout pipe.
async fn emit_list_changed(stdout: &Arc<TokioMutex<tokio::io::Stdout>>) {
    emit_notification(stdout, "notifications/tools/list_changed", None).await;
}

// ── V30 Phase 0 channel spike ──────────────────────────────────────────────
//
// V30 Phase 0 channel spike — remove when #15 spike concludes.
//
// Everything below is inert unless `CIMP_CHANNEL_SPIKE` is set to a non-empty
// path: with the var unset the handshake, the tool list, the dispatch, and the
// spawned tasks are byte-identical to the pre-spike child. The spike answers
// two questions for issue #15:
//
//   1. does Claude Code actually deliver `notifications/claude/channel` pushed
//      by a stdio MCP child into a live session (declared via
//      `capabilities.experimental["claude/channel"]` + `instructions` on the
//      2025-06-18 handshake, which is the era where channels are honoured —
//      PROTOCOL_VERSION must NOT be bumped);
//   2. does Claude Code auto-background an MCP tool call past ~2min
//      (`CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS`), and do `notifications/progress`
//      frames reset that stall timer (`spike_slow` vs `spike_slow_progress`).

/// Env var gating the whole spike. Its value is the path of the *trigger file*
/// the push task watches; unset or empty means the spike is off.
const SPIKE_ENV: &str = "CIMP_CHANNEL_SPIKE";

/// Injected into Claude's system prompt via the top-level `instructions` field
/// of the `initialize` result, so a delivered channel message is echoed back
/// verbatim and delivery can be verified from the transcript alone.
const SPIKE_INSTRUCTIONS: &str = "cimp-offload may push out-of-band notices as <channel source=\"cimp-offload\"> messages (V30 spike). When one arrives, explicitly acknowledge it and repeat its content and meta attributes verbatim so delivery can be verified.";

/// The shared stdout handle, republished for the spike tools.
///
/// The shared dispatch (`mcp_stdio::serve`) hands a handler only
/// `(method, params)` — it has no stdout — so `spike_slow_progress`, which must
/// emit `notifications/progress` *while* its own `tools/call` is still in
/// flight, picks the mutex up from here. Set once in [`serve`], and only when
/// the spike is enabled.
static SPIKE_STDOUT: std::sync::OnceLock<Arc<TokioMutex<tokio::io::Stdout>>> =
    std::sync::OnceLock::new();

/// The trigger-file path when the spike is enabled, else `None`. The file need
/// not exist yet — the push task treats its appearance as the first change.
fn spike_trigger_path() -> Option<std::path::PathBuf> {
    let raw = std::env::var(SPIKE_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(trimmed))
}

/// Whether the V30 channel spike is armed for this child.
fn spike_enabled() -> bool {
    spike_trigger_path().is_some()
}

/// Add the experimental channel capability + the top-level `instructions`
/// string to an otherwise-unchanged `initialize` result. Pure, so a test can
/// pin both the addition and the untouched base (notably `protocolVersion`).
fn spike_decorate_initialize(result: &mut Value) {
    result["capabilities"]["experimental"] = json!({ "claude/channel": {} });
    result["instructions"] = Value::String(SPIKE_INSTRUCTIONS.to_string());
}

/// The two spike tool descriptors, appended to `tools/list` only while the
/// spike is armed. Same descriptor shape as `offload_task` / `offload_batch`.
fn spike_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "spike_slow",
            "description": "V30 spike probe (temporary): sleeps server-side for `seconds` and returns a confirmation string. It does no work and emits no progress notifications — its only purpose is to exercise this client's long-running MCP tool-call behaviour (auto-backgrounding past ~2 minutes). Safe to call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "seconds": {
                        "type": "number",
                        "description": "How long to sleep before returning. Default 150 (just over the 2-minute auto-background threshold); clamped to 1–600."
                    }
                }
            }
        }),
        json!({
            "name": "spike_slow_progress",
            "description": "V30 spike probe (temporary): identical to `spike_slow`, but emits a `notifications/progress` frame every `interval` seconds while it sleeps, provided the call carried a `progressToken`. Its purpose is to test whether progress notifications keep a long tool call in the foreground. Safe to call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "seconds": {
                        "type": "number",
                        "description": "How long to sleep before returning. Default 150; clamped to 1–600."
                    },
                    "interval": {
                        "type": "number",
                        "description": "Seconds between progress notifications. Default 15; clamped to 1–600."
                    }
                }
            }
        }),
    ]
}

/// Read one clamped whole-second argument from a spike tool's `arguments`.
/// A missing, non-numeric, or out-of-range value degrades to the default /
/// nearest bound rather than erroring — a spike probe must never fail on input.
fn spike_secs(args: &Value, key: &str, default: u64, lo: u64, hi: u64) -> u64 {
    args.get(key)
        .and_then(|v| v.as_f64())
        .filter(|f| f.is_finite())
        .map(|f| f.round().clamp(lo as f64, hi as f64) as u64)
        .unwrap_or(default)
        .clamp(lo, hi)
}

/// Dispatch a `spike_*` tool call. Reached only while the spike is armed.
async fn handle_spike_tool(name: &str, params: &Value) -> Result<Value, (i64, String)> {
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let seconds = spike_secs(&args, "seconds", 150, 1, 600);
    let pid = std::process::id();
    let text = match name {
        "spike_slow" => {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            format!("spike_slow completed after {seconds}s (pid {pid})")
        }
        "spike_slow_progress" => {
            let interval = spike_secs(&args, "interval", 15, 1, 600);
            // MCP puts the progress token on the REQUEST's `params._meta`, and
            // `mcp_stdio::serve` forwards `params` whole — so it is already
            // here, no threading needed. Its type is opaque (string or int):
            // echo it back verbatim.
            let token = params
                .get("_meta")
                .and_then(|m| m.get("progressToken"))
                .filter(|t| !t.is_null())
                .cloned();
            match (token, SPIKE_STDOUT.get()) {
                (Some(token), Some(stdout)) => {
                    let mut elapsed = 0u64;
                    while elapsed + interval < seconds {
                        tokio::time::sleep(Duration::from_secs(interval)).await;
                        elapsed += interval;
                        emit_notification(
                            stdout,
                            "notifications/progress",
                            Some(json!({
                                "progressToken": token,
                                "progress": elapsed,
                                "total": seconds
                            })),
                        )
                        .await;
                    }
                    tokio::time::sleep(Duration::from_secs(seconds - elapsed)).await;
                    let sent = elapsed / interval;
                    format!(
                        "spike_slow_progress completed after {seconds}s (pid {pid}) \
                         — sent {sent} progress notifications, one every {interval}s"
                    )
                }
                (Some(_), None) => {
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    format!(
                        "spike_slow_progress completed after {seconds}s (pid {pid}) \
                         (progressToken received but the shared stdout handle was unavailable \
                         — no notifications were sent)"
                    )
                }
                (None, _) => {
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    format!(
                        "spike_slow_progress completed after {seconds}s (pid {pid}) \
                         (no progressToken received — client did not request progress)"
                    )
                }
            }
        }
        _ => return Err((-32602, format!("unknown tool: {name}"))),
    };
    Ok(json!({ "content": [{ "type": "text", "text": text }] }))
}

/// Long-lived spike task: push one automatic channel message shortly after
/// startup, then push the trigger file's contents every time it changes.
///
/// Holds its own clone of the shared stdout mutex, exactly like
/// [`events_relay`] — that is the sanctioned way to write out of band here.
async fn spike_push_task(stdout: Arc<TokioMutex<tokio::io::Stdout>>, trigger: std::path::PathBuf) {
    /// Late enough that the session has finished its handshake and the user is
    /// mid-turn when it lands (the point is an UNSOLICITED push).
    const AUTO_DELAY: Duration = Duration::from_secs(20);
    const POLL: Duration = Duration::from_secs(2);

    tokio::time::sleep(AUTO_DELAY).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    spike_push(
        &stdout,
        &format!("spike auto push: pid={} t={now}", std::process::id()),
        "spike_auto",
        0,
    )
    .await;

    // Baseline the trigger's mtime so a file that ALREADY exists at startup is
    // not mistaken for a change; `None` (absent) is a valid baseline, so the
    // file simply appearing counts as the first trigger.
    let mut last = spike_mtime(&trigger).await;
    let mut seq = 1u64;
    loop {
        tokio::time::sleep(POLL).await;
        let current = spike_mtime(&trigger).await;
        if current == last {
            continue;
        }
        last = current;
        if current.is_none() {
            continue; // the file was deleted — nothing to push
        }
        // `tokio::fs` (not `std::fs`): this child runs a CURRENT-THREAD runtime
        // shared with in-flight tool calls, so the poll must not block it.
        match tokio::fs::read_to_string(&trigger).await {
            Ok(body) => {
                let body = body.trim();
                if body.is_empty() {
                    eprintln!(
                        "cimp-offload channel spike: {} changed but is empty — nothing pushed",
                        trigger.display()
                    );
                    continue;
                }
                spike_push(&stdout, body, "spike_file", seq).await;
                seq += 1;
            }
            Err(e) => eprintln!(
                "cimp-offload channel spike: failed to read {}: {e}",
                trigger.display()
            ),
        }
    }
}

/// The trigger file's modification time, or `None` when it is absent or
/// unreadable (both mean "no change to report" for the poller).
async fn spike_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    tokio::fs::metadata(path).await.ok()?.modified().ok()
}

/// Push one `notifications/claude/channel` frame. `meta` keys must match
/// `^[a-zA-Z_][a-zA-Z0-9_]*$` or the client silently drops them, and values are
/// sent as strings (hence `seq` is stringified).
async fn spike_push(
    stdout: &Arc<TokioMutex<tokio::io::Stdout>>,
    content: &str,
    kind: &str,
    seq: u64,
) {
    emit_notification(
        stdout,
        "notifications/claude/channel",
        Some(json!({
            "content": content,
            "meta": { "kind": kind, "seq": seq.to_string() }
        })),
    )
    .await;
}

/// A backend resolved from config to a connectable endpoint, before the
/// health probe. Local backends contribute their parsed command's URL +
/// slot count; remote backends their configured URL/auth/declared values.
struct ResolvedBackend {
    name: String,
    base_url: String,
    auth_token: Option<String>,
    cloud_blocked: bool,
    is_cloud: bool,
    tier: BackendTier,
    tool_scope: ToolScope,
    slots: u32,
    declared_context: Option<u32>,
    /// Local backend launched with `--kv-unified`: `/props` then reports the
    /// FULL shared window instead of the per-slot one, so the probe divides
    /// it by `slots` (see [`crate::offload::server::per_slot_n_ctx`]).
    kv_unified: bool,
}

impl ResolvedBackend {
    /// Resolve one configured backend. Returns `None` for a backend that
    /// can't even be addressed (e.g. a Local entry with an unparseable or
    /// empty command, or a Remote with no URL) — it's simply absent from
    /// the pool rather than fatal.
    fn from_config(b: &OffloadBackend) -> Option<Self> {
        match &b.kind {
            OffloadBackendKind::Local { server_command, .. } => {
                if server_command.trim().is_empty() {
                    return None;
                }
                let cmd = ServerCommand::parse(server_command).ok()?;
                Some(Self {
                    name: b.name.clone(),
                    base_url: cmd.base_url(),
                    auth_token: None,
                    cloud_blocked: false,
                    is_cloud: false,
                    tier: b.tier,
                    tool_scope: b.tool_scope.clone(),
                    slots: cmd.parallel.max(1),
                    declared_context: b.declared_context,
                    kv_unified: cmd.kv_unified,
                })
            }
            OffloadBackendKind::Remote {
                base_url,
                auth_token,
                is_cloud,
                ..
            } => {
                if base_url.trim().is_empty() {
                    return None;
                }
                Some(Self {
                    name: b.name.clone(),
                    base_url: base_url.trim_end_matches('/').to_string(),
                    auth_token: if auth_token.is_empty() {
                        None
                    } else {
                        Some(auth_token.clone())
                    },
                    cloud_blocked: b.cloud_blocked(),
                    is_cloud: *is_cloud,
                    tier: b.tier,
                    tool_scope: b.tool_scope.clone(),
                    // A remote endpoint doesn't report `-np`; assume one
                    // slot (the user sizes real parallelism on the box).
                    slots: 1,
                    declared_context: b.declared_context,
                    // No parsed command for a remote endpoint — and with one
                    // assumed slot there'd be nothing to divide anyway.
                    kv_unified: false,
                })
            }
        }
    }
}

/// Health-probe one resolved backend: `(ready, n_ctx, slots)`. `n_ctx` is the
/// discovered-or-declared window; `slots` is the `/props` `total_slots` when
/// the endpoint reports it (a llama-server does), else the backend's declared
/// count. For a cloud endpoint (often no `/health`), any HTTP response counts
/// as reachable; a LAN llama-server must answer `/health` 2xx.
async fn probe(client: &reqwest::Client, b: &ResolvedBackend) -> (bool, Option<u32>, u32) {
    let health = client
        .get(format!("{}/health", b.base_url))
        .timeout(Duration::from_secs(5));
    let health = match &b.auth_token {
        Some(t) => health.bearer_auth(t),
        None => health,
    };
    let ready = match health.send().await {
        Ok(r) => {
            let status = r.status();
            if b.is_cloud {
                // Cloud may lack /health (404/405 still proves reachability),
                // but a bad token (401/403) or server error (5xx) means it's
                // not actually usable — don't report it ready.
                !(status == reqwest::StatusCode::UNAUTHORIZED
                    || status == reqwest::StatusCode::FORBIDDEN
                    || status.is_server_error())
            } else {
                status.is_success()
            }
        }
        Err(_) => false,
    };
    if !ready {
        return (false, b.declared_context, b.slots);
    }
    // Best-effort /props for the authoritative window + slot count; fall back
    // to declared values.
    let props = client
        .get(format!("{}/props", b.base_url))
        .timeout(Duration::from_secs(5));
    let props = match &b.auth_token {
        Some(t) => props.bearer_auth(t),
        None => props,
    };
    let body: Option<Value> = match props.send().await {
        Ok(r) if r.status().is_success() => r.json::<Value>().await.ok(),
        _ => None,
    };
    let n_ctx = body
        .as_ref()
        .and_then(|v| {
            v.get("default_generation_settings")
                .and_then(|g| g.get("n_ctx"))
                .and_then(|x| x.as_u64())
                .or_else(|| v.get("n_ctx").and_then(|x| x.as_u64()))
        })
        // Under `--kv-unified` the reported window is the shared one; the
        // router budgets one slot. A declared fallback is already per-slot.
        .map(|n| per_slot_n_ctx(n as u32, b.slots, b.kv_unified))
        .or(b.declared_context);
    let slots = body
        .as_ref()
        .and_then(|v| v.get("total_slots").and_then(|x| x.as_u64()))
        .map(|n| (n as u32).max(1))
        .unwrap_or(b.slots);
    (ready, n_ctx, slots)
}

/// Resolve, probe, and route the configured backend pool, then run the
/// agent loop against the chosen backend. On a connection-class failure it
/// asks the router for one fail-over backend and retries once.
/// Bounds how many self-contained agent loops the **child** runs at once when
/// the app isn't reachable. The proxy path is gated app-side; this degraded
/// fallback has no global gate, so without this an `offload_batch` of N tasks
/// would open N simultaneous streams to a one- or two-slot server.
static CHILD_GATE: Semaphore = Semaphore::const_new(3);

async fn run_offload(
    instructions: String,
    context: Option<String>,
    thinking: ThinkingMode,
    tier: TierHint,
    schema: Option<serde_json::Value>,
) -> Result<String, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let settings = current_offload_settings();

    if !settings.enabled {
        return Err("offload is disabled — enable it in cImp settings".into());
    }

    // Resolve the enabled pool.
    let resolved: Vec<ResolvedBackend> = settings
        .effective_backends()
        .iter()
        .filter(|b| b.enabled)
        .filter_map(ResolvedBackend::from_config)
        .collect();
    if resolved.is_empty() {
        return Err("no offload backend is configured — add one in cImp Settings → Offload".into());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(settings.offload_timeout_secs.max(30)))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    // Probe all backends concurrently (a down cloud backend mustn't stall
    // the others). The per-call child can't see app-side slot usage, so
    // `in_flight` is reported as 0 — cross-process spill needs the warm
    // pool (V8-01 open decision); tier/context/tool routing is unaffected.
    let mut handles = Vec::with_capacity(resolved.len());
    for (i, b) in resolved.iter().enumerate() {
        let client = client.clone();
        let url = b.base_url.clone();
        let auth = b.auth_token.clone();
        let is_cloud = b.is_cloud;
        let declared = b.declared_context;
        let slots = b.slots;
        let kv_unified = b.kv_unified;
        handles.push(tauri::async_runtime::spawn(async move {
            let rb = ResolvedBackend {
                name: String::new(),
                base_url: url,
                auth_token: auth,
                cloud_blocked: false,
                is_cloud,
                tier: BackendTier::Quality,
                tool_scope: ToolScope::All,
                slots,
                declared_context: declared,
                kv_unified,
            };
            (i, probe(&client, &rb).await)
        }));
    }
    let mut probed: Vec<(bool, Option<u32>, u32)> = vec![(false, None, 1); resolved.len()];
    for h in handles {
        if let Ok((i, res)) = h.await {
            probed[i] = res;
        }
    }

    // Build router views.
    let views: Vec<BackendView> = resolved
        .iter()
        .enumerate()
        .map(|(i, b)| BackendView {
            name: b.name.clone(),
            base_url: b.base_url.clone(),
            ready: probed[i].0,
            cloud_blocked: b.cloud_blocked,
            n_ctx: probed[i].1,
            slots: probed[i].2,
            in_flight: 0,
            tier: b.tier,
            tool_scope: b.tool_scope.clone(),
            budget_high_water_pct: settings.budget_high_water_pct,
        })
        .collect();

    let req = router::analyze_task(&instructions, context.as_deref(), tier);

    // First selection.
    let chosen = router::select(&views, &req).map_err(|e: RouteError| e.to_string())?;
    tracing::info!(
        target: "offload",
        task_chars = instructions.len(),
        est_ctx = req.estimated_context,
        tier = ?req.tier_hint,
        backend = %views[chosen].name,
        "offload: routed task → backend `{}`",
        views[chosen].name
    );

    let result = run_on_backend(
        &client,
        &settings,
        &cwd,
        &resolved[chosen],
        &views[chosen],
        &instructions,
        context.clone(),
        thinking,
        schema.clone(),
    )
    .await;

    // Single re-route on a connection-class failure: drop the failed
    // backend and re-select among the rest (fail-over). `active` tracks which
    // backend actually served the run, so F5 escalation reasons about the right
    // tier.
    let (result, active) = match result {
        Ok(text) => (Ok(text), chosen),
        Err(e) if is_connection_error(&e) && views.len() > 1 => {
            let mut alt_views = views.clone();
            alt_views[chosen].ready = false; // exclude the failed backend
            match router::select(&alt_views, &req) {
                Ok(next) if next != chosen => {
                    tracing::warn!(
                        failed = %resolved[chosen].name,
                        reroute = %resolved[next].name,
                        "offload: re-routing after connection failure"
                    );
                    let r = run_on_backend(
                        &client,
                        &settings,
                        &cwd,
                        &resolved[next],
                        &views[next],
                        &instructions,
                        context.clone(),
                        thinking,
                        schema.clone(),
                    )
                    .await;
                    (r, next)
                }
                _ => (Err(e), chosen),
            }
        }
        Err(e) => (Err(e), chosen),
    };

    // V21 F5 — tier escalation (fallback path): mirrors the app service. A
    // fast-tier run that came back only partially verified re-runs once on a
    // distinct, ready quality backend; quality wins, a failed escalation keeps
    // the fast answer. Inert without a second, quality-tier backend and gated by
    // `escalate_partial`; `escalation_target` blocks quality→quality and
    // same-instance re-runs, and the single call bounds it to one escalation.
    let want_escalate = settings.escalate_partial
        && result
            .as_ref()
            .map(|t| agent::answer_verified_level(t) == agent::VerifiedLevel::Partially)
            .unwrap_or(false);
    if want_escalate {
        if let Some(q) = router::escalation_target(&views, &req, active) {
            tracing::info!(
                from = %resolved[active].name,
                to = %resolved[q].name,
                "offload: escalating partially-verified fast-tier answer to the quality backend"
            );
            let esc = run_on_backend(
                &client,
                &settings,
                &cwd,
                &resolved[q],
                &views[q],
                &instructions,
                context.clone(),
                thinking,
                schema.clone(),
            )
            .await;
            if let Ok(q_text) = esc {
                return Ok(agent::append_escalation_note(&q_text));
            }
        }
    }
    result
}

/// Run the agent loop against one chosen backend with its tool scope, auth,
/// and per-slot budget.
#[allow(clippy::too_many_arguments)]
async fn run_on_backend(
    client: &reqwest::Client,
    settings: &OffloadSettings,
    cwd: &std::path::Path,
    backend: &ResolvedBackend,
    view: &BackendView,
    instructions: &str,
    context: Option<String>,
    thinking: ThinkingMode,
    schema: Option<serde_json::Value>,
) -> Result<String, String> {
    let ctx = ToolCtx::new(
        settings.allowed_roots.clone(),
        settings.command_allowlist.clone(),
        settings.command_policies.clone(),
        cwd,
    );
    let router = NativeRouter::new(
        tools::enabled_defs(&settings.tools),
        ctx,
        backend.tool_scope.clone(),
    );
    let cfg = AgentConfig {
        base_url: backend.base_url.clone(),
        model: None,
        max_steps: settings.max_steps.max(1),
        budget_tokens: view.per_slot_budget(),
        n_ctx: view.n_ctx,
        slots: view.slots,
        per_tool_result_token_cap: settings.per_tool_result_token_cap.max(256),
        auth_token: backend.auth_token.clone(),
        per_call_timeout: Duration::from_secs(settings.offload_timeout_secs.max(30)),
    };
    let task = OffloadTask {
        instructions: instructions.to_string(),
        context: context.clone(),
        thinking,
        schema: schema.clone(),
    };
    let secs = settings.offload_timeout_secs.max(30);
    let deadline = Instant::now() + Duration::from_secs(secs);
    // Self-contained child path: no external cancel source, so a never-tripped
    // token. The request is still bounded by the deadline and the stream.
    let cancel = tokio_util::sync::CancellationToken::new();
    // Bound concurrent self-contained runs — this fallback path has no app-side
    // slot gate (acquire is infallible; the semaphore is never closed).
    let _permit = CHILD_GATE.acquire().await.expect("CHILD_GATE never closed");
    // The headless child path doesn't feed the dashboard run log → no trace.
    let first = agent::run(client, &cfg, &router, task, deadline, None, &cancel).await;
    // Mirror the app's On→Auto retry: a `thinking:on` run that produced no
    // answer (the model spent its output budget thinking) gets one more shot
    // with `auto`. Without this, the degraded child path surfaces a hard error
    // for a task the app path would have recovered.
    let result = match first {
        Err(AppError::OffloadNoAnswer(_)) if thinking == ThinkingMode::On => {
            let task = OffloadTask {
                instructions: instructions.to_string(),
                context,
                thinking: ThinkingMode::Auto,
                schema,
            };
            let deadline = Instant::now() + Duration::from_secs(secs);
            agent::run(client, &cfg, &router, task, deadline, None, &cancel).await
        }
        other => other,
    };
    result.map_err(|e| e.to_string())
}

/// Whether an agent-loop error looks like a transport/connection failure
/// (so a fail-over re-route is worth trying) rather than a model/logic
/// error (which would just repeat on another backend).
fn is_connection_error(e: &str) -> bool {
    let e = e.to_lowercase();
    e.contains("chat request failed")
        || e.contains("chat stream failed")
        || e.contains("connection")
        || e.contains("timed out")
        || e.contains("timeout")
        || e.contains("refused")
        || e.contains("/props request failed")
}

/// Load just the offload block from the layered settings, read-only.
fn current_offload_settings() -> OffloadSettings {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::settings::load_readonly(&cwd).offload
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins this child's use of the shared loopback line parser: heartbeats
    /// (`{"hb":true}` — anything without `ok`) and blanks are skipped, the
    /// single `ok`-bearing line is the result, and this child's fallback error
    /// text applies.
    #[test]
    fn parse_result_line_skips_heartbeats_and_blanks() {
        let parse = |raw: &[u8]| parse_result_line(raw, "offload failed");
        assert!(parse(b"{\"hb\":true}\n").is_none());
        assert!(parse(b"   \n").is_none());
        assert!(parse(b"not json").is_none());
    }

    #[test]
    fn parse_result_line_reads_success_and_error() {
        let parse = |raw: &[u8]| parse_result_line(raw, "offload failed");
        let ok = parse(b"{\"ok\":true,\"text\":\"done\"}\n");
        assert_eq!(ok, Some(Ok("done".to_string())));

        let err = parse(b"{\"ok\":false,\"error\":\"no backend\"}");
        assert_eq!(err, Some(Err("no backend".to_string())));

        // `ok:false` with no message falls back to a generic error.
        assert_eq!(
            parse(b"{\"ok\":false}"),
            Some(Err("offload failed".to_string()))
        );
    }

    #[test]
    fn schema_arg_accepts_objects_only() {
        // An object schema is threaded through verbatim.
        let obj = json!({ "type": "object", "properties": { "count": { "type": "integer" } } });
        let args = json!({ "instructions": "x", "schema": obj });
        assert_eq!(schema_arg(&args), Some(obj));
        // Non-object hints (string/array/null) or an absent key degrade to None
        // (a normal prose run), never an error.
        assert_eq!(schema_arg(&json!({ "schema": "not-a-schema" })), None);
        assert_eq!(schema_arg(&json!({ "schema": [1, 2, 3] })), None);
        assert_eq!(schema_arg(&json!({ "schema": null })), None);
        assert_eq!(schema_arg(&json!({ "instructions": "x" })), None);
    }

    #[test]
    fn batch_threads_per_subtask_schemas() {
        // Mirror `handle_batch_tool`'s per-subtask parse: each subtask carries
        // its own (independent) schema, and one without a schema stays prose.
        let tasks = json!([
            { "instructions": "count", "schema": { "type": "object" } },
            { "instructions": "summarize" },
            { "instructions": "list", "schema": { "type": "array" } },
        ]);
        let arr = tasks.as_array().unwrap();
        let schemas: Vec<Option<Value>> = arr.iter().map(schema_arg).collect();
        assert_eq!(schemas[0], Some(json!({ "type": "object" })));
        assert_eq!(schemas[1], None);
        assert_eq!(schemas[2], Some(json!({ "type": "array" })));
    }

    /// The `emit_list_changed` → `emit_notification` refactor must not have
    /// changed one byte on the wire: a params-less notification carries NO
    /// `params` key at all.
    #[test]
    fn notification_frame_matches_the_pre_refactor_wire_shape() {
        assert_eq!(
            notification_frame("notifications/tools/list_changed", None).to_string(),
            r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#
        );
        let with_params =
            notification_frame("notifications/progress", Some(json!({ "progress": 1 })));
        assert_eq!(with_params["params"]["progress"], json!(1));
        assert_eq!(with_params["jsonrpc"], "2.0");
    }

    // ── V30 Phase 0 channel spike — remove when #15 spike concludes. ──────

    /// The spike decoration adds the experimental channel capability and the
    /// `instructions` string WITHOUT disturbing the rest of the handshake —
    /// notably `protocolVersion`, which must stay on the legacy era where
    /// Claude Code honours channels, and `tools.listChanged`.
    #[test]
    fn spike_initialize_adds_channel_capability_only() {
        let mut result = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": { "name": SERVER_NAME, "version": "test" }
        });
        spike_decorate_initialize(&mut result);
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], json!(true));
        assert!(result["capabilities"]["experimental"]["claude/channel"].is_object());
        assert!(result["instructions"]
            .as_str()
            .is_some_and(|s| s.contains("<channel source=\"cimp-offload\">")));
    }

    /// The channel push frame the spike sends: method, content, and meta keys
    /// that satisfy the client's `^[a-zA-Z_][a-zA-Z0-9_]*$` filter (others are
    /// silently dropped) with string values.
    #[test]
    fn spike_channel_frame_shape() {
        let frame = notification_frame(
            "notifications/claude/channel",
            Some(json!({
                "content": "hello",
                "meta": { "kind": "spike_file", "seq": "3" }
            })),
        );
        assert_eq!(frame["method"], "notifications/claude/channel");
        assert_eq!(frame["params"]["content"], "hello");
        let meta = frame["params"]["meta"].as_object().unwrap();
        for (k, v) in meta {
            assert!(
                k.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "meta key `{k}` would be dropped by the client"
            );
            assert!(v.is_string(), "meta value for `{k}` must be a string");
        }
    }

    #[test]
    fn spike_tools_are_well_formed() {
        let tools = spike_tools();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(names, vec!["spike_slow", "spike_slow_progress"]);
        for t in &tools {
            // Same descriptor shape as offload_task/offload_batch, and the
            // `spike_` prefix the dispatch switches on.
            assert!(t["name"].as_str().unwrap().starts_with("spike_"));
            assert!(!t["description"].as_str().unwrap_or_default().is_empty());
            assert_eq!(t["inputSchema"]["type"], "object");
            assert!(t["inputSchema"]["properties"]["seconds"].is_object());
        }
        assert!(tools[1]["inputSchema"]["properties"]["interval"].is_object());
    }

    #[test]
    fn spike_secs_defaults_and_clamps() {
        let d = |v: Value| spike_secs(&v, "seconds", 150, 1, 600);
        assert_eq!(d(Value::Null), 150);
        assert_eq!(d(json!({})), 150);
        assert_eq!(d(json!({ "seconds": "nope" })), 150);
        assert_eq!(d(json!({ "seconds": 30 })), 30);
        assert_eq!(d(json!({ "seconds": 30.6 })), 31);
        assert_eq!(d(json!({ "seconds": 0 })), 1);
        assert_eq!(d(json!({ "seconds": -5 })), 1);
        assert_eq!(d(json!({ "seconds": 9999 })), 600);
        assert_eq!(d(json!({ "seconds": f64::INFINITY })), 150);
    }

    /// The progress token rides on the REQUEST's `params._meta`, which the
    /// shared dispatch forwards whole — pin that read so a future change to
    /// `mcp_stdio::serve`'s params handling is caught here.
    #[test]
    fn spike_reads_progress_token_from_request_meta() {
        let params = json!({
            "name": "spike_slow_progress",
            "arguments": { "seconds": 30 },
            "_meta": { "progressToken": "abc-1" }
        });
        let token = params
            .get("_meta")
            .and_then(|m| m.get("progressToken"))
            .filter(|t| !t.is_null())
            .cloned();
        assert_eq!(token, Some(json!("abc-1")));
        let without = json!({ "name": "spike_slow_progress", "arguments": {} });
        assert!(without
            .get("_meta")
            .and_then(|m| m.get("progressToken"))
            .is_none());
    }

    #[test]
    fn tool_descriptors_advertise_schema_param() {
        let single = offload_task_tool();
        assert!(
            single["inputSchema"]["properties"]["schema"].is_object(),
            "offload_task must advertise the optional schema param"
        );
        let batch = offload_batch_tool();
        assert!(
            batch["inputSchema"]["properties"]["tasks"]["items"]["properties"]["schema"]
                .is_object(),
            "offload_batch subtasks must advertise the optional schema param"
        );
    }
}

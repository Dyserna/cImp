//! MCP server toward Claude — the `ccimp --offload-mcp` subcommand.
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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::Semaphore;

use crate::error::AppError;

use super::loopback::read_discovery;

use crate::offload::agent::{self, AgentConfig, NativeRouter, OffloadTask, ThinkingMode};
use crate::offload::router::{self, BackendView, RouteError, TierHint};
use crate::offload::server::ServerCommand;
use crate::offload::tools::{self, ToolCtx};
use crate::settings::{
    BackendTier, OffloadBackend, OffloadBackendKind, OffloadSettings, ToolScope,
};

const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "ccimp-offload";

/// Entry point for `ccimp --offload-mcp`. Builds a current-thread tokio
/// runtime and serves the stdio loop until stdin closes. Never panics —
/// a crash here would garble Claude Code's MCP session.
pub fn run() {
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

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    // Set by a spawned handler when its response write fails (Claude closed
    // stdout): stop accepting new work whose results could never be delivered.
    let shutdown = Arc::new(AtomicBool::new(false));

    while let Ok(Some(line)) = lines.next_line().await {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue, // ignore malformed frames
        };
        let id = req.get("id").cloned();
        let method = req
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        // Notifications (no id) get no response — don't spawn a handler for them
        // (it would run `handle` only to discard the result).
        if id.is_none() {
            continue;
        }

        // Spawn each request so multiple in-flight tool calls run concurrently
        // — e.g. two parallel `offload_task`s must occupy both llama-server
        // slots at once. Awaiting `handle` inline here serialized them: the read
        // loop wouldn't pull the second request off stdin until the first
        // (minutes-long) offload finished. Responses are matched by `id`, so
        // out-of-order completion is fine; the shared `stdout` mutex serializes
        // the writes.
        let stdout = stdout.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            // Run the handler on its own task so a panic inside it surfaces as a
            // JSON-RPC error (the client gets a reply) rather than being swallowed
            // by the dropped JoinHandle — which would hang the caller forever
            // waiting on a response that never comes.
            let response = match tokio::spawn(handle_owned(method, params)).await {
                Ok(r) => r,
                Err(e) => Err((-32603, format!("offload handler panicked: {e}"))),
            };
            let id = id.unwrap_or(Value::Null);
            let frame = match response {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            };
            let mut bytes = frame.to_string();
            bytes.push('\n');
            let mut out = stdout.lock().await;
            if out.write_all(bytes.as_bytes()).await.is_err() || out.flush().await.is_err() {
                // stdout is gone (Claude closed the pipe): signal the read loop to
                // stop spawning handlers whose results can't be delivered.
                shutdown.store(true, Ordering::Relaxed);
            }
        });
    }
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
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") }
        })),
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
            Ok(json!({ "tools": tools }))
        }
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name.starts_with("graph_") {
                // Warm path: let the app's single index serve it (no second
                // cross-process DB open; lets the app record it for the monitor).
                // Fall back to a direct read-only open when the app isn't up.
                match proxy_graph(&params).await {
                    Some(r) => r,
                    None => crate::graph::handle_mcp_call(&params).await,
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
                    "description": "Reasoning effort. 'off' for cheap deterministic work (extract/list/lookup), 'on' for genuine analysis, 'auto' (default) to let the worker decide per step."
                },
                "tier": {
                    "type": "string",
                    "enum": ["auto", "fast", "quality"],
                    "description": "Backend bias when multiple offload backends are configured. 'fast' routes trivial single-pass work (summarize/extract/classify) to the small/fast backend; 'quality' forces the large/capable one for real reasoning or big context; 'auto' (default) lets the router decide by task size. Local-file tasks always run on a backend with file access (never a cloud backend)."
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
                context. (No offload backend is configured/enabled — set one up in ccImp \
                Settings → Offload.)"
            .to_string();
    }

    let parts: Vec<String> = backends.iter().map(|b| backend_label(b, settings)).collect();
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
         self-contained instruction; you get back only the synthesized result. You can run \
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
    match run_one(instructions, context, thinking_str, tier_str).await {
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
) -> Result<String, String> {
    let thinking = ThinkingMode::parse(&thinking_str);
    let tier = TierHint::parse(&tier_str);
    match proxy_run(&instructions, context.as_deref(), &thinking_str, &tier_str).await {
        Some(r) => r,
        None => run_offload(instructions, context, thinking, tier).await,
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
        return Ok(tool_error("offload_batch requires a non-empty `tasks` array"));
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
            tokio::spawn(async move {
                if instructions.trim().is_empty() {
                    return Err("subtask requires non-empty `instructions`".to_string());
                }
                run_one(instructions, context, thinking_str, tier_str).await
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

fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
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
                                "description": "Reasoning effort for this subtask. 'off' for deterministic extract/list, 'on' for genuine analysis, 'auto' (default) to let the worker decide."
                            },
                            "tier": {
                                "type": "string",
                                "enum": ["auto", "fast", "quality"],
                                "description": "Backend bias for this subtask: 'fast', 'quality', or 'auto' (default)."
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

/// Base URL of the app's loopback endpoint from the discovery file.
fn proxy_base() -> Option<(String, String)> {
    let d = read_discovery()?;
    Some((format!("http://127.0.0.1:{}", d.port), d.token))
}

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
                    if let Some(r) = parse_run_line(&raw) {
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
        if let Some(r) = parse_run_line(&buf) {
            result = Some(r);
        }
    }
    Some(result.unwrap_or_else(|| Err("offload stream ended without a result".into())))
}

/// Parse one NDJSON line from the streamed `/run` body. Returns `None` for a
/// heartbeat (`{"hb":true}`) or unparseable line, `Some(Ok(text))` for the
/// final success line, `Some(Err(..))` for a task-level error line.
fn parse_run_line(raw: &[u8]) -> Option<Result<String, String>> {
    let line = std::str::from_utf8(raw).ok()?.trim();
    if line.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("hb").is_some() {
        return None; // heartbeat
    }
    let ok = v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false);
    if ok {
        Some(Ok(v
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string()))
    } else {
        Some(Err(v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("offload failed")
            .to_string()))
    }
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
    let body = json!({ "cwd": cwd, "name": name, "args": args });
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
        .post(format!("{base}/mcp/list"))
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
            "ccImp app is not running — its MCP tools are only available while the app is up",
        ));
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| (-32603, format!("client build failed: {e}")))?;
    let body = json!({ "name": name, "arguments": args });
    let resp = match client
        .post(format!("{base}/mcp/call"))
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

/// Write a `tools/list_changed` notification on the shared stdout pipe.
async fn emit_list_changed(stdout: &Arc<TokioMutex<tokio::io::Stdout>>) {
    let frame = json!({
        "jsonrpc": "2.0",
        "method": "notifications/tools/list_changed"
    })
    .to_string();
    let mut out = stdout.lock().await;
    let _ = out.write_all(frame.as_bytes()).await;
    let _ = out.write_all(b"\n").await;
    let _ = out.flush().await;
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
        .map(|n| n as u32)
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
) -> Result<String, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let settings = current_offload_settings();

    if !settings.enabled {
        return Err("offload is disabled — enable it in ccImp settings".into());
    }

    // Resolve the enabled pool.
    let resolved: Vec<ResolvedBackend> = settings
        .effective_backends()
        .iter()
        .filter(|b| b.enabled)
        .filter_map(ResolvedBackend::from_config)
        .collect();
    if resolved.is_empty() {
        return Err(
            "no offload backend is configured — add one in ccImp Settings → Offload".into(),
        );
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
        &client, &settings, &cwd, &resolved[chosen], &views[chosen], &instructions,
        context.clone(), thinking,
    )
    .await;

    // Single re-route on a connection-class failure: drop the failed
    // backend and re-select among the rest (fail-over).
    match result {
        Ok(text) => Ok(text),
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
                    run_on_backend(
                        &client, &settings, &cwd, &resolved[next], &views[next],
                        &instructions, context, thinking,
                    )
                    .await
                }
                _ => Err(e),
            }
        }
        Err(e) => Err(e),
    }
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

    #[test]
    fn parse_run_line_skips_heartbeats_and_blanks() {
        assert!(parse_run_line(b"{\"hb\":true}\n").is_none());
        assert!(parse_run_line(b"   \n").is_none());
        assert!(parse_run_line(b"not json").is_none());
    }

    #[test]
    fn parse_run_line_reads_success_and_error() {
        let ok = parse_run_line(b"{\"ok\":true,\"text\":\"done\"}\n");
        assert_eq!(ok, Some(Ok("done".to_string())));

        let err = parse_run_line(b"{\"ok\":false,\"error\":\"no backend\"}");
        assert_eq!(err, Some(Err("no backend".to_string())));

        // `ok:false` with no message falls back to a generic error.
        assert_eq!(
            parse_run_line(b"{\"ok\":false}"),
            Some(Err("offload failed".to_string()))
        );
    }
}

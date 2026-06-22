//! V8-01 MCP server toward Claude — the `ccimp --offload-mcp` subcommand.
//!
//! A minimal, hand-rolled stdio JSON-RPC 2.0 MCP server (newline-delimited
//! messages on stdin/stdout). Implements `initialize` (declaring
//! `tools.listChanged`), `tools/list`, and `tools/call`, exposing one tool:
//! `offload_task(instructions, context?, thinking?) -> string`.
//!
//! On a call it loads the same layered `settings.json` the app uses
//! (read-only), connects to the **app-owned** `llama-server` over HTTP
//! (it never spawns its own model), runs the agent loop, and returns the
//! synthesized result. Dispatched in `main()` before Tauri init, exactly
//! like `--statusline`, so it stays GUI-free and fast to spawn.
//!
//! MVP scope: native tools only (`read_file`/`code_search`/`run_command`)
//! and config-derived capability reporting. The MCP **host** (aggregating
//! the user's tool servers) and the warm-pool/health-accurate variant are
//! Phase C / the target design.

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::offload::agent::{self, AgentConfig, NativeRouter, OffloadTask, ThinkingMode};
use crate::offload::server::ServerCommand;
use crate::offload::tools::{self, ToolCtx};
use crate::settings::OffloadSettings;

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
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // ignore malformed frames
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        // Notifications (no id) get no response.
        let is_notification = id.is_none();
        let response = handle(method, params).await;

        if is_notification {
            continue;
        }
        let id = id.unwrap_or(Value::Null);
        let frame = match response {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        };
        let mut bytes = frame.to_string();
        bytes.push('\n');
        if stdout.write_all(bytes.as_bytes()).await.is_err() {
            break;
        }
        let _ = stdout.flush().await;
    }
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
        "tools/list" => Ok(json!({ "tools": [offload_task_tool()] })),
        "tools/call" => handle_tools_call(params).await,
        _ => Err((-32601, format!("method not found: {method}"))),
    }
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
                }
            },
            "required": ["instructions"]
        }
    })
}

/// Render the capability line from the enabled native tools (MVP:
/// config-derived; Phase C adds healthy MCP servers).
fn offload_task_description(settings: &OffloadSettings) -> String {
    let mut caps: Vec<&str> = Vec::new();
    if settings.tools.code_search {
        caps.push("code search");
    }
    if settings.tools.read_file {
        caps.push("file read");
    }
    if settings.tools.run_command && !settings.command_allowlist.is_empty() {
        caps.push("allowlisted commands");
    }
    let avail = if caps.is_empty() {
        "no tools currently enabled".to_string()
    } else {
        caps.join(", ")
    };
    format!(
        "Delegate a token-heavy subtask (broad codebase search, large-file/log summarization) to a \
         local model to conserve this session's context. Pass a self-contained instruction; you get \
         back only the synthesized result. Available now: {avail}."
    )
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
    let thinking = ThinkingMode::parse(args.get("thinking").and_then(|v| v.as_str()).unwrap_or("auto"));

    match run_offload(instructions, context, thinking).await {
        Ok(text) => Ok(json!({ "content": [{ "type": "text", "text": text }] })),
        // A "not ready/busy" condition is returned as a tool result (not a
        // protocol error) so Opus can read it and retry/adapt.
        Err(msg) => Ok(tool_error(&msg)),
    }
}

fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

/// Connect to the app-owned server and run one offload task.
async fn run_offload(
    instructions: String,
    context: Option<String>,
    thinking: ThinkingMode,
) -> Result<String, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let settings = current_offload_settings();

    if !settings.enabled {
        return Err("offload is disabled — enable it in ccImp settings".into());
    }
    if settings.server_command.trim().is_empty() {
        return Err("offload server_command is not configured in ccImp settings".into());
    }
    let cmd = ServerCommand::parse(&settings.server_command).map_err(|e| e.to_string())?;
    let base_url = cmd.base_url();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(settings.offload_timeout_secs.max(30)))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    // Health gate: the app owns the process; if it isn't up, fail soft.
    let health = client
        .get(format!("{base_url}/health"))
        .timeout(Duration::from_secs(5))
        .send()
        .await;
    if health.map(|r| !r.status().is_success()).unwrap_or(true) {
        return Err(
            "offload server is not running — start it in ccImp (Settings → Offload), or enable autostart"
                .into(),
        );
    }

    // Discover the per-slot budget from /props.
    let budget_tokens = discover_budget(&client, &base_url, &cmd, settings.budget_high_water_pct).await;

    let ctx = ToolCtx::new(
        settings.allowed_roots.clone(),
        settings.command_allowlist.clone(),
        &cwd,
    );
    let router = NativeRouter {
        defs: tools::enabled_defs(&settings.tools),
        ctx,
    };
    let cfg = AgentConfig {
        base_url,
        model: None,
        max_steps: settings.max_steps.max(1),
        budget_tokens,
        per_tool_result_token_cap: settings.per_tool_result_token_cap.max(256),
    };
    let task = OffloadTask {
        instructions,
        context,
        thinking,
    };
    let deadline = Instant::now() + Duration::from_secs(settings.offload_timeout_secs.max(30));
    agent::run(&client, &cfg, &router, task, deadline)
        .await
        .map_err(|e| e.to_string())
}

/// GET /props and compute `(n_ctx / np) * high_water`. Returns `None`
/// when the endpoint doesn't report `n_ctx` (the loop then relies on
/// `max_steps`/deadline).
async fn discover_budget(
    client: &reqwest::Client,
    base_url: &str,
    cmd: &ServerCommand,
    high_water_pct: u8,
) -> Option<u32> {
    let v: Value = client
        .get(format!("{base_url}/props"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let n_ctx = v
        .get("default_generation_settings")
        .and_then(|g| g.get("n_ctx"))
        .and_then(|x| x.as_u64())
        .or_else(|| v.get("n_ctx").and_then(|x| x.as_u64()))? as u32;
    let per_slot = n_ctx / cmd.parallel.max(1);
    Some(per_slot.saturating_mul(high_water_pct.min(100) as u32) / 100)
}

/// Load just the offload block from the layered settings, read-only.
fn current_offload_settings() -> OffloadSettings {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::settings::load_readonly(&cwd).offload
}

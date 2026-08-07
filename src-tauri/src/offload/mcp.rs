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

use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::offload::toolclass::Profile;
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

/// V30 Phase A: what the MCP client told us about itself at `initialize`.
///
/// Its one consumer is the stderr handshake line in [`record_client_init`] —
/// the line one looks for in the host's MCP server log when a push goes
/// missing. The client's declared `capabilities` are deliberately NOT stored:
/// the review of Phase B found the stated rationale ("Phase B reads it to
/// decide whether a push has anywhere to land") to be false — the child gates
/// pushes on [`CHANNEL_DECLARED`], its own server-side declaration, because
/// Claude Code never advertises channel support on the client side at all.
#[derive(Debug, Clone)]
struct ClientInit {
    /// The client's `clientInfo` object verbatim (`{name, version, …}`), or
    /// `Value::Null` when the client sent none.
    client_info: Value,
}

impl ClientInit {
    /// `clientInfo.name`, when the client sent one.
    fn name(&self) -> Option<&str> {
        self.client_info.get("name").and_then(Value::as_str)
    }

    /// `clientInfo.version`, when the client sent one.
    fn version(&self) -> Option<&str> {
        self.client_info.get("version").and_then(Value::as_str)
    }
}

/// The client's `initialize` params, recorded once. `OnceLock` (like [`CONSUMER`]
/// / [`TAB`]) because MCP allows exactly one `initialize` per connection: a
/// second one is a protocol violation and must not be able to rewrite what the
/// session was established with.
static CLIENT_INIT: std::sync::OnceLock<ClientInit> = std::sync::OnceLock::new();

/// Record the client's `initialize` params. Idempotent — a duplicate
/// `initialize` (protocol violation) leaves the first record standing.
fn record_client_init(params: &Value) {
    let init = ClientInit {
        client_info: params.get("clientInfo").cloned().unwrap_or(Value::Null),
    };
    let name = init.name().unwrap_or("<unknown>").to_string();
    let version = init.version().unwrap_or("<unknown>").to_string();
    let protocol = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("<unset>")
        .to_string();
    if CLIENT_INIT.set(init).is_ok() {
        // `eprintln!`, not `tracing!`: `main` dispatches `--offload-mcp` BEFORE
        // `logging::init`, so this process has no subscriber and a `tracing`
        // event would go nowhere. stderr is where the child's diagnostics
        // actually land — the host agent captures it in its MCP server log,
        // which is exactly where one looks when a channel registration
        // misbehaves. (stdout is the JSON-RPC pipe and must never be written
        // to directly.)
        eprintln!(
            "cimp-offload: MCP client initialized — {name} {version} \
             (protocol {protocol}, consumer {})",
            consumer()
        );
    }
}

/// What the client sent at `initialize`, or `None` before the handshake.
/// Test-only: the production consumer of [`CLIENT_INIT`] is the one-shot
/// stderr line inside [`record_client_init`], and this accessor exists so the
/// write-once contract (a duplicate `initialize` must not rewrite the record)
/// stays pinned by a test rather than by prose.
#[cfg(test)]
fn client_init() -> Option<&'static ClientInit> {
    CLIENT_INIT.get()
}

// ── V30 Phase B: handshake facts the `/events` relay needs ─────────────────

/// Whether this child ACTUALLY put `capabilities.experimental["claude/channel"]`
/// on the wire at `initialize`.
///
/// Recorded from the handshake itself rather than re-derived at use time: the
/// half that matters is what the *client* was told. A push to a host that never
/// negotiated channels is silently dropped client-side (Phase 0, T6), so this
/// flag is what stops the child manufacturing a notification into the void.
static CHANNEL_DECLARED: AtomicBool = AtomicBool::new(false);

/// Whether the `initialize` handler has run at all. Also the write-once claim
/// token for [`record_channel_declaration`] (`compare_exchange`, not
/// load-then-store: two interleaved `initialize` frames must not be able to
/// both pass the check and let the loser's decision win).
static INITIALIZE_SEEN: AtomicBool = AtomicBool::new(false);

/// Whether the `initialize` RESPONSE has been written to stdout. Distinct from
/// [`INITIALIZE_SEEN`] on purpose — see [`release_initialize_relay`].
static INITIALIZE_ANSWERED: AtomicBool = AtomicBool::new(false);

/// Wake-up for [`await_initialize`], signalled once the handshake reply is out.
fn initialize_notify() -> &'static tokio::sync::Notify {
    static NOTIFY: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

/// Record the handshake outcome. Write-once: MCP allows exactly one
/// `initialize` per connection, and a second one must not be able to flip the
/// capability the session was actually established with — the relay bakes that
/// answer into its `channels=` subscription.
///
/// The claim is a `compare_exchange` on [`INITIALIZE_SEEN`] because
/// `mcp_stdio::serve` spawns every request on its own task: two `initialize`
/// frames in flight together would both clear a plain load-then-store check,
/// and the second writer's decision would silently overwrite the first's.
/// [`CHANNEL_DECLARED`] is stored after the claim succeeds, which is safe
/// because nothing reads it before [`release_initialize_relay`] runs — strictly
/// after this function returns, on the same task.
fn record_channel_declaration(declared: bool) {
    if INITIALIZE_SEEN
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return; // a handshake is already on record
    }
    CHANNEL_DECLARED.store(declared, Ordering::Release);
}

/// Release the `/events` relay — called from `mcp_stdio::serve`'s
/// post-response hook once the `initialize` REPLY is on the wire.
///
/// Separate from [`record_channel_declaration`] to keep JSON-RPC ordering: the
/// handler returns strictly before its response is serialized, so releasing the
/// relay there opened a window in which an unsolicited
/// `notifications/claude/channel` could precede the handshake reply on the same
/// pipe. Idempotent, and safe to call for a failed write too (the relay would
/// otherwise idle for the full [`await_initialize`] bound on a dead pipe).
fn release_initialize_relay() {
    INITIALIZE_ANSWERED.store(true, Ordering::Release);
    initialize_notify().notify_waiters();
}

/// Whether a `claude/channel` notification from this child would be honoured.
fn channel_declared() -> bool {
    CHANNEL_DECLARED.load(Ordering::Acquire)
}

/// Block until the `initialize` response has been written (bounded).
///
/// The relay is spawned at [`serve`] start, *before* the client's `initialize`
/// arrives, so connecting immediately would register this child with
/// `channels=0` for the life of that connection — and the app would never push
/// to a tab that is in fact channel-capable. Waiting costs nothing: the
/// handshake is the first frame a client sends, typically within milliseconds.
///
/// Each wake re-checks the flag rather than trusting the wake-up, which closes
/// the race where the release lands between the load and the `notified()`
/// registration (`notify_waiters` only reaches waiters already registered). The
/// bound means a client that never handshakes (or a hand-run child) still gets
/// its `tools/list_changed` relay — just with no channel identity.
async fn await_initialize() {
    const MAX_WAIT: Duration = Duration::from_secs(30);
    const POLL: Duration = Duration::from_millis(100);
    let deadline = Instant::now() + MAX_WAIT;
    while !INITIALIZE_ANSWERED.load(Ordering::Acquire) {
        if Instant::now() >= deadline {
            eprintln!(
                "cimp-offload: no `initialize` within {}s — subscribing to /events without \
                 channel identity",
                MAX_WAIT.as_secs()
            );
            return;
        }
        let _ = tokio::time::timeout(POLL, initialize_notify().notified()).await;
    }
}

/// V30 (M5): whether this child was spawned with `--channel-push`, i.e. cImp
/// decided AT TAB SPAWN that session push is armed for this tab.
///
/// Argv, not a settings read: the client half of the gate (Claude's
/// `--dangerously-load-development-channels`) is composed in the very same
/// overlay at the very same moment, so baking both into argv is what makes them
/// one decision. A fresh settings read at `initialize` would drift — the MCP
/// child can crash-restart at any time and re-handshake against a settings file
/// the running Claude process never saw.
static CHANNEL_PUSH_ARG: AtomicBool = AtomicBool::new(false);

/// Entry point for
/// `cimp --offload-mcp [--consumer <name>] [--tab <tab-id>] [--channel-push]`.
/// Builds a current-thread tokio runtime and serves the stdio loop until stdin
/// closes. `consumer` selects which MCP-server set the app proxies to this child
/// (`claude` default, or `opencode`); `tab` is the cImp tab id this child was
/// spawned for (V28 — forwarded on `/graph_run` so the app can scope the
/// `context_*` memory tools to THIS tab's session); `channel_push` is the V30
/// session-push gate baked in at spawn (see [`CHANNEL_PUSH_ARG`]). Never panics
/// — a crash here would garble the host agent's MCP session.
pub fn run(consumer: &str, tab: Option<&str>, channel_push: bool) {
    let _ = CONSUMER.set(consumer.trim().to_ascii_lowercase());
    // Tab ids are case-sensitive settings keys — store verbatim (trimmed), never
    // lowercased like the consumer name.
    if let Some(t) = tab.map(str::trim).filter(|t| !t.is_empty()) {
        let _ = TAB.set(t.to_string());
    }
    CHANNEL_PUSH_ARG.store(channel_push, Ordering::Release);
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

    // The shared stdio JSON-RPC loop (`mcp_stdio`): spawns each request so
    // multiple in-flight tool calls run concurrently (two parallel
    // `offload_task`s must occupy both llama-server slots at once), captures
    // handler panics as JSON-RPC errors, and stops accepting work once a
    // response write fails (Claude closed the pipe).
    //
    // `after_response` releases the `/events` relay once the `initialize`
    // RESPONSE is on the wire (see [`release_initialize_relay`]) — the relay's
    // first act may be a `notifications/claude/channel`, which must never
    // precede the handshake reply on the same pipe.
    crate::mcp_stdio::serve(stdout, "offload", handle_owned, after_response).await;
}

/// Called by [`crate::mcp_stdio::serve`] once a request's response frame has
/// been written (or its write failed and the pipe is gone). The only method
/// that needs the hook is `initialize`: JSON-RPC ordering says nothing may be
/// written on this connection before the handshake reply, and the `/events`
/// relay's very next write can be an unsolicited notification.
fn after_response(method: &str) {
    if method == "initialize" {
        release_initialize_relay();
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
        "initialize" => {
            // V30 Phase A: the client's `clientInfo` is recorded (it used to be
            // discarded) so the handshake is reported exactly once on stderr —
            // the line one looks for when a push goes missing.
            record_client_init(&params);
            let mut result = json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") }
            });
            // Declare the Claude Code channel capability + system-prompt
            // `instructions` when session push is armed for THIS child (a pure
            // argv read — see `session_push_enabled`).
            let declared = if session_push_enabled() {
                decorate_initialize_channel(&mut result, CHANNEL_INSTRUCTIONS);
                true
            } else {
                false
            };
            if declared {
                // Stderr, for the same reason as `record_client_init` — and
                // the one line that tells a user debugging a missing push
                // whether the SERVER half of the handshake happened at all.
                eprintln!(
                    "cimp-offload: declared the claude/channel capability \
                     (session push armed for consumer {})",
                    consumer()
                );
            }
            // V30 Phase B: publish the handshake outcome — it is both this
            // child's push gate and the `channels=` identity it registers with
            // on `/events` (whose relay is parked in `await_initialize` until
            // exactly this point).
            record_channel_declaration(declared);
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
            Ok(json!({ "tools": tools }))
        }
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name.starts_with("graph_") || name.starts_with("context_") || name == "run_check" {
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

/// V32 Phase A: the shared `profile` parameter schema, identical on
/// `offload_task` and each `offload_batch` subtask so a caller never has to
/// learn two spellings. The `enum` here is advisory only — the value is
/// re-validated post-hoc ([`Profile::parse`]) at both the child and the app
/// parse boundaries.
fn profile_param_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["research", "code"],
        "description": "Optional task shape, for injection containment. 'research' = web/document work: the worker gets web/MCP-server tools and NEVER local file/search/command tools. 'code' = local work: the worker gets local tools and NEVER web/MCP-server tools. Omit and the worker latches on its own first tool call — once it has used one side, the other is unavailable for the rest of the task. Never put secrets or sensitive code in the instructions of a research task: the task prompt is visible to whatever web content the task fetches."
    })
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
                },
                "profile": profile_param_schema()
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
         subtasks; they queue if all slots are busy. Backends: {}.{}{}",
        parts.join("; "),
        routing_note,
        crate::offload::toolclass::PROFILE_TOOL_NOTE,
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
    // V32 Phase A: validate `profile` here, at the child's parse boundary —
    // the declared `enum` in the input schema is an upstream guarantee, and a
    // typo must surface as a tool error rather than silently drop the
    // containment profile.
    let profile = match profile_arg(&args) {
        Ok(p) => p,
        Err(msg) => return Ok(tool_error(&msg)),
    };
    match run_one(
        "offload_task",
        instructions,
        context,
        thinking_str,
        tier_str,
        schema,
        profile,
    )
    .await
    {
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
    // V32 C-1c: which of the two offload tools the caller invoked, forwarded to
    // `/run` for the app's taint gate. `&'static str` so only the two pinned
    // names can reach it.
    tool: &'static str,
    instructions: String,
    context: Option<String>,
    thinking_str: String,
    tier_str: String,
    // V21 F9: optional JSON Schema — forwarded to the warm app (`/run`) or run
    // in the self-contained fallback; constrains the worker's final output.
    schema: Option<serde_json::Value>,
    // V32 Phase A: already validated by the caller's parse boundary; forwarded
    // to the warm app in its canonical spelling and passed straight into the
    // self-contained fallback's agent loop.
    profile: Option<Profile>,
) -> Result<String, String> {
    let thinking = ThinkingMode::parse(&thinking_str);
    let tier = TierHint::parse(&tier_str);
    match proxy_run(
        tool,
        &instructions,
        context.as_deref(),
        &thinking_str,
        &tier_str,
        schema.as_ref(),
        profile,
    )
    .await
    {
        Some(r) => r,
        None => run_offload(instructions, context, thinking, tier, schema, profile).await,
    }
}

/// V32 Phase A: extract and validate the optional `profile` argument of an
/// `offload_task` / `offload_batch`-subtask arguments object. Absent ⇒ `None`
/// (latch dynamically); present-but-not-a-string or an unrecognized value ⇒ a
/// tool-facing error, never a silent fallback. Shared by both tools so they
/// validate identically.
fn profile_arg(args: &Value) -> Result<Option<Profile>, String> {
    match args.get("profile") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Profile::parse(s).map(Some),
        Some(_) => Err(
            "`profile` must be a string — expected \"research\" or \"code\" (omit the argument to \
             let the task latch dynamically on its first tool call)"
                .to_string(),
        ),
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
            // V32 Phase A: per-subtask containment profile, validated here so
            // one bad subtask fails as its own section instead of silently
            // running uncontained (or failing the whole batch).
            let profile = profile_arg(t);
            tokio::spawn(async move {
                if instructions.trim().is_empty() {
                    return Err("subtask requires non-empty `instructions`".to_string());
                }
                let profile = profile?;
                run_one(
                    "offload_batch",
                    instructions,
                    context,
                    thinking_str,
                    tier_str,
                    schema,
                    profile,
                )
                .await
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
        "description": format!(
            "Run several offload subtasks IN PARALLEL across the local worker's slots in a single \
             call. Prefer this over issuing multiple `offload_task` calls when you want real \
             concurrency: separate tool calls are serialized by the MCP client, but this one call \
             fans its subtasks out to the app at once (bounded by the backend's slot count; extras \
             queue). Each subtask is independent and self-contained; you get back all results, one \
             section per subtask, with per-subtask errors inline.{}",
            crate::offload::toolclass::PROFILE_TOOL_NOTE,
        ),
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
                            },
                            "profile": profile_param_schema()
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
///
/// V32 C-1c: the body also carries this child's identity (`tab` + `consumer`)
/// and `tool`, so the app can gate the delegation against the calling tab's
/// taint latch. A containment refusal arrives as `Some(Err(refusal))` — a
/// task-level error, deliberately, so it is NOT retried through the
/// self-contained fallback, which would run the very sub-task the latch just
/// refused.
async fn proxy_run(
    // V32 C-1c: which of the two offload tools the caller invoked, so the app's
    // refusal and its activity row name the tool the model actually called.
    tool: &'static str,
    instructions: &str,
    context: Option<&str>,
    thinking: &str,
    tier: &str,
    schema: Option<&serde_json::Value>,
    profile: Option<Profile>,
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
    let mut body = json!({
        "instructions": instructions,
        "context": context,
        "thinking": thinking,
        "tier": tier,
        "cwd": cwd,
        // V21 F9: forward the JSON Schema so the warm app constrains the final
        // turn. Omitted (null, `serde(default)` on the app side) when unset.
        "schema": schema,
        // V32 Phase A: forward the containment profile in its canonical
        // spelling. The app re-validates it (`Profile::parse` in
        // `loopback::handle_run`) rather than trusting this side.
        "profile": profile.map(Profile::as_str),
        // V32 C-1c: who is delegating. The latch registry is keyed by
        // `(agent, tab)`, so both halves have to ride along or the app gates
        // an OpenCode tab's call against a Claude tab's latch. Sent in the body
        // exactly as `/graph_run` does. `tool` names which of the two offload
        // tools asked — an `offload_batch` fans out to one `/run` per subtask,
        // so the route cannot tell them apart on its own.
        "consumer": consumer(),
        "tool": tool,
    });
    // Omitted entirely when this child has no tab identity, so the body stays
    // byte-identical to the pre-C-1c shape for a hand-run child.
    if let (Some(t), Some(map)) = (tab(), body.as_object_mut()) {
        map.insert("tab".to_string(), Value::String(t.to_string()));
    }
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
    // V28: `tab` identifies which session's memory scope this call belongs to.
    // (V32 Phase B sends it on `/mcp/call` too — not for memory, which external
    // servers have none of, but because the proxy's taint latch is keyed by the
    // same tab identity.) Omitted entirely when this child has no tab identity,
    // so the body stays byte-identical to the pre-V28 shape.
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
    // The child's cwd is the calling session's project root — sent so the
    // app can attribute the Tool Activity row to that project.
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    // V32 Phase B: `tab` rides along so the app can key this call to THIS
    // tab's session taint latch. Same fail-open shape as `/graph_run` — a child
    // spawned without `--tab` omits the field and the app leaves it unlatched
    // (its results are still spotlight-wrapped).
    let mut body = json!({ "name": name, "arguments": args, "cwd": cwd });
    if let (Some(t), Some(map)) = (tab(), body.as_object_mut()) {
        map.insert("tab".to_string(), Value::String(t.to_string()));
    }
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
/// `GET /events` SSE connection and dispatch its frames —
///
/// - `change` → `notifications/tools/list_changed` (the pre-V30 pulse);
/// - `push` → `notifications/claude/channel` (V30 Phase B), built from the
///   semantic payload the app addressed at THIS child's tab.
///
/// Reconnects when the app comes/goes.
async fn events_relay(stdout: Arc<TokioMutex<tokio::io::Stdout>>) {
    // Build the client once and reuse it across reconnects — the old code
    // rebuilt a fresh reqwest::Client on every 2s retry (~1800/hr while the app
    // is down). No client timeout: this is a long-lived streaming connection.
    let Ok(client) = reqwest::Client::builder().build() else {
        return;
    };
    // V30 Phase B: the subscription carries this child's identity, and the
    // `channels` half of it only exists once the handshake has happened. This
    // task is spawned before the client's `initialize` arrives, so park here
    // until it has (bounded — see `await_initialize`).
    await_initialize().await;
    let query = events_query(tab(), consumer(), channel_declared());
    loop {
        if let Some((base, token)) = proxy_base() {
            {
                if let Ok(mut resp) = client
                    .get(format!("{base}/events{query}"))
                    .bearer_auth(&token)
                    .send()
                    .await
                {
                    // Parse the SSE byte stream frame by frame. The parser owns
                    // the chunk-boundary problem end to end (TCP can split
                    // anywhere, including mid-line and mid-frame), which the
                    // pre-V30 byte-marker sniffing only approximated with a
                    // carry buffer.
                    // Bound each read: the app sends an SSE keep-alive every
                    // 20s, so a 60s gap means the connection went half-open
                    // (e.g. the app was hard-killed without a FIN). Break to
                    // reconnect rather than hang here forever — otherwise
                    // list_changed notifications would stop reaching Claude.
                    const READ_IDLE: Duration = Duration::from_secs(60);
                    let mut parser = SseParser::default();
                    while let Ok(Ok(Some(chunk))) =
                        tokio::time::timeout(READ_IDLE, resp.chunk()).await
                    {
                        for frame in parser.feed(&chunk) {
                            dispatch_sse_frame(&stdout, &frame).await;
                        }
                    }
                }
            }
        }
        // App down or stream ended — back off and retry.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// The `?tab=&consumer=&channels=` query this child subscribes to `/events`
/// with. `channels` is `1` only when the capability was really declared, so the
/// app's registry reflects the handshake and not the settings file. Pure, so
/// the exact identity string is unit-testable.
fn events_query(tab: Option<&str>, consumer: &str, channels: bool) -> String {
    let mut q = String::from("?");
    if let Some(t) = tab {
        // No escaping: tab ids are cImp-generated (`[a-z0-9-]`) and the app
        // parses this query without percent-decoding.
        q.push_str("tab=");
        q.push_str(t);
        q.push('&');
    }
    q.push_str("consumer=");
    q.push_str(consumer);
    q.push_str(if channels {
        "&channels=1"
    } else {
        "&channels=0"
    });
    q
}

/// One dispatched SSE frame.
#[derive(Debug, Default, PartialEq, Eq)]
struct SseFrame {
    /// The `event:` field, or SSE's default `message` when the frame carried
    /// none.
    event: String,
    /// The accumulated `data:` payload (multiple `data:` lines joined with
    /// `\n`, per the SSE spec).
    data: String,
}

/// A minimal SSE frame parser, fed arbitrary byte chunks.
///
/// Grammar implemented (the subset the app emits, plus what any conformant
/// producer may legally send):
///
/// ```text
/// stream    := line*
/// line      := field CR? LF
/// field     := ':' comment          -- ignored (the app's ": keep-alive")
///            | name ':' ' '? value  -- one optional leading space is stripped
///            | name                 -- a nameless-value field, value = ""
///            | ''                   -- a blank line DISPATCHES the frame
/// ```
///
/// `event` sets the frame's name (last one wins), `data` appends (joined with
/// `\n`), any other field (`id`, `retry`, …) is ignored. A frame is emitted on
/// the blank line only if at least one of `event`/`data` was seen, so runs of
/// blank lines and leading keep-alives produce nothing. Pure and socket-free —
/// the reason it is a struct rather than inline stream handling.
#[derive(Default)]
struct SseParser {
    /// Bytes of the line currently being accumulated.
    line: Vec<u8>,
    event: Option<String>,
    data: Option<String>,
}

impl SseParser {
    /// Hard cap on a single line. The app's frames are tiny; a line beyond this
    /// means a broken or hostile producer, and truncating (rather than growing
    /// without bound) keeps this child's memory bounded. The truncated line
    /// simply fails to parse into a usable push.
    const MAX_LINE: usize = 1 << 20;

    /// Feed one chunk, returning every frame it completed.
    fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        let mut out = Vec::new();
        for &b in chunk {
            if b == b'\n' {
                let raw = std::mem::take(&mut self.line);
                if let Some(frame) = self.end_line(&raw) {
                    out.push(frame);
                }
            } else if self.line.len() < Self::MAX_LINE {
                self.line.push(b);
            }
        }
        out
    }

    /// Consume one complete line; `Some` when it terminated a frame.
    fn end_line(&mut self, raw: &[u8]) -> Option<SseFrame> {
        // Lossy: a mangled byte must not kill an otherwise-healthy stream (the
        // same reasoning as `mcp_stdio::serve`'s invalid-UTF-8 handling).
        let line = String::from_utf8_lossy(raw);
        let line = line.strip_suffix('\r').unwrap_or(&line);

        if line.is_empty() {
            // Blank line: dispatch whatever has accumulated.
            let (event, data) = (self.event.take(), self.data.take());
            if event.is_none() && data.is_none() {
                return None;
            }
            return Some(SseFrame {
                event: event.unwrap_or_else(|| "message".to_string()),
                data: data.unwrap_or_default(),
            });
        }
        if line.starts_with(':') {
            return None; // comment / keep-alive
        }
        let (name, value) = match line.split_once(':') {
            Some((n, v)) => (n, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match name {
            "event" => self.event = Some(value.to_string()),
            "data" => match &mut self.data {
                Some(existing) => {
                    existing.push('\n');
                    existing.push_str(value);
                }
                None => self.data = Some(value.to_string()),
            },
            _ => {}
        }
        None
    }
}

/// Turn a `push` frame's `data` payload into `notifications/claude/channel`
/// params, or `None` when it is unusable.
///
/// Both checks the parse itself now makes (#47): `PushNotice`'s `Deserialize`
/// runs a validating `TryFrom`, which rejects empty content outright — an empty
/// `<channel>` message would cost the session a turn and say nothing ("empty is
/// not absent") — and drops any meta key the client would silently discard. The
/// two halves of this wire are different processes and can be different builds
/// (a child outlives a settings change, and an old exe can be talking to a new
/// app), so this stays a *parse-boundary* guarantee rather than an assumption
/// about the sender. A rejected payload lands here as `None`.
fn channel_params(data: &str) -> Option<Value> {
    let notice: crate::offload::service::PushNotice = serde_json::from_str(data).ok()?;
    let content = notice.content().to_string();
    let meta: serde_json::Map<String, Value> = notice
        .meta
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    Some(json!({ "content": content, "meta": Value::Object(meta) }))
}

/// Relay one parsed SSE frame to the host agent. Unknown event names are
/// ignored (forward compatibility: a newer app may emit frames this child
/// predates).
async fn dispatch_sse_frame(stdout: &Arc<TokioMutex<tokio::io::Stdout>>, frame: &SseFrame) {
    match frame.event.as_str() {
        "change" => emit_list_changed(stdout).await,
        "push" => {
            if !channel_declared() {
                // The app addressed us, but this session never negotiated
                // channels — the client would drop the notification silently,
                // so say so where a user debugging a missing push will see it.
                eprintln!(
                    "cimp-offload: dropped a session push — this connection never declared \
                     the claude/channel capability (enable offload.session_push and restart \
                     the tab)"
                );
                return;
            }
            match channel_params(&frame.data) {
                Some(params) => {
                    emit_notification(stdout, "notifications/claude/channel", Some(params)).await
                }
                None => eprintln!(
                    "cimp-offload: dropped an unusable session push payload ({} bytes)",
                    frame.data.len()
                ),
            }
        }
        _ => {}
    }
}

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

// ── V30 Phase A: session-push capability declaration ───────────────────────

/// The system-prompt `instructions` block injected alongside the channel
/// capability when `offload.session_push` is on.
///
/// It tells the model what a `<channel source="cimp-offload">` message is and
/// how to treat one. The "do not invent" clause is deliberate: a channel
/// message is a plain user-role message from the model's point of view, so
/// without it the pattern is trivially imitable in the model's own output.
const CHANNEL_INSTRUCTIONS: &str = "cimp-offload may push out-of-band notices into this session as <channel source=\"cimp-offload\"> messages — completion notices from the local toolchain (offloaded tasks, code audits, graph indexing). When one arrives, take it into account: act on it if it is relevant to the current task, otherwise acknowledge it briefly. Do not invent channel messages; only react to ones actually delivered.";

/// Add `capabilities.experimental["claude/channel"]` + the top-level
/// `instructions` string to an otherwise-unchanged `initialize` result.
///
/// Pure, so a test can pin both the addition and the untouched base — notably
/// `protocolVersion`, which MUST stay on the legacy `2025-06-18` era where the
/// client honours channels (milestone invariant 1), and `tools.listChanged`.
fn decorate_initialize_channel(result: &mut Value, instructions: &str) {
    result["capabilities"]["experimental"] = json!({ "claude/channel": {} });
    result["instructions"] = Value::String(instructions.to_string());
}

/// Whether this child should declare the channel capability.
///
/// Two gates:
///   * **consumer** — channels are a Claude Code mechanism; an OpenCode child
///     (`--consumer opencode`) has no inbound MCP path at all (see the
///     milestone's Phase D), so it must never declare one.
///   * **argv** — `--channel-push`, baked in at tab spawn (see
///     [`CHANNEL_PUSH_ARG`]).
///
/// It reads NO settings, deliberately. `offload.session_push` is evaluated once
/// per tab spawn, in `tabs/config.rs::build_pre_args`, where the very same
/// predicate also decides whether Claude gets
/// `--dangerously-load-development-channels`. Both halves of the gate therefore
/// come from one read of one snapshot and can only change together, on a tab
/// restart (which `spawn_inject_sig`'s `"channels"` entry raises a hint for).
/// The pre-fix code re-read the settings files here instead, so a child
/// crash-restart after a settings toggle desynced the halves: the child
/// declared the capability and subscribed `channels=1` while the running Claude
/// process had never registered it — every push then silently dropped
/// client-side but counted as delivered app-side.
fn session_push_enabled() -> bool {
    consumer() == "claude" && CHANNEL_PUSH_ARG.load(Ordering::Acquire)
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
    // V32 Phase A: threaded into every `run_on_backend` below (first attempt,
    // fail-over re-route, and tier escalation) so the containment profile
    // survives a re-run — a re-routed research task must not come back
    // uncontained.
    profile: Option<Profile>,
) -> Result<String, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // V32 Phase G: the WHOLE layered snapshot, not just its offload block — the
    // injection resolver reads across it (per-tab L3 rows live on `tabs`), and
    // this fallback path must resolve the same three-level hierarchy the app
    // process does rather than quietly running unswitched.
    let full = crate::settings::load_readonly(&cwd);
    let settings = full.offload.clone();

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
        &full,
        &cwd,
        &resolved[chosen],
        &views[chosen],
        &instructions,
        context.clone(),
        thinking,
        schema.clone(),
        profile,
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
                        &full,
                        &cwd,
                        &resolved[next],
                        &views[next],
                        &instructions,
                        context.clone(),
                        thinking,
                        schema.clone(),
                        profile,
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
                &full,
                &cwd,
                &resolved[q],
                &views[q],
                &instructions,
                context.clone(),
                thinking,
                schema.clone(),
                profile,
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
    // V32 Phase G: the full settings snapshot `settings` was taken from, for
    // the injection resolver (which reads across blocks). Passed beside the
    // offload half rather than replacing it so the existing `settings.…` reads
    // in this function stay as they were.
    all: &crate::settings::Settings,
    cwd: &std::path::Path,
    backend: &ResolvedBackend,
    view: &BackendView,
    instructions: &str,
    context: Option<String>,
    thinking: ThinkingMode,
    schema: Option<serde_json::Value>,
    profile: Option<Profile>,
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
        // V32 Phase C: the app-not-running fallback runs a NativeRouter — no
        // MCP host, so no EXTERNAL tool is reachable and the budget is inert
        // here. It is still filled from the user's settings rather than a
        // hardcoded value, so the two paths can never disagree if this one ever
        // grows a host.
        task_scope: crate::offload::outbound::new_task_scope(),
        // V32 Phase G: resolved at the `offload-worker` pseudo-scope, like the
        // in-app path, so the master switch reaches this fallback too.
        external_budget: crate::settings::injection::budget_limits(
            all,
            crate::settings::injection::Scope::OffloadWorker,
        ),
        latch_active: crate::settings::injection::effective(
            crate::settings::injection::Feature::TaintLatch,
            crate::settings::injection::Scope::OffloadWorker,
            all,
        ),
        canary_active: crate::settings::injection::effective(
            crate::settings::injection::Feature::Canary,
            crate::settings::injection::Scope::OffloadWorker,
            all,
        ),
    };
    let task = OffloadTask {
        instructions: instructions.to_string(),
        context: context.clone(),
        thinking,
        schema: schema.clone(),
        profile,
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
                profile,
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

    // ── V30 Phase A: session push ────────────────────────────────────────

    /// A base `initialize` result, exactly as the handler builds it.
    fn base_initialize() -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": { "name": SERVER_NAME, "version": "test" }
        })
    }

    /// The production decoration adds the experimental channel capability and
    /// the `instructions` block WITHOUT disturbing the rest of the handshake.
    /// `protocolVersion` is the load-bearing one (milestone invariant 1): the
    /// client skips channel registration entirely on the modern MCP era, so a
    /// bump here would silently kill session push.
    #[test]
    fn channel_decoration_preserves_the_legacy_handshake() {
        let mut result = base_initialize();
        decorate_initialize_channel(&mut result, CHANNEL_INSTRUCTIONS);
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], json!(true));
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert!(result["capabilities"]["experimental"]["claude/channel"].is_object());
        assert_eq!(result["instructions"], json!(CHANNEL_INSTRUCTIONS));
    }

    /// An undecorated handshake carries neither key — the default-off setting
    /// must be a byte-identical, pre-V30 `initialize` result.
    #[test]
    fn undecorated_initialize_declares_no_channel() {
        let result = base_initialize();
        assert!(result["capabilities"].get("experimental").is_none());
        assert!(result.get("instructions").is_none());
    }

    /// The production `instructions` text must name the exact wire form the
    /// model will see, describe what to do with a notice, and forbid inventing
    /// them (a channel message is an ordinary user-role message, so the shape
    /// is trivially imitable).
    #[test]
    fn channel_instructions_cover_the_delivery_contract() {
        assert!(CHANNEL_INSTRUCTIONS.contains("<channel source=\"cimp-offload\">"));
        assert!(CHANNEL_INSTRUCTIONS.contains("Do not invent channel messages"));
    }

    /// V30 Phase A: the client's `clientInfo` is no longer discarded — it is
    /// what the one-shot stderr handshake line reports. `record_client_init`
    /// runs against a process-wide `OnceLock`, so this test exercises the
    /// *parse*.
    #[test]
    fn client_init_parses_client_info() {
        let params = json!({
            "protocolVersion": "2025-06-18",
            "clientInfo": { "name": "claude-code", "version": "2.1.222" },
            "capabilities": { "roots": { "listChanged": true } }
        });
        let init = ClientInit {
            client_info: params.get("clientInfo").cloned().unwrap_or(Value::Null),
        };
        assert_eq!(init.name(), Some("claude-code"));
        assert_eq!(init.version(), Some("2.1.222"));

        // A client that sends none must not panic or fabricate values.
        let bare = ClientInit {
            client_info: Value::Null,
        };
        assert_eq!(bare.name(), None);
        assert_eq!(bare.version(), None);
    }

    /// `record_client_init` must never rewrite a recorded handshake: MCP
    /// permits exactly one `initialize` per connection, and a second one is a
    /// protocol violation, not a re-negotiation.
    #[test]
    fn record_client_init_is_write_once() {
        record_client_init(&json!({ "clientInfo": { "name": "first" } }));
        record_client_init(&json!({ "clientInfo": { "name": "second" } }));
        assert_eq!(client_init().and_then(ClientInit::name), Some("first"));
    }

    // ── V30 Phase B: /events identity + the SSE frame parser ─────────────

    /// The subscription identity the app's registry keys on. `channels`
    /// reflects the HANDSHAKE, so it is always present and explicit — a `0` is
    /// meaningfully different from the pre-V30 child's absent param.
    #[test]
    fn events_query_carries_tab_consumer_and_channels() {
        assert_eq!(
            events_query(Some("claude-2"), "claude", true),
            "?tab=claude-2&consumer=claude&channels=1"
        );
        assert_eq!(
            events_query(None, "opencode", false),
            "?consumer=opencode&channels=0"
        );
    }

    /// The handshake fact the relay subscribes with is recorded once and never
    /// re-negotiated — a duplicate `initialize` (a protocol violation) must not
    /// flip the capability after the relay has baked it into its `channels=`
    /// subscription. The claim is a `compare_exchange`, so two frames racing
    /// through `mcp_stdio::serve`'s per-request spawn can't both win. This is
    /// the only test that touches these process-wide statics.
    #[test]
    fn channel_declaration_is_write_once() {
        record_channel_declaration(true);
        record_channel_declaration(false);
        assert!(channel_declared());
        assert!(INITIALIZE_SEEN.load(Ordering::Acquire));

        // The relay stays parked until the initialize RESPONSE is on the wire —
        // recording the decision alone must not release it (JSON-RPC ordering:
        // nothing precedes the handshake reply on this pipe).
        assert!(
            !INITIALIZE_ANSWERED.load(Ordering::Acquire),
            "recording the handshake must not release the /events relay"
        );
        release_initialize_relay();
        assert!(INITIALIZE_ANSWERED.load(Ordering::Acquire));
    }

    /// The gate the child declares on is pure argv (`--channel-push`), never a
    /// settings read: the client half (`--dangerously-load-development-channels`)
    /// is composed from the same snapshot at the same moment, so a child
    /// crash-restart after a settings toggle cannot desync the two halves.
    #[test]
    fn session_push_gate_is_argv_only() {
        // Same process-wide statics as above; `consumer()` defaults to "claude"
        // when `run` never set it, which is the case in tests.
        CHANNEL_PUSH_ARG.store(false, Ordering::Release);
        assert!(!session_push_enabled(), "no --channel-push ⇒ no declaration");
        CHANNEL_PUSH_ARG.store(true, Ordering::Release);
        assert!(session_push_enabled());
        CHANNEL_PUSH_ARG.store(false, Ordering::Release);
    }

    /// Feed the parser one byte at a time: no frame may be lost or duplicated
    /// when TCP splits mid-line, mid-frame, or between `\r` and `\n`.
    #[test]
    fn sse_parser_survives_arbitrary_chunk_boundaries() {
        let stream = "event: change\ndata: {}\n\nevent: push\ndata: {\"content\":\"hi\"}\n\n";
        let mut whole = SseParser::default();
        let whole_frames = whole.feed(stream.as_bytes());

        let mut split = SseParser::default();
        let mut split_frames = Vec::new();
        for b in stream.as_bytes() {
            split_frames.extend(split.feed(&[*b]));
        }
        assert_eq!(whole_frames, split_frames);
        assert_eq!(whole_frames.len(), 2);
        assert_eq!(whole_frames[0].event, "change");
        assert_eq!(whole_frames[0].data, "{}");
        assert_eq!(whole_frames[1].event, "push");
        assert_eq!(whole_frames[1].data, "{\"content\":\"hi\"}");
    }

    /// Comments (the app's `: connected` prime and its 20 s `: keep-alive`s)
    /// and stray blank lines must produce no frames at all — a keep-alive that
    /// dispatched an empty frame would spam `tools/list_changed`.
    #[test]
    fn sse_parser_ignores_comments_and_keepalives() {
        let mut p = SseParser::default();
        assert!(p.feed(b": connected\n\n").is_empty());
        assert!(p.feed(b": keep-alive\n\n").is_empty());
        assert!(p.feed(b"\n\n\n").is_empty());
        // …and a real frame still lands after them.
        let frames = p.feed(b": keep-alive\n\nevent: change\ndata: {}\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "change");
    }

    /// The rest of the grammar: CRLF terminators, multiple `data:` lines joined
    /// with `\n`, ignored fields (`id`/`retry`), a value-less field, an absent
    /// `event:` defaulting to `message`, and only ONE optional space stripped.
    #[test]
    fn sse_parser_grammar_details() {
        let mut p = SseParser::default();
        let frames = p.feed(
            b"id: 7\r\nretry: 3000\r\nevent: push\r\ndata: one\r\ndata:  two\r\ndata\r\n\r\n",
        );
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "push");
        assert_eq!(frames[0].data, "one\n two\n");

        let mut p = SseParser::default();
        let frames = p.feed(b"data: bare\n\n");
        assert_eq!(frames[0].event, "message", "SSE's default event name");
        assert_eq!(frames[0].data, "bare");
    }

    /// The child owns the JSON-RPC framing: the app sends only the semantic
    /// payload, and this is where it becomes `notifications/claude/channel`
    /// params.
    #[test]
    fn channel_params_builds_the_notification_payload() {
        let params = channel_params(r#"{"content":"audit done","meta":{"kind":"audit"}}"#).unwrap();
        assert_eq!(params["content"], "audit done");
        assert_eq!(params["meta"]["kind"], "audit");
        let frame = notification_frame("notifications/claude/channel", Some(params));
        assert_eq!(frame["method"], "notifications/claude/channel");
        assert_eq!(frame["params"]["content"], "audit done");

        // A push with no meta still carries an (empty) meta object.
        let bare = channel_params(r#"{"content":"hi"}"#).unwrap();
        assert!(bare["meta"].as_object().is_some_and(|m| m.is_empty()));
    }

    /// Re-validation at the child's parse boundary: keys the client would drop
    /// silently are dropped here (visibly), and a content-less push is refused
    /// rather than costing the session a turn to say nothing.
    #[test]
    fn channel_params_rejects_unusable_payloads() {
        let filtered =
            channel_params(r#"{"content":"x","meta":{"ok_1":"a","bad-key":"b","2nd":"c"}}"#)
                .unwrap();
        let meta = filtered["meta"].as_object().unwrap();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta["ok_1"], "a");

        assert!(channel_params(r#"{"content":"   "}"#).is_none());
        assert!(channel_params(r#"{"content":""}"#).is_none());
        assert!(channel_params("not json").is_none());
        assert!(channel_params(r#"{"meta":{"kind":"x"}}"#).is_none());
    }

    /// The two halves of the wire format must agree byte for byte: what the app
    /// writes into an `event: push` frame is what this child parses out of it.
    #[test]
    fn app_push_frame_round_trips_through_the_parser() {
        let notice = crate::offload::service::PushNotice::new(
            "multi\nline\ncontent",
            &[],
            [("kind", "audit_done"), ("seq", "3")],
        );
        let bytes = crate::offload::loopback::push_frame(&notice);
        let mut p = SseParser::default();
        let frames = p.feed(&bytes);
        assert_eq!(frames.len(), 1, "one frame, whatever the content contains");
        assert_eq!(frames[0].event, "push");
        let params = channel_params(&frames[0].data).unwrap();
        assert_eq!(params["content"], "multi\nline\ncontent");
        assert_eq!(params["meta"]["kind"], "audit_done");
        assert_eq!(params["meta"]["seq"], "3");

        // …and the frame the child finally writes satisfies the client's meta
        // contract: keys `^[a-zA-Z_][a-zA-Z0-9_]*$` (others silently dropped)
        // with STRING values. Pinned end to end from a real producer notice —
        // this is what the Phase 0 spike rig used to verify by hand.
        let frame = notification_frame("notifications/claude/channel", Some(params));
        assert_eq!(frame["method"], "notifications/claude/channel");
        let meta = frame["params"]["meta"].as_object().unwrap();
        assert!(!meta.is_empty());
        for (k, v) in meta {
            assert!(
                crate::offload::service::valid_meta_key(k),
                "meta key `{k}` would be dropped by the client"
            );
            assert!(v.is_string(), "meta value for `{k}` must be a string");
        }
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

    // ── V32 Phase A — the `profile` param ──────────────────────────────────

    /// Both tools advertise `profile` with the same shape, and both carry the
    /// secrets warning in their description (the accepted-residual note from
    /// locked decision 4 — a research task's prompt is visible to whatever it
    /// fetches).
    #[test]
    fn tool_descriptors_advertise_profile_and_the_secrets_warning() {
        let single = offload_task_tool();
        assert_eq!(
            single["inputSchema"]["properties"]["profile"],
            profile_param_schema(),
            "offload_task must advertise the optional profile param"
        );
        let batch = offload_batch_tool();
        assert_eq!(
            batch["inputSchema"]["properties"]["tasks"]["items"]["properties"]["profile"],
            profile_param_schema(),
            "offload_batch subtasks must advertise the optional profile param"
        );
        assert_eq!(
            profile_param_schema()["enum"],
            json!(["research", "code"]),
            "the advertised enum must match `Profile::parse`"
        );
        let batch_desc = batch["description"].as_str().unwrap();
        assert!(
            batch_desc.contains("NEVER include secrets"),
            "offload_batch description lost the secrets warning"
        );
        // `offload_task`'s description is rendered from live settings (and may
        // be replaced by the app's `/describe`), so pin the shared note there
        // rather than the whole rendered string.
        assert!(crate::offload::toolclass::PROFILE_TOOL_NOTE.contains("NEVER include secrets"));
    }

    /// Validation at the parse boundary: absent ⇒ no profile, a known value
    /// parses, and anything else is a tool-facing error rather than a silent
    /// fallback to "no containment".
    #[test]
    fn profile_arg_validates_at_the_parse_boundary() {
        assert_eq!(profile_arg(&json!({ "instructions": "x" })).unwrap(), None);
        assert_eq!(profile_arg(&json!({ "profile": null })).unwrap(), None);
        assert_eq!(
            profile_arg(&json!({ "profile": "research" })).unwrap(),
            Some(Profile::Research)
        );
        assert_eq!(
            profile_arg(&json!({ "profile": "code" })).unwrap(),
            Some(Profile::Code)
        );
        // Unknown value: rejected, never defaulted.
        let err = profile_arg(&json!({ "profile": "web" })).unwrap_err();
        assert!(err.contains("invalid `profile`"), "err: {err}");
        // Wrong JSON type: also rejected (the schema `type` is not trusted).
        let err = profile_arg(&json!({ "profile": 3 })).unwrap_err();
        assert!(err.contains("must be a string"), "err: {err}");
    }

    /// Mirror of `handle_batch_tool`'s per-subtask parse: an invalid profile
    /// fails only ITS subtask (as that section's error), while its siblings
    /// still carry their own validated profiles.
    #[test]
    fn batch_threads_per_subtask_profiles_and_isolates_a_bad_one() {
        let tasks = json!([
            { "instructions": "research it", "profile": "research" },
            { "instructions": "read it", "profile": "code" },
            { "instructions": "no profile" },
            { "instructions": "bad", "profile": "wat" },
        ]);
        let parsed: Vec<Result<Option<Profile>, String>> = tasks
            .as_array()
            .unwrap()
            .iter()
            .map(profile_arg)
            .collect();
        assert_eq!(parsed[0], Ok(Some(Profile::Research)));
        assert_eq!(parsed[1], Ok(Some(Profile::Code)));
        assert_eq!(parsed[2], Ok(None));
        assert!(parsed[3].is_err());
    }
}

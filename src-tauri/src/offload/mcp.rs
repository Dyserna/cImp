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

use super::loopback::{forget_resolved_discovery, parse_result_line, proxy_base_for, ChildIdentity};

/// Root-aware endpoint resolution for this child: its cwd is the agent's
/// project directory (inherited at spawn — the injected mcp-config sets no
/// cwd), so with several cImp instances off one install the child connects
/// to the instance actually serving ITS project, not the last one launched.
///
/// Since locked decision 30 (#48 F-11) the resolution is also **liveness
/// verified** — a candidate has to answer a token-authenticated `GET /health` —
/// and memoized per process, so this stays a cheap call on the per-tool-call path
/// it sits on. [`forget_resolved_discovery`] is what keeps the memo from
/// outliving the instance; see [`proxy_graph`].
fn proxy_base() -> Option<(String, String)> {
    proxy_base_for(
        std::env::current_dir().ok().as_deref(),
        // #48 F-32: who this child is, so a skipped (possibly planted) discovery
        // entry can be reported to the app as an activity row rather than only
        // to this child's stderr. Both values are cImp-authored argv.
        ChildIdentity {
            consumer: consumer(),
            tab: tab(),
        },
    )
}

use crate::offload::agent::{self, AgentConfig, NativeRouter, OffloadTask, ThinkingMode};
use crate::offload::router::{self, BackendView, RouteError, TierHint};
use crate::offload::server::{per_slot_n_ctx, ServerCommand};
use crate::offload::toolclass::Profile;
use crate::offload::tools::{self, ToolCtx};
use crate::settings::{
    BackendTier, OffloadBackend, OffloadBackendKind, OffloadSettings, ToolScope,
};

/// The MCP protocol version cImp itself speaks. A CONSUMER may pin another —
/// `HarnessPlugin::mcp_protocol_version` — because "which era does this client
/// honour" is a fact about the client (V40 Phase E, locked decision 25).
const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "cimp-offload";

/// The consumer this child serves (from `--consumer <name>`, default
/// `"claude"`). Threaded onto the loopback `/mcp/*` queries so the app returns
/// the right per-consumer MCP-server tool set. Set once at startup.
static CONSUMER: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The harness behind [`consumer`], or `None` when this child serves one of
/// cImp's own in-app consumers (`offload`, `audit`) or a token nobody
/// registered.
///
/// V40 Phase E: the ONE lookup the MCP-client specifics go through. Everything
/// Claude-shaped about this handshake — the `claude/channel` capability key, the
/// `notifications/claude/channel` method, the protocol-era pin — is now asked of
/// this harness's plugin instead of written by core for whoever happened to have
/// push armed.
fn consumer_harness() -> Option<crate::harness::HarnessId> {
    crate::harness::HarnessId::from_consumer(consumer())
}

/// V28 (issue #13): the cImp TAB this child was spawned for (from
/// `--tab <tab-id>`). One `--offload-mcp` child runs per tab and its argv is
/// composed entirely by cImp, so the tab identity can be baked in at spawn —
/// which is what lets the app resolve *which* session of this agent the
/// `context_*` memory tools should scope to. Unset for a child spawned by hand
/// (or by a pre-V28 cImp), which fails open to the old behavior.
static TAB: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The configured consumer name, lowercased; [`DEFAULT_HARNESS`] when unset —
/// a child spawned before the flag existed, see that constant's doc comment.
///
/// [`DEFAULT_HARNESS`]: crate::harness::DEFAULT_HARNESS
fn consumer() -> &'static str {
    CONSUMER.get().map(String::as_str).unwrap_or_else(|| crate::harness::DEFAULT_HARNESS
        .id()
        .expect("DEFAULT_HARNESS names a registered harness"))
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
            let harness = consumer_harness();
            // The consumer's own protocol pin, if it has one (locked decision
            // 25). Claude Code honours channel notifications only in the
            // `2025-06-18` era; a harness that pins nothing gets cImp's version.
            let protocol = harness
                .and_then(|h| h.plugin())
                .and_then(|p| p.mcp_protocol_version())
                .unwrap_or(PROTOCOL_VERSION);
            let mut result = json!({
                "protocolVersion": protocol,
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") }
            });
            // Declare the Claude Code channel capability + system-prompt
            // `instructions` when session push is armed for THIS child (a pure
            // argv read — see `session_push_enabled`).
            let declared = if session_push_enabled() {
                decorate_initialize_channel(&mut result, harness);
                true
            } else {
                false
            };
            if declared {
                // Stderr, for the same reason as `record_client_init` — and
                // the one line that tells a user debugging a missing push
                // whether the SERVER half of the handshake happened at all.
                eprintln!(
                    "cimp-offload: declared the session-push channel capability \
                     (armed for consumer {})",
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
            // V39 Phase B (locked decision 3): one `delegate_task_<harness>`
            // per harness whose Manual tab exists, is not this child's own tab
            // and passes the worker gate. NOT inside the `offload.enabled`
            // guard: delegation drives a harness tab and needs no offload
            // backend, so tying it to that switch would hide it for exactly the
            // users who never configured a local model.
            tools.extend(delegate_task_tools(tab()));
            // V9-01 code-knowledge-graph tools (present only when the graph
            // feature is enabled for this project).
            //
            // V38 F-3: built FOR THIS CONSUMER. `run_command`'s advertisement is
            // per-consumer (Settings → Tool Plugins has one switch for Claude
            // Code and one for OpenCode), and this child knows which one it
            // serves from its own `--consumer` argv — the same identity it
            // already passes on every `tools/call`.
            tools.extend(crate::graph::mcp_tools_for(consumer()));
            // Claude-Code-exposed MCP servers (those with `claude_access`),
            // proxied through the app's warm host. Empty when the app is down.
            tools.extend(proxy_mcp_list().await);
            Ok(json!({ "tools": tools }))
        }
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            // V38 F-3 adds `run_command` to this set. It is not a graph tool and
            // never was one — like `run_check` it simply lives on this dispatch
            // surface, and it MUST be routed here rather than falling through to
            // `handle_tools_call` (which serves `offload_task` alone and would
            // answer a perfectly valid call with "unknown tool").
            if name.starts_with("graph_")
                || name.starts_with("context_")
                || name == "run_check"
                || name == "run_command"
            {
                // Graph + session-memory tools, plus `run_check` (V12 Phase A —
                // independent of the graph, but shares this dispatch surface).
                // Warm path: let the app's single index serve it (no second
                // cross-process DB open; lets the app record it for the monitor
                // and scope memory to this consumer). Fall back to a direct
                // read-only open when the app isn't up.
                //
                // #48 finding M-8: the fallback is handed this child's TAB
                // identity, because that is what decides whether it may still
                // serve LOCAL-CAPABILITY there — a tab has a latch in the app
                // that this path cannot read, a hand-run/cron child has no latch
                // anywhere. See `graph::mcp::headless_refusal`. It is NOT handed
                // the `ProxyMiss` reason: the reason remains the attacker's to
                // influence — locked decision 30 raised `Transport` from "one
                // `Write` to `.cimp-discovery/`" to "one write plus a listener",
                // which is a cost increase and not a guarantee (same doc comment).
                //
                // Note also that not every reason reaches this fallback any more:
                // `ProxyMiss::declined` returns the app's own verdict for the two
                // reasons that mean it answered, so `None` here now means "no
                // instance answered at all", which is what the fallback was
                // always documented to be for.
                match proxy_graph(&params).await {
                    Some(r) => r,
                    None => crate::graph::handle_mcp_call(&params, consumer(), tab()).await,
                }
            } else if name.starts_with(DELEGATE_TOOL_PREFIX) {
                // V39 Phase B: the harness id is the suffix, resolved through
                // the registry rather than by a literal match arm — which is
                // also what keeps this file free of harness names.
                let tool = name.to_string();
                handle_delegate_tool(&tool, params).await
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
        _ => offload_task_description(&current_settings_full()),
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
    json!({
        "name": "offload_task",
        "description": offload_task_description(&current_settings_full()),
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
fn offload_task_description(full: &crate::settings::Settings) -> String {
    let settings = &full.offload;
    // V39 Phase C: facades included, exactly as an unreachable LAN box is
    // included — this renderer describes the CONFIGURED pool, and whether a
    // member can serve right now is what the app's live `/describe` is for. The
    // child cannot drive a tab itself (`ResolvedBackend::from_config` drops the
    // kind), so if this text is the one in play the app is down and every
    // facade in it is unreachable — which is equally true of the LAN box beside
    // it, and is why neither is silently omitted.
    let backends: Vec<OffloadBackend> = full
        .effective_offload_backends()
        .into_iter()
        .filter(|b| b.enabled)
        .collect();

    if backends.is_empty() {
        return "Delegate a token-heavy subtask to a local model to conserve this session's \
                context. (No offload backend is configured/enabled — set one up in cImp \
                Settings → Offload task tools.)"
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
        // **The facade's whole point** (locked decision 3): off this machine's
        // model, reachable, not cloud — which is exactly what `LAN` already
        // means to a reader of this prose. Never "tab", never the harness name,
        // never the tab name: a driver that could tell would be able to steer
        // its sibling deliberately, and the user's audit trail (the banner, the
        // Events rows) is what makes the hand-off visible, not the tool text.
        OffloadBackendKind::HarnessTab { .. } => "LAN",
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
    match proxy_graph_outcome(params).await {
        Ok(v) => Some(v),
        Err(miss) => {
            // Locked decision 30, the re-resolution half: whatever went wrong, the
            // endpoint this child is holding is now suspect (a rotated token reads
            // as `HttpStatus(401)`, a restarted app as `Transport`), so the memo is
            // dropped and the NEXT call re-resolves. Without this the memo could
            // wedge a live tab headless for the rest of its life — the precise cost
            // that made "bake the endpoint in at spawn" a non-goal.
            forget_resolved_discovery();
            miss.report();
            // `Some` = the app ANSWERED: report its verdict as a tool error rather
            // than silently re-running the call on the weaker path. `None` = fall
            // back, which is what the caller's `None` arm does.
            miss.declined().map(|text| {
                Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": true }))
            })
        }
    }
}

/// Every distinct reason [`proxy_graph`] hands the call to the headless
/// fallback (#48, finding M-2).
///
/// # Why these are named
///
/// The fallback is a security-relevant path: until this commit it wrote
/// unquarantined, unattributed memory, and it still serves reads with no latch
/// and no session identity. A fallback's reachability is the union of its
/// triggers, and the triggers were collapsed into one `Option::None` produced
/// by five different `?`s spread across thirty lines — so "how does an attacker
/// get onto the headless path" had no answer anybody could enumerate, and the
/// documented justification for the path's behaviour silently assumed the
/// answer was "the app is closed".
///
/// It is not. A corrupted `<portable_root>/.cimp-discovery/<pid>.json` does not
/// by itself produce [`NoInstance`](Self::NoInstance) — the legacy
/// `.cimp-offload.json` still resolves (#48 F-26) — and a well-formed entry with
/// a dead port used to produce [`Transport`](Self::Transport) in ONE write, which
/// Claude's own `Write` tool reaches. **Locked decision 30 (#48 F-11) closed that
/// one write:** `loopback::select_verified` now requires a candidate to answer a
/// token-authenticated `GET /health`, so a planted entry naming a dead port is
/// skipped and the real instance serves the call. What remains, accepted
/// knowingly, is one write **plus a listener** — see `loopback::responds`.
/// `HttpStatus(500)` means the app answered and refused, which is a completely
/// different fact about the system and used to be indistinguishable from "cImp is
/// not running".
///
/// **Not every reason falls back.** [`declined`](Self::declined) splits the set:
/// the two that mean the app ANSWERED are surfaced as tool errors, because
/// re-running such a call headless discards a live instance's verdict and
/// substitutes a weaker path's (decision 30, part two).
///
/// Each is reported once per process (see [`report`](Self::report)): the
/// condition is stable — a corrupt discovery file stays corrupt for the child's
/// whole life — so a line per tool call would be noise that buries the first
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProxyMiss {
    /// No usable loopback endpoint from EITHER discovery store: no parseable
    /// per-instance entry under `.cimp-discovery/` **and** no parseable
    /// `.cimp-offload.json` (`loopback::select_discovery`'s legacy fallback).
    ///
    /// #48 F-26: this was documented as "no discovery file, or none of the
    /// entries parsed", which reads as one write. It is two — the legacy file
    /// still resolves after the per-instance entry is corrupted, and a call that
    /// still resolves is still served. [`Transport`](Self::Transport) is the
    /// single-write trigger; see its doc.
    NoInstance,
    /// The HTTP client itself could not be constructed (TLS/backend init).
    /// Process-local and unrelated to the app's state.
    ClientBuild,
    /// The request never got an answer: connection refused, DNS, the 30 s
    /// timeout.
    ///
    /// **This is NOT the property it used to claim.** The old wording — "the app
    /// is genuinely unreachable, the case the fallback was designed for" — reads
    /// as a safety guarantee and is not one (#48 F-11, sharpened by F-26). One
    /// `Write` of a *well-formed* `.cimp-discovery/<n>.json` naming a deeper root
    /// than the running instance and a dead `port` used to make
    /// `loopback::select_discovery` prefer that entry, and the dead port landed
    /// here.
    ///
    /// Locked decision 30 removed the write-only form of that steer — a candidate
    /// must now answer a token-authenticated `GET /health` — but the honest
    /// reading of this variant is still **"the endpoint this child resolved did
    /// not answer"**, and which endpoint it resolved remains steerable by anything
    /// that can write one file *and* bind the port it names. So it stays the
    /// cheapest reason to manufacture rather than the most trustworthy one, and
    /// nothing may gate on it: see `graph::mcp::headless_refusal`, which gates on
    /// `--tab` instead.
    Transport,
    /// The connection was ESTABLISHED and then the child's own client-side cap
    /// elapsed before the app finished answering.
    ///
    /// This is the opposite fact from [`Transport`](Self::Transport): the app
    /// was listening, it accepted the call, and it is — as far as this child
    /// knows — still working on it. Until this variant existed a slow answer
    /// was mapped to `Transport` and therefore reached
    /// `graph::mcp::headless_refusal`, which told the caller **"cImp is not
    /// reachable"** about an app that was running fine and about a check that
    /// completed successfully seconds later. Every such refusal landed at
    /// exactly the client cap (+30 000 ms) while the app's own activity row for
    /// the same call was `ok=true`.
    ///
    /// It does **not** fall back — see [`declined`](Self::declined). Re-running
    /// a long `run_check`/`run_command` on the headless path would start a
    /// SECOND execution of work that is still running, which is worse than
    /// saying nothing.
    SlowCall,
    /// The app answered with a non-2xx status. It is RUNNING and it declined:
    /// 401 is a stale bearer token, 5xx is a fault inside the warm path.
    ///
    /// Since locked decision 30 this does **not** fall back — see
    /// [`declined`](Self::declined).
    HttpStatus(u16),
    /// A 2xx answer whose body was not JSON. The app is running and something
    /// between it and here is rewriting the response.
    ///
    /// Since locked decision 30 this does **not** fall back — see
    /// [`declined`](Self::declined).
    Unparseable,
}

impl ProxyMiss {
    /// A stable, distinct label per reason — what the log line carries and what
    /// the test enumerates.
    fn as_str(&self) -> &'static str {
        match self {
            ProxyMiss::NoInstance => "no-instance",
            ProxyMiss::ClientBuild => "client-build",
            ProxyMiss::Transport => "transport",
            ProxyMiss::SlowCall => "slow-call",
            ProxyMiss::HttpStatus(_) => "http-status",
            ProxyMiss::Unparseable => "unparseable-response",
        }
    }

    /// The message this reason is reported to the CALLER with, or `None` when the
    /// call may be re-run on the headless fallback.
    ///
    /// Locked decision 30, part two (#48 F-11): `HttpStatus` and `Unparseable`
    /// both mean **the app answered**. `HttpStatus` is a running instance
    /// declining (401 on a rotated token, 5xx inside the warm path) and
    /// `Unparseable` is a 2xx whose body something rewrote. Re-running such a call
    /// headless silently discards a live instance's verdict and substitutes a
    /// weaker path's — a different and arguably worse bug than the steering
    /// primitive F-11 filed, because there the app never spoke at all. Under the
    /// `--tab` rule M-8 settled the containment consequence is already gone; this
    /// closes the diagnostic dishonesty.
    ///
    /// [`SlowCall`](Self::SlowCall) joins them on a neighbouring property: the
    /// app did not finish answering, but it DID accept the call, so the honest
    /// report is "outcome unknown here" — never "cImp is not reachable", which
    /// is what the headless fallback says, and never a silent second execution
    /// of work that is still running.
    ///
    /// Exhaustive on purpose: a further reason cannot join the set without a
    /// deliberate answer to "does the app's own verdict exist for this one".
    fn declined(&self) -> Option<String> {
        match self {
            ProxyMiss::HttpStatus(code) => Some(format!(
                "NOT RUN: cImp is running and answered this call with HTTP {code}, so the call was \
                 not re-run without it — an answer from the app, even a refusal, is the app's \
                 verdict, and running the call on the fallback path instead would hide it. \
                 Nothing ran and nothing was read. If this is a 401 the tab is holding a token \
                 from a previous cImp launch and restarting the tab fixes it; a 5xx is a fault \
                 inside cImp's own warm path. Say so in your answer and retry — this is a \
                 transient condition, not a permanent boundary."
            )),
            ProxyMiss::SlowCall => Some(
                "NO RESULT YET: this call took longer than the proxy's client-side cap, so this \
                 tool gave up waiting for the answer. cImp itself IS running — it accepted the \
                 call — and the operation may still be completing inside the app right now. The \
                 call was deliberately NOT re-run without cImp: re-running it on the fallback \
                 path would start a SECOND execution of work that is still in flight. Do not \
                 report cImp as unreachable and do not report the operation as failed — its \
                 outcome is simply unknown here. Check the resulting state directly (files, the \
                 app's own activity/output), or re-run this call once the operation would have \
                 finished."
                    .to_string(),
            ),
            ProxyMiss::Unparseable => Some(
                "NOT RUN: cImp answered this call with a success status but a body that was not \
                 JSON, so something between cImp and this tool is rewriting responses. The call \
                 was not re-run without cImp: a rewritten answer is a reason to stop, not a \
                 reason to run the same call on a weaker path. Nothing ran and nothing was read. \
                 Report this verbatim — it is not a normal condition."
                    .to_string(),
            ),
            ProxyMiss::NoInstance | ProxyMiss::ClientBuild | ProxyMiss::Transport => None,
        }
    }

    /// Say it once per reason per process, on stderr — the same channel the
    /// handshake diagnostics use, because `tracing` is not initialized in this
    /// child.
    fn report(&self) {
        static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
            std::sync::OnceLock::new();
        let key = match self {
            ProxyMiss::HttpStatus(code) => format!("http-status:{code}"),
            other => other.as_str().to_string(),
        };
        let mut seen = SEEN
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !seen.insert(key.clone()) {
            return;
        }
        if matches!(self, ProxyMiss::SlowCall) {
            // Distinct wording: this one is NOT "the app declined". The app
            // accepted the call and is presumed still working on it; the child
            // simply stopped waiting.
            eprintln!(
                "cimp-offload: a graph/context call exceeded this child's client-side timeout \
                 ({key}) — cImp accepted the call and may still be completing it. The caller was \
                 told the outcome is unknown, NOT that cImp is unreachable, and the call was not \
                 re-run on the headless fallback (which would double-execute work still in \
                 flight)."
            );
            return;
        }
        if self.declined().is_some() {
            eprintln!(
                "cimp-offload: cImp ANSWERED a graph/context call and declined it ({key}) — the \
                 call was surfaced to the caller as a tool error, NOT re-run on the headless \
                 fallback (locked decision 30). `http-status:401` means this tab's MCP child holds \
                 a token from an earlier cImp launch — restart the tab. A 5xx is a fault inside \
                 cImp's warm path."
            );
            return;
        }
        eprintln!(
            "cimp-offload: graph/context calls are taking the HEADLESS fallback ({key}) — the \
             app's warm index is not serving them. Reads still work; persistent memory writes are \
             refused until cImp is reachable. If cImp IS running, no discovery candidate answered \
             a token-authenticated `GET /health`: check BOTH `.cimp-discovery/<pid>.json` \
             (preferred, deepest matching root wins, and since locked decision 30 an entry that \
             does not answer is skipped) and `.cimp-offload.json` (the legacy fallback) next to \
             the executable."
        );
    }
}

#[cfg(test)]
mod proxy_miss_tests {
    use super::ProxyMiss;

    /// The set is enumerable and its members are distinguishable — the property
    /// finding M-2 asked for. "No instance" and "the app answered 500" were the
    /// same `None`; a security fallback whose trigger set nobody can enumerate
    /// is a fallback whose reachability nobody can bound.
    ///
    /// Written as an exhaustive `match` so a sixth reason added later fails to
    /// compile here rather than joining the set unlabelled.
    #[test]
    fn every_fallback_reason_is_named_and_distinct() {
        let all = [
            ProxyMiss::NoInstance,
            ProxyMiss::ClientBuild,
            ProxyMiss::Transport,
            ProxyMiss::SlowCall,
            ProxyMiss::HttpStatus(500),
            ProxyMiss::Unparseable,
        ];
        for m in &all {
            // Exhaustiveness: the compiler is the enumeration guard.
            let _: () = match m {
                ProxyMiss::NoInstance
                | ProxyMiss::ClientBuild
                | ProxyMiss::Transport
                | ProxyMiss::SlowCall
                | ProxyMiss::HttpStatus(_)
                | ProxyMiss::Unparseable => (),
            };
            assert!(!m.as_str().is_empty());
        }
        let labels: std::collections::HashSet<&str> = all.iter().map(|m| m.as_str()).collect();
        assert_eq!(labels.len(), all.len(), "two reasons share a label");
        // The two the review conflated are not equal, and the status is carried
        // rather than discarded — 401 (stale token) and 500 (warm-path fault)
        // are different incidents.
        assert_ne!(ProxyMiss::NoInstance, ProxyMiss::HttpStatus(500));
        assert_ne!(ProxyMiss::HttpStatus(401), ProxyMiss::HttpStatus(500));
        assert_ne!(ProxyMiss::Transport, ProxyMiss::NoInstance);
        // "the app never answered" vs "the app is still answering" are the two
        // the slow-check defect conflated.
        assert_ne!(ProxyMiss::Transport, ProxyMiss::SlowCall);
    }

    /// Locked decision 30, part two (#48 F-11): the app's own verdict is never
    /// re-run headless.
    ///
    /// The split is asserted in BOTH directions on purpose. Widening it would
    /// start refusing work the fallback exists to do (`NoInstance` is the app
    /// being closed, which is the ordinary case); narrowing it puts the silent
    /// re-run back.
    #[test]
    fn only_the_reasons_that_mean_the_app_answered_refuse_to_fall_back() {
        for answered in [
            ProxyMiss::HttpStatus(401),
            ProxyMiss::HttpStatus(500),
            ProxyMiss::Unparseable,
        ] {
            let text = answered
                .declined()
                .unwrap_or_else(|| panic!("{answered:?} must not fall back"));
            // The message has to say what happened, that nothing ran, and that it
            // is transient — the same three facts every other boundary string
            // carries (`graph::mcp::HEADLESS_WRITE_UNAVAILABLE` and friends).
            assert!(text.starts_with("NOT RUN:"), "{text}");
            assert!(text.contains("Nothing ran and nothing was read."), "{text}");
        }
        for falls_back in [
            ProxyMiss::NoInstance,
            ProxyMiss::ClientBuild,
            ProxyMiss::Transport,
        ] {
            assert!(
                falls_back.declined().is_none(),
                "{falls_back:?} must still reach the headless fallback"
            );
        }
        // A 401 names the one action that fixes it, because a rotated token is by
        // far the likeliest way a running app declines: the memo is dropped on
        // every miss, so a child that keeps seeing 401 is holding a stale token in
        // its own `--tab` MCP config, not a stale endpoint.
        let four_oh_one = ProxyMiss::HttpStatus(401).declined().expect("declined");
        assert!(four_oh_one.contains("restarting the tab"), "{four_oh_one}");
    }

    /// The slow-check defect, pinned at the boundary string.
    ///
    /// A `run_check` running `cargo build` (40–66 s here) blew the flat 30 s
    /// client cap, mapped to `Transport`, took the headless fallback and told
    /// the caller **"cImp is not reachable"** — while the app's own row for the
    /// same call was `ok=true`. Two properties keep that from coming back:
    /// `SlowCall` must not fall back (so `headless_refusal` never sees it), and
    /// its message must not claim the app is down or the work is dead.
    #[test]
    fn a_slow_call_is_reported_as_unknown_not_as_unreachable() {
        let text = ProxyMiss::SlowCall
            .declined()
            .expect("a slow call must NOT reach the headless fallback");
        // (a) the cap was the child's, (b) cImp is running and may still be
        // working, (c) what the caller can do instead.
        assert!(text.contains("client-side cap"), "{text}");
        assert!(text.contains("cImp itself IS running"), "{text}");
        assert!(text.contains("may still be completing"), "{text}");
        assert!(text.contains("re-run this call"), "{text}");
        // The lie the defect produced: `HEADLESS_CAPABILITY_UNAVAILABLE`'s
        // "cImp is not reachable". This string says the opposite, on purpose.
        assert!(!text.contains("not reachable"), "{text}");
        assert!(text.contains("Do not report cImp as unreachable"), "{text}");
        // Content-free: no path, no command, no tool output echoed back.
        assert_eq!(text, ProxyMiss::SlowCall.declined().expect("stable"));
        // And it is distinct from every other boundary string.
        for other in [
            ProxyMiss::HttpStatus(401),
            ProxyMiss::HttpStatus(500),
            ProxyMiss::Unparseable,
        ] {
            assert_ne!(other.declined().expect("declined"), text);
        }
    }
}

#[cfg(test)]
mod proxy_timeout_tests {
    use super::{
        proxy_call_timeout, PROXY_CALL_TIMEOUT, PROXY_CONNECT_TIMEOUT,
        PROXY_LOCAL_CAPABILITY_TIMEOUT,
    };

    /// The tools that EXECUTE get the backstop cap; the tools that READ keep the
    /// wedge detector.
    ///
    /// `run_check`/`run_command` were the whole defect: a flat 30 s cap is below
    /// the runtime of literally every compile-shaped check in this repo
    /// (cargo-build 40–66 s, cargo-clippy ~49 s, cargo-test minutes).
    #[test]
    fn local_capability_tools_get_the_long_cap() {
        for executes in ["run_check", "run_command", "security_audit", "quality_audit"] {
            assert_eq!(
                proxy_call_timeout(executes),
                PROXY_LOCAL_CAPABILITY_TIMEOUT,
                "{executes} executes work and must not be abandoned at 30 s"
            );
        }
        // Everything outside that class keeps the wedge detector — TRUSTED
        // symbol/edge queries, the PERSISTENT-WRITE memory tool, and the
        // unknown⇒EXTERNAL default. (Note the class, not the name prefix, is the
        // rule: several `graph_*` readers ARE LocalCapability, and they get the
        // long cap too, which costs nothing because they answer in
        // milliseconds.)
        for reads in ["graph_callers", "graph_impact", "context_note", "unknown"] {
            assert_eq!(proxy_call_timeout(reads), PROXY_CALL_TIMEOUT, "{reads}");
        }
        assert_eq!(
            proxy_call_timeout("graph_repo_map"),
            PROXY_LOCAL_CAPABILITY_TIMEOUT
        );
    }

    /// The connect cap stays short and independent of the overall cap: "nothing
    /// is listening" must still fail fast into the headless fallback even for a
    /// tool whose overall cap is half an hour.
    #[test]
    fn the_connect_cap_is_short_for_every_class() {
        assert!(PROXY_CONNECT_TIMEOUT < PROXY_CALL_TIMEOUT);
        assert!(PROXY_CONNECT_TIMEOUT.as_secs() <= 2);
        assert!(PROXY_LOCAL_CAPABILITY_TIMEOUT > PROXY_CALL_TIMEOUT);
    }
}

/// How long the child waits for the TCP connect to the app's loopback
/// endpoint. This is the "is anything listening" probe and nothing else —
/// loopback connects complete in milliseconds, so 2 s is already generous and a
/// dead port still fails fast into [`ProxyMiss::Transport`] → the headless
/// fallback.
const PROXY_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Overall client-side cap for an ordinary `/graph_run` call (graph queries,
/// context/memory tools): they are index reads and answer in well under a
/// second, so 30 s is a wedge detector.
const PROXY_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Overall client-side cap for a
/// [`ToolClass::LocalCapability`](crate::offload::toolclass::ToolClass::LocalCapability)
/// `/graph_run` call — `run_check`, `run_command`, the audits.
///
/// # Why this is huge rather than "generous"
///
/// These tools EXECUTE things: a `cargo build` takes 40–66 s here, `cargo
/// clippy` ~49 s, `cargo test` minutes. Under the old flat 30 s cap the child
/// abandoned the request mid-flight, mapped the timeout to
/// [`ProxyMiss::Transport`] and fell back to
/// `graph::mcp::headless_refusal`, which answered **"cImp is not reachable"** —
/// about a running app, for a check that finished successfully seconds later
/// (every such refusal landed at exactly +30 000 ms).
///
/// The right bound for "how long may this run" is NOT here. The app bounds
/// every check/command execution server-side with its own per-run timeout, and
/// that bound is the one users configure and the one that produces a real
/// result. This cap exists only as a backstop against a wedged connection that
/// never produces a response at all, so it is set far above any legitimate
/// server-side bound (30 min) instead of competing with it. Anything under that
/// ceiling now surfaces as [`ProxyMiss::SlowCall`], which does not lie about
/// reachability and does not re-execute the work.
const PROXY_LOCAL_CAPABILITY_TIMEOUT: Duration = Duration::from_secs(1800);

/// Which overall cap a `/graph_run` call gets, from the tool's class.
///
/// Split out of [`proxy_graph_outcome`] so the rule is testable without a
/// listening app: the tools that EXECUTE (`ToolClass::LocalCapability`) get the
/// backstop cap, everything else keeps the wedge detector.
fn proxy_call_timeout(name: &str) -> Duration {
    match crate::offload::toolclass::classify(name) {
        crate::offload::toolclass::ToolClass::LocalCapability => PROXY_LOCAL_CAPABILITY_TIMEOUT,
        _ => PROXY_CALL_TIMEOUT,
    }
}

/// Which [`ProxyMiss`] a `reqwest` send/body error means.
///
/// The distinction the fix turns on: a CONNECT-phase failure is the app not
/// answering at all (→ [`ProxyMiss::Transport`] → headless fallback, unchanged,
/// this is the case the fallback exists for), while a timeout AFTER the
/// connection was established is a slow answer from a running app (→
/// [`ProxyMiss::SlowCall`], which does not fall back and does not claim
/// unreachability). Anything else (connection reset, body decode fault) keeps
/// the pre-existing mapping supplied by the caller.
fn proxy_send_miss(err: &reqwest::Error) -> ProxyMiss {
    if err.is_connect() {
        return ProxyMiss::Transport;
    }
    if err.is_timeout() {
        return ProxyMiss::SlowCall;
    }
    ProxyMiss::Transport
}

/// The five-outcome half of [`proxy_graph`]. `Ok` is the app's answer (which may
/// itself be a tool error), `Err` names why the caller must fall back.
async fn proxy_graph_outcome(params: &Value) -> Result<Result<Value, (i64, String)>, ProxyMiss> {
    let (base, token) = proxy_base().ok_or(ProxyMiss::NoInstance)?;
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    // Two caps, not one. The connect cap answers "is the app listening" (fast,
    // loopback); the overall cap is class-dependent, because a `run_check` that
    // compiles this repo legitimately runs for minutes and used to be abandoned
    // at a flat 30 s and then reported as "cImp is not reachable".
    let client = reqwest::Client::builder()
        .connect_timeout(PROXY_CONNECT_TIMEOUT)
        .timeout(proxy_call_timeout(name))
        .build()
        .map_err(|_| ProxyMiss::ClientBuild)?;
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
        .map_err(|e| proxy_send_miss(&e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ProxyMiss::HttpStatus(status.as_u16()));
    }
    // Body read: a timeout here is the same fact as a timeout on `send` — the
    // app accepted the call and the cap elapsed while it was still answering —
    // and `reqwest` tells us so cheaply via `is_timeout()`, so it maps to
    // `SlowCall` rather than to `Unparseable` ("something is rewriting
    // responses"), which would be an alarming and wrong diagnosis. Every other
    // body fault stays `Unparseable`.
    let v: Value = resp.json().await.map_err(|e| {
        if e.is_timeout() {
            ProxyMiss::SlowCall
        } else {
            ProxyMiss::Unparseable
        }
    })?;
    let ok = v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false);
    if ok {
        let text = v.get("text").and_then(|t| t.as_str()).unwrap_or_default();
        Ok(Ok(json!({ "content": [{ "type": "text", "text": text }] })))
    } else {
        let err = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("graph query failed");
        // NOT a miss: the app answered and said the tool failed. Falling back
        // here would re-run the call headless and hide the app's own verdict.
        Ok(Ok(
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
                    // V37 Phase F: announce a possibly-changed tool list as the
                    // FIRST act of every (re)connection.
                    //
                    // This relay is the only channel a `change` frame travels
                    // on, so any pulse emitted while it was disconnected is
                    // gone. That used to be a narrow window; Phase F made it a
                    // routine one, because the proxy child is now injected into
                    // every AI tab — including tabs that spawn while the app's
                    // loopback is not running at all (offload, graph, Code Audit
                    // and every MCP grant off ⇒ `Settings::loopback_needed()` is
                    // false, so there is nothing to subscribe to). The very act
                    // that starts the loopback — granting a server — also emits
                    // the pulse, ~300ms later via the C5 debounce, while this
                    // task is still inside its 2s backoff. Without this line the
                    // first grant on a cold install would still need a tab
                    // restart, which is precisely the defect Phase F exists to
                    // remove.
                    //
                    // Correct in general, not just for that case: after any gap
                    // this child cannot know whether the surface moved, and
                    // `tools/list_changed` is idempotent — the client answers
                    // with one `tools/list`, which is exactly the question it
                    // should be asking. Ordering is safe: `await_initialize`
                    // above parks this task until the handshake reply is on the
                    // wire.
                    emit_list_changed(&stdout).await;
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
                     the channel capability (enable offload.session_push and restart \
                     the tab)"
                );
                return;
            }
            // The method name is the consumer's (locked decision 25) — the twin
            // of the capability key it declared at `initialize`. A push sent
            // under any other name is dropped client-side, silently.
            let Some(method) = consumer_harness()
                .and_then(|h| h.plugin())
                .and_then(|p| p.push_notification_method())
            else {
                eprintln!(
                    "cimp-offload: dropped a session push — consumer {} declares no channel \
                     notification method",
                    consumer()
                );
                return;
            };
            match channel_params(&frame.data) {
                Some(params) => emit_notification(stdout, method, Some(params)).await,
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

/// The system-prompt `instructions` block, from the model-visible text
/// inventory (V40 Phase E, locked decision 24 — the text itself lives in
/// `harness::instructions`, which is what makes it enumerable).
fn channel_instructions(harness: Option<crate::harness::HarnessId>) -> &'static str {
    crate::harness::instructions::text(harness, crate::harness::instructions::Slot::Channel)
}

/// Add the consumer's own channel capability + the top-level `instructions`
/// string to an otherwise-unchanged `initialize` result.
///
/// **Two halves with two owners** (V40 Phase E, locked decision 25). The
/// `instructions` block is cImp's text about cImp's channel, so it comes from
/// the model-visible inventory; the capability KEY lives in one vendor's
/// namespace (`experimental["claude/channel"]`) and is written by that harness's
/// plugin. Core used to write both, for whichever consumer had push armed — so
/// the second harness to grow an inbound MCP path would have been handed
/// Claude's key.
///
/// Pure, so a test can pin both the addition and the untouched base — notably
/// `protocolVersion`, which MUST stay on the legacy `2025-06-18` era where the
/// client honours channels (milestone invariant 1), and `tools.listChanged`.
fn decorate_initialize_channel(result: &mut Value, harness: Option<crate::harness::HarnessId>) {
    if let Some(plugin) = harness.and_then(|h| h.plugin()) {
        plugin.decorate_initialize(result);
    }
    result["instructions"] = Value::String(channel_instructions(harness).to_string());
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
    // V40 Phase D (locked decision 25): the consumer's PLUGIN answers whether
    // it has an inbound MCP path at all. This was `consumer() == "claude"` —
    // core asserting that exactly one harness can receive a push, which is a
    // fact about that harness rather than about pushing, and which silently
    // answered `false` for every harness added later.
    crate::harness::HarnessId::from_consumer(consumer())
        .and_then(|h| h.plugin())
        .is_some_and(|p| p.supports_session_push())
        && CHANNEL_PUSH_ARG.load(Ordering::Acquire)
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
    /// Off this machine — LAN *or* cloud. The same bit `OffloadService`'s pool
    /// entries carry, and the one the graph/audit data boundaries key on
    /// (#48, finding F-10): `is_cloud` alone would let a LAN worker reach the
    /// local index and the audit report on the headless path while the in-app
    /// path denies it.
    is_remote: bool,
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
                // V33 Phase E. This is the DEGRADED path (the per-call child
                // running its own agent loop because the app is unreachable),
                // and it must authenticate exactly like the app-side pool does —
                // otherwise turning on `--api-key` silently breaks offload only
                // when the app is down, which is the hardest possible time to
                // notice. V33 stage 3: through `effective_auth_token`, so it
                // also inherits the `--api-key` fallback the pool has. Reading
                // the raw field here is exactly how the two paths would drift.
                let token = b.kind.effective_auth_token();
                Some(Self {
                    name: b.name.clone(),
                    base_url: cmd.base_url(),
                    auth_token: (!token.is_empty()).then_some(token),
                    cloud_blocked: false,
                    is_cloud: false,
                    is_remote: false,
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
                    is_remote: true,
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
            // **Not resolvable here, by construction.** This is the DEGRADED
            // path — the per-call child running its own agent loop because the
            // app is unreachable — and the worker of a facade is a tab inside
            // that very app. An unreachable app does not mean "drive the tab
            // from here", it means there is no worker. `None` drops it from the
            // child's pool exactly as an unaddressable backend is dropped, so
            // the child routes to a real endpoint or says it has none.
            OffloadBackendKind::HarnessTab { .. } => None,
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
        return Err(
            "no offload backend is configured — add one in cImp Settings → Offload task tools"
                .into(),
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
        let kv_unified = b.kv_unified;
        handles.push(tauri::async_runtime::spawn(async move {
            let rb = ResolvedBackend {
                name: String::new(),
                base_url: url,
                auth_token: auth,
                cloud_blocked: false,
                is_cloud,
                // Probe-only shim: `probe` reads `is_cloud`, `base_url` and
                // `auth_token` and nothing else, so this field is not a policy
                // decision here.
                is_remote: is_cloud,
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
    // V33 Phase F: NO pre-mutation checkpoint on this path, and that is the
    // correct answer rather than a gap. This is the HEADLESS child — a separate
    // `cimp --offload-mcp` process that runs the agent loop only when the app is
    // unreachable (see this module's header). There is no `WorkbenchService` in
    // it and no `AppHandle` to reach one through, and reproducing the Workbench
    // in a short-lived stdio child would mean two writers on one `.cimp/
    // shadow.git` with no shared per-root lock — the exact `cp-<seq>` collision
    // `shadow::SHADOW_LOCKS` exists to prevent. `ToolCtx::new` therefore leaves
    // `checkpoint: None`, which is a documented state, not a missing field.
    let ctx = ToolCtx::new(
        settings.allowed_roots.clone(),
        settings.command_allowlist.clone(),
        settings.command_policies.clone(),
        cwd,
    );
    // #48, finding F-10: the headless child's router used to carry only the
    // tool scope, which does not cover `graph_*` or the audit tools — so a
    // cloud backend that emitted an unadvertised `graph_snippet` got repo
    // source text. Same policy, same constructor, as the in-app path.
    let router = NativeRouter::new(
        tools::enabled_defs(&settings.tools),
        ctx,
        crate::offload::backend_gate::BackendGate::for_worker(
            backend.tool_scope.clone(),
            backend.is_remote,
            all,
        ),
    );
    // #48/M-1: ONE scope for both attempts of this child run. The `thinking`
    // retry below is a second `agent::run`, so a per-call budget/scope id was
    // exactly the reset the app path had. This router is a `NativeRouter` (no MCP
    // host ⇒ no EXTERNAL tool is reachable) so the budget is inert here **today**
    // — threaded anyway, because "inert today" is how finding F-10 happened, and
    // because the app path and this one must not drift.
    let mut task_scope = agent::TaskScope::for_task();
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
        // grows a host. (#48/M-1: the scope id that used to sit on this struct is
        // now `task_scope` above, threaded into both attempts.)
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
    let first = agent::run(
        client,
        &cfg,
        &router,
        task,
        deadline,
        None,
        &cancel,
        &mut task_scope,
    )
    .await;
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
            // #48/M-1: the SAME `task_scope` — this retry is a second attempt at
            // one task, not a new task.
            agent::run(
                client,
                &cfg,
                &router,
                task,
                deadline,
                None,
                &cancel,
                &mut task_scope,
            )
            .await
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

    /// The harness this child's tests speak for. `consumer()` answers the
    /// default harness when `run` never set it, which is the case in tests.
    fn test_harness() -> Option<crate::harness::HarnessId> {
        consumer_harness()
    }

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
        let harness = test_harness();
        let mut result = base_initialize();
        decorate_initialize_channel(&mut result, harness);
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], json!(true));
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        // The capability key is the CONSUMER's, written by its plugin (locked
        // decision 25) — core no longer knows the namespace.
        let experimental = result["capabilities"]["experimental"]
            .as_object()
            .expect("the consumer declared its channel capability");
        assert_eq!(experimental.len(), 1);
        assert!(experimental.values().all(|v| v.is_object()));
        assert_eq!(result["instructions"], json!(channel_instructions(harness)));
        // …and the pin the consumer declares is the era it is answered in.
        assert_eq!(
            harness
                .and_then(|h| h.plugin())
                .and_then(|p| p.mcp_protocol_version()),
            Some("2025-06-18"),
            "the channel-honouring era is a pin, not a preference"
        );
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
        let text = channel_instructions(test_harness());
        assert!(text.contains("<channel source=\"cimp-offload\">"));
        assert!(text.contains("Do not invent channel messages"));
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
        // Opaque keys: `events_query` percent-free-formats whatever it is
        // handed, so naming a real harness here would read as if the query
        // string knew what one was (V40 Phase G).
        assert_eq!(
            events_query(Some("tab-b"), "harness-x", true),
            "?tab=tab-b&consumer=harness-x&channels=1"
        );
        assert_eq!(
            events_query(None, "harness-y", false),
            "?consumer=harness-y&channels=0"
        );
    }

    /// **V37 Phase F: every reconnect announces a possibly-changed tool list.**
    ///
    /// The relay is the only channel a `change` frame travels on, so a pulse
    /// emitted while it was disconnected is gone — and Phase F made that a
    /// routine case, not a rare one: the proxy child now rides tabs that spawn
    /// while the app's loopback is not running at all, and the very act that
    /// starts the loopback (granting an MCP server) emits its pulse while this
    /// task is still inside its 2s backoff. Without the announcement, the first
    /// grant on a cold install would still need a tab restart.
    ///
    /// Pinned by reading this file because the behaviour has no other observable
    /// surface: `events_relay` is one `loop` around a live HTTP stream, with no
    /// seam a unit test can drive. Substring checks are single-line on purpose
    /// (the tree is CRLF locally and LF on the Linux runner).
    #[test]
    fn the_events_relay_announces_a_changed_tool_list_on_every_connect() {
        let src = include_str!("mcp.rs");
        let relay = src
            .split("async fn events_relay(")
            .nth(1)
            .expect("events_relay exists");
        // Not scoped further on purpose: each needle's FIRST occurrence after
        // the `fn` header is the real one, so the ordering assertions hold
        // without a body delimiter that would have to know about line endings.
        let body = relay;
        let announce = body
            .find("emit_list_changed(&stdout).await;")
            .expect("the relay must announce a possibly-changed list on connect");
        let parse = body
            .find("parser.feed(&chunk)")
            .expect("the relay parses SSE frames");
        assert!(
            announce < parse,
            "the announcement must precede the frame loop — a pulse that arrived \
             while this child was disconnected is never replayed"
        );
        // Ordering against the handshake is the other half of the safety
        // argument: nothing may be written before the `initialize` reply.
        let park = body
            .find("await_initialize().await;")
            .expect("the relay parks until the handshake reply is on the wire");
        assert!(park < announce, "the announcement must follow await_initialize");
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
        // Same process-wide statics as above; `consumer()` falls back to
        // `harness::DEFAULT_HARNESS` when `run` never set it, which is the case
        // in tests.
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
        let method = test_harness()
            .and_then(|h| h.plugin())
            .and_then(|p| p.push_notification_method())
            .expect("this consumer declares a push method");
        let frame = notification_frame(method, Some(params));
        assert_eq!(frame["method"], method);
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
        let method = test_harness()
            .and_then(|h| h.plugin())
            .and_then(|p| p.push_notification_method())
            .expect("this consumer declares a push method");
        let frame = notification_frame(method, Some(params));
        assert_eq!(frame["method"], method);
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

// ── V39 Phase B: the generated `delegate_task_<harness>` set ────────────────
//
// Locked decision 3. **No harness literal appears below**: the id set comes
// from `harness::contract::harness_ids()` and the display names from
// `Harness::display_name()`, so a third harness gets a tool by having registry
// rows and an input profile — not by an edit here.

/// The prefix every generated delegation tool carries. One spelling, shared by
/// the generator, the dispatcher and the suffix parser, so a rename cannot
/// leave a tool that lists but does not dispatch.
const DELEGATE_TOOL_PREFIX: &str = "delegate_task_";

/// **The pinned contract sentence** (locked decision 3), from the model-visible
/// inventory, already templated with this harness's descriptor label.
///
/// V40 Phase E, locked decision 24: the sentence is text cImp puts in front of a
/// model, so it lives in `harness::instructions` with every other such string
/// rather than in a `format!` nothing can enumerate.
fn delegate_tool_contract(harness: crate::harness::HarnessId) -> &'static str {
    crate::harness::instructions::text(
        Some(harness),
        crate::harness::instructions::Slot::DelegateContract,
    )
}

/// The full settings snapshot, read live from disk on every call.
///
/// The same read [`current_offload_settings`] makes, widened to the whole file
/// because the delegation surface needs the tab list and the harness-version
/// block as well as the offload block. Live by construction, which is what
/// makes locked decision 15's "takes effect on the next turn without restarting
/// either tab" true on this side — the child re-reads the file for every
/// `tools/list`, so a role moved in the app is visible on the next turn with
/// nothing spawn-baked in between.
fn current_settings_full() -> crate::settings::Settings {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::settings::load_readonly(&cwd)
}

/// Every harness id that should be advertised as a delegation target for a
/// consumer whose own tab is `own_tab`, with its display name and the name of
/// the Manual tab it drives.
///
/// The three conditions of locked decision 8, each a *no dead tool* rule rather
/// than a permission check:
///
/// * the harness has a Manual tab (else there is nothing to name),
/// * that tab is not the caller's own (a tab must never see a tool that drives
///   itself),
/// * the worker gate is clean (a harness whose input profile is recorded broken
///   would take a truncated request).
///
/// A harness with no input profile never reaches here: `input_profile` answers
/// `None` and it is skipped, which is the fail-closed half of decision 16.
fn delegate_targets(
    settings: &crate::settings::Settings,
    own_tab: Option<&str>,
) -> Vec<(crate::harness::HarnessId, String)> {
    use crate::settings::{DelegationRole, TabConfig};
    let mut out = Vec::new();
    for harness in crate::harness::registry::all() {
        let Some(id) = harness.id() else { continue };
        if harness.plugin().and_then(|p| p.input_profile()).is_none() {
            continue;
        }
        // **V40 Phase B, amendment 0-f: the gate is asked per harness.** It was
        // asked ONCE, above the loop, against a single scalar shared by every
        // harness — so a `"fail"` recorded against one TUI removed every
        // `delegate_task_*` tool, including for harnesses the spike had never
        // been run against, and a `"pass"` recorded against one vouched for all
        // of them. Now a blocked harness drops out of the set and the others
        // stay advertised.
        if crate::harness::contract::gate_for(
            crate::harness::contract::CAP_DELEGATION_WORKER,
            settings,
            harness,
        )
        .blocked
        {
            continue;
        }
        let manual = settings.tabs.iter().find_map(|t| match t {
            TabConfig::AiTool(c)
                if c.delegation_role == DelegationRole::Manual
                    && crate::tabs::tab_consumer(c) == Some(id) =>
            {
                Some(c)
            }
            _ => None,
        });
        let Some(cfg) = manual else { continue };
        if own_tab.is_some_and(|own| own == cfg.id) {
            continue;
        }
        out.push((harness, cfg.name.clone()));
    }
    out
}

/// The generated tool descriptors for this child.
fn delegate_task_tools(own_tab: Option<&str>) -> Vec<Value> {
    delegate_targets(&current_settings_full(), own_tab)
        .into_iter()
        .map(|(harness, tab_name)| delegate_task_tool(harness, &tab_name))
        .collect()
}

/// One `delegate_task_<id>` descriptor.
///
/// Both halves of the description are inventory rows (locked decision 24): the
/// pinned contract sentence, templated with this harness's label, and the detail
/// paragraph, whose one runtime value is the Manual tab's CURRENT name — which
/// changes when the user renames the tab and therefore cannot be baked in.
fn delegate_task_tool(harness: crate::harness::HarnessId, tab_name: &str) -> Value {
    let id = harness.id().expect("a delegation target is a real harness");
    let detail = crate::harness::instructions::text(
        Some(harness),
        crate::harness::instructions::Slot::DelegateToolDetail,
    )
    .replace("{tab}", tab_name);
    json!({
        "name": format!("{DELEGATE_TOOL_PREFIX}{id}"),
        "description": format!("{} {detail}", delegate_tool_contract(harness)),
        "inputSchema": {
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The request, written the way you would type it to that harness yourself. It is sent VERBATIM - no header, no framing, nothing identifying you is added."
                },
                "context": {
                    "type": "string",
                    "description": "Optional extra context (paths, prior findings) appended to the task."
                },
                "timeout_s": {
                    "type": "integer",
                    "description": "How long to wait for the worker's turn to finish before giving up. Omit for the configured default. On a timeout the worker is NOT interrupted - it keeps running, visibly, in its own tab."
                }
            },
            "required": ["task"]
        }
    })
}

/// Dispatch one `delegate_task_<id>` call: forward it to the app, which owns
/// the tabs.
///
/// Unlike `offload_task` there is **no self-contained fallback**. There cannot
/// be: the worker is a tab in the app's process, so an unreachable app does not
/// mean "do it here", it means there is no worker at all. Saying so is the only
/// honest answer.
async fn handle_delegate_tool(name: &str, params: Value) -> Result<Value, (i64, String)> {
    let Some(harness) = name
        .strip_prefix(DELEGATE_TOOL_PREFIX)
        .filter(|h| !h.is_empty())
    else {
        return Err((-32602, format!("unknown tool: {name}")));
    };
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let task = args
        .get("task")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if task.trim().is_empty() {
        return Ok(tool_error(&format!("{name} requires a non-empty `task`")));
    }
    let context = args
        .get("context")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let timeout_s = args.get("timeout_s").and_then(|v| v.as_u64());
    let Some(own_tab) = tab() else {
        // The engine's preflight refuses this too; answering here as well saves
        // a headless child a round trip to be told the same thing. Both halves
        // exist because either can be reached first (a facade route in Phase C
        // does not come through this file at all).
        return Ok(tool_error(
            "delegation is not available to a headless consumer: cImp needs to know which tab is \
             asking, and this process was not started for one",
        ));
    };
    match proxy_delegate(harness, &task, context.as_deref(), timeout_s, own_tab).await {
        Some(Ok(text)) => Ok(json!({ "content": [{ "type": "text", "text": text }] })),
        Some(Err(msg)) => Ok(tool_error(&msg)),
        None => Ok(tool_error(
            "cImp is not reachable, so no tab can be driven. Delegation runs inside the app that \
             owns the tabs - there is no local fallback for it.",
        )),
    }
}

/// `POST /delegate` - forward one delegation to the app.
///
/// `None` means the app is unreachable. `Some(Err)` means the app answered, and
/// its answer is the refusal/timeout text, surfaced verbatim.
async fn proxy_delegate(
    harness: &str,
    task: &str,
    context: Option<&str>,
    timeout_s: Option<u64>,
    own_tab: &str,
) -> Option<Result<String, String>> {
    let (base, token) = proxy_base()?;
    // Connect timeout only, no overall request timeout: a delegation is a whole
    // model turn on another harness and legitimately runs for minutes. The
    // engine's own deadline is what bounds it. A second, shorter bound here
    // would abandon a live worker and report a failure the app knows nothing
    // about - the defect `c8d1619` fixed for `run_check`, which this must not
    // reintroduce.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .pool_max_idle_per_host(0)
        .build()
        .ok()?;
    let body = json!({
        "harness": harness,
        "task": task,
        "context": context,
        "timeout_s": timeout_s,
        "consumer": consumer(),
        "tab": own_tab,
    });
    let resp = client
        .post(format!("{base}/delegate"))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .ok()?;
    let v: Value = resp.json().await.ok()?;
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        let text = v
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let worker = v.get("worker").and_then(Value::as_str).unwrap_or("");
        let ms = v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
        let screened = v.get("screened").and_then(Value::as_bool).unwrap_or(false);
        // The meta footer mirrors what an `offload_task` result carries - which
        // worker, how long, and whether the text crossed the screening boundary
        // - so harness-side guidance needs no special casing for this tool.
        let verdict = if screened { "screened" } else { "unscreened" };
        Some(Ok(format!(
            "{text}\n\n[delegated to {worker} - {ms} ms - {verdict}]"
        )))
    } else {
        Some(Err(v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("delegation failed")
            .to_string()))
    }
}

#[cfg(test)]
mod delegate_tool_tests {
    use super::*;
    use crate::settings::{DelegationRole, Settings, TabConfig};

    fn settings_with_manual(role_tab: &str) -> Settings {
        let mut s = Settings::default();
        s.tabs.push(crate::settings::default_claude_tab());
        s.tabs.push(crate::settings::default_opencode_tab());
        for t in s.tabs.iter_mut() {
            if let TabConfig::AiTool(c) = t {
                if c.id == role_tab {
                    c.delegation_role = DelegationRole::Manual;
                }
            }
        }
        s
    }

    /// **The pinned sentence is on every generated tool** (locked decision 3).
    ///
    /// It is the only thing that distinguishes this tool from `offload_task` to
    /// a model reading the list, so a description that lost it would turn a
    /// user-directed instrument into one the model may reach for on its own.
    #[test]
    fn every_generated_delegate_tool_opens_with_the_pinned_sentence() {
        let ids = crate::harness::registry::harness_ids();
        assert!(!ids.is_empty());
        for id in ids {
            let harness = crate::harness::HarnessId::from_id(id).expect("registry harness");
            let tool = delegate_task_tool(harness, "api-work");
            let desc = tool["description"].as_str().expect("a description");
            assert!(
                desc.starts_with(delegate_tool_contract(harness)),
                "`delegate_task_{id}` does not open with the pinned contract sentence:\n{desc}"
            );
            assert!(
                desc.contains(harness.label()),
                "the contract must name the harness it drives: {desc}"
            );
            assert!(
                desc.contains("Never call it on your own initiative"),
                "the who-decides clause is the whole contract: {desc}"
            );
            assert!(
                desc.contains("offload_task"),
                "the description must point at the tool the model MAY call itself: {desc}"
            );
            assert!(
                desc.contains("api-work"),
                "the description must name the tab it drives: {desc}"
            );
            assert_eq!(tool["name"], json!(format!("delegate_task_{id}")));
            let schema = &tool["inputSchema"];
            assert_eq!(schema["required"], json!(["task"]));
            for prop in ["task", "context", "timeout_s"] {
                assert!(
                    schema["properties"].get(prop).is_some(),
                    "missing `{prop}` in the input schema"
                );
            }
            assert_eq!(schema["properties"]["timeout_s"]["type"], json!("integer"));
            // No tab argument, by decision 3: tool = harness = tab.
            assert!(schema["properties"].get("tab").is_none());
        }
    }

    /// **V39 regression 8, re-asserted after Phase E moved the descriptions**
    /// (locked decision 24): the advertised tool SET is exactly one
    /// `delegate_task_<id>` per registered harness with a Manual tab, and
    /// nothing else moved with the text.
    ///
    /// The descriptions are rendered from the instruction inventory now. A
    /// refactor of *text* must not change which tools exist, and this is the
    /// assertion that says so — the set diff, both directions, against the
    /// registry rather than against a hard-coded pair.
    #[test]
    fn the_generated_delegate_tool_set_is_exactly_the_registrys() {
        use std::collections::BTreeSet;
        // The "own tab" perspectives come from the reserved tab ids the
        // settings below actually create, not from literals.
        let own_tabs = [
            None,
            Some(crate::settings::CLAUDE_TAB_ID),
            Some(crate::settings::OPENCODE_TAB_ID),
        ];
        for own in own_tabs {
            let mut s = Settings::default();
            s.tabs.push(crate::settings::default_claude_tab());
            s.tabs.push(crate::settings::default_opencode_tab());
            for t in s.tabs.iter_mut() {
                if let TabConfig::AiTool(c) = t {
                    c.delegation_role = DelegationRole::Manual;
                }
            }
            let expected: BTreeSet<String> = crate::harness::registry::all()
                .filter(|h| h.plugin().and_then(|p| p.input_profile()).is_some())
                .filter_map(|h| h.id())
                .filter(|id| own != Some(id))
                .map(|id| format!("{DELEGATE_TOOL_PREFIX}{id}"))
                .collect();
            let got: BTreeSet<String> = delegate_targets(&s, own)
                .into_iter()
                .map(|(h, tab)| {
                    let tool = delegate_task_tool(h, &tab);
                    tool["name"].as_str().expect("a name").to_string()
                })
                .collect();
            assert_eq!(
                got, expected,
                "the delegate tool set changed for own_tab={own:?} — text moved, tools must not"
            );
            assert!(!expected.is_empty(), "the fixture advertises nothing");
        }
    }

    /// A harness with no Manual tab gets no tool - **no dead tools**.
    #[test]
    fn only_harnesses_with_a_manual_tab_are_advertised() {
        let none = Settings::default();
        assert!(delegate_targets(&none, None).is_empty(), "no tabs, no tools");

        let s = settings_with_manual(crate::settings::CLAUDE_TAB_ID);
        let ids: Vec<&str> = delegate_targets(&s, None)
            .iter()
            .map(|(h, _)| h.token())
            .collect();
        assert_eq!(ids.len(), 1, "exactly the harness that has a Manual tab");
    }

    /// **A tab never sees a tool that drives itself** (locked decision 3, and
    /// live-verify 6's second clause).
    #[test]
    fn a_tab_is_not_offered_a_tool_that_drives_itself() {
        let s = settings_with_manual(crate::settings::CLAUDE_TAB_ID);
        assert_eq!(delegate_targets(&s, None).len(), 1);
        assert!(
            delegate_targets(&s, Some(crate::settings::CLAUDE_TAB_ID)).is_empty(),
            "the Manual tab itself must not be offered its own delegation tool"
        );
        assert_eq!(
            delegate_targets(&s, Some(crate::settings::OPENCODE_TAB_ID)).len(),
            1,
            "…while a different tab still sees it"
        );
    }

    /// The harness a reserved tab id runs, from the registry.
    ///
    /// V39's tests wrote `"claude"` beside `CLAUDE_TAB_ID` by hand. The two are
    /// joined by a descriptor; a test that hard-codes the join asserts today's
    /// roster instead of the relation under test (V40 Phase G, decision 28).
    fn worker_harness(tab_id: &str) -> crate::harness::HarnessId {
        crate::harness::HarnessId::from_tab_id(tab_id)
            .unwrap_or_else(|| panic!("{tab_id} is a reserved tab id with no descriptor"))
    }

    /// **A blocked worker gate removes that harness's tool** - the fail-closed
    /// half of locked decision 16, observed where the model would see it.
    #[test]
    fn a_blocked_worker_gate_advertises_nothing() {
        let mut s = settings_with_manual(crate::settings::CLAUDE_TAB_ID);
        assert_eq!(delegate_targets(&s, None).len(), 1);
        s.harness_row(worker_harness(crate::settings::CLAUDE_TAB_ID).token())
            .input_profile_status = "fail".to_string();
        assert!(
            delegate_targets(&s, None).is_empty(),
            "a recorded input-profile failure must remove that harness's delegation tool, not \
             just refuse at call time"
        );
    }

    /// **V40 Phase B, amendment 0-f: the failure is scoped to the harness it
    /// was recorded against.**
    ///
    /// The status used to be ONE scalar for every harness, which is two defects
    /// in one field: a `"fail"` recorded against Claude's TUI removed the
    /// OpenCode tool as well (a worker the user could have used, gone with no
    /// explanation naming it), and a `"pass"` recorded against Claude vouched
    /// for a TUI nobody had typed into. Both directions are checked here.
    #[test]
    fn a_blocked_worker_gate_removes_only_that_harness() {
        let mut s = settings_with_manual(crate::settings::CLAUDE_TAB_ID);
        s.tabs.push(crate::settings::TabConfig::AiTool({
            let crate::settings::TabConfig::AiTool(mut c) =
                crate::settings::default_opencode_tab()
            else {
                unreachable!("the default OpenCode tab is an AI tab")
            };
            c.delegation_role = crate::settings::DelegationRole::Manual;
            c
        }));
        assert_eq!(delegate_targets(&s, None).len(), 2, "both are workers");

        // The two harnesses come from the two tabs the settings hold, so this
        // asserts the RELATION (a failure is scoped to the harness it names)
        // rather than today's roster.
        let a = worker_harness(crate::settings::CLAUDE_TAB_ID);
        let b = worker_harness(crate::settings::OPENCODE_TAB_ID);

        // `a`'s spike failed; `b`'s did not.
        s.harness_row(a.token()).input_profile_status = "fail".to_string();
        let left = delegate_targets(&s, None);
        assert_eq!(left.len(), 1, "only the failing harness drops out: {left:?}");
        assert_eq!(left[0].0, b);

        // …and the reverse: `a` passing does not vouch for `b`.
        let mut s2 = s.clone();
        s2.harness_row(a.token()).input_profile_status = "pass".to_string();
        s2.harness_row(b.token()).input_profile_status = "fail".to_string();
        let left = delegate_targets(&s2, None);
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(left[0].0, a);
    }

    // ── V39 Phase C — the facade in the tool prose ──────────────────────────

    /// One Remote-offload Claude tab named `lan-worker-2`, beside nothing else.
    fn settings_with_facade(backend_name: &str) -> Settings {
        let mut s = Settings::default();
        s.tabs.push(crate::settings::facade_tab("worker-tab", backend_name));
        s
    }

    /// **The driver must not be able to tell** (locked decision 3). The kind
    /// label for a facade is `LAN` — the same word a trusted off-box model
    /// server gets — and the description must carry the user's backend name
    /// and no trace of the tab, its id or its harness.
    #[test]
    fn the_backend_prose_names_the_backend_and_never_the_tab() {
        let s = settings_with_facade("lan-worker-2");
        let facade = s
            .effective_offload_backends()
            .into_iter()
            .find(|b| b.name == "lan-worker-2")
            .expect("the facade is in the pool");
        let label = backend_label(&facade, &s.offload);
        assert!(
            label.starts_with("lan-worker-2 (LAN, "),
            "a facade reads as a LAN backend: {label}"
        );

        let desc = offload_task_description(&s);
        assert!(desc.contains("lan-worker-2"), "the backend name is advertised: {desc}");
        // Every registered harness's id AND label, from the registry: a third
        // harness must be covered by this leak check the day it is registered.
        let mut leaks: Vec<String> =
            vec!["worker-tab".into(), "tab worker-tab".into(), "tab \"".into()];
        for h in crate::harness::registry::all() {
            leaks.push(h.token().to_string());
            leaks.push(h.label().to_string());
        }
        for leak in &leaks {
            assert!(
                !desc.contains(leak),
                "the facade leaked {leak:?} into the offload_task description: {desc}"
            );
        }
    }

    /// A tab's declared context reaches the prose the same way a configured
    /// backend's does — the facade is described in the pool's own vocabulary,
    /// not in one of its own.
    #[test]
    fn a_facade_is_described_in_the_same_vocabulary_as_every_other_backend() {
        let mut s = settings_with_facade("lan-worker-2");
        for t in s.tabs.iter_mut() {
            if let TabConfig::AiTool(c) = t {
                c.delegation_backend.declared_context = Some(128_000);
                c.delegation_backend.tier = crate::settings::BackendTier::Fast;
            }
        }
        let facade = s
            .effective_offload_backends()
            .into_iter()
            .find(|b| b.name == "lan-worker-2")
            .expect("facade");
        let label = backend_label(&facade, &s.offload);
        assert_eq!(label, "lan-worker-2 (LAN, fast, ~128k ctx, all tools)");
    }

    /// **Role exclusivity, observed at the two surfaces** (locked decision 8:
    /// one enum, not two flags). A Remote-offload tab is a backend and NOT a
    /// `delegate_task_*` target; a Manual tab is the reverse. Asserted here
    /// rather than on the enum because "one enum" is only useful if both
    /// consumers actually read it.
    #[test]
    fn a_remote_offload_tab_is_a_backend_and_not_a_delegate_target() {
        let s = settings_with_facade("lan-worker-2");
        assert!(
            delegate_targets(&s, None).is_empty(),
            "a Remote-offload tab must not be advertised as a delegate_task_* target"
        );
        assert!(
            s.effective_offload_backends()
                .iter()
                .any(|b| b.name == "lan-worker-2"),
            "…and it must be in the offload pool"
        );
    }

    #[test]
    fn a_manual_tab_is_a_delegate_target_and_not_a_backend() {
        let s = settings_with_manual(crate::settings::CLAUDE_TAB_ID);
        assert_eq!(delegate_targets(&s, None).len(), 1);
        assert!(
            !s.effective_offload_backends()
                .iter()
                .any(|b| matches!(b.kind, OffloadBackendKind::HarnessTab { .. })),
            "a Manual tab must not synthesize an offload backend"
        );
    }

    /// **The degraded child cannot drive a tab, and says so by having no such
    /// backend.** The facade is in the child's *prose* (it is configured, and an
    /// unreachable LAN box is described too) but never in the pool the child
    /// would route to itself — an unreachable app does not mean "do it here", it
    /// means there is no worker.
    #[test]
    fn the_headless_child_resolves_no_facade_backend() {
        let s = settings_with_facade("lan-worker-2");
        let facade = s
            .effective_offload_backends()
            .into_iter()
            .find(|b| b.name == "lan-worker-2")
            .expect("facade");
        assert!(
            ResolvedBackend::from_config(&facade).is_none(),
            "the child must not resolve a facade into its own pool"
        );
    }

    /// The prefix round-trips: what the generator names is what the dispatcher
    /// claims, and the suffix is the registry's own harness id.
    #[test]
    fn the_tool_name_prefix_round_trips_to_a_registry_harness_id() {
        for id in crate::harness::registry::harness_ids() {
            let name = format!("{DELEGATE_TOOL_PREFIX}{id}");
            assert!(name.starts_with(DELEGATE_TOOL_PREFIX));
            assert_eq!(name.strip_prefix(DELEGATE_TOOL_PREFIX), Some(id));
            assert!(crate::harness::HarnessId::from_id(id).is_some());
        }
        // A bare prefix names no harness and must not dispatch.
        assert_eq!(
            DELEGATE_TOOL_PREFIX
                .strip_prefix(DELEGATE_TOOL_PREFIX)
                .filter(|h: &&str| !h.is_empty()),
            None
        );
    }
}

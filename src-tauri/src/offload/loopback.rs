//! V8-03 loopback proxy channel — the authenticated localhost endpoint the
//! per-session `--offload-mcp` child forwards to when the app is running.
//!
//! A minimal hand-rolled HTTP/1.1 service on `127.0.0.1:0` (ephemeral port),
//! gated by a per-launch bearer token. It exposes exactly three routes —
//! purpose-built for offload, **not** a general local API:
//!
//! - `POST /run` — run one `offload_task` against the warm app-side pool.
//! - `GET  /describe` — the live capability description for the tool.
//! - `GET  /events` — an SSE stream of capability-change pulses the child
//!   relays to Claude as `notifications/tools/list_changed`.
//!
//! The `{port, token, pid}` are advertised in a discovery file written next
//! to the exe (the portable root — never `~/.claude`), created when offload
//! is enabled and removed on exit. The token rotates every launch. This is
//! the one genuinely new security surface: loopback-only bind + token auth +
//! a user-readable discovery file (tightened where the OS allows). A
//! malicious *local* process that reads the file could drive offloads or
//! observe task text — the same localhost-dev-server trust assumption,
//! documented in MAINTENANCE.md.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};

use super::agent::ThinkingMode;
use super::mcp_host::Consumer;
use super::router::TierHint;
use super::service::OffloadService;

/// Discovery-file name under the portable root (next to `settings.json`).
const DISCOVERY_FILE: &str = ".cimp-offload.json";

/// The discovery file the child reads to find + authenticate to the app.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Discovery {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

/// `<exe-dir>/.cimp-offload.json` — the portable-root discovery path. Falls
/// back to the cwd if `current_exe()` is unavailable (mirrors
/// `settings::global_path`).
pub fn discovery_path() -> PathBuf {
    match std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
    {
        Some(dir) => dir.join(DISCOVERY_FILE),
        None => PathBuf::from(DISCOVERY_FILE),
    }
}

/// Read the discovery file, if present and parseable.
pub fn read_discovery() -> Option<Discovery> {
    let text = std::fs::read_to_string(discovery_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// A per-launch random bearer token (two v4 UUIDs of entropy, hex). Avoids
/// pulling a separate RNG crate — `uuid` is already a dependency.
fn make_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// A running loopback endpoint. Holds the port/token and the discovery path
/// so [`Self::stop`] can remove the file on exit. `port`/`token` round out
/// the handle for diagnostics + any future direct status read.
pub struct Loopback {
    #[allow(dead_code)]
    pub port: u16,
    #[allow(dead_code)]
    pub token: String,
    discovery: PathBuf,
}

impl Loopback {
    /// Bind the endpoint, write the discovery file, and spawn the accept
    /// loop. Returns the handle (managed in `AppState`). Idempotent at the
    /// file level — an existing (stale) discovery file is overwritten.
    pub async fn start(service: Arc<OffloadService>, app: AppHandle) -> AppResult<Arc<Self>> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| AppError::Offload(format!("loopback bind failed: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| AppError::Offload(format!("loopback addr failed: {e}")))?
            .port();
        let token = make_token();
        let discovery = discovery_path();

        let disc = Discovery {
            port,
            token: token.clone(),
            pid: std::process::id(),
        };
        write_discovery(&discovery, &disc)?;
        info!(port, "offload loopback: listening on 127.0.0.1");

        // Accept loop.
        let accept_token = token.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let svc = service.clone();
                        let app = app.clone();
                        let tok = accept_token.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = handle_conn(stream, svc, app, tok).await {
                                debug!(error = %e, "offload loopback: connection ended");
                            }
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "offload loopback: accept failed");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });

        Ok(Arc::new(Self {
            port,
            token,
            discovery,
        }))
    }

    /// Remove the discovery file (graceful exit / disable). The accept task
    /// is detached and dies with the process.
    ///
    /// Only removes the file if it still belongs to *this* process. The
    /// discovery path is shared per exe-dir, so a second app instance can
    /// overwrite it with its own port/token/pid; deleting that on our exit
    /// would leave the surviving instance undiscoverable to its offload
    /// children. (The start-time clobber itself is inherent to running two
    /// instances from one install and is left as last-writer-wins.)
    pub fn stop(&self) {
        if let Some(d) = read_discovery() {
            if d.pid != std::process::id() {
                debug!(
                    owner_pid = d.pid,
                    "offload loopback: discovery file owned by another instance; not removing"
                );
                return;
            }
        }
        if let Err(e) = std::fs::remove_file(&self.discovery) {
            if e.kind() != std::io::ErrorKind::NotFound {
                debug!(error = %e, "offload loopback: discovery cleanup failed");
            }
        }
    }
}

/// Write the discovery file, tightening permissions to user-only where the
/// OS supports it (best-effort).
fn write_discovery(path: &PathBuf, disc: &Discovery) -> AppResult<()> {
    let body = serde_json::to_string(disc)
        .map_err(|e| AppError::Offload(format!("discovery serialize: {e}")))?;
    std::fs::write(path, body).map_err(|e| AppError::Offload(format!("discovery write: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// A `POST /run` request body.
#[derive(Deserialize)]
struct RunBody {
    instructions: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    tier: Option<String>,
    /// The calling session's working directory (the repo Claude Code runs in),
    /// used as the native-tool root when no explicit `allowed_roots` is set.
    #[serde(default)]
    cwd: Option<String>,
    /// V21 F9: optional JSON Schema — when set, the worker's final answer is
    /// grammar-constrained to matching JSON. Absent on legacy child requests.
    #[serde(default)]
    schema: Option<serde_json::Value>,
}

/// A `POST /run` response.
#[derive(Serialize)]
struct RunResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// A parsed HTTP request: method, path, headers (lowercased keys), and body.
struct Request {
    method: String,
    path: String,
    auth: Option<String>,
    body: Vec<u8>,
}

/// Read and parse one HTTP/1.1 request from the stream (headers + an
/// optional Content-Length body). Bounded so a malformed client can't make
/// us read forever.
async fn read_request(stream: &mut TcpStream) -> AppResult<Request> {
    const MAX_HEADER: usize = 16 * 1024;
    const MAX_BODY: usize = 4 * 1024 * 1024;
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];

    // Read until the header terminator.
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEADER {
            return Err(AppError::Offload("request headers too large".into()));
        }
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| AppError::Offload(format!("read failed: {e}")))?;
        if n == 0 {
            return Err(AppError::Offload("connection closed before headers".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut auth = None;
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            match key.as_str() {
                "authorization" => auth = Some(val.to_string()),
                "content-length" => content_length = val.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    if content_length > MAX_BODY {
        return Err(AppError::Offload("request body too large".into()));
    }

    // Body bytes already buffered past the header terminator, plus any more.
    let mut body: Vec<u8> = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| AppError::Offload(format!("read body failed: {e}")))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    Ok(Request {
        method,
        path,
        auth,
        body,
    })
}

/// Find the first index of `needle` in `hay`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Whether the request carries the expected bearer token. Uses a constant-time
/// comparison so a local attacker can't recover the per-launch token byte by
/// byte from 401-response timing (best-effort; the length check leaks only the
/// fixed token length).
fn authorized(req: &Request, token: &str) -> bool {
    match &req.auth {
        Some(h) => h
            .strip_prefix("Bearer ")
            .map(|t| ct_eq(t.as_bytes(), token.as_bytes()))
            .unwrap_or(false),
        None => false,
    }
}

/// Constant-time byte-slice equality: compares all bytes of equal-length inputs
/// without short-circuiting on the first mismatch.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Handle one connection: route by method+path after checking auth.
async fn handle_conn(
    mut stream: TcpStream,
    service: Arc<OffloadService>,
    app: AppHandle,
    token: String,
) -> AppResult<()> {
    // Cap how long we'll wait for a complete request: a half-open or idle
    // connection (TCP probe, crashed child holding the socket) would otherwise
    // wedge this handler task forever, leaking one task per such connection.
    let req = match tokio::time::timeout(Duration::from_secs(30), read_request(&mut stream)).await {
        Ok(r) => r?,
        Err(_) => return Err(AppError::Offload("request read timed out".into())),
    };

    if !authorized(&req, &token) {
        write_simple(&mut stream, 401, "text/plain", b"unauthorized").await?;
        return Ok(());
    }

    // Match on the path without its query string (`/mcp/list?consumer=opencode`
    // must route the same as `/mcp/list`); handlers read the query themselves.
    let route = req.path.split('?').next().unwrap_or(&req.path);
    match (req.method.as_str(), route) {
        ("POST", "/run") => handle_run(&mut stream, &service, &req).await,
        ("POST", "/graph_run") => handle_graph_run(&mut stream, &app, &req).await,
        ("POST", "/context/retrieve") => handle_context_retrieve(&mut stream, &app, &req).await,
        ("POST", "/context/compaction") => handle_context_compaction(&mut stream, &app, &req).await,
        ("POST", "/context/should_read") => handle_should_read(&mut stream, &app, &req).await,
        ("POST", "/context/post_edit") => handle_post_edit(&mut stream, &app, &req).await,
        ("POST", "/memory/event") => handle_memory_event(&mut stream, &app, &req).await,
        ("POST", "/activity/contract_drift") => handle_contract_drift(&mut stream, &req).await,
        ("POST", "/mcp/list") => handle_mcp_list(&mut stream, &service, &req).await,
        ("POST", "/mcp/call") => handle_mcp_call(&mut stream, &service, &req).await,
        ("GET", "/describe") => {
            let text = service.describe().await;
            write_simple(
                &mut stream,
                200,
                "text/plain; charset=utf-8",
                text.as_bytes(),
            )
            .await
        }
        ("GET", "/events") => handle_events(stream, service).await,
        ("GET", "/health") => write_simple(&mut stream, 200, "text/plain", b"ok").await,
        _ => write_simple(&mut stream, 404, "text/plain", b"not found").await,
    }
}

/// How often the streamed `/run` response emits a heartbeat line while the task
/// is still running. The proxy treats a gap several times this long as "the
/// worker wedged" (see `mcp.rs`), so this must stay well under that idle window.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// `POST /run`: decode the task, run it on the warm pool, and **stream** the
/// result as newline-delimited JSON — periodic `{"hb":true}` heartbeats while
/// the (possibly minutes-long) task runs, then a final `{"ok":..}` line.
///
/// The heartbeats are the whole point of this being a stream: they let the
/// proxy distinguish a slow-but-alive job (keep waiting) from a dead app
/// (fall back), so a long run is never abandoned and re-executed. The response
/// has no `Content-Length`; the body is delimited by connection close.
async fn handle_run(
    stream: &mut TcpStream,
    service: &Arc<OffloadService>,
    req: &Request,
) -> AppResult<()> {
    let body: RunBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!("bad request body: {e}")),
            };
            return write_json(stream, 400, &r).await;
        }
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

    let session_cwd = body.cwd.map(std::path::PathBuf::from);

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
        body.schema,
        cancel.clone(),
    );
    tokio::pin!(run_fut);

    // Split so heartbeats/result (write half) and the disconnect probe (read
    // half) can run concurrently on the one connection.
    let (mut rd, mut wr) = stream.split();
    // Stream head — no Content-Length; the body is close-delimited NDJSON. Sent
    // up front so the proxy's `send()` resolves immediately and it knows the app
    // is alive before the (possibly long) task even starts.
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: application/x-ndjson\r\n\
                Cache-Control: no-cache\r\n\
                Connection: close\r\n\r\n";
    wr.write_all(head.as_bytes())
        .await
        .map_err(|e| AppError::Offload(format!("run head: {e}")))?;
    wr.flush().await.ok();

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
                if wr.write_all(b"{\"hb\":true}\n").await.is_err() {
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
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(e.to_string()),
        },
    };
    // Final line: one JSON object (serde emits no embedded newlines) + `\n`,
    // then the connection closes. `ok:false` here too is a task-level error the
    // child renders as a tool result so Claude can read + adapt.
    let mut line = serde_json::to_vec(&r)
        .unwrap_or_else(|_| br#"{"ok":false,"error":"failed to serialize result"}"#.to_vec());
    line.push(b'\n');
    wr.write_all(&line)
        .await
        .map_err(|e| AppError::Offload(format!("run result: {e}")))?;
    wr.flush().await.ok();
    Ok(())
}

/// A `POST /graph_run` request body (the warm code-graph query path).
#[derive(Deserialize)]
struct GraphRunBody {
    /// The calling session's working directory; the project root is resolved
    /// from it (same ancestor-walk the MCP child uses).
    #[serde(default)]
    cwd: Option<String>,
    /// The `graph_*` / `context_*` tool name.
    name: String,
    /// The tool arguments.
    #[serde(default)]
    args: Value,
    /// The requesting consumer (`"claude"` / `"opencode"`); selects the activity
    /// source and the `context_*` tools' per-agent session scope. Defaults to
    /// Claude when absent.
    #[serde(default)]
    consumer: Option<String>,
}

/// A `POST /mcp/call` request body (a Claude-exposed MCP tool invocation).
#[derive(Deserialize)]
struct McpCallBody {
    /// The namespaced `<server>__<tool>` name.
    name: String,
    /// The tool arguments.
    #[serde(default)]
    arguments: Value,
}

/// `POST /graph_run`: run one `graph_*` tool against the app's WARM graph index
/// (single shared connection — no second cross-process open of the SQLite store)
/// and return its text. The `GraphService` is resolved from managed state at
/// request time, so this is robust against the graph-vs-loopback startup order.
async fn handle_graph_run(stream: &mut TcpStream, app: &AppHandle, req: &Request) -> AppResult<()> {
    let body: GraphRunBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!("bad request body: {e}")),
            };
            return write_json(stream, 400, &r).await;
        }
    };
    let graph = match app.try_state::<Arc<crate::graph::GraphService>>() {
        Some(g) => g.inner().clone(),
        None => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some("graph service not ready".into()),
            };
            return write_json(stream, 200, &r).await;
        }
    };
    let cwd = body
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let consumer = body.consumer.as_deref().unwrap_or("claude");
    let r = match graph
        .run_graph_tool(&cwd, &body.name, &body.args, consumer)
        .await
    {
        Ok(text) => RunResult {
            ok: true,
            text: Some(text),
            error: None,
        },
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(e),
        },
    };
    // 200 even on a tool-level error: the child renders `error` as a tool result.
    write_json(stream, 200, &r).await
}

/// A `POST /context/retrieve` request body (from the Claude UserPromptSubmit
/// hook or the OpenCode injection plugin).
#[derive(Deserialize)]
struct ContextRetrieveBody {
    /// The calling session's working directory; the project root is resolved
    /// from it (defaults to `.`).
    #[serde(default)]
    cwd: Option<String>,
    /// The user's prompt to rank context against.
    prompt: String,
    /// The agent session id (scopes the working-set boost); optional.
    #[serde(default)]
    session_id: Option<String>,
    /// V13 Phase C: which agent shim is calling — `"claude"` (set by
    /// `context_hook.rs`) or `"opencode"` (set by the generated plugin);
    /// absent/`None` for an unrecognized caller. Recorded on the checkpoint
    /// it triggers (see [`WorkbenchService::on_prompt`](crate::workbench::WorkbenchService::on_prompt)),
    /// not otherwise used by context retrieval itself.
    #[serde(default)]
    agent: Option<String>,
}

/// `POST /context/retrieve`: rank files for the prompt and return the injectable
/// digest as `{ ok, text }`. Gated on `context_injection` — returns empty text
/// (never blocks a turn) when injection is off or nothing clears the threshold.
async fn handle_context_retrieve(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: ContextRetrieveBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let cwd = body
        .cwd
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    // V13 Phase C: fire the prompt-tap checkpoint trigger for EVERY prompt
    // that reaches this route, BEFORE the `context_injection` gate below —
    // checkpointing must fire even when injection is off or yields nothing
    // (Decision 4: decoupled from the injection toggle, reusing its
    // transport). Fire-and-forget: `on_prompt`'s own min-gap check is cheap
    // and the real snapshot work runs on a background task inside it, so
    // this never delays the turn waiting on a `git` round trip.
    // FIX 8 (V13 code review): only spawn the task at all when checkpoints
    // are actually on — `on_prompt`/`maybe_snapshot` already no-op when
    // they're off, but that check used to happen AFTER a task was already
    // spawned for every single prompt, which is needless per-prompt work
    // (a task spawn plus a settings read) for a feature the user has
    // disabled.
    if let Some(workbench) = app.try_state::<Arc<crate::workbench::WorkbenchService>>() {
        let workbench = workbench.inner().clone();
        if workbench.checkpoints_enabled() {
            let root = cwd.clone();
            let agent = body.agent.clone();
            let prompt_head: String = body.prompt.chars().take(80).collect();
            tauri::async_runtime::spawn(async move {
                workbench.on_prompt(&root, agent, &prompt_head).await;
            });
        }
    }

    let empty = serde_json::json!({ "ok": true, "text": "", "files": [], "tokens_est": 0 });
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return write_json(stream, 200, &empty).await;
    };
    let graph = graph.inner().clone();
    // The injection toggle is enforced here (the service's retrieve does not) so
    // the preview surface can reuse the same core while injection is off.
    if !graph.context_injection_enabled() {
        return write_json(stream, 200, &empty).await;
    }
    let r = graph.retrieve_context(&cwd, &body.prompt, body.session_id.as_deref());
    // V11 Phase B: prepend the once-per-session project map. Done here (the real
    // injection path), not in `retrieve_context`, so the preview surface — which
    // also calls `retrieve_context` — never consumes the once-per-session flag.
    let mut text = r.context_md;
    if let Some(map) = graph.session_greeting(&cwd, body.session_id.as_deref()) {
        text = if text.is_empty() {
            map
        } else {
            format!("{map}\n\n{text}")
        };
    }
    // V12 Phase F: drain any auto-check block a slow post-edit run parked for
    // this session (see `GraphService::post_edit`'s budget/park path) — a
    // turn is never blocked waiting for a check, but its result still reaches
    // the model on the very next opportunity.
    if let Some(pending) = graph.drain_auto_check(body.session_id.as_deref()) {
        text = if text.is_empty() {
            pending
        } else {
            format!("{text}\n\n{pending}")
        };
    }
    // Same char→token estimate as the retrieval core (shared divisor so the two
    // can't drift). Estimated from the FULL injected text (digest + greeting +
    // drained auto-check), not just the digest.
    let tokens_est = crate::graph::est_tokens(text.chars().count());
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "text": text, "files": r.files_used, "tokens_est": tokens_est }),
    )
    .await
}

/// A `POST /context/compaction` request body (the Claude `PreCompact` shim).
#[derive(Deserialize)]
struct ContextCompactionBody {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    /// `"manual"` / `"auto"`; recorded, not currently branched on.
    #[serde(default)]
    #[allow(dead_code)]
    trigger: Option<String>,
}

/// `POST /context/compaction` (V11 Phase D): always runs the session's
/// compaction side effects (clear injection dedup, mark post-compaction) and
/// returns a compact working-set/notes block as `{ ok, text }` to carry through
/// the summary. Never blocks — an empty block is returned as empty text.
async fn handle_context_compaction(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: ContextCompactionBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let empty = serde_json::json!({ "ok": true, "text": "" });
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return write_json(stream, 200, &empty).await;
    };
    let graph = graph.inner().clone();
    let cwd = body
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let block = graph.compaction_context(&cwd, body.session_id.as_deref());
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "text": block.unwrap_or_default() }),
    )
    .await
}

/// A `POST /context/should_read` request body (the Claude `PreToolUse` Read
/// advisor shim).
#[derive(Deserialize)]
struct ShouldReadBody {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    file_path: String,
    /// 1-based read offset, when the agent asked for a windowed read.
    #[serde(default)]
    offset: Option<u32>,
    /// V17 Phase B: the `Read` line limit, when the agent asked for a slice.
    /// Forwarded so the verdict can tell a full read from a head-peek (a
    /// deliberate slice always passes — Phase C's first-read branch).
    #[serde(default)]
    limit: Option<u32>,
}

/// `POST /context/should_read` (V11 Phase E): the read-advisor verdict for a
/// `Read`. Returns `{ ok, verdict: "pass" }` to let the read through, or
/// `{ ok, verdict: "remind", text }` to deny-with-content. Fails open to `pass`
/// on any missing state — the advisor must never block a legitimate read.
async fn handle_should_read(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let pass = serde_json::json!({ "ok": true, "verdict": "pass" });
    let body: ShouldReadBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return write_json(stream, 200, &pass).await;
    };
    let graph = graph.inner().clone();
    let cwd = body
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    match graph.should_read(
        &cwd,
        body.session_id.as_deref(),
        &body.file_path,
        body.offset,
        body.limit,
    ) {
        Some(text) => {
            write_json(
                stream,
                200,
                &serde_json::json!({ "ok": true, "verdict": "remind", "text": text }),
            )
            .await
        }
        None => write_json(stream, 200, &pass).await,
    }
}

/// A `POST /activity/contract_drift` request body (V16 Feature 3): a hook
/// shim reporting a payload that was missing required fields.
#[derive(Deserialize)]
struct ContractDriftBody {
    shim: String,
    #[serde(default)]
    missing: Vec<String>,
    #[serde(default)]
    session_id: Option<String>,
}

/// Rate-limit state for `handle_contract_drift`: `(shim, session_id)` pairs
/// already recorded this app run. A systematically broken payload fires the
/// shim on every hook invocation — without this, one bad session would
/// flood the Activity store's graph ring. Process-lifetime is the right
/// scope: the Advisor's `drift.payload.v1` reads events since process
/// start anyway. A missing `session_id` (itself likely part of the drift)
/// buckets under the empty string — still one event per shim per run.
static CONTRACT_DRIFT_SEEN: std::sync::OnceLock<std::sync::Mutex<HashSet<(String, String)>>> =
    std::sync::OnceLock::new();

/// `POST /activity/contract_drift` (V16 Feature 3): record a shim's
/// payload-drift report as an Activity event (`source: "harness"`,
/// `tool: "contract_drift"`), rate-limited to one per shim per session.
/// Always answers `{ok: true}` — the shim is fail-open and fire-and-forget.
async fn handle_contract_drift(stream: &mut TcpStream, req: &Request) -> AppResult<()> {
    let ok = serde_json::json!({ "ok": true });
    let body: ContractDriftBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let session = body.session_id.unwrap_or_default();
    let fresh = {
        let seen = CONTRACT_DRIFT_SEEN.get_or_init(|| std::sync::Mutex::new(HashSet::new()));
        let mut seen = seen.lock().unwrap_or_else(|p| p.into_inner());
        seen.insert((body.shim.clone(), session.clone()))
    };
    if fresh {
        let missing = body.missing.join(", ");
        crate::activity::record_bg(crate::activity::ActivityRecord {
            entry: crate::activity::ActivityEntry::new(
                crate::activity::ActivityKind::Graph,
                crate::activity::now_ms(),
                String::new(), // no root — the report is about the harness, not a project
                "harness".to_string(),
                "contract_drift".to_string(),
                format!("{}: {missing}", body.shim),
                missing.chars().count(),
                0,
                false, // a drift report is never "ok" — it flags the entry in the feed
            ),
            request: format!(
                "shim {} payload missing required fields (session {session})",
                body.shim
            ),
            response: missing,
        });
    }
    write_json(stream, 200, &ok).await
}

/// A `POST /context/post_edit` request body (the Claude `PostToolUse` shim, or
/// the OpenCode plugin's `tool.execute.after` hook).
#[derive(Deserialize)]
struct ContextPostEditBody {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    file_path: String,
    /// Recorded for symmetry with the shim's payload; not currently branched
    /// on (the matcher/plugin already scope this to edit-class tools).
    #[serde(default)]
    #[allow(dead_code)]
    tool_name: Option<String>,
}

/// `POST /context/post_edit` (V12 Phase F): debounce this session's edits, run
/// the project's configured checks single-flight per root, diff against the
/// session's own baseline, and return only NEW/worsened diagnostics (plus an
/// optional auto-impact note) as `{ ok, text }`. Fails open to empty text on
/// any missing state — the hook must never block or perturb an edit.
async fn handle_post_edit(stream: &mut TcpStream, app: &AppHandle, req: &Request) -> AppResult<()> {
    let empty = serde_json::json!({ "ok": true, "text": "" });
    let body: ContextPostEditBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return write_json(stream, 200, &empty).await;
    };
    let graph = graph.inner().clone();
    let cwd = body
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let text = graph
        .post_edit(&cwd, body.session_id.as_deref(), &body.file_path)
        .await
        .unwrap_or_default();
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "text": text }),
    )
    .await
}

/// A `POST /memory/event` request body (the OpenCode plugin's tool hook — the
/// only memory ingress for OpenCode, whose OOB SSE stream carries no tool
/// events, AND — V14 Phase C — the only *usage* ingress for OpenCode, for the
/// same reason). Claude records both in-process via the transcript tap instead.
#[derive(Deserialize)]
struct MemoryEventBody {
    #[serde(default)]
    cwd: Option<String>,
    session_id: String,
    #[serde(default)]
    agent: Option<String>,
    // Tool-event shape (V10): present on `tool.execute.after` POSTs. Optional
    // now that the same route also carries usage bodies (V24 Phase F), which
    // have no `tool`.
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    args: Value,
    // V24 Phase F usage shape: `kind == "usage"`, emitted by the plugin's
    // `event` hook on a completed assistant turn.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    parent_session_id: Option<String>,
    #[serde(default)]
    msg_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    in_tok: u32,
    #[serde(default)]
    out_tok: u32,
    #[serde(default)]
    cache_read: u32,
    #[serde(default)]
    cache_make: u32,
}

/// V24 Phase F: from an OpenCode usage POST body, the target session id and the
/// [`crate::graph::UsageEvent::Turn`] to record — or `None` when the body has no
/// usable data (missing/empty `msg_id`, or all four token totals are zero).
///
/// When `parent_session_id` is present the spend rolls up to the PARENT session
/// with `origin: Agent` (sub-agent spend is the parent's spend — mirrors the
/// Claude sub-agent contract); otherwise it's the reporting session with
/// `origin: Session`. A model id that is absent/empty maps to `None` (unknown
/// model), matching the Claude tap. Pure, so the mapping is unit-tested without
/// a live handler.
fn usage_event_from_body(body: &MemoryEventBody) -> Option<(String, crate::graph::UsageEvent)> {
    let msg_id = body.msg_id.clone().filter(|m| !m.is_empty())?;
    // The plugin only forwards COMPLETED turns, so an all-zero body is a
    // degenerate/creation emit — skip it rather than plant an empty turn row.
    // `est_only` is unaffected either way (it's derived from the summed token
    // totals, which a zero row doesn't move), so skipping only keeps the turn
    // series free of noise; it never resurrects real data.
    if body.in_tok == 0 && body.out_tok == 0 && body.cache_read == 0 && body.cache_make == 0 {
        return None;
    }
    let (target, origin) = match body.parent_session_id.as_deref() {
        Some(p) if !p.is_empty() => (p.to_string(), crate::graph::UsageOrigin::Agent),
        _ => (body.session_id.clone(), crate::graph::UsageOrigin::Session),
    };
    Some((
        target,
        crate::graph::UsageEvent::Turn {
            msg_id,
            model: body.model.clone().filter(|m| !m.is_empty()),
            in_tok: body.in_tok,
            out_tok: body.out_tok,
            cache_read: body.cache_read,
            cache_make: body.cache_make,
            origin,
        },
    ))
}

/// V24 Phase F: for a tool-event body (`tool.execute.after`), the parent
/// session id when the reporting session is a task-tool CHILD (sub-agent), else
/// `None`. A child's tool events mirror the Claude sidechain contract (see
/// `oob/claude.rs` `record_tool_events`, which early-returns on `isSidechain`):
/// they are dropped rather than recorded against the child, and only the parent
/// is marked live. Pure, so the routing is unit-tested without a live handler.
fn tool_event_parent(body: &MemoryEventBody) -> Option<String> {
    body.parent_session_id
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// `POST /memory/event`: classify an agent tool call and record it as a memory
/// event, AND (V14 Phase C) record its estimated usage. Best-effort — an
/// unclassifiable tool or a missing graph service is a silent no-op (200 with
/// memory recording skipped; usage recording no-ops internally the same way),
/// never an error the plugin has to handle.
async fn handle_memory_event(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: MemoryEventBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let ok = serde_json::json!({ "ok": true });
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return write_json(stream, 200, &ok).await;
    };
    let graph = graph.inner().clone();
    let cwd = body
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let agent = body.agent.as_deref().unwrap_or("opencode");

    // V24 Phase F: the usage arm — a completed assistant turn's real token
    // totals (OpenCode's only exact-token ingress; see the spike note atop
    // `oob/opencode.rs`). Distinct body shape (`kind == "usage"`, no `tool`),
    // so it short-circuits the tool-event path below.
    if body.kind.as_deref() == Some("usage") {
        if let Some((target, event)) = usage_event_from_body(&body) {
            // Roll-up target = the parent when a child (sub-agent) session
            // reported, else the reporting session itself. `record_usage`
            // upserts by `msg_id`, so the plugin's duplicate final emit is
            // harmless.
            graph.record_usage(&cwd, &target, agent, event);
            // Mark the SAME id live: the target is the session row that exists
            // / gets the spend attributed (the parent when a child reports),
            // so that's the row the Sessions list should flag active.
            graph.mark_live_session(&target, agent, &target);
        }
        return write_json(stream, 200, &ok).await;
    }

    // Tool-event path (V10): requires a `tool` name. A body without one and not
    // a usage event has nothing to record.
    let Some(tool_name) = body.tool.clone() else {
        return write_json(stream, 200, &ok).await;
    };

    // V24 Phase F: a task-tool CHILD (sub-agent) session's tool events are the
    // sub-agent's own working set, not the parent's — mirror the Claude sidechain
    // contract (oob/claude.rs `record_tool_events` early-returns on isSidechain)
    // and drop them entirely: no mem event, no tool-result chars against the
    // child, and the child is never marked live. The child's real token spend
    // still reaches the parent via the usage arm above; mark the PARENT live so
    // the sub-agent's activity keeps the parent's row active.
    if let Some(parent) = tool_event_parent(&body) {
        graph.mark_live_session(&parent, agent, &parent);
        return write_json(stream, 200, &ok).await;
    }

    if let Some((kind, arg)) = crate::graph::classify_tool(&tool_name) {
        let get = |k: &str| body.args.get(k).and_then(Value::as_str);
        let (path, detail) = match arg {
            crate::graph::MemArg::Path => (
                get("file_path")
                    .or_else(|| get("filePath"))
                    .or_else(|| get("notebook_path"))
                    .or_else(|| get("path"))
                    .unwrap_or("")
                    .to_string(),
                None,
            ),
            crate::graph::MemArg::Pattern => (
                get("pattern")
                    .or_else(|| get("path"))
                    .or_else(|| get("query"))
                    .unwrap_or("")
                    .to_string(),
                None,
            ),
            crate::graph::MemArg::Command => (
                String::new(),
                get("command").map(|c| c.chars().take(200).collect::<String>()),
            ),
        };
        // Skip an event with no usable target: an empty path (Path/Pattern) or a
        // Command whose `command` arg was absent (detail is None) — recording it
        // would just evict useful events from the ring.
        let recordable = match arg {
            crate::graph::MemArg::Command => detail.is_some(),
            _ => !path.is_empty(),
        };
        if recordable {
            graph.record_mem_event(
                &cwd,
                &body.session_id,
                agent,
                kind,
                &path,
                None,
                None,
                detail.as_deref(),
            );
        }
    }

    // V14 Phase C: OpenCode's usage tap (see the C3 spike note atop
    // `oob/opencode.rs` — its SSE stream carries no usage fields, so this
    // hook, which already fires after every tool call, is the only place
    // that can record OpenCode usage). Unlike the memory recording above,
    // this runs for EVERY tool call, not just ones `classify_tool` maps to a
    // filesystem/query target — usage wants the full picture. `chars` is
    // estimated from the tool's serialized INPUT args (its actual output
    // isn't visible to this hook). This path records only tool-result chars,
    // never Turn tokens, so a session that never got a real usage event stays
    // est-only in the X-ray (V24 Phase E derives `est_only` from zero token
    // totals — see `usage_row_for_session`).
    let chars = serde_json::to_string(&body.args)
        .map(|s| s.chars().count())
        .unwrap_or(0) as u32;
    graph.record_usage(
        &cwd,
        &body.session_id,
        agent,
        crate::graph::UsageEvent::ToolResult {
            tool: Some(tool_name.clone()),
            chars,
        },
    );

    // V24 Phase B: OpenCode has no tab binding on this path, so the live-session
    // registry is keyed by the reporting session id itself; the entry expires by
    // TTL (there is no cancel signal to clear it — see the C3 spike note atop
    // `oob/opencode.rs`).
    graph.mark_live_session(&body.session_id, agent, &body.session_id);

    write_json(stream, 200, &ok).await
}

/// `POST /mcp/list`: the proxied MCP tool descriptors for the requesting
/// consumer (servers with that consumer's access flag), for the per-session
/// child to merge into its `tools/list`. The consumer is taken from the
/// `?consumer=` query (Claude when absent). Returns
/// `{ "tools": [ {name, description, inputSchema}, … ] }`.
async fn handle_mcp_list(
    stream: &mut TcpStream,
    service: &Arc<OffloadService>,
    req: &Request,
) -> AppResult<()> {
    let tools = service.mcp_tool_descriptors(consumer_of(req)).await;
    write_json(stream, 200, &serde_json::json!({ "tools": tools })).await
}

/// `POST /mcp/call`: run one proxied MCP tool for the requesting consumer.
/// Body `{name, arguments}`; consumer from `?consumer=` (Claude when absent).
/// The service guards the call against any tool not offered by a server
/// exposed to that consumer. 200 even on a tool-level error (the child renders
/// `error` as a tool result).
async fn handle_mcp_call(
    stream: &mut TcpStream,
    service: &Arc<OffloadService>,
    req: &Request,
) -> AppResult<()> {
    let body: McpCallBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!("bad request body: {e}")),
            };
            return write_json(stream, 400, &r).await;
        }
    };
    let r = match service
        .mcp_call(consumer_of(req), &body.name, body.arguments)
        .await
    {
        Ok(text) => RunResult {
            ok: true,
            text: Some(text),
            error: None,
        },
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(e),
        },
    };
    write_json(stream, 200, &r).await
}

/// Parse the `?consumer=<name>` query value off a request path into a
/// [`Consumer`]. Absent / unknown ⇒ Claude (the original default).
fn consumer_of(req: &Request) -> Consumer {
    let raw = req
        .path
        .split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("consumer=")))
        .unwrap_or("claude");
    Consumer::parse(raw)
}

/// `GET /events`: an SSE stream emitting a `change` event per capability
/// pulse, with periodic keep-alive comments so idle proxies don't drop it.
async fn handle_events(mut stream: TcpStream, service: Arc<OffloadService>) -> AppResult<()> {
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Connection: keep-alive\r\n\r\n";
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| AppError::Offload(format!("events head: {e}")))?;
    // Prime the stream so the child's reader unblocks immediately.
    let _ = stream.write_all(b": connected\n\n").await;
    let _ = stream.flush().await;

    let mut rx = service.subscribe_changes();
    loop {
        let tick = tokio::time::sleep(Duration::from_secs(20));
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Ok(()) => {
                        if stream.write_all(b"event: change\ndata: {}\n\n").await.is_err() {
                            break;
                        }
                        let _ = stream.flush().await;
                    }
                    // Lagged: still emit one change so the child re-syncs.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if stream.write_all(b"event: change\ndata: {}\n\n").await.is_err() {
                            break;
                        }
                        let _ = stream.flush().await;
                    }
                    Err(_) => break, // sender dropped
                }
            }
            _ = tick => {
                if stream.write_all(b": keep-alive\n\n").await.is_err() {
                    break;
                }
                let _ = stream.flush().await;
            }
        }
    }
    Ok(())
}

/// Write a one-shot response with a JSON-serializable body.
async fn write_json<T: Serialize>(stream: &mut TcpStream, status: u16, body: &T) -> AppResult<()> {
    let json = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    write_simple(stream, status, "application/json; charset=utf-8", &json).await
}

/// Write a one-shot HTTP response and close.
async fn write_simple(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> AppResult<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| AppError::Offload(format!("write head: {e}")))?;
    stream
        .write_all(body)
        .await
        .map_err(|e| AppError::Offload(format!("write body: {e}")))?;
    let _ = stream.flush().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_subslice_locates_header_end() {
        let hay = b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody";
        let pos = find_subslice(hay, b"\r\n\r\n").unwrap();
        assert_eq!(&hay[pos..pos + 4], b"\r\n\r\n");
    }

    #[test]
    fn authorized_requires_exact_bearer() {
        let req = Request {
            method: "POST".into(),
            path: "/run".into(),
            auth: Some("Bearer abc123".into()),
            body: Vec::new(),
        };
        assert!(authorized(&req, "abc123"));
        assert!(!authorized(&req, "nope"));
        let none = Request {
            method: "GET".into(),
            path: "/describe".into(),
            auth: None,
            body: Vec::new(),
        };
        assert!(!authorized(&none, "abc123"));
    }

    #[test]
    fn token_is_long_and_random() {
        let a = make_token();
        let b = make_token();
        assert_ne!(a, b);
        assert!(a.len() >= 32);
    }

    // ── V24 Phase F: OpenCode usage-event arm ──────────────────────────────

    fn usage_body(json: serde_json::Value) -> MemoryEventBody {
        serde_json::from_value(json).expect("usage body deserializes")
    }

    #[test]
    fn usage_body_well_formed_records_session_turn() {
        // No parent → recorded against the reporting session with origin Session.
        let body = usage_body(serde_json::json!({
            "cwd": ".", "agent": "opencode", "kind": "usage",
            "session_id": "ses_main", "msg_id": "msg_1", "model": "qwen3-coder",
            "in_tok": 100, "out_tok": 40, "cache_read": 20, "cache_make": 5,
        }));
        let (target, event) =
            usage_event_from_body(&body).expect("well-formed body yields an event");
        assert_eq!(target, "ses_main");
        match &event {
            crate::graph::UsageEvent::Turn {
                msg_id,
                model,
                in_tok,
                out_tok,
                cache_read,
                cache_make,
                origin,
            } => {
                assert_eq!(msg_id, "msg_1");
                assert_eq!(model.as_deref(), Some("qwen3-coder"));
                assert_eq!(
                    (*in_tok, *out_tok, *cache_read, *cache_make),
                    (100, 40, 20, 5)
                );
                assert_eq!(*origin, crate::graph::UsageOrigin::Session);
            }
            _ => panic!("expected a Turn event"),
        }

        // Recording it lands a real turn row (est_only clears).
        let dir = std::env::temp_dir().join(format!("cimp-usage-sess-{}", uuid::Uuid::new_v4()));
        let idx = crate::graph::GraphIndex::open(&dir, ".ckg").expect("open");
        idx.record_usage_event(&target, "opencode", &event, 100)
            .unwrap();
        let series = idx.usage_turn_series("ses_main").unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].msg_id, "msg_1");
        assert_eq!(series[0].origin, crate::graph::UsageOrigin::Session);
        assert_eq!(series[0].in_tok, 100);
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_body_with_parent_rolls_up_as_agent() {
        // A child (sub-agent) session's spend is attributed to the PARENT with
        // origin Agent — mirrors the Claude sub-agent contract.
        let body = usage_body(serde_json::json!({
            "kind": "usage", "session_id": "ses_child", "parent_session_id": "ses_parent",
            "msg_id": "msg_a", "model": "qwen3-coder",
            "in_tok": 7, "out_tok": 3, "cache_read": 0, "cache_make": 0,
        }));
        let (target, event) = usage_event_from_body(&body).expect("child body yields an event");
        assert_eq!(target, "ses_parent", "spend rolls up to the parent");
        match &event {
            crate::graph::UsageEvent::Turn { origin, .. } => {
                assert_eq!(*origin, crate::graph::UsageOrigin::Agent);
            }
            _ => panic!("expected a Turn event"),
        }

        let dir = std::env::temp_dir().join(format!("cimp-usage-parent-{}", uuid::Uuid::new_v4()));
        let idx = crate::graph::GraphIndex::open(&dir, ".ckg").expect("open");
        idx.record_usage_event(&target, "opencode", &event, 100)
            .unwrap();
        // The turn lives on the parent, not the child.
        assert_eq!(idx.usage_turn_series("ses_parent").unwrap().len(), 1);
        assert!(idx.usage_turn_series("ses_child").unwrap().is_empty());
        let series = idx.usage_turn_series("ses_parent").unwrap();
        assert_eq!(series[0].origin, crate::graph::UsageOrigin::Agent);
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_body_malformed_or_empty_is_ignored() {
        // Missing msg_id → no event.
        let no_msg = usage_body(serde_json::json!({
            "kind": "usage", "session_id": "s", "in_tok": 10,
        }));
        assert!(usage_event_from_body(&no_msg).is_none());
        // Empty msg_id → no event.
        let empty_msg = usage_body(serde_json::json!({
            "kind": "usage", "session_id": "s", "msg_id": "", "in_tok": 10,
        }));
        assert!(usage_event_from_body(&empty_msg).is_none());
        // All-zero token totals (degenerate/creation emit) → skipped.
        let all_zero = usage_body(serde_json::json!({
            "kind": "usage", "session_id": "s", "msg_id": "m",
            "in_tok": 0, "out_tok": 0, "cache_read": 0, "cache_make": 0,
        }));
        assert!(usage_event_from_body(&all_zero).is_none());
    }

    #[test]
    fn usage_upsert_by_msg_id_does_not_duplicate() {
        // The plugin emits the final turn twice (spike-confirmed) — same msg_id,
        // so the second overwrites the first in place rather than appending.
        let mk = |out: u64| {
            usage_body(serde_json::json!({
                "kind": "usage", "session_id": "ses", "msg_id": "dup",
                "in_tok": 50, "out_tok": out, "cache_read": 0, "cache_make": 0,
            }))
        };
        let dir = std::env::temp_dir().join(format!("cimp-usage-dup-{}", uuid::Uuid::new_v4()));
        let idx = crate::graph::GraphIndex::open(&dir, ".ckg").expect("open");
        for out in [10u64, 20u64] {
            let (target, event) = usage_event_from_body(&mk(out)).expect("event");
            idx.record_usage_event(&target, "opencode", &event, 100)
                .unwrap();
        }
        let series = idx.usage_turn_series("ses").unwrap();
        assert_eq!(series.len(), 1, "duplicate msg_id upserts, not appends");
        assert_eq!(series[0].out_tok, 20, "last emit wins");
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_event_parent_flags_child_sessions_only() {
        // V24 code-review: a tool event with no parent is the reporting session's
        // own event (recorded normally); one carrying a non-empty parent is a
        // task-tool child whose events are dropped and rolled up to the parent.
        let own = usage_body(serde_json::json!({
            "session_id": "s", "tool": "read", "args": {}
        }));
        assert_eq!(
            tool_event_parent(&own),
            None,
            "no parent field → own session"
        );
        let empty_parent = usage_body(serde_json::json!({
            "session_id": "s", "tool": "read", "args": {}, "parent_session_id": ""
        }));
        assert_eq!(
            tool_event_parent(&empty_parent),
            None,
            "empty parent → own session"
        );
        let child = usage_body(serde_json::json!({
            "session_id": "ses_child", "tool": "read", "args": {}, "parent_session_id": "ses_parent"
        }));
        assert_eq!(
            tool_event_parent(&child),
            Some("ses_parent".to_string()),
            "child → parent"
        );
    }

    #[test]
    fn discovery_round_trips() {
        let d = Discovery {
            port: 8123,
            token: "tok".into(),
            pid: 42,
        };
        let s = serde_json::to_string(&d).unwrap();
        let back: Discovery = serde_json::from_str(&s).unwrap();
        assert_eq!(back.port, 8123);
        assert_eq!(back.token, "tok");
        assert_eq!(back.pid, 42);
    }
}

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
    match std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
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

    Ok(Request { method, path, auth, body })
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
        ("POST", "/mcp/list") => handle_mcp_list(&mut stream, &service, &req).await,
        ("POST", "/mcp/call") => handle_mcp_call(&mut stream, &service, &req).await,
        ("GET", "/describe") => {
            let text = service.describe().await;
            write_simple(&mut stream, 200, "text/plain; charset=utf-8", text.as_bytes()).await
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
    let mut line = serde_json::to_vec(&r).unwrap_or_else(|_| {
        br#"{"ok":false,"error":"failed to serialize result"}"#.to_vec()
    });
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
async fn handle_graph_run(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
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
    let r = match graph.run_graph_tool(&cwd, &body.name, &body.args, consumer).await {
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
    let cwd = body.cwd.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let r = graph.retrieve_context(&cwd, &body.prompt, body.session_id.as_deref());
    // V11 Phase B: prepend the once-per-session project map. Done here (the real
    // injection path), not in `retrieve_context`, so the preview surface — which
    // also calls `retrieve_context` — never consumes the once-per-session flag.
    let mut text = r.context_md;
    if let Some(map) = graph.session_greeting(&cwd, body.session_id.as_deref()) {
        text = if text.is_empty() { map } else { format!("{map}\n\n{text}") };
    }
    // V12 Phase F: drain any auto-check block a slow post-edit run parked for
    // this session (see `GraphService::post_edit`'s budget/park path) — a
    // turn is never blocked waiting for a check, but its result still reaches
    // the model on the very next opportunity.
    if let Some(pending) = graph.drain_auto_check(body.session_id.as_deref()) {
        text = if text.is_empty() { pending } else { format!("{text}\n\n{pending}") };
    }
    let tokens_est = text.chars().count() / 4;
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
    let cwd = body.cwd.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
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
}

/// `POST /context/should_read` (V11 Phase E): the read-advisor verdict for a
/// `Read`. Returns `{ ok, verdict: "pass" }` to let the read through, or
/// `{ ok, verdict: "remind", text }` to deny-with-content. Fails open to `pass`
/// on any missing state — the advisor must never block a legitimate read.
async fn handle_should_read(stream: &mut TcpStream, app: &AppHandle, req: &Request) -> AppResult<()> {
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
    let cwd = body.cwd.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    match graph.should_read(&cwd, body.session_id.as_deref(), &body.file_path, body.offset) {
        Some(text) => {
            write_json(stream, 200, &serde_json::json!({ "ok": true, "verdict": "remind", "text": text })).await
        }
        None => write_json(stream, 200, &pass).await,
    }
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
    let cwd = body.cwd.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let text = graph
        .post_edit(&cwd, body.session_id.as_deref(), &body.file_path)
        .await
        .unwrap_or_default();
    write_json(stream, 200, &serde_json::json!({ "ok": true, "text": text })).await
}

/// A `POST /memory/event` request body (the OpenCode plugin's tool hook — the
/// only memory ingress for OpenCode, whose OOB SSE stream carries no tool
/// events). Claude records in-process via the transcript tap instead.
#[derive(Deserialize)]
struct MemoryEventBody {
    #[serde(default)]
    cwd: Option<String>,
    session_id: String,
    #[serde(default)]
    agent: Option<String>,
    tool: String,
    #[serde(default)]
    args: Value,
}

/// `POST /memory/event`: classify an agent tool call and record it as a memory
/// event. Best-effort — an unclassifiable tool or a missing graph service is a
/// silent no-op (200), never an error the plugin has to handle.
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
    if let Some((kind, arg)) = crate::graph::classify_tool(&body.tool) {
        let get = |k: &str| body.args.get(k).and_then(Value::as_str);
        let (path, detail) = match arg {
            crate::graph::MemArg::Path => (
                get("file_path").or_else(|| get("filePath")).or_else(|| get("notebook_path")).or_else(|| get("path")).unwrap_or("").to_string(),
                None,
            ),
            crate::graph::MemArg::Pattern => (get("pattern").or_else(|| get("path")).or_else(|| get("query")).unwrap_or("").to_string(), None),
            crate::graph::MemArg::Command => (String::new(), get("command").map(|c| c.chars().take(200).collect::<String>())),
        };
        // Skip an event with no usable target: an empty path (Path/Pattern) or a
        // Command whose `command` arg was absent (detail is None) — recording it
        // would just evict useful events from the ring.
        let recordable = match arg {
            crate::graph::MemArg::Command => detail.is_some(),
            _ => !path.is_empty(),
        };
        if recordable {
            let cwd = body.cwd.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
            let agent = body.agent.as_deref().unwrap_or("opencode");
            graph.record_mem_event(&cwd, &body.session_id, agent, kind, &path, None, None, detail.as_deref());
        }
    }
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
    let r = match service.mcp_call(consumer_of(req), &body.name, body.arguments).await {
        Ok(text) => RunResult { ok: true, text: Some(text), error: None },
        Err(e) => RunResult { ok: false, text: None, error: Some(e) },
    };
    write_json(stream, 200, &r).await
}

/// Parse the `?consumer=<name>` query value off a request path into a
/// [`Consumer`]. Absent / unknown ⇒ Claude (the original default).
fn consumer_of(req: &Request) -> Consumer {
    let raw = req
        .path
        .split_once('?')
        .and_then(|(_, q)| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("consumer="))
        })
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

    #[test]
    fn discovery_round_trips() {
        let d = Discovery { port: 8123, token: "tok".into(), pid: 42 };
        let s = serde_json::to_string(&d).unwrap();
        let back: Discovery = serde_json::from_str(&s).unwrap();
        assert_eq!(back.port, 8123);
        assert_eq!(back.token, "tok");
        assert_eq!(back.pid, 42);
    }
}

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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};

use super::agent::ThinkingMode;
use super::router::TierHint;
use super::service::OffloadService;

/// Discovery-file name under the portable root (next to `settings.json`).
const DISCOVERY_FILE: &str = ".ccimp-offload.json";

/// The discovery file the child reads to find + authenticate to the app.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Discovery {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

/// `<exe-dir>/.ccimp-offload.json` — the portable-root discovery path. Falls
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
    pub async fn start(service: Arc<OffloadService>) -> AppResult<Arc<Self>> {
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
                        let tok = accept_token.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = handle_conn(stream, svc, tok).await {
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
    pub fn stop(&self) {
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

/// Whether the request carries the expected bearer token.
fn authorized(req: &Request, token: &str) -> bool {
    match &req.auth {
        Some(h) => h
            .strip_prefix("Bearer ")
            .map(|t| t == token)
            .unwrap_or(false),
        None => false,
    }
}

/// Handle one connection: route by method+path after checking auth.
async fn handle_conn(
    mut stream: TcpStream,
    service: Arc<OffloadService>,
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

    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/run") => handle_run(&mut stream, &service, &req).await,
        ("GET", "/describe") => {
            let text = service.describe().await;
            write_simple(&mut stream, 200, "text/plain; charset=utf-8", text.as_bytes()).await
        }
        ("GET", "/events") => handle_events(stream, service).await,
        ("GET", "/health") => write_simple(&mut stream, 200, "text/plain", b"ok").await,
        _ => write_simple(&mut stream, 404, "text/plain", b"not found").await,
    }
}

/// `POST /run`: decode the task, run it on the warm pool, return the result.
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
    let result = service
        .run(body.instructions, body.context, thinking, tier, session_cwd)
        .await;
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
    // 200 even on a task-level error: the child renders `error` as a tool
    // result so Claude can read + adapt, exactly like the in-process path.
    write_json(stream, 200, &r).await
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

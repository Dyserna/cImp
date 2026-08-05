//! V8-03 loopback proxy channel — the authenticated localhost endpoint the
//! per-session `--offload-mcp` child forwards to when the app is running.
//!
//! A minimal hand-rolled HTTP/1.1 service on `127.0.0.1:0` (ephemeral port),
//! gated by a per-launch bearer token. It exposes exactly three routes —
//! purpose-built for offload, **not** a general local API:
//!
//! - `POST /run` — run one `offload_task` against the warm app-side pool.
//! - `GET  /describe` — the live capability description for the tool.
//! - `GET  /events` — an SSE stream the child relays to Claude: capability
//!   pulses (`event: change` → `notifications/tools/list_changed`) and, since
//!   V30 Phase B, addressed session pushes (`event: push` →
//!   `notifications/claude/channel`).
//!
//! The `{port, token, pid}` are advertised in a discovery file written next
//! to the exe (the portable root — never `~/.claude`), created when offload
//! is enabled and removed on exit. The token rotates every launch. This is
//! the one genuinely new security surface: loopback-only bind + token auth +
//! a user-readable discovery file (tightened where the OS allows). A
//! malicious *local* process that reads the file could drive offloads or
//! observe task text — the same localhost-dev-server trust assumption,
//! documented in MAINTENANCE.md.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
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
use super::service::{OffloadService, PushNotice};

/// Discovery-file name under the portable root (next to `settings.json`).
/// Legacy single-instance location, still written for anything that only
/// knows this path; the per-instance directory below is authoritative.
const DISCOVERY_FILE: &str = ".cimp-offload.json";

/// Per-instance discovery DIRECTORY under the portable root: one
/// `<pid>.json` per running instance, each carrying that instance's launch
/// `root`. The legacy single file is last-writer-wins, so with two cImp
/// instances open a child spawned by project A's agent could connect to
/// project B's app — and audits/graph queries would run against the WRONG
/// project. Readers resolve root-aware via [`read_discovery_for`].
const DISCOVERY_DIR: &str = ".cimp-discovery";

/// The discovery file the child reads to find + authenticate to the app.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Discovery {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    /// The launch project root this instance serves (canonicalized at
    /// write). `#[serde(default)]` — absent in legacy files.
    #[serde(default)]
    pub root: String,
}

/// The portable root (exe dir), falling back to the cwd if `current_exe()`
/// is unavailable (mirrors `settings::global_path`).
fn portable_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `<exe-dir>/.cimp-offload.json` — the legacy portable-root discovery path.
pub fn discovery_path() -> PathBuf {
    portable_root().join(DISCOVERY_FILE)
}

/// `<exe-dir>/.cimp-discovery/` — the per-instance discovery directory.
fn discovery_dir() -> PathBuf {
    portable_root().join(DISCOVERY_DIR)
}

/// This process's per-instance discovery file.
fn own_discovery_path() -> PathBuf {
    discovery_dir().join(format!("{}.json", std::process::id()))
}

/// Read the legacy single discovery file, if present and parseable.
pub fn read_discovery() -> Option<Discovery> {
    let text = std::fs::read_to_string(discovery_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Every parseable per-instance discovery entry (stale ones included — they
/// are swept at instance start; see [`sweep_stale_discoveries`]).
fn read_all_discoveries() -> Vec<Discovery> {
    let Ok(entries) = std::fs::read_dir(discovery_dir()) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|t| serde_json::from_str(&t).ok())
        .collect()
}

/// This instance's own discovery entry — the per-instance file first, then
/// the legacy file when it still belongs to this pid. Used by in-app writers
/// that bake port+token into generated artifacts (the OpenCode plugin) and
/// must never pick up a sibling instance's endpoint.
pub fn read_own_discovery() -> Option<Discovery> {
    if let Ok(text) = std::fs::read_to_string(own_discovery_path()) {
        if let Ok(d) = serde_json::from_str::<Discovery>(&text) {
            return Some(d);
        }
    }
    read_discovery().filter(|d| d.pid == std::process::id())
}

/// Canonicalized-or-raw form of a path for ancestry comparison. Both the
/// writer (instance root) and readers (child cwd) go through this, so the
/// `\\?\` extended prefix `std::fs::canonicalize` adds on Windows appears on
/// both sides and cancels out in the component-wise comparison.
fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Component-wise "is `root` an ancestor of (or equal to) `hint`" — case
/// insensitive on Windows, where on-disk casing and agent-reported cwds
/// routinely disagree.
fn is_ancestor_or_equal(root: &Path, hint: &Path) -> bool {
    let rc: Vec<_> = root.components().collect();
    let hc: Vec<_> = hint.components().collect();
    if rc.is_empty() || rc.len() > hc.len() {
        return false;
    }
    rc.iter().zip(hc.iter()).all(|(a, b)| {
        let (a, b) = (
            a.as_os_str().to_string_lossy(),
            b.as_os_str().to_string_lossy(),
        );
        if cfg!(windows) {
            a.eq_ignore_ascii_case(&b)
        } else {
            a == b
        }
    })
}

/// Pick the instance serving `hint` from the per-instance entries: the
/// DEEPEST root that is an ancestor of the hint wins (nested checkouts
/// resolve to the closest instance; same-root duplicates tie-break on pid —
/// arbitrary but deterministic). With no hint or no match: a sole surviving
/// entry is unambiguous, else fall back to the legacy last-writer-wins file.
/// Pure — unit-tested directly.
fn select_discovery(mut entries: Vec<Discovery>, hint: Option<&Path>) -> Option<Discovery> {
    if let Some(h) = hint {
        let mut best: Option<(usize, &Discovery)> = None;
        for d in &entries {
            if d.root.is_empty() {
                continue;
            }
            let root = PathBuf::from(&d.root);
            if is_ancestor_or_equal(&root, h) {
                let depth = root.components().count();
                let better = match &best {
                    None => true,
                    Some((bd, bde)) => depth > *bd || (depth == *bd && d.pid > bde.pid),
                };
                if better {
                    best = Some((depth, d));
                }
            }
        }
        if let Some((_, d)) = best {
            return Some(d.clone());
        }
    }
    if entries.len() == 1 {
        return entries.pop();
    }
    read_discovery()
}

/// Root-aware discovery: resolve the instance serving `hint` (a child's cwd
/// or a hook payload's cwd). `None` hint degrades to sole-entry / legacy.
pub fn read_discovery_for(hint: Option<&Path>) -> Option<Discovery> {
    let hint = hint.map(canon);
    select_discovery(read_all_discoveries(), hint.as_deref())
}

/// Base URL + bearer token of the loopback endpoint of the instance serving
/// `hint` — the one endpoint resolver every stdio MCP child uses
/// (`offload/mcp.rs`, `audit/mcp.rs`). `None` ⇒ no instance is running.
pub fn proxy_base_for(hint: Option<&Path>) -> Option<(String, String)> {
    let d = read_discovery_for(hint)?;
    Some((format!("http://127.0.0.1:{}", d.port), d.token))
}

/// Delete per-instance entries whose endpoint no longer answers — hard-killed
/// instances leave their `<pid>.json` behind (removal is graceful-exit only).
/// Run once per instance start; a 200ms connect probe per entry bounds the
/// cost. Entries for OUR pid are removed unconditionally (a previous run's
/// leftover under a reused pid — ours gets rewritten right after).
async fn sweep_stale_discoveries(own_pid: u32) {
    for d in read_all_discoveries() {
        let stale = if d.pid == own_pid {
            true
        } else {
            !tokio::time::timeout(
                Duration::from_millis(200),
                tokio::net::TcpStream::connect(("127.0.0.1", d.port)),
            )
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
        };
        if stale {
            let path = discovery_dir().join(format!("{}.json", d.pid));
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    debug!(error = %e, pid = d.pid, "offload loopback: stale discovery cleanup failed");
                }
            } else if d.pid != own_pid {
                info!(
                    pid = d.pid,
                    port = d.port,
                    "offload loopback: swept stale discovery entry"
                );
            }
        }
    }
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
    /// This instance's `<pid>.json` under the per-instance directory.
    own_file: PathBuf,
}

impl Loopback {
    /// Bind the endpoint, write the discovery files (the per-instance
    /// `<pid>.json` plus the legacy single file), sweep stale sibling
    /// entries, and spawn the accept loop. Returns the handle (managed in
    /// `AppState`). Idempotent at the file level — existing (stale) files
    /// are overwritten. `root` is the launch project root this instance
    /// serves; children match their cwd against it (`read_discovery_for`).
    pub async fn start(
        service: Arc<OffloadService>,
        app: AppHandle,
        root: &Path,
    ) -> AppResult<Arc<Self>> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| AppError::Offload(format!("loopback bind failed: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| AppError::Offload(format!("loopback addr failed: {e}")))?
            .port();
        let token = make_token();
        let discovery = discovery_path();
        let own_file = own_discovery_path();

        sweep_stale_discoveries(std::process::id()).await;

        let disc = Discovery {
            port,
            token: token.clone(),
            pid: std::process::id(),
            root: canon(root).to_string_lossy().to_string(),
        };
        if let Some(parent) = own_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        write_discovery(&own_file, &disc)?;
        // Keep the legacy single file in step (last-writer-wins, as before):
        // it is the no-hint / no-per-instance-match fallback.
        write_discovery(&discovery, &disc)?;
        info!(port, root = %disc.root, "offload loopback: listening on 127.0.0.1");

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
            own_file,
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
        // The per-instance file is ours by construction (pid-named).
        if let Err(e) = std::fs::remove_file(&self.own_file) {
            if e.kind() != std::io::ErrorKind::NotFound {
                debug!(error = %e, "offload loopback: own discovery cleanup failed");
            }
        }
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

/// Parse one NDJSON line of a loopback-streamed run body (`/run`,
/// `/audit/run`) from the child's side — kept beside [`RunResult`] so the
/// encoder and the decoder of the wire shape live in one file. The final
/// result line is the ONLY one carrying an `ok` boolean; every other line —
/// heartbeats of any shape, blanks, unparseable bytes — yields `None`, so the
/// heartbeat wire format is not load-bearing. `fallback_error` fills in when
/// an `ok:false` line carries no `error` text.
pub fn parse_result_line(raw: &[u8], fallback_error: &str) -> Option<Result<String, String>> {
    let line = std::str::from_utf8(raw).ok()?.trim();
    if line.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let ok = v.get("ok").and_then(|b| b.as_bool())?;
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
            .unwrap_or(fallback_error)
            .to_string()))
    }
}

/// The stream head shared by the NDJSON-streaming routes (`/run`,
/// `/audit/run`): no `Content-Length` — the body is close-delimited — sent up
/// front so the child's `send()` resolves immediately and it knows the app is
/// alive before the (possibly long) task even starts.
const NDJSON_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\n\
    Content-Type: application/x-ndjson\r\n\
    Cache-Control: no-cache\r\n\
    Connection: close\r\n\r\n";

/// One heartbeat line of an NDJSON-streamed run body. Carries no `ok` field,
/// so [`parse_result_line`] skips it; the exact shape is not load-bearing.
const HEARTBEAT_LINE: &[u8] = b"{\"hb\":true}\n";

/// Write the shared [`NDJSON_HEAD`] stream head.
async fn write_ndjson_head<W>(wr: &mut W, label: &str) -> AppResult<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    wr.write_all(NDJSON_HEAD)
        .await
        .map_err(|e| AppError::Offload(format!("{label} head: {e}")))?;
    wr.flush().await.ok();
    Ok(())
}

/// Serialize + write the final [`RunResult`] line: one JSON object (serde
/// emits no embedded newlines) + `\n`, then the caller lets the connection
/// close. This is the single `ok`-bearing line [`parse_result_line`] keys off.
async fn write_result_line<W>(wr: &mut W, r: &RunResult, label: &str) -> AppResult<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut line = serde_json::to_vec(r)
        .unwrap_or_else(|_| br#"{"ok":false,"error":"failed to serialize result"}"#.to_vec());
    line.push(b'\n');
    wr.write_all(&line)
        .await
        .map_err(|e| AppError::Offload(format!("{label} result: {e}")))?;
    wr.flush().await.ok();
    Ok(())
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
        ("POST", "/audit/run") => handle_audit_run(&mut stream, &app, &req).await,
        ("POST", "/context/retrieve") => handle_context_retrieve(&mut stream, &app, &req).await,
        ("POST", "/context/compaction") => handle_context_compaction(&mut stream, &app, &req).await,
        ("POST", "/context/should_read") => handle_should_read(&mut stream, &app, &req).await,
        ("POST", "/context/post_edit") => handle_post_edit(&mut stream, &app, &req).await,
        ("POST", "/memory/event") => handle_memory_event(&mut stream, &app, &req).await,
        ("POST", "/activity/contract_drift") => handle_contract_drift(&mut stream, &req).await,
        ("POST", "/permission/event") => handle_permission_event(&mut stream, &app, &req).await,
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
        ("GET", "/events") => handle_events(stream, service, &req).await,
        ("GET", "/health") => write_simple(&mut stream, 200, "text/plain", b"ok").await,
        // V30 Phase B test rig — remove with the Phase 0 spike (#15). The route
        // does not exist at all unless `CIMP_CHANNEL_SPIKE` is armed.
        ("POST", "/push_test") if channel_spike_armed() => {
            handle_push_test(&mut stream, &service, &req).await
        }
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
    write_ndjson_head(&mut wr, "run").await?;

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
                if wr.write_all(HEARTBEAT_LINE).await.is_err() {
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
        // `ok:false` is a task-level error the child renders as a tool result
        // so Claude can read + adapt.
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(e.to_string()),
        },
    };
    write_result_line(&mut wr, &r, "run").await
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
    /// V28 (issue #13): the cImp TAB id the calling MCP child was spawned for
    /// (`cimp --offload-mcp --tab <id>`), used to resolve *which* session of
    /// this agent the `context_*` memory tools should scope to. Optional by
    /// design — a child spawned before the upgrade sends no `tab`, and an
    /// unknown/stale one resolves to `None`; both fall back to the pre-V28
    /// most-recent-session behavior rather than erroring the call.
    #[serde(default)]
    tab: Option<String>,
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
    // V28: resolve the calling TAB to the session it currently reports, so the
    // `context_*` memory tools scope to this tab's own session instead of "the
    // most recent session for this agent" (two same-agent tabs used to share —
    // and steal — one memory scope). Fail-open: no `tab`, an unknown key, a
    // different agent's entry, or a TTL-stale one all yield `None`, which is
    // exactly the pre-V28 behavior.
    let session = body
        .tab
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .and_then(|tab| {
            graph.live_session_for_tab(tab, crate::graph::source_for_consumer(consumer))
        });
    let r = match graph
        .run_graph_tool(&cwd, &body.name, &body.args, consumer, session.as_deref())
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

/// A `POST /audit/run` request body (V26 code-audit MCP surface).
///
/// Deliberately tiny: `category` reuses
/// [`Category`](crate::audit::adapters::Category)'s own lowercase serde (so
/// `"security"` / `"quality"` deserialize directly — a bad word is a clean parse
/// error → 400). `consumer` names the agent that triggered the scan (`claude` /
/// `opencode`, from the child's `--consumer` flag; absent on a legacy child ⇒
/// `claude`) and selects which `expose_*` toggle the route re-enforces at run
/// time — see [`handle_audit_run`]. No `cwd`: an audit always runs against the
/// app's own launch project root, never the caller's directory.
#[derive(Deserialize)]
struct AuditRunBody {
    category: crate::audit::adapters::Category,
    #[serde(default)]
    consumer: Option<String>,
    /// The child's working directory (the agent's project), sent for
    /// verification only — the scan always runs against this app's own
    /// launch root. `#[serde(default)]` keeps older children compatible.
    #[serde(default)]
    cwd: Option<String>,
}

/// `POST /audit/run` (V26): run one full code-audit scan of the requested
/// category to completion and **stream** the reply as newline-delimited JSON —
/// periodic [`HEARTBEAT_LINE`]s while the (possibly minutes-long) scan runs,
/// then exactly one final `RunResult { ok, text, error }` line. This is the
/// app-side half of the `cimp-code-audit` MCP server: the stdio child
/// (`audit/mcp.rs::run_via_loopback`) POSTs here and forwards the result to
/// Claude / OpenCode.
///
/// **Per-consumer re-gate:** the `expose_claude` / `expose_opencode` toggles
/// gate MCP-server *advertisement* at tab spawn, but a child spawned while its
/// consumer was opted in outlives the toggle — so this route re-enforces the
/// toggle named by `body.consumer` on every run. Unchecking "Expose to …" thus
/// takes effect immediately for already-running tabs (they get a clean tool
/// error), no restart needed for the *enforcement* half. The master `enabled`
/// switch is separately re-enforced by `begin_scan`.
///
/// **Why a stream, framed exactly like `handle_run`:** the child aborts after
/// 45 s of silence, and a real audit can outlast that, so the heartbeats (every
/// [`HEARTBEAT_INTERVAL`]) prove the scan is still alive — the child skips any
/// line lacking an `ok` field and keeps only the single `ok`-bearing final line
/// (see [`parse_result_line`]). The response carries no `Content-Length`
/// (`Connection: close`, close-delimited); each JSON is emitted on its own line
/// so the child's line reader always sees complete frames.
///
/// **Why no caller-disconnect probe (unlike `handle_run`):** the audit entry
/// [`run_audit`](crate::audit::mcp::run_audit) is not cancellable, and
/// `run_scan_and_wait` clears the runner's `scanning` flag only when the scan's
/// `run()` future completes. Dropping that future to react to a disconnect would
/// wedge the runner in `scanning`, so instead a failed heartbeat write (the
/// caller-gone signal) drains the scan to completion off the wire and discards
/// the unsendable result — the runner ends clean either way. There is also no
/// llama-server slot to free promptly, which is the only reason `handle_run`
/// probes at all.
///
/// Tool-level failures (master switch off, no tools enabled, `"a scan is already
/// in progress"`) flow through as the final `{ok:false, error}` line over HTTP
/// 200 — the child renders them as a readable tool error, mirroring
/// [`handle_graph_run`]. Only a malformed body is a 400.
async fn handle_audit_run(stream: &mut TcpStream, app: &AppHandle, req: &Request) -> AppResult<()> {
    let body: AuditRunBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            // Malformed body / unknown category → 400 (the child treats any
            // non-200 as a hard failure), mirroring `handle_graph_run`.
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!("bad request body: {e}")),
            };
            return write_json(stream, 400, &r).await;
        }
    };
    let category = body.category;
    let consumer = body
        .consumer
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("claude")
        .to_ascii_lowercase();

    // Resolve the runner from managed state at request time (robust to the
    // audit-vs-loopback startup order). `main.rs` manages it as `Arc<AuditState>`
    // (and publishes the same handle via `audit::set_global`). Not ready → a
    // single `ok:false` line over 200, same shape as `handle_graph_run`.
    let state = match app.try_state::<Arc<crate::audit::AuditState>>() {
        Some(s) => s.inner().clone(),
        None => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some("audit service not ready".into()),
            };
            return write_json(stream, 200, &r).await;
        }
    };

    // Re-enforce this consumer's expose toggle at run time (see the doc
    // comment above): a still-registered child whose consumer has since been
    // opted out gets a clean tool error, not a scan.
    if !state.consumer_exposed(&consumer) {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(format!(
                "code audit is not exposed to {consumer} — re-enable it in cImp Settings → Code Audit"
            )),
        };
        return write_json(stream, 200, &r).await;
    }

    // Wrong-instance guard: the scan always runs against THIS app's launch
    // root, so a child whose cwd falls outside it was misrouted (stale or
    // foreign discovery entry — possible with several cImp instances off one
    // install). A clean error beats silently auditing the wrong project.
    if let Some(child_cwd) = body.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let served_root = canon(&state.root());
        if !is_ancestor_or_equal(&served_root, &canon(Path::new(child_cwd))) {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!(
                    "this cImp instance serves {} — launch cImp in {} (or close the other instance) to audit it",
                    served_root.display(),
                    child_cwd
                )),
            };
            return write_json(stream, 200, &r).await;
        }
    }

    write_ndjson_head(stream, "audit").await?;

    // Run the scan concurrently with the heartbeat interval: whichever branch
    // fires, `run_audit` still owns clearing `scanning`.
    let run_fut = crate::audit::mcp::run_audit(&state, category, &consumer);
    tokio::pin!(run_fut);

    let mut beat = tokio::time::interval(HEARTBEAT_INTERVAL);
    beat.tick().await; // consume the immediate first tick

    let result = loop {
        tokio::select! {
            biased;
            r = &mut run_fut => break r,
            _ = beat.tick() => {
                // A failed heartbeat write means the caller went away. Stop
                // beating, but drain the (uncancellable) scan to completion so
                // the runner leaves `scanning` — then drop the unsendable result.
                if stream.write_all(HEARTBEAT_LINE).await.is_err() {
                    debug!("audit loopback: heartbeat write failed; caller gone, draining scan");
                    let _ = (&mut run_fut).await;
                    return Ok(());
                }
                stream.flush().await.ok();
            }
        }
    };

    let r = match result {
        Ok(text) => RunResult {
            ok: true,
            text: Some(text),
            error: None,
        },
        // Busy / disabled / no-tools errors intentionally arrive here as
        // `ok:false` — a tool result the child surfaces, not a protocol failure.
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(e),
        },
    };
    write_result_line(stream, &r, "audit").await
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

// ── NC-2 (issue #5): hook-driven permission detection ────────────────────────

/// A `POST /permission/event` request body — the Claude `--notify-hook` shim
/// forwarding a `Notification` or `PermissionDenied` hook payload.
#[derive(Deserialize, Default)]
struct PermissionEventBody {
    /// The hook payload's `cwd` (already resolved by the shim).
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    /// `~/.claude/projects/<slug>/<session_id>.jsonl` — the second mapping key.
    #[serde(default)]
    transcript_path: Option<String>,
    /// The payload's `hook_event_name` (`"Notification"` / `"PermissionDenied"`).
    #[serde(default)]
    event: String,
    /// The notification's type when the payload carries one (`permission_prompt`,
    /// `idle_prompt`, …).
    #[serde(default)]
    notification_type: Option<String>,
    /// The notification's prose, used to classify when no type field arrived.
    #[serde(default)]
    message: Option<String>,
    /// Present on `PermissionDenied`; logged, not branched on.
    #[serde(default)]
    tool_name: Option<String>,
}

/// Which edge of the existing `awaiting_permission` flag a hook payload maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionEdge {
    /// A permission prompt is now on screen ⇒ `PermissionPromptDetected`.
    Detected,
    /// The pending call was denied ⇒ `PermissionPromptResolved`.
    Resolved,
}

/// Substrings that identify a permission notification when the payload carries
/// no notification-type field. Claude Code's permission notification reads
/// "Claude needs your permission to use <tool>", so both fragments are checked
/// (either wording survives a small rephrasing). Deliberately narrower than a
/// bare `"permission"` test, which a "permission denied" filesystem-error
/// notification would trip.
const PERMISSION_MESSAGE_MARKERS: [&str; 2] = ["your permission", "permission to use"];

/// The notification type that means "a permission prompt is on screen" — the
/// value Claude Code's `Notification` matcher filters on.
const PERMISSION_NOTIFICATION_TYPE: &str = "permission_prompt";

/// NC-2: map a hook payload to a permission edge, or `None` to ignore it.
///
///   * `PermissionDenied` (auto-classifier blocked the call) resolves the
///     prompt. Note the docs describe this as the auto-mode classifier's own
///     denial, NOT necessarily the user pressing "No" — treating it as a
///     resolution is still right (nothing is awaiting the user afterwards), and
///     an over-eager clear is harmless: the regex fallback re-detects a prompt
///     that is genuinely still on screen.
///   * `Notification` is classified by its TYPE when the payload carries one
///     (the field the matcher filters on), else by its prose. Every other
///     notification type — `idle_prompt`, `auth_success`, `elicitation_*`,
///     `agent_*` — is IGNORED.
///
/// **Idle notifications are deliberately dropped.** `idle_prompt` ("waiting for
/// your input") is semantically close to the `awaiting_question` pipe, but that
/// pipe's meaning today is "an AskUserQuestion-style menu is on screen" and the
/// regex detector owns it; wiring idle there would flip the badge/TTS on every
/// turn boundary. Revisit only with a separate signal.
fn classify_permission_event(
    event: &str,
    notification_type: &str,
    message: &str,
) -> Option<PermissionEdge> {
    match event {
        "PermissionDenied" => Some(PermissionEdge::Resolved),
        "Notification" => {
            let kind = notification_type.trim();
            if !kind.is_empty() {
                return (kind.eq_ignore_ascii_case(PERMISSION_NOTIFICATION_TYPE))
                    .then_some(PermissionEdge::Detected);
            }
            let msg = message.to_ascii_lowercase();
            PERMISSION_MESSAGE_MARKERS
                .iter()
                .any(|m| msg.contains(m))
                .then_some(PermissionEdge::Detected)
        }
        _ => None,
    }
}

/// One tab a permission event could belong to: its id, the Claude session id it
/// is currently running (from the graph's live-session registry — `None` for a
/// configured-but-not-running tab), and the directory it launches in.
#[derive(Debug, Clone)]
struct PermissionTabCandidate {
    tab: String,
    session_id: Option<String>,
    cwd: PathBuf,
}

/// NC-2: resolve a hook payload to exactly one tab, or `None` to DROP the event.
///
/// Fallback order — `session_id` → `transcript_path` → unique `cwd`:
///
///   1. **session id.** The live-session registry maps a Claude tab id to the
///      session its transcript tail last saw; a hook payload names that same id.
///   2. **transcript path.** The transcript filename stem IS the session id
///      (`<slug>/<session_id>.jsonl`), so this recovers the match when the
///      `session_id` field itself goes missing/renamed.
///   3. **cwd**, and only when it identifies exactly ONE Claude tab. Tabs
///      normally all inherit the app's launch dir, so this usually resolves only
///      for a single-Claude-tab setup (or a worktree tab with its own cwd).
///
/// Never guesses: an ambiguous or unmatched payload returns `None` and the event
/// is dropped, leaving detection to the TUI-regex fallback. Guessing would flip
/// the badge/TTS/avatar for the WRONG tab, which is worse than a missed hook.
fn resolve_permission_tab(
    candidates: &[PermissionTabCandidate],
    session_id: &str,
    transcript_path: &str,
    cwd: &str,
) -> Option<String> {
    let by_session = |sid: &str| -> Option<String> {
        if sid.is_empty() {
            return None;
        }
        let mut hits = candidates
            .iter()
            .filter(|c| c.session_id.as_deref() == Some(sid));
        let first = hits.next()?;
        // A session id belongs to one tab; if two tabs somehow claim it, refuse
        // rather than pick.
        hits.next().is_none().then(|| first.tab.clone())
    };

    if let Some(tab) = by_session(session_id) {
        return Some(tab);
    }
    if let Some(tab) = transcript_session_id(transcript_path).and_then(|s| by_session(&s)) {
        return Some(tab);
    }
    let target = norm_dir(cwd)?;
    let mut hits = candidates
        .iter()
        .filter(|c| norm_dir(&c.cwd.to_string_lossy()).as_deref() == Some(target.as_str()));
    let first = hits.next()?;
    hits.next().is_none().then(|| first.tab.clone())
}

/// The session id encoded in a Claude transcript path
/// (`…/projects/<slug>/<session_id>.jsonl`), or `None` for an empty/odd path.
fn transcript_session_id(transcript_path: &str) -> Option<String> {
    let stem = Path::new(transcript_path.trim()).file_stem()?;
    let stem = stem.to_string_lossy().into_owned();
    (!stem.is_empty()).then_some(stem)
}

/// A directory string normalized for comparison: separators unified, trailing
/// separators dropped, and — on Windows, whose paths are case-insensitive —
/// case-folded. `None` for an empty/whitespace path.
fn norm_dir(dir: &str) -> Option<String> {
    let s = dir.trim().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if s.is_empty() {
        return None;
    }
    Some(if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s.to_string()
    })
}

/// `POST /permission/event` (NC-2): the hook-driven half of permission
/// detection. Maps the payload to a tab and emits the SAME `StateSignal`s the
/// TUI-regex detector emits, so the whole downstream pipeline
/// (`awaiting_permission` → TTS enqueue, per-tab badge, avatar) is untouched.
/// Both producers are idempotent at the state manager, so a hook and a regex
/// match for the same prompt collapse to one edge.
///
/// Always answers 200 `{ok:true}` (with a `mapped` field for diagnosis): the
/// shim ignores the response and must never be given a reason to retry.
async fn handle_permission_event(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: PermissionEventBody = match serde_json::from_slice(&req.body) {
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
    let session_id = body.session_id.unwrap_or_default();
    let transcript_path = body.transcript_path.unwrap_or_default();
    let cwd = body.cwd.unwrap_or_default();
    let Some(edge) = classify_permission_event(
        &body.event,
        body.notification_type.as_deref().unwrap_or(""),
        body.message.as_deref().unwrap_or(""),
    ) else {
        debug!(
            event = %body.event,
            kind = body.notification_type.as_deref().unwrap_or(""),
            "permission hook: ignored (not a permission edge)"
        );
        return write_json(
            stream,
            200,
            &serde_json::json!({ "ok": true, "mapped": false, "reason": "ignored" }),
        )
        .await;
    };

    // Snapshot everything we need from managed state, then drop the guards —
    // nothing borrowed from `AppHandle` is held across the response write.
    let resolved = app.try_state::<crate::ipc::AppState>().map(|state| {
        let sessions: Vec<(String, String)> = app
            .try_state::<Arc<crate::graph::GraphService>>()
            .map(|g| g.live_claude_sessions())
            .unwrap_or_default();
        let candidates: Vec<PermissionTabCandidate> =
            crate::tabs::claude_tab_dirs(&state.settings.current(), &state.launch.cwd)
                .into_iter()
                .map(|(tab, dir)| PermissionTabCandidate {
                    session_id: sessions
                        .iter()
                        .find(|(k, _)| *k == tab)
                        .map(|(_, s)| s.clone()),
                    tab,
                    cwd: dir,
                })
                .collect();
        (
            resolve_permission_tab(&candidates, &session_id, &transcript_path, &cwd),
            state.state_signals.clone(),
        )
    });
    let Some((Some(tab), signals)) = resolved else {
        debug!(
            event = %body.event,
            session = %session_id,
            cwd = %cwd,
            "permission hook: no unambiguous tab — dropped (regex fallback still covers it)"
        );
        return write_json(
            stream,
            200,
            &serde_json::json!({ "ok": true, "mapped": false, "reason": "no tab" }),
        )
        .await;
    };

    let tab_id = crate::state::TabId::from_str(&tab);
    let signal = match edge {
        PermissionEdge::Detected => {
            crate::state::StateSignal::PermissionPromptDetected { tab: tab_id }
        }
        PermissionEdge::Resolved => {
            crate::state::StateSignal::PermissionPromptResolved { tab: tab_id }
        }
    };
    // Edge-triggered and best-effort, exactly like the PTY processor's
    // `try_send`: a full channel means the state manager is saturated, and the
    // regex detector's next scan re-raises the edge anyway.
    let _ = signals.try_send(signal);
    info!(
        event = %body.event,
        tool = body.tool_name.as_deref().unwrap_or(""),
        ?edge,
        %tab,
        "permission hook: state signal sent"
    );
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "mapped": true, "tab": tab }),
    )
    .await
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

/// Read one `?key=value` query parameter off a request path.
///
/// Deliberately no percent-decoding: every value on these routes is composed by
/// cImp itself (consumer names, tab ids — `[a-z0-9-]`), never by a user or a
/// browser. Matches the pre-V30 behaviour of [`consumer_of`], which this now
/// backs.
fn query_param<'a>(path: &'a str, key: &str) -> Option<&'a str> {
    let (_, query) = path.split_once('?')?;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Parse the `?consumer=<name>` query value off a request path into a
/// [`Consumer`]. Absent / unknown ⇒ Claude (the original default).
fn consumer_of(req: &Request) -> Consumer {
    Consumer::parse(query_param(&req.path, "consumer").unwrap_or("claude"))
}

/// Render one `event: push` SSE frame from a [`PushNotice`].
///
/// The frame grammar is the SSE minimum the child's parser understands:
/// `event: push\ndata: <one-line JSON>\n\n`. `serde_json` escapes every control
/// character, so the payload is *guaranteed* to be a single line however
/// multi-line the pushed content is — the one-line invariant the wire format
/// depends on is enforced by the encoder, not by the caller. Pure, so the shape
/// is unit-testable without a socket — including from the child's side of the
/// wire (`mcp::tests`), which pins encoder and decoder against each other.
pub(super) fn push_frame(notice: &PushNotice) -> Vec<u8> {
    let data =
        serde_json::to_string(notice).unwrap_or_else(|_| r#"{"content":"","meta":{}}"#.to_string());
    format!("event: push\ndata: {data}\n\n").into_bytes()
}

/// `GET /events`: an SSE stream carrying two event types to one per-tab
/// `--offload-mcp` child —
///
/// - `event: change` — the pre-V30 capability pulse, sent to EVERY subscriber
///   (semantics unchanged; the child relays it as `tools/list_changed`);
/// - `event: push` — V30 Phase B, sent only to subscribers a push is addressed
///   to, carrying the semantic [`PushNotice`] payload the child wraps into
///   `notifications/claude/channel`.
///
/// Periodic keep-alive comments (every 20 s) keep idle intermediaries — and the
/// child's own 60 s read-idle watchdog — from dropping the connection.
///
/// The subscriber's identity comes from the child's query params
/// (`?tab=&consumer=&channels=`); `channels=1` means the child ACTUALLY declared
/// the capability on its handshake, not that the setting is on. Registration
/// happens after auth (the caller's job) and is undone by
/// [`PushGuard`](super::service::PushGuard)'s `Drop` when this loop exits — for
/// any reason at all.
async fn handle_events(
    mut stream: TcpStream,
    service: Arc<OffloadService>,
    req: &Request,
) -> AppResult<()> {
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

    // V30 Phase B: register this child in the instance's push registry. The
    // guard is bound for the whole loop — dropping it (on ANY exit below, or on
    // task cancellation) is the sole deregistration path.
    let tab = query_param(&req.path, "tab")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let consumer = query_param(&req.path, "consumer")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("claude")
        .to_string();
    // Anything but an explicit affirmative means "no channels" — a pre-V30
    // child sends no `channels` param at all and must never be pushed to.
    let channels = matches!(query_param(&req.path, "channels"), Some("1") | Some("true"));
    debug!(
        tab = ?tab,
        consumer = %consumer,
        channels,
        "offload loopback: /events subscriber connected"
    );
    let (_push_guard, mut push_rx) = service.register_push_subscriber(tab, consumer, channels);

    let mut rx = service.subscribe_changes();
    loop {
        let tick = tokio::time::sleep(Duration::from_secs(20));
        tokio::select! {
            // V30 Phase B: an addressed push for THIS child.
            notice = push_rx.recv() => {
                match notice {
                    Some(n) => {
                        if stream.write_all(&push_frame(&n)).await.is_err() {
                            break;
                        }
                        let _ = stream.flush().await;
                    }
                    // Unreachable while `_push_guard` lives (it owns the only
                    // path that removes our sender), but a closed queue can
                    // never yield another notice — stop selecting on it.
                    None => break,
                }
            }
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

// ── V30 Phase B test rig — remove with the Phase 0 spike (#15) ───────────────
//
// Lets the push bus be driven end-to-end with `curl` before Phase C has any
// real producers: with the app running and a Claude tab open,
//
//   curl -s -H "Authorization: Bearer <token>" -H 'Content-Type: application/json' \
//        -d '{"tab":"claude","content":"hello","meta":{"kind":"manual"}}' \
//        http://127.0.0.1:<port>/push_test
//
// should surface a `<channel source="cimp-offload" kind="manual">hello</channel>`
// in that tab. Inert (and 404) unless `CIMP_CHANNEL_SPIKE` is armed — the same
// env gate the child-side spike rig uses, so one variable arms both halves.

/// Whether the V30 channel spike rig is armed for this process.
fn channel_spike_armed() -> bool {
    std::env::var("CIMP_CHANNEL_SPIKE")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// A `POST /push_test` request body.
#[derive(Deserialize)]
struct PushTestBody {
    /// Target tab; absent ⇒ broadcast to every channel-capable child.
    #[serde(default)]
    tab: Option<String>,
    content: String,
    #[serde(default)]
    meta: BTreeMap<String, String>,
}

/// `POST /push_test`: drive [`OffloadService::push_to_tab`] /
/// [`OffloadService::push_broadcast`] by hand and report the delivery count.
async fn handle_push_test(
    stream: &mut TcpStream,
    service: &Arc<OffloadService>,
    req: &Request,
) -> AppResult<()> {
    let body: PushTestBody = match serde_json::from_slice(&req.body) {
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
    if body.content.trim().is_empty() {
        return write_json(
            stream,
            400,
            &serde_json::json!({ "ok": false, "error": "`content` must be non-empty" }),
        )
        .await;
    }
    let notice = PushNotice::new(body.content, body.meta);
    let (delivered, target) = match body.tab.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(tab) => (
            usize::from(service.push_to_tab(tab, notice)),
            tab.to_string(),
        ),
        None => (service.push_broadcast(notice), "*".to_string()),
    };
    info!(
        target = %target,
        delivered,
        subscribers = service.push_subscriber_count(),
        "offload loopback: /push_test (V30 spike rig)"
    );
    write_json(
        stream,
        200,
        &serde_json::json!({
            "delivered": delivered,
            "target": target,
            "subscribers": service.push_subscriber_count(),
        }),
    )
    .await
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

    // ── V30 Phase B: /events subscriber identity + the push frame ─────────

    #[test]
    fn query_param_reads_the_events_identity() {
        let path = "/events?tab=claude-2&consumer=claude&channels=1";
        assert_eq!(query_param(path, "tab"), Some("claude-2"));
        assert_eq!(query_param(path, "consumer"), Some("claude"));
        assert_eq!(query_param(path, "channels"), Some("1"));
        assert_eq!(query_param(path, "nope"), None);
        // A prefix must not match a different key, and a bare path has none.
        assert_eq!(query_param("/events?consumer=opencode", "consume"), None);
        assert_eq!(query_param("/events", "tab"), None);
        // The pre-V30 child sends no query at all: it must still parse as the
        // default consumer, with no tab and no channels.
        let legacy = Request {
            method: "GET".into(),
            path: "/events".into(),
            auth: None,
            body: Vec::new(),
        };
        assert_eq!(consumer_of(&legacy), Consumer::Claude);
        assert!(!matches!(
            query_param(&legacy.path, "channels"),
            Some("1") | Some("true")
        ));
    }

    /// The wire contract the child's SSE parser depends on: one `event:` line,
    /// one single-line `data:` line, blank-line terminated — even when the
    /// pushed content itself contains newlines (serde escapes them).
    #[test]
    fn push_frame_is_a_single_line_sse_data_payload() {
        let notice = PushNotice::new(
            "line one\nline two\r\nline three",
            [("kind", "audit_done"), ("seq", "3")],
        );
        let frame = String::from_utf8(push_frame(&notice)).unwrap();
        let lines: Vec<&str> = frame.split('\n').collect();
        assert_eq!(lines[0], "event: push");
        assert!(lines[1].starts_with("data: "));
        // event / data / "" / "" — exactly one data line, blank-line terminated.
        assert_eq!(lines.len(), 4, "frame was: {frame:?}");
        assert_eq!(lines[2], "");
        assert_eq!(lines[3], "");
        let round: PushNotice = serde_json::from_str(&lines[1]["data: ".len()..]).unwrap();
        assert_eq!(round, notice);
    }

    #[test]
    fn token_is_long_and_random() {
        let a = make_token();
        let b = make_token();
        assert_ne!(a, b);
        assert!(a.len() >= 32);
    }

    // ── NC-2: permission-hook classification + tab mapping ─────────────────

    #[test]
    fn permission_denied_resolves_and_unknown_events_are_ignored() {
        assert_eq!(
            classify_permission_event("PermissionDenied", "", ""),
            Some(PermissionEdge::Resolved)
        );
        // Events we never registered (and the PermissionRequest we chose NOT to
        // adopt) must not move the flag even if one somehow reaches the route.
        for event in ["PermissionRequest", "PreToolUse", "", "Stop"] {
            assert_eq!(
                classify_permission_event(event, "permission_prompt", ""),
                None
            );
        }
    }

    #[test]
    fn notification_type_drives_classification_when_present() {
        assert_eq!(
            classify_permission_event("Notification", "permission_prompt", ""),
            Some(PermissionEdge::Detected)
        );
        // Case/whitespace tolerance — the value is echoed from the payload.
        assert_eq!(
            classify_permission_event("Notification", " Permission_Prompt ", ""),
            Some(PermissionEdge::Detected)
        );
        // Every other documented type is ignored, including idle (which is NOT
        // wired to the question pipe — see the classifier's doc comment) …
        for kind in [
            "idle_prompt",
            "auth_success",
            "elicitation_dialog",
            "elicitation_complete",
            "elicitation_response",
            "agent_needs_input",
            "agent_completed",
            "something_new",
        ] {
            assert_eq!(classify_permission_event("Notification", kind, ""), None);
        }
        // … and a present type WINS over the prose fallback, so a future
        // permission-flavoured message under a non-permission type can't leak in.
        assert_eq!(
            classify_permission_event(
                "Notification",
                "idle_prompt",
                "Claude needs your permission to use Bash"
            ),
            None
        );
    }

    #[test]
    fn notification_message_classifies_when_type_field_is_absent() {
        assert_eq!(
            classify_permission_event(
                "Notification",
                "",
                "Claude needs your permission to use Bash"
            ),
            Some(PermissionEdge::Detected)
        );
        assert_eq!(
            classify_permission_event("Notification", "", "Permission to use Edit is required"),
            Some(PermissionEdge::Detected)
        );
        // Idle prose, and a "permission denied" error that must NOT be read as
        // a prompt (why the marker is narrower than a bare "permission").
        for msg in [
            "Claude is waiting for your input",
            "Error: permission denied while reading /etc/shadow",
            "",
        ] {
            assert_eq!(
                classify_permission_event("Notification", "", msg),
                None,
                "{msg}"
            );
        }
    }

    fn cand(tab: &str, session: Option<&str>, cwd: &str) -> PermissionTabCandidate {
        PermissionTabCandidate {
            tab: tab.to_string(),
            session_id: session.map(str::to_string),
            cwd: PathBuf::from(cwd),
        }
    }

    #[test]
    fn tab_mapping_prefers_session_id_then_transcript_then_unique_cwd() {
        let tabs = [
            cand("claude", Some("sess-a"), "C:/proj"),
            cand("ai-2", Some("sess-b"), "C:/proj/wt"),
            cand("claude-local", None, "C:/proj"),
        ];
        // 1. session id.
        assert_eq!(
            resolve_permission_tab(&tabs, "sess-b", "", ""),
            Some("ai-2".to_string())
        );
        // 2. transcript stem, when the session field went missing.
        assert_eq!(
            resolve_permission_tab(
                &tabs,
                "",
                "C:/Users/x/.claude/projects/slug/sess-a.jsonl",
                ""
            ),
            Some("claude".to_string())
        );
        // 3. cwd, but only where it names exactly one tab: `C:/proj` is shared
        // by two tabs (ambiguous ⇒ drop), the worktree dir is unique.
        assert_eq!(resolve_permission_tab(&tabs, "", "", "C:/proj"), None);
        assert_eq!(
            resolve_permission_tab(&tabs, "", "", "C:/proj/wt"),
            Some("ai-2".to_string())
        );
        // Separator/trailing-slash normalization (and, on Windows, case).
        assert_eq!(
            resolve_permission_tab(&tabs, "", "", "C:\\proj\\wt\\"),
            Some("ai-2".to_string())
        );
        // Nothing matches ⇒ dropped, never guessed.
        assert_eq!(
            resolve_permission_tab(&tabs, "sess-zz", "", "D:/elsewhere"),
            None
        );
        assert_eq!(resolve_permission_tab(&tabs, "", "", ""), None);
        assert_eq!(resolve_permission_tab(&[], "sess-a", "", "C:/proj"), None);
    }

    #[test]
    fn tab_mapping_refuses_a_session_claimed_by_two_tabs() {
        let tabs = [
            cand("claude", Some("dup"), "C:/a"),
            cand("claude-local", Some("dup"), "C:/b"),
        ];
        assert_eq!(resolve_permission_tab(&tabs, "dup", "", ""), None);
    }

    #[test]
    fn permission_event_body_tolerates_a_minimal_payload() {
        // Only the event name — every other field defaults, so a drifted
        // payload still deserializes and is simply unmappable (dropped).
        let body: PermissionEventBody =
            serde_json::from_value(serde_json::json!({ "event": "Notification" }))
                .expect("minimal body deserializes");
        assert_eq!(body.event, "Notification");
        assert!(body.session_id.is_none() && body.cwd.is_none());
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
            root: "P:\\proj".into(),
        };
        let s = serde_json::to_string(&d).unwrap();
        let back: Discovery = serde_json::from_str(&s).unwrap();
        assert_eq!(back.port, 8123);
        assert_eq!(back.token, "tok");
        assert_eq!(back.pid, 42);
        // Legacy files (pre-root) still parse: `root` defaults empty.
        let legacy: Discovery = serde_json::from_str(r#"{"port":1,"token":"t","pid":9}"#).unwrap();
        assert_eq!(legacy.root, "");
    }

    fn disc(pid: u32, port: u16, root: &str) -> Discovery {
        Discovery {
            port,
            token: format!("tok{pid}"),
            pid,
            root: root.to_string(),
        }
    }

    #[test]
    fn select_discovery_routes_by_root() {
        // Two instances off one install: a child whose cwd is inside project
        // B must reach B's instance, never last-writer-wins.
        let entries = vec![disc(1, 1001, "P:\\proj\\a"), disc(2, 1002, "P:\\proj\\b")];
        let picked = select_discovery(entries, Some(Path::new("P:\\proj\\b\\src"))).expect("match");
        assert_eq!(picked.pid, 2);
    }

    #[test]
    fn select_discovery_deepest_matching_root_wins() {
        // Nested checkouts: the closest (deepest) serving instance wins.
        let entries = vec![disc(1, 1001, "P:\\proj"), disc(2, 1002, "P:\\proj\\nested")];
        let picked =
            select_discovery(entries, Some(Path::new("P:\\proj\\nested\\src"))).expect("match");
        assert_eq!(picked.pid, 2);
        // A hint outside the nested root resolves to the outer instance.
        let entries = vec![disc(1, 1001, "P:\\proj"), disc(2, 1002, "P:\\proj\\nested")];
        let picked = select_discovery(entries, Some(Path::new("P:\\proj\\other"))).expect("match");
        assert_eq!(picked.pid, 1);
    }

    #[cfg(windows)]
    #[test]
    fn select_discovery_is_case_insensitive_on_windows() {
        let entries = vec![disc(1, 1001, "p:\\PROJ\\A")];
        let picked =
            select_discovery(entries, Some(Path::new("P:\\proj\\a\\deep"))).expect("match");
        assert_eq!(picked.pid, 1);
    }

    #[test]
    fn select_discovery_sole_entry_wins_without_a_root_match() {
        // One running instance is unambiguous even when the hint doesn't
        // land inside its root (e.g. an agent launched outside any project).
        let entries = vec![disc(7, 1007, "P:\\elsewhere")];
        let picked = select_discovery(entries, Some(Path::new("Q:\\other"))).expect("sole entry");
        assert_eq!(picked.pid, 7);
    }

    #[test]
    fn is_ancestor_or_equal_rejects_prefix_strings_and_unrelated() {
        assert!(is_ancestor_or_equal(
            Path::new("P:\\proj\\a"),
            Path::new("P:\\proj\\a")
        ));
        // Component-wise, not string-prefix: `P:\proj\a` is NOT an ancestor
        // of `P:\proj\ab`.
        assert!(!is_ancestor_or_equal(
            Path::new("P:\\proj\\a"),
            Path::new("P:\\proj\\ab")
        ));
        assert!(!is_ancestor_or_equal(
            Path::new("P:\\proj\\a\\deep"),
            Path::new("P:\\proj\\a")
        ));
        assert!(!is_ancestor_or_equal(Path::new(""), Path::new("P:\\proj")));
    }

    #[test]
    fn audit_run_body_parses_both_categories_and_rejects_junk() {
        use crate::audit::adapters::Category;
        // Both wire categories deserialize; `consumer` is optional and ignored.
        let sec: AuditRunBody =
            serde_json::from_slice(br#"{"category":"security","consumer":"claude"}"#).unwrap();
        assert_eq!(sec.category, Category::Security);
        assert_eq!(sec.consumer.as_deref(), Some("claude"));
        let qual: AuditRunBody = serde_json::from_slice(br#"{"category":"quality"}"#).unwrap();
        assert_eq!(qual.category, Category::Quality);
        assert!(
            qual.consumer.is_none(),
            "consumer defaults to None when absent"
        );
        // A bad category word (or a missing `category`) is a clean parse error →
        // the route answers 400.
        assert!(serde_json::from_slice::<AuditRunBody>(br#"{"category":"bogus"}"#).is_err());
        assert!(serde_json::from_slice::<AuditRunBody>(br#"{"consumer":"x"}"#).is_err());
    }

    #[test]
    fn graph_run_body_round_trips_the_v28_tab_field() {
        // V28: the per-tab MCP child tags `/graph_run` with the tab it serves.
        let tagged: GraphRunBody = serde_json::from_slice(
            br#"{"cwd":"P:\\proj","name":"context_recall","args":{},"consumer":"opencode","tab":"opencode"}"#,
        )
        .expect("tagged body parses");
        assert_eq!(tagged.tab.as_deref(), Some("opencode"));
        assert_eq!(tagged.consumer.as_deref(), Some("opencode"));
        assert_eq!(tagged.name, "context_recall");
    }

    #[test]
    fn graph_run_body_still_accepts_pre_v28_bodies() {
        // Fail-open on the wire: a child spawned before the upgrade (or by hand)
        // sends no `tab` at all, and an explicit `null` must read the same. Both
        // resolve to `None`, i.e. the pre-V28 most-recent-session scoping — never
        // a 400 that would break the tool call.
        let absent: GraphRunBody =
            serde_json::from_slice(br#"{"name":"context_notes","args":{},"consumer":"claude"}"#)
                .expect("pre-V28 body still parses");
        assert!(absent.tab.is_none());
        assert!(absent.cwd.is_none());

        let null: GraphRunBody =
            serde_json::from_slice(br#"{"name":"context_notes","args":{},"tab":null}"#)
                .expect("explicit null parses");
        assert!(null.tab.is_none());

        // An unknown extra field (a NEWER child talking to an older app) is
        // likewise tolerated rather than rejected.
        let extra: GraphRunBody =
            serde_json::from_slice(br#"{"name":"context_notes","args":{},"future_field":1}"#)
                .expect("unknown fields ignored");
        assert!(extra.tab.is_none());
    }
}

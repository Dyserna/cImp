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
//! (It has grown a few more since — the graph/audit/context/memory routes the
//! per-tab child and the harness hooks post to, plus `GET /status`, the V32
//! Phase B taint-latch debug view.)
//!
//! The `{port, token, pid}` are advertised in a discovery file written next
//! to the exe (the portable root — never `~/.claude`), created when offload
//! is enabled and removed on exit. The token rotates every launch. This is
//! the one genuinely new security surface: loopback-only bind + token auth +
//! a user-readable discovery file (tightened where the OS allows). A
//! malicious *local* process that reads the file could drive offloads or
//! observe task text — the same localhost-dev-server trust assumption,
//! documented in MAINTENANCE.md.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

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
use super::detection;
use super::outbound::{self, Budget};
use super::toolclass::{self, Latch, Profile, ProxyGate, ToolClass, WriteTaint};
use super::discovery::{
    canon, discovery_path, external_project_root, is_ancestor_or_equal, own_discovery_path,
    read_all_discoveries, read_discovery, sweep_stale_discoveries, Discovery,
    MAX_DISCOVERY_PROBES,
};

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
    /// V32 Phase A: optional task shape (`"research"` | `"code"`) that
    /// pre-applies the worker's taint latch. Kept as a raw string on the wire
    /// and re-validated here (see [`handle_run`]) rather than deserialized into
    /// the enum, so an invalid value produces the tool-facing error message
    /// instead of a generic serde "bad request body".
    #[serde(default)]
    profile: Option<String>,
    /// V32 C-1c (2026-08-07 review): the cImp tab this child serves
    /// (`cimp --offload-mcp --tab <id>`), resolved to the tab's taint latch.
    /// Absent on a legacy child ⇒ the fail-open anonymous scope.
    #[serde(default)]
    tab: Option<String>,
    /// V32 C-1c: which agent is calling (`claude` / `opencode`, from the
    /// child's `--consumer` flag), sent in the body exactly as `/graph_run`
    /// does. The latch registry is keyed by `(agent, tab)`, so a missing
    /// consumer would key an OpenCode tab's calls under the Claude agent and
    /// gate against a latch that is not the caller's. Absent ⇒ `claude`, the
    /// route's long-standing default.
    #[serde(default)]
    consumer: Option<String>,
    /// V32 C-1c: which of the two offload tools the caller invoked, so the
    /// refusal and its activity row name the tool the model actually called.
    /// An `offload_batch` fans out to one `/run` per subtask, so this route
    /// serves both. Validated against the two known names at the parse boundary
    /// ([`offload_tool_name`]) rather than trusted — it reaches an activity row.
    #[serde(default)]
    tool: Option<String>,
}

/// The offload tool name a `/run` body names, defaulted and validated (C-1c).
///
/// Anything other than the two real names — absent, a legacy child that sends
/// no `tool`, or an invented string — reads as `offload_task`. This is a
/// *labelling* input, not a capability one: both names classify
/// LOCAL-CAPABILITY, so no value can change the gate's verdict, and pinning the
/// vocabulary keeps a caller from choosing what an activity row says.
fn offload_tool_name(raw: Option<&str>) -> &'static str {
    match raw.map(str::trim) {
        Some("offload_batch") => "offload_batch",
        _ => "offload_task",
    }
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
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    auth: Option<String>,
    /// V35 Phase J: the `X-CIMP-*` identity headers, when the caller sent any.
    pub(crate) cimp: CimpHeaders,
    pub(crate) body: Vec<u8>,
}

/// The identity a **Claude `type: "http"` hook** carries, which its body cannot.
///
/// A hook's body is the harness's own payload — cImp gets no field in it — so
/// the tab id, the harness discriminator, the CHP version and the hello
/// declaration ride headers baked into the emitted hook entry at spawn
/// (`harness::claude::hook`). Every value here is caller-supplied and is
/// validated/bounded at the point of use exactly as the equivalent body fields
/// are on the CHP routes.
#[derive(Debug, Default, Clone)]
pub(crate) struct CimpHeaders {
    pub(crate) tab: Option<String>,
    pub(crate) agent: Option<String>,
    pub(crate) chp: Option<u32>,
    pub(crate) hello: Option<String>,
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
    let mut cimp = CimpHeaders::default();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            match key.as_str() {
                "authorization" => auth = Some(val.to_string()),
                "content-length" => content_length = val.parse().unwrap_or(0),
                // V35 Phase J. Read for EVERY request, not only the Claude hook
                // routes: the parser has no route context, and a header on a
                // route that does not consult it is simply unread. The names are
                // the lowercase of `claude_hook`'s canonical spellings, pinned
                // by `the_cimp_headers_are_read_under_the_names_the_overlay_emits`.
                "x-cimp-tab" => cimp.tab = Some(val.to_string()),
                "x-cimp-agent" => cimp.agent = Some(val.to_string()),
                // A non-numeric version is `None`, i.e. pre-CHP — never an
                // error. Same tolerated-absent rule as the `chp` body field.
                "x-cimp-chp" => cimp.chp = val.parse().ok(),
                "x-cimp-hello" => cimp.hello = Some(val.to_string()),
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
        cimp,
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

/// **Every path core's own router serves**, scraped from the dispatch `match`.
///
/// Test-only, and it exists for one assertion: a plugin route that duplicates a
/// core path would never run, because core's arms are matched first. Read as
/// text rather than as a list, so the answer cannot drift from the `match` the
/// way a hand-kept enumeration would.
#[cfg(test)]
pub(crate) fn core_route_paths() -> std::collections::BTreeSet<&'static str> {
    // V40 review L-2: scoped to the dispatch `match` itself, not the whole
    // file. Scanning the file meant a line beginning `("` anywhere (a tuple in
    // a test, say) had to be tolerated, which forced the scan to SKIP anything
    // it could not parse — and a wrapped dispatch arm is exactly that: the path
    // would drop out of the set, `no_plugin_route_shadows_a_core_route` would
    // still pass on the smaller set, and a plugin could then declare a
    // shadowing route with the test green and its handler never running.
    // Inside the block there is nothing to tolerate, so an unreadable arm is a
    // panic.
    //
    // Both delimiters are built with `concat!` so this function's own source
    // does not contain them: a self-matching needle would cut the block at the
    // scanner instead of at the dispatch and answer the empty set.
    let src = include_str!("loopback.rs");
    let block = src
        .split_once(concat!("match (req.method.as_str(), ", "route) {"))
        .expect("the loopback dispatch `match` is gone — re-point this scan")
        .1;
    let block = block
        .split_once(concat!("_ => match crate::harness::", "ingress::route("))
        .expect("the plugin-route fallthrough arm is gone — re-point this scan")
        .0;
    let mut out = std::collections::BTreeSet::new();
    for line in block.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("(\"") else {
            continue;
        };
        // A dispatch arm reads `(<method>, <path>) => handler(..)`.
        let path = rest
            .split_once("\", \"")
            .and_then(|(_method, tail)| tail.split_once('"'))
            .map(|(path, _)| path)
            .filter(|path| path.starts_with('/'))
            .unwrap_or_else(|| {
                panic!(
                    "loopback dispatch: `{line}` opens a route arm this scan cannot read (a                      wrapped arm, or a rustfmt pass). Left unread it would drop the path from                      `core_route_paths()` and let a plugin shadow it with every test green."
                )
            });
        // `&'static str` from the embedded source, which is `'static`.
        out.insert(path);
    }
    out
}

/// One parsed request, for the plugin-route tests that need a `Request` they
/// cannot build from outside this module.
#[cfg(test)]
pub(crate) fn request_for_test(
    method: &str,
    path: &str,
    tab: Option<&str>,
    chp: Option<u32>,
) -> Request {
    Request {
        method: method.to_string(),
        path: path.to_string(),
        auth: None,
        cimp: CimpHeaders {
            tab: tab.map(str::to_string),
            agent: None,
            chp,
            hello: None,
        },
        body: Vec::new(),
    }
}

/// The two routes whose identity-less bodies do NOT default to
/// [`crate::harness::DEFAULT_HARNESS`], spelled once each so the lookup and the
/// dispatch arm cannot part company.
const MEMORY_EVENT_ROUTE: &str = "/memory/event";
/// See [`MEMORY_EVENT_ROUTE`].
const LATCH_STATE_ROUTE: &str = "/latch/state";

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
    // V35 Phase I: observe the CHP protocol version this caller speaks, BEFORE
    // dispatch and beside it rather than inside nine handlers — the routes' own
    // body types stay byte-identical, which is this phase's exit criterion. It
    // reads only, answers nothing and cannot reject: a route's behaviour is the
    // same whether this line runs or not.
    note_chp(&app, route, &req);
    match (req.method.as_str(), route) {
        ("POST", "/run") => handle_run(&mut stream, &service, &app, &req).await,
        ("POST", "/graph_run") => handle_graph_run(&mut stream, &app, &req).await,
        ("POST", "/audit/run") => handle_audit_run(&mut stream, &app, &req).await,
        ("POST", "/context/retrieve") => handle_context_retrieve(&mut stream, &app, &req).await,
        ("POST", "/workbench/tool_checkpoint") => handle_tool_checkpoint(&mut stream, &app, &req).await,
        ("POST", "/context/compaction") => handle_context_compaction(&mut stream, &app, &req).await,
        ("POST", "/context/should_read") => handle_should_read(&mut stream, &app, &req).await,
        ("POST", "/context/post_edit") => handle_post_edit(&mut stream, &app, &req).await,
        ("POST", "/memory/event") => handle_memory_event(&mut stream, &app, &req).await,
        ("POST", "/activity/contract_drift") => handle_contract_drift(&mut stream, &req).await,
        ("POST", "/activity/discovery_skipped") => handle_discovery_skipped(&mut stream, &app, &req).await,
        ("POST", "/latch/beacon") => handle_latch_beacon(&mut stream, &app, &req).await,
        ("POST", "/latch/state") => handle_latch_state(&mut stream, &app, &req).await,
        ("POST", "/session/hello") => handle_session_hello(&mut stream, &app, &req).await,
        // ── V35 Phase L: the read path, as CHP pushes ────────────────────────
        //
        // The harness-neutral half of the three capabilities Phase L moves off
        // the Tier-C readers. A harness whose hook body cannot carry a CHP
        // envelope reaches the same cores through its OWN routes, appended
        // below from the registry; these are what a harness whose plugin CAN
        // build a body posts to, and what the tests drive.
        ("POST", "/session/assistant_text") => handle_session_assistant_text(&mut stream, &app, &req).await,
        ("POST", "/session/tool_result") => handle_session_tool_result(&mut stream, &app, &req).await,
        ("POST", "/session/subagent") => handle_session_subagent(&mut stream, &app, &req).await,
        // V40 Phase D (locked decisions 18 and 30): the neutral activity edges.
        // Phase C declared them; these are the producers, and what makes them
        // `live` in `chp::EVENTS`. A harness that reports its own turn
        // boundaries posts here instead of leaving core to infer them from the
        // terminal — which is the same fact `ActivitySource::OutOfBand`
        // declares at L1, arriving over the wire instead.
        ("POST", "/session/output_started") => handle_harness_output(&mut stream, &app, &req, true).await,
        ("POST", "/session/output_stopped") => handle_harness_output(&mut stream, &app, &req, false).await,
        ("POST", "/session/subagents_active") => handle_subagents_active(&mut stream, &app, &req).await,
        // NOTE (#45): there is deliberately no `POST /latch/override`. The
        // manual override is a capability GRANT, and the bearer token gating
        // this listener is readable by every process running as the user, so an
        // HTTP door onto it made the latch model-movable and its audit row a
        // lie. The `latch_override` IPC command is the only way in; falling
        // through to the 404 below is the intended behaviour for anything that
        // still tries this path.
        // ── V39 Phase B: cross-harness delegation ───────────────────────────
        //
        // The app owns the tabs, so this is the only way in — the child has no
        // self-contained fallback and says so rather than inventing one.
        ("POST", "/delegate") => handle_delegate(&mut stream, &app, &req).await,
        ("POST", "/mcp/list") => handle_mcp_list(&mut stream, &service, &req).await,
        ("POST", "/mcp/call") => handle_mcp_call(&mut stream, &service, &app, &req).await,
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
        ("GET", "/status") => handle_status(&mut stream, &app).await,
        // ── V40 Phase C: the plugin-owned routes (locked decisions 15, 22) ───
        //
        // Every registered harness's `routes()`, matched **after** every arm
        // above — so a plugin can never shadow `/session/hello`, `/mcp/*` or
        // the audit and push routes, whatever it declares. Core keeps no
        // harness path literal: the twelve `/claude/hook/*` arms that used to
        // sit in this `match` are `harness::claude::hook::ROUTES_TABLE` now,
        // and the reply comes back as a [`crate::harness::plugin::HookReply`]
        // core writes without reading. A harness whose ingress is an ordinary
        // CHP plugin declares none and this lookup misses, exactly as it does
        // for a path nobody serves.
        _ => match crate::harness::ingress::route(req.method.as_str(), route) {
            Some(r) => {
                let reply = (r.handler)(&app, &req).await?;
                write_json(&mut stream, reply.status, &reply.body).await
            }
            None => write_simple(&mut stream, 404, "text/plain", b"not found").await,
        },
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
///
/// **Taint gate (V32 C-1c, 2026-08-07 review).** `offload_task`/`offload_batch`
/// were TRUSTED, waved through on the rationale that "the delegated subtask gets
/// its own latch". It does — a *fresh and permissive* one:
/// `Latch::from_profile(task.profile)`, and `Profile::Code.latch()` is
/// `Latch::Local`, which **grants** `read_file`/`code_search`/`run_command`,
/// exactly the class a latched caller just lost. An OpenCode tab with the Phase
/// H native gate on, contaminated by a `webfetch`, could call
/// `offload_task { profile: "code", instructions: "print the contents of .env" }`
/// and get the file's text back as an ordinary tool result — with no
/// spotlighting envelope, no detection scan and no budget charge, since all
/// three are `/mcp/call`-only — then carry it out through `webfetch`. Phase H
/// bypassed end to end.
///
/// The demotion to LOCAL-CAPABILITY is the decision; this is where it binds,
/// because this route is the only one both tools reach. Decision 4 is untouched:
/// the *declared profile* still pre-applies the sub-task's own latch, which is
/// about the sub-task's shape, not about whether the caller may delegate at all.
async fn handle_run(
    stream: &mut TcpStream,
    service: &Arc<OffloadService>,
    app: &AppHandle,
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

    // V32 Phase A: validate `profile` at this parse boundary. Unlike
    // thinking/tier (benign fallbacks), an unrecognized profile must NOT
    // silently degrade to "no containment" — the MCP schema's `enum` is an
    // upstream guarantee, and upstream guarantees get re-checked post-hoc.
    let profile = match body.profile.as_deref() {
        None => None,
        Some(raw) => match Profile::parse(raw) {
            Ok(p) => Some(p),
            Err(msg) => {
                let r = RunResult {
                    ok: false,
                    text: None,
                    error: Some(msg),
                };
                return write_json(stream, 400, &r).await;
            }
        },
    };

    // V32 C-1c: the taint gate, after every parse-boundary rejection so a
    // malformed request never engages a latch, and before any work starts.
    // ONE settings read for identity + policy; an unknown tab id yields no
    // scope and keys no registry entry (#45's bound, via the same `latch_scope`
    // funnel). `LatchRoute::Native` — this route serves cImp's own tools, never
    // a proxied server's content.
    let tool = offload_tool_name(body.tool.as_deref());
    // V40 review H-1: the same one-resolution funnel `/mcp/call` uses. This
    // route's gate is `LatchRoute::Native` over `offload_task`/`offload_batch`
    // — the V32 C-1c gate whose doc paragraph above describes the `.env`
    // exfiltration it closes — so a consumer token that resolved to no tab
    // scope disabled exactly that gate.
    let Some((_, run_agent)) = proxy_identity(body.consumer.as_deref()) else {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(unknown_consumer_message()),
        };
        return write_json(stream, 400, &r).await;
    };
    let settings = live_settings(app);
    let scoping = latch_scope(app, &settings, run_agent, body.tab.as_deref());
    if let LatchScoping::Unknown(tab) = &scoping {
        warn!(
            target: "offload",
            tab = %tab,
            tool = %tool,
            "loopback: /run has no configured tab to latch against — delegation is ungated"
        );
    }
    let scope = scoping.scope();
    let policy = GatePolicy::resolve(&settings, scope);
    // `CallProvenance::internal()`: cImp's own dispatch, and a native route
    // serves no fetched page — there is no content origin to name (#48, F-3).
    if let Err(refusal) = latches().gate(
        scope,
        LatchRoute::Native,
        tool,
        policy,
        CallProvenance::internal(),
    ) {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(refusal.to_string()),
        };
        // 200 with `ok:false`: a task-level error the child renders as a tool
        // result, the same framing `/run`'s own failures use. Sent before
        // `write_ndjson_head`, so a plain single-JSON body — which the child's
        // reader handles as the unterminated trailing line.
        return write_json(stream, 200, &r).await;
    }

    let session_cwd = body.cwd.map(std::path::PathBuf::from);
    // V33 Phase F: the requesting tab, for the pre-mutation checkpoint the
    // worker takes before `run_command`. Read BEFORE the `service.run` call
    // below, which consumes the rest of `body`, and narrowed through the SAME
    // `tab_identity` funnel `/context/retrieve`'s prompt-tap checkpoint uses
    // (V33 C5) — an id naming no configured tab of this consumer is a forged or
    // stale claim, and a checkpoint is the one record that exists to be trusted
    // after an incident, so it degrades to "cannot attribute" rather than to
    // "some other tab".
    let checkpoint_tab = match tab_identity(&settings, run_agent, body.tab.as_deref()) {
        TabIdentity::Configured(t) => Some(t.to_string()),
        TabIdentity::Anonymous | TabIdentity::Unknown(_) => None,
    };

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
        checkpoint_tab.as_deref(),
        body.schema,
        profile,
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
    ///
    /// **Absent ⇒ `"."`, which resolves to nothing.** The field is optional for
    /// back-compat with children that predate it, and the `"."` default is a
    /// placeholder, NOT a working directory: it would resolve against *cImp's*
    /// process cwd (its install directory), which is never the caller's
    /// project. Every consumer therefore refuses it rather than guessing —
    /// graph tools with "no code graph found from .", `run_check` with an
    /// explicit "not an absolute project root", and `sandbox::plan` with an
    /// `Unavailable` skip. rc.9 live: a `/graph_run` post that omitted this
    /// field reached the AppContainer engine, which mapped a drive letter to
    /// `\??\.` and failed the spawn with a bare `CreateProcessW failed (267)`.
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
    /// The calling session's working directory (the child's cwd), used to
    /// attribute the Tool Activity row to a project. Optional by design — a
    /// child from before this field sends none, and the row just gets an
    /// empty root.
    #[serde(default)]
    cwd: Option<String>,
    /// V32 Phase B: the cImp TAB id the calling MCP child was spawned for.
    /// V28 sent `tab` on `/graph_run` only — external servers hold no cImp
    /// memory scope — but the taint latch needs the *same* identity on both
    /// tool-serving routes or a tab could launder an external fetch past its
    /// own latch. Optional on exactly the same fail-open terms as
    /// [`GraphRunBody::tab`].
    #[serde(default)]
    tab: Option<String>,
}

// ── V32 Phase B — the consumer-side taint latch ────────────────────────────

/// The identity one gated call carries: which agent, which tab, and which
/// session that tab is currently running.
///
/// `agent` is always the normalized `claude`/`opencode` vocabulary
/// ([`crate::graph::source_for_consumer`]) because the two gated routes learn
/// the consumer differently — `/graph_run` from the body, `/mcp/call` from the
/// `?consumer=` query — and one tab MUST key identically from either, or its
/// web fetches and its graph reads would latch two separate scopes.
#[derive(Debug, PartialEq, Eq)]
struct LatchScope {
    agent: &'static str,
    tab: String,
    /// The session the V28 registry currently reports for this tab, or `None`
    /// when it withholds one (no live entry yet, TTL-stale, or the H1
    /// same-root ambiguity). `None` is *absence of evidence*, never evidence of
    /// a new session — see [`TabLatch::observe`].
    session: Option<String>,
    /// The project root this tab runs against, in [`crate::activity::root_key`]
    /// form — the `root` column of the activity rows this scope's calls
    /// produce (#48, finding F-3).
    ///
    /// It rides on the scope rather than on each call because it is a property
    /// of the tab, and because [`latch_scope`] is the ONE funnel every gated
    /// route resolves identity through: a row written from a path that has a
    /// scope therefore cannot be written without a root, which is the mistake
    /// `beacon_row` and the memory-quarantine row made (`root: ""`, so neither
    /// can be filtered per project, so neither can appear on a per-project
    /// surface).
    ///
    /// **Resolved from settings, not from the request.** The gated bodies all
    /// carry a `cwd`, but that is the calling child's claim about itself; the
    /// tab id is config-derived and validated against the same snapshot
    /// ([`is_configured_tab`]), so `crate::tabs::ai_tab_dir` gives a root with
    /// the same trust level as the identity it hangs off. Empty only if no
    /// working directory can be resolved at all — see [`tab_root_key`].
    root: String,
}

impl LatchScope {
    /// The registry key. Tuple, not a formatted string, so no tab id
    /// containing the separator could collide with another agent's tab.
    fn key(&self) -> (&'static str, String) {
        (self.agent, self.tab.clone())
    }

    /// V32 Phase G: this scope as the injection resolver addresses it. The two
    /// vocabularies are deliberately the same pair (`agent`, `tab`) so a tab's
    /// latch and a tab's override row can never key differently.
    fn injection(&self) -> crate::settings::injection::Scope<'_> {
        crate::settings::injection::Scope::Tab {
            agent: self.agent,
            tab: &self.tab,
        }
    }

    /// The human-readable scope label carried by V32 `injection_flag` activity
    /// rows. Formatted (unlike [`key`](Self::key)) because it is for a reader,
    /// not for equality.
    fn label(&self) -> String {
        format!("{}:{}", self.agent, self.tab)
    }

    /// This scope as an activity-row attribution (#48 F-29).
    ///
    /// A scope exists only for a **configured** tab id ([`is_configured_tab`],
    /// checked by [`latch_scope`]), so [`Attribution::Tab`] is a fact here — the
    /// same reading [`LatchScoping::attribution`] takes for its `Scoped` arm,
    /// which now delegates here so the two cannot drift.
    ///
    /// The tab id is taken from the field, never re-split out of
    /// [`label`](Self::label): a round trip through a formatted string is how
    /// `"{agent}:(no tab identity)"` became a *tab named `(no tab identity)`*
    /// once already (`outbound::scope_attribution`'s doc).
    fn attribution(&self) -> crate::activity::Attribution {
        crate::activity::Attribution::Tab(self.tab.clone())
    }
}

/// Whether `tab` names an AI tab the **user has configured for `agent`** (#45;
/// consumer-scoped by V33 C5, finding F-4).
///
/// This is the predicate that makes [`latches`]' "bounded by construction"
/// claim true rather than aspirational. Every registry entry is keyed on a
/// tab id that arrives in a request body, so without this the map's key space
/// is "whatever a caller typed" — no TTL, no cap, no eviction, and every entry
/// serialized into every `/status` response and every 4 s `latch_status` poll.
/// With it, the key space is a subset of the user's own tab list.
///
/// **V33 C5 — the pair, not the halves.** Until V33 this asked only "is this
/// *some* configured AI tab id", while every registry key is the PAIR
/// `(agent, tab)` ([`LatchScope::key`]) and `agent` is caller-asserted on every
/// route that has one. A caller could therefore key a latch under
/// `("claude", <an OpenCode tab's id>)` and the pair was verified on no route in
/// the system. It is now verified here, at the one funnel
/// ([`latch_scope`]) every entry-creating path resolves through: the id must
/// name a configured tab **of the asserted consumer**, classified by
/// [`crate::tabs::tab_consumer`] — the same call the launch path makes when it
/// decides what to inject into that tab, so the two ends cannot drift.
///
/// **Not a live exploit today, a restored invariant.** The V32 review rated the
/// cross-keyed case harmless on the routes that exist: a latch keyed under the
/// wrong agent is freshly open, engages a scope nobody reads, and refuses
/// nothing. What it bought was a registry key space twice the size of the tab
/// list and a `(consumer, tab)` pair no route checked.
///
/// **The check is still "is this a configured tab id of this consumer", NOT "is
/// this the tab that owns this connection".** The stricter form would break
/// legitimate beacons today: the OpenCode plugin file is written per *directory*
/// (one file per tab since #48's H-2 fix, but every tab in a directory still
/// loads every file), so the tab id baked into it may belong to a different tab
/// sharing the same working dir. Whoever fixes H-2's remainder may tighten this;
/// until then, binding a beacon to its connection would reject real beacons from
/// real tabs.
///
/// **`AiTool` tabs only.** Shell and Preview tabs host no harness, so nothing
/// legitimate can beacon or gate as one.
///
/// **The empty-list escape**, and why it is keyed on the WHOLE list rather than
/// on this consumer's slice. With no AI tab configured at all the predicate
/// accepts everything, because [`live_settings`] falls back to
/// `Settings::default()` (whose `tabs` is empty) when managed state is not up
/// yet — and a request arriving in that window must not be rejected on the
/// strength of a list we could not read. That condition is "settings are
/// unreadable", which is global; narrowing the *floor* to "this consumer has no
/// tabs" would have widened it instead, handing every forged id a scope on any
/// install that runs only Claude tabs or only OpenCode ones — i.e. re-opening
/// exactly the unbounded key space #45 closed. So the floor keeps its original
/// trigger and only the positive test is consumer-scoped, which makes this
/// change a strict tightening of the admitted set.
pub(crate) fn is_configured_tab(settings: &crate::settings::Settings, agent: &'static str, tab: &str) -> bool {
    names_a_configured_ai_tab_for(settings, agent, tab) || ai_tab_ids(settings).next().is_none()
}

/// Every configured AI tab's id, in settings order — **every consumer's**.
///
/// One caller, and it is not the latch: [`is_configured_tab`]'s availability
/// floor, whose condition is "settings are unreadable", not "this consumer has
/// no tabs". Identity checks use [`ai_tab_ids_for`] instead. (The second
/// caller, the C-2 collision check, went with V40 Phase D's key spaces.)
fn ai_tab_ids(settings: &crate::settings::Settings) -> impl Iterator<Item = &str> {
    settings.tabs.iter().filter_map(|t| match t {
        crate::settings::TabConfig::AiTool(c) => Some(c.id.as_str()),
        _ => None,
    })
}

/// Every configured AI tab id belonging to `agent` (`"claude"` / `"opencode"`),
/// in settings order — V33 C5's key space.
fn ai_tab_ids_for<'a>(
    settings: &'a crate::settings::Settings,
    agent: &'static str,
) -> impl Iterator<Item = &'a str> {
    settings.tabs.iter().filter_map(move |t| match t {
        crate::settings::TabConfig::AiTool(c) if crate::tabs::tab_consumer(c) == Some(agent) => {
            Some(c.id.as_str())
        }
        _ => None,
    })
}

/// Whether `id` exactly names a configured AI tab **of `agent`** —
/// [`is_configured_tab`] without its availability floor.
fn names_a_configured_ai_tab_for(
    settings: &crate::settings::Settings,
    agent: &'static str,
    id: &str,
) -> bool {
    ai_tab_ids_for(settings, agent).any(|t| t == id)
}

// `names_a_configured_ai_tab` lived here: "does this session id collide with a
// configured TAB id", the C-2 guard on `/memory/event`'s registry writes. V40
// Phase D deleted it with the collision it guarded — the live-session registry
// has two key spaces now (locked decision 20), a body-supplied id goes into the
// session space, and no string can be in both. See
// `mark_live_session_from_body`.

/// Which of three cases a request body's `(agent, tab)` falls into, decided
/// **without** the `AppHandle` [`latch_scope`]'s session lookup needs.
///
/// V33 C5: `agent` is part of the question, not context carried alongside it —
/// `Configured` now means "a tab of THIS consumer", which is what the registry
/// key `(agent, tab)` has always asserted and nothing checked.
///
/// Split out (#48) for two reasons. It is the enforcement point for the
/// registry bound, and a bound asserted by calling [`is_configured_tab`] beside
/// `latch_scope` rather than through it survives deleting the call from
/// `latch_scope` — which is what
/// `tests::only_configured_ai_tab_ids_can_ever_key_a_latch` did. And it names
/// the distinction #45 collapsed: "no tab id" and "an id that names no
/// configured tab" are the same for the *registry* and not the same for a
/// *verdict*.
#[derive(Debug, PartialEq, Eq)]
enum TabIdentity<'a> {
    /// No `tab` at all (absent, empty, or whitespace) — a child spawned before
    /// `--tab` existed.
    Anonymous,
    /// A non-empty id naming no configured AI tab. The trimmed id is carried
    /// for the log lines and error messages that have to quote it back.
    Unknown(&'a str),
    /// A configured AI tab id, trimmed.
    Configured(&'a str),
}

fn tab_identity<'a>(
    settings: &crate::settings::Settings,
    agent: &'static str,
    tab: Option<&'a str>,
) -> TabIdentity<'a> {
    let Some(tab) = tab.map(str::trim).filter(|t| !t.is_empty()) else {
        return TabIdentity::Anonymous;
    };
    if is_configured_tab(settings, agent, tab) {
        TabIdentity::Configured(tab)
    } else {
        TabIdentity::Unknown(tab)
    }
}

/// Resolve the calling tab's latch scope, keeping the two identity-less cases
/// apart (#48).
///
/// [`LatchScoping::scope`] is `None` for both, which is the **fail-open** case
/// (locked, and V28's existing discipline): a child spawned before `--tab`
/// existed sends nothing, and a tool call must never fail for lack of identity.
/// It is deliberately NOT promoted to a global latch — one identityless call
/// would then latch every consumer at once. Such calls still get the
/// spotlighting envelope on EXTERNAL results, which needs no identity.
///
/// #45 widened "no identity" to include **an id that is not a configured tab**,
/// and V33 C5 widened it again to **an id that is not a configured tab of the
/// asserted consumer** ([`is_configured_tab`]). This is the single funnel every entry-creating path
/// resolves through — `/graph_run` and `/mcp/call` via `gate`, `/latch/beacon`
/// via `beacon` — so validating here is what bounds the registry, rather than
/// three route-local checks that can drift apart. An unknown id creates no row
/// and gates nothing; a caller that invents ids only ever talks to a scope that
/// does not exist, which is where it started. **That part is unchanged.**
///
/// What #48 changes is only that the two cases are now *distinguishable* by the
/// caller. Folding them into one `Option::None` also folded them into
/// `handle_latch_state`'s hard-off verdict, which was a Phase H regression: see
/// [`LatchScoping::Unknown`].
///
/// Takes the settings snapshot rather than reading its own, so a handler
/// resolves identity and policy under the SAME snapshot (the "ONE settings read
/// for the whole call" discipline `/mcp/call` documents).
fn latch_scope(
    app: &AppHandle,
    settings: &crate::settings::Settings,
    agent: &'static str,
    tab: Option<&str>,
) -> LatchScoping {
    match tab_identity(settings, agent, tab) {
        TabIdentity::Anonymous => LatchScoping::Anonymous,
        TabIdentity::Unknown(tab) => LatchScoping::Unknown(tab.to_string()),
        TabIdentity::Configured(tab) => {
            let session = app
                .try_state::<Arc<crate::graph::GraphService>>()
                .and_then(|g| g.live_session_for_tab(tab, agent));
            LatchScoping::Scoped(LatchScope {
                agent,
                tab: tab.to_string(),
                session,
                root: tab_root_key(app, settings, tab),
            })
        }
    }
}

/// The project root one configured AI tab runs against, as an activity
/// `root_key` (#48, finding F-3). See [`LatchScope::root`] for why the tab —
/// rather than the request body's `cwd` — is the source.
///
/// Two fallbacks, in order, and both are deliberate:
///
/// 1. **The app's launch directory**, when the id resolves to no AI tab config.
///    That is reachable through [`is_configured_tab`]'s empty-list escape (a
///    request that arrives before managed state is up), and the launch dir is
///    what such a tab *would* run in — [`crate::tabs::ai_tab_dir`] returns the
///    per-tab `cwd` override or exactly this.
/// 2. **The process cwd**, when managed state is not up at all. The app sets
///    `LaunchContext::cwd` from the process cwd at startup and never chdirs, so
///    this is the same directory by another route rather than a guess.
///
/// An empty string is possible only if even `current_dir()` fails (a deleted
/// cwd). It is not papered over with a placeholder: a root that cannot be
/// resolved must read as absent, not as some other project.
fn tab_root_key(app: &AppHandle, settings: &crate::settings::Settings, tab: &str) -> String {
    let launch = app
        .try_state::<crate::ipc::AppState>()
        .map(|s| s.launch.cwd.clone())
        .or_else(|| std::env::current_dir().ok());
    let Some(launch) = launch else {
        return String::new();
    };
    let dir = crate::tabs::ai_tab_dir(settings, tab, &launch).unwrap_or(launch);
    crate::activity::root_key(&dir)
}

/// The outcome of [`latch_scope`] — [`TabIdentity`] with the session folded in.
#[derive(Debug, PartialEq, Eq)]
enum LatchScoping {
    /// No tab identity at all. Fail-open everywhere.
    Anonymous,
    /// An id that names no configured AI tab — a forged one, or (the case that
    /// makes this worth a variant) a **stale real one**: the OpenCode plugin is
    /// written per working *directory* with one tab id baked in (the unfixed
    /// H-2), so removing or re-id'ing that tab leaves the file on disk still
    /// naming an id the settings no longer have.
    ///
    /// It keys no registry entry — that is #45's bound and it is untouched —
    /// but it must not be read as "containment is off" either, because the two
    /// look identical to the plugin and only one of them is a decision anyone
    /// took. See `handle_latch_state`.
    Unknown(String),
    /// A configured AI tab. The only variant that can key the registry.
    Scoped(LatchScope),
}

impl LatchScoping {
    /// The scope, when there is one. `None` for both identity-less variants —
    /// the fail-open reading `gate`, `beacon` and `budget_gate` take, and the
    /// reason an unknown id still creates no registry entry.
    fn scope(&self) -> Option<&LatchScope> {
        match self {
            LatchScoping::Scoped(s) => Some(s),
            _ => None,
        }
    }

    /// Consume into the scope, for the callers that need to keep it.
    fn into_scope(self) -> Option<LatchScope> {
        match self {
            LatchScoping::Scoped(s) => Some(s),
            _ => None,
        }
    }

    /// #51 / #48 F-20 — which tab an activity row written for this call belongs
    /// to.
    ///
    /// The mapping is one-to-one onto [`crate::activity::Attribution`], and that
    /// is the whole point: both enums were derived from the same three facts — no
    /// tab identity at all / an id naming no configured tab / a configured tab —
    /// and the row's column exists to report which of the three this call was.
    /// Written here, once, so `/graph_run` and `/mcp/call` cannot answer it
    /// differently, and so a future route gets the answer by resolving identity
    /// rather than by remembering to.
    ///
    /// Both handlers used to call [`Self::into_scope`] immediately, which
    /// collapses `Anonymous` and `Unknown` into one `None`. That collapse is
    /// right for the latch (both fail open) and wrong for the row, which has to
    /// keep them apart — that collapse IS finding F-20.
    ///
    /// **Not `Attribution::from_child_argv`.** A tab id that reached this frame
    /// came out of a request BODY, which a caller can invent; the argv
    /// constructor's own doc forbids it here. [`latch_scope`] has already run the
    /// id through [`is_configured_tab`], and `Unrecognized` is what the
    /// unvalidated case is called on the row.
    ///
    /// **#48 F-39 / locked decision 42 — that invented id is BOUNDED here.** It
    /// is an arbitrary-length string from a request body and it lands in a row's
    /// attribution column, in a **capped per-lane ring**: a caller choosing how
    /// many bytes one row occupies is choosing how much of the lane it fills, and
    /// the rows that fall out the other end are the genuine ones. Same
    /// consequence as F-37 by a different route, and the same cure F-32 already
    /// left in the tree — [`bounded_id`], **applied AFTER classification**, which
    /// is the load-bearing half: [`latch_scope`] resolved this variant by running
    /// the FULL string through [`is_configured_tab`], so no truncated invented id
    /// can ever fold onto a configured one. Bounding at the parse boundary
    /// instead would close a bloat hole by opening an impersonation hole.
    ///
    /// [`Attribution::Unattributed`](crate::activity::Attribution::Unattributed)
    /// is deliberately unreachable from this function: a route that resolved a
    /// `LatchScoping` at all DOES know, and "the writer does not know" would be a
    /// false claim — the one thing that column must never make.
    fn attribution(&self) -> crate::activity::Attribution {
        match self {
            LatchScoping::Anonymous => crate::activity::Attribution::Headless,
            LatchScoping::Unknown(id) => {
                crate::activity::Attribution::Unrecognized(bounded_id(id))
            }
            LatchScoping::Scoped(s) => s.attribution(),
        }
    }

    /// The injection-hierarchy scope this call resolves features against.
    /// Both identity-less variants resolve as an **unknown caller**
    /// (`Scope::UnknownCaller`), the same fail-open reading
    /// [`GatePolicy::resolve`] has always taken for a scope-less call: the
    /// app-wide answer is the honest floor when there is no tab to ask about,
    /// and it is what an unrecognized id resolved to before #45 (L3 not found ⇒
    /// `Inherit` ⇒ L2 ⇒ L1).
    ///
    /// #48 F-35: that variant was called `Scope::App` until locked decision 36
    /// split it in two. Behaviour is unchanged — this site was always asking the
    /// identity-less question, and it keeps N-1's elevation (any configured
    /// tab's L3 `On` is honoured, because the caller IS one of those tabs).
    fn injection(&self) -> crate::settings::injection::Scope<'_> {
        self.scope().map_or(
            crate::settings::injection::Scope::UnknownCaller,
            LatchScope::injection,
        )
    }
}

/// One tab's latch, together with the session identity it was engaged for.
struct TabLatch {
    session: Option<String>,
    latch: Latch,
    /// V32 Phase C: this session's EXTERNAL call/byte spend (locked decision
    /// 11). It lives *here*, beside the latch, precisely so it inherits the
    /// latch's scope and reset rule — one conversation, one budget, both
    /// cleared together when the tab's session rotates. (H-2: `contaminated`
    /// no longer rides along; a permissive reset and an un-tainting reset need
    /// different evidence — see [`TabLatch::contaminated`].)
    budget: Budget,
    /// Whether this session's taint-latch refusal has already been reported to
    /// the Tool Activity feed. One row per scope: the latch is sticky, so every
    /// later refusal restates the same fact.
    latch_flagged: bool,
    /// Whether this session's native-web BEACON has already been reported
    /// (#48). Same one-row-per-scope bound and the same reset as
    /// [`latch_flagged`](Self::latch_flagged), and it exists for the same
    /// reason: a caller that POSTs `/latch/beacon` in a loop must produce one
    /// row, not one per request, or it floods a capped feed and evicts the rows
    /// the audit trail exists to keep.
    ///
    /// It is a separate bit from "the latch moved" because #45 keyed the row on
    /// the latch transition alone, and a beacon can change this conversation's
    /// state **without** moving the latch: a tab already latched `Local`
    /// (Phase A's other direction) takes the contamination bit and keeps its
    /// latch, which then quarantines every later `context_note` — silently,
    /// under the old condition.
    beacon_flagged: bool,
    /// V32 Phase F (locked decision 15): whether external content has entered
    /// this conversation *at all* — set the moment an EXTERNAL call is admitted
    /// (proxied, or beaconed from a harness-native web tool) and cleared by
    /// exactly three USER actions, none of them reachable over HTTP (see "Step
    /// 4" and "The 2026-08-10 amendment" below).
    ///
    /// It exists because decision 15 lets the USER move the latch, and the two
    /// facts then come apart: the latch says what the session may do NEXT,
    /// while contamination says what is already in its context window. A note
    /// written after "switch to local" was still composed by a model that read
    /// an attacker's page, so persistence must stay quarantined — contamination
    /// is a property of the conversation, not of the latch position.
    ///
    /// That argument governs [`LatchOverride::FlipLocal`] and only it. The
    /// 2026-08-10 amendment below draws the line where the user's own click
    /// draws it: a *workflow step* cannot un-taint a conversation, but a
    /// deliberate return to FULL access — the strictly larger risk — is the
    /// verdict the flip is not.
    ///
    /// # H-2 (2026-08-08 re-review): the bit is STICKY, and why
    ///
    /// It used to be cleared by [`TabLatch::observe`] on a session rotation,
    /// "because only a new conversation has a clean context". The premise was
    /// wrong about *who decides* a rotation happened. A Claude tab's session id
    /// is the file stem of the newest `*.jsonl` under its transcript root, and
    /// **decision 3 puts Claude's native Bash outside every cImp latch** — so
    /// the model can create that file itself. Every bar the tap could put in
    /// front of the rotation (the file exists / the file grew / the file
    /// decodes as a transcript naming that session) is a bar over a file the
    /// attacker is writing. C-2 fixed "the file exists", H-2 defeated "the file
    /// grew" with one trailing newline, and "it decodes" falls to `echo
    /// '{"sessionId":"…"}'`. **No filesystem-derived fact can be the trust root
    /// for un-tainting a context window**, so the reset is gone rather than
    /// re-armed.
    ///
    /// This amends locked decision 15: contamination is now a property of the
    /// **tab**, not of the conversation, deliberately — because the conversation
    /// boundary is attacker-controlled and the tab id is not (it is
    /// config-derived, and [`is_configured_tab`] bounds the key space).
    ///
    /// # Step 4 (2026-08-09): the clear path H-2 left open, and its trust root
    ///
    /// H-2 removed the last automatic reset and left "restart cImp" as the only
    /// escape — which is why the field doc used to end there. What it did not
    /// settle is what *may* clear the bit, and the answer is not a better piece
    /// of evidence: it is **authority**. A human acting in cImp's own UI is a
    /// fact no shell can fabricate (the webview holds no bearer token, and
    /// [`apply_latch_override`] is reachable only from the capability-scoped
    /// `latch_override` IPC command), and it is the same trust root every other
    /// consent surface in this app already uses.
    ///
    /// So exactly three things clear it, all rooted in that click:
    ///
    /// 1. [`LatchOverride::ClearContamination`] — the user judged the flagged
    ///    content harmless. Cleared immediately; nothing else about the tab or
    ///    its session changes.
    /// 2. [`LatchOverride::AwaitSessionClear`] +
    ///    [`awaiting_session_clear`](Self::awaiting_session_clear) — the user
    ///    restored a checkpoint. The bit **stays set** and lifts only when a
    ///    proved session rotation is observed. See that field for why a forgeable
    ///    rotation signal is acceptable *there* and nowhere else.
    /// 3. [`LatchOverride::Unlatch`] — the user restored FULL access. See "The
    ///    2026-08-10 amendment" below for the argument.
    ///
    /// **The accepted cost is unchanged for everything else.** A genuine
    /// `/clear` in a tab nobody armed keeps the bit: that conversation's
    /// `context_note` writes stay quarantined (they are stored and held for
    /// review, not dropped) and the badge keeps saying "contaminated".
    /// [`LatchOverride::FlipLocal`] — the workflow flip — still cannot clear it,
    /// and neither can any HTTP route: `/latch/beacon` only ever tightens, and
    /// `POST /latch/override` has not existed since #45.
    ///
    /// # The 2026-08-10 amendment: a full unlatch IS a verdict
    ///
    /// **The 2026-08-10 amendment to decision 15** (user: *"if the user restores
    /// full access then the tab should be cleared, it's the user's decision."*).
    /// A full unlatch hands back read AND web with the injected content still in
    /// the context window; that is the strictly larger risk, and it is taken
    /// behind the popover's own confirmation. Leaving the *memory* half
    /// quarantined after it made the product overrule a judgement it had just
    /// asked the user to make. Same trust root as (1): authority, not evidence.
    ///
    /// **Clearing the STATE never erases the EVIDENCE.** The
    /// [`outbound::Screen::Contamination`] row that set the bit stays in its own
    /// retention lane, and every release writes an
    /// [`outbound::Screen::ContaminationCleared`] row beside it — including the
    /// unlatch's ([`unlatch_clear_row`]). "Cleared" and "never contaminated" are
    /// therefore distinguishable in the feed even though the live view is
    /// identical, which is the point.
    contaminated: bool,
    /// Step 4: the **one-shot arm** — the only thing that lets a session
    /// rotation clear [`contaminated`](Self::contaminated).
    ///
    /// Set by [`LatchOverride::AwaitSessionClear`], i.e. by a user who restored
    /// a checkpoint. Consumed by [`observe`](Self::observe) the next time a
    /// changed session id arrives, which clears the contamination bit and
    /// disarms in the same move. Also cleared by an immediate
    /// [`LatchOverride::ClearContamination`], which supersedes it (there is
    /// nothing left to wait for).
    ///
    /// # Why restore does not simply clear, and why the arm is safe
    ///
    /// **Restore is the case where clearing is *least* justified.** Rolling back
    /// files cannot remove injected text from the model's context window, so the
    /// conversation the user is worried about is still running. The UI therefore
    /// tells them to `/clear`, and cImp waits until it sees that happen.
    ///
    /// **And this is the one place a filesystem-derived rotation signal may be
    /// trusted.** H-2's argument is intact: a Claude tab's session id comes from
    /// a directory the model's own Bash can write, so the signal is forgeable.
    /// What changes here is what the signal *decides*. It is not carrying the
    /// decision — the click is. An attacker cannot click restore, so a forged
    /// rotation only helps in the case where the user has **already decided** the
    /// bit should go; the worst outcome is that it lifts slightly earlier than
    /// their actual `/clear`. The signal answers "has the authorised thing
    /// happened yet?", never "should it happen?". H-2's decode proof still gates
    /// it: `observe` only ever sees session ids the live-session registry
    /// published, and that registry takes Claude ids from
    /// `harness::claude::read::LiveSessionGate` (a decoded record naming the session) and
    /// OpenCode ids from a `session.created` on the harness's own event stream.
    ///
    /// # Lifetime
    ///
    /// It lives in the registry entry, so it has the entry's lifetime: it
    /// survives a tab restart (which is itself a rotation, and therefore fires
    /// it) and dies with the process. An app restart drops the whole entry —
    /// contamination included — so an arm outliving one is meaningless by
    /// construction.
    awaiting_session_clear: bool,
    /// #48 (F-23): this tab's `local` latch was put there by the USER's
    /// [`LatchOverride::FlipLocal`] click — it was **not** earned by a
    /// local-capability tool call.
    ///
    /// # Why the position alone is not enough
    ///
    /// `Latch::Local` has two causes and the refusals for them are different
    /// statements. Reached by [`Latch::engage`] it means *"this session read a
    /// file, so the web side closed"*; reached by decision 15's workflow flip it
    /// means *"a human closed the web side and handed local capability back"*.
    /// The web-direction refusal used to serve the first sentence in both cases
    /// ([`toolclass::REFUSAL_NATIVE_WEB_BLOCKED`]), which is F-23: a refusal
    /// stating a cause it did not check. The gate cannot recover that cause from
    /// [`Latch`] — the enum is a position, not a history — so the fact is
    /// recorded here at the one site that performs the flip, beside the
    /// [`outbound::Screen::LatchOverride`] row that records the same act for the
    /// audit trail.
    ///
    /// # Lifetime, and why it cannot outlive its latch
    ///
    /// Set **only** in `apply_override`'s [`LatchOverride::FlipLocal`] arm, and
    /// cleared everywhere the latch leaves `Local`: the [`LatchOverride::Unlatch`]
    /// arm and [`observe`](Self::observe)'s rotation reset. Those are the only
    /// three writes to [`latch`](Self::latch) in this module, so the field cannot
    /// describe a latch position that is no longer in force — a stale `true` on a
    /// re-latched `local` would attribute a tool call's latch to the user, which
    /// is F-23 with the operands swapped.
    ///
    /// It is deliberately **not** a "the user touched this tab" flag: `Unlatch`,
    /// `ClearContamination` and `AwaitSessionClear` are user actions too and none
    /// of them leaves the latch `local`. What this answers is exactly one
    /// question — *why is the web side closed?* — and it is read by exactly one
    /// consumer, the native-web direction of the Phase H gate, through
    /// [`LatchView::local_by_user_flip`].
    local_by_user_flip: bool,
}

impl TabLatch {
    /// A brand-new, uncontaminated entry. One constructor so a field added
    /// later cannot be initialized two different ways at the two sites that
    /// create rows (`gate` and the Phase F `beacon`).
    fn fresh() -> Self {
        TabLatch {
            session: None,
            latch: Latch::Open,
            budget: Budget::default(),
            latch_flagged: false,
            beacon_flagged: false,
            contaminated: false,
            awaiting_session_clear: false,
            local_by_user_flip: false,
        }
    }

    /// The user-facing projection of this entry (Phase F): what the badge
    /// shows and which override buttons the popover may enable.
    fn view(&self) -> LatchView {
        LatchView {
            latch: self.latch.label(),
            contaminated: self.contaminated,
            // Decision 15's two moves, as availability rather than as UI
            // knowledge: the frontend must not re-derive "when is flip legal"
            // from the label, or the rule would live in two places and drift.
            can_flip_local: self.latch == Latch::External,
            can_unlatch: self.latch != Latch::Open,
            // Step 4's two, published on the same principle. `can_clear` is
            // deliberately not `contaminated` spelled twice in TypeScript: the
            // legality rule for a move belongs to the backend even when it is
            // currently one field wide.
            can_clear: self.contaminated,
            awaiting_session_clear: self.awaiting_session_clear,
            // F-23: published rather than re-derived, for the same reason
            // `can_flip_local` is — a consumer that inferred "the user must have
            // flipped it" from `latch == "local" && contaminated` would be
            // guessing, and would be wrong for the tab that fetched a page,
            // latched EXTERNAL and was never flipped at all.
            local_by_user_flip: self.local_by_user_flip,
        }
    }

    /// Step 4: **the one place [`contaminated`](Self::contaminated) is
    /// cleared.** Returns what the tab looked like immediately before, or `None`
    /// when it was not contaminated at all.
    ///
    /// All three authorised paths funnel through here — the user's immediate
    /// resume, the full unlatch (2026-08-10 amendment) and the armed rotation —
    /// so a field that has to be reset alongside the bit cannot be reset on one
    /// path and forgotten on the other.
    ///
    /// # What it resets, and why the two report bits are not optional
    ///
    /// `latch_flagged` and `beacon_flagged` are one-row-per-scope claim bits:
    /// once set, the refusal and beacon rows they gate are never written again
    /// for this tab-session. Leaving them set across a clear would make a
    /// **re-contamination silent** — the tab would take external content again
    /// and the feed would show nothing new, which is exactly the class of bug
    /// #48 fixed for the `Local`-latched beacon. Clearing the bit means this tab
    /// gets to report its containment events afresh.
    ///
    /// (The `contamination` row itself is self-limiting through
    /// [`note_contamination`]'s `mem::replace`, so clearing the bit re-arms that
    /// one automatically — which is what the re-contamination test asserts,
    /// rather than asserting these two booleans directly.)
    ///
    /// # What it deliberately does NOT touch
    ///
    /// * **The latch.** Resume "changes nothing else"; the latch has its own two
    ///   buttons and its own rules. Leaving it where it is can only be the
    ///   tighter choice. (The unlatch path moves the latch too — but it does so
    ///   in its own arm of [`LatchRegistry::apply_override`], *after* this
    ///   function has run, so `PriorTaint::latch` still names the latch the bit
    ///   was released from. This function never touches the latch on any path.)
    /// * **The budget.** Spend is not a report flag — the same reason
    ///   [`LatchRegistry::apply_override`] does not refill it.
    /// * **Quarantined notes.** Locked decision 10 keeps promote-or-discard
    ///   behind the Memory view's own review, which is a separate consent
    ///   surface. Clearing the tab bit stops *future* writes being quarantined;
    ///   notes already held stay held. Nothing in this module can reach them, and
    ///   that is the point.
    fn clear_contamination(&mut self) -> Option<PriorTaint> {
        if !self.contaminated {
            // An arm can only be set on a contaminated tab, but if one ever
            // outlived its bit it would be a trap waiting to fire on the next
            // rotation. Drop it.
            self.awaiting_session_clear = false;
            return None;
        }
        let prior = PriorTaint {
            latch: self.latch.label(),
            armed: self.awaiting_session_clear,
            session: self.session.clone(),
        };
        self.contaminated = false;
        self.awaiting_session_clear = false;
        self.latch_flagged = false;
        self.beacon_flagged = false;
        Some(prior)
    }

    /// Fold the currently-observed session id into this entry, resetting the
    /// latch when the tab's session has demonstrably **rotated**.
    ///
    /// This is what makes the LATCH's scope "the tab's live session" rather
    /// than "the tab": a tab restart starts a new harness session, the V28
    /// registry re-stamps the tab with the new id, and the new conversation is
    /// not denied `read`/`bash` on the strength of the previous one's fetch.
    /// (The tab id itself never rotates — it is config-derived — so keying on
    /// it alone would strand a tab latched until the app restarted.)
    ///
    /// **What a rotation does NOT do (H-2).** It does not clear
    /// [`contaminated`](Self::contaminated). Everything reset here is
    /// permissive state that the next real call re-earns; the contamination bit
    /// is the one fact whose reset an attacker would *want*, and the rotation
    /// signal is derived from a file that attacker can create. See the field's
    /// doc for the full argument and its accepted cost.
    ///
    /// **The one exception, and it is not an exception to that argument (step
    /// 4).** If — and only if — the user has armed
    /// [`awaiting_session_clear`](Self::awaiting_session_clear) by restoring a
    /// checkpoint, this rotation is the awaited event and the bit is cleared
    /// here. H-2 is untouched: an unarmed tab is unchanged by any rotation,
    /// forged or genuine, which is what
    /// `a_session_rotation_resets_the_latch_but_never_the_contamination_bit`
    /// still asserts. The arm is the authority; the rotation only says "yet".
    ///
    /// The three cases, and why `None` is not one of them:
    /// - a *different* session id ⇒ new scope, latch back to [`Latch::Open`];
    /// - the *same* id ⇒ unchanged;
    /// - **no** id ⇒ unchanged, and the stored id is kept. The registry
    ///   withholds a session for reasons that have nothing to do with restarts
    ///   (TTL staleness, the H1 same-root ambiguity, a tab that has not yet
    ///   emitted a session-bearing event). Treating that silence as a restart
    ///   would hand an injected model a trivial latch reset: keep calling until
    ///   the registry blinks.
    #[must_use = "an armed tab clears its contamination bit here and the clear owes the feed a \
                  row — call ContaminationCleared::record() after dropping the registry lock"]
    fn observe(&mut self, session: Option<&str>) -> Option<ClearedOnRotation> {
        let s = session?;
        match self.session.as_deref() {
            Some(prev) if prev == s => None,
            Some(prev) => {
                let prior_session = prev.to_string();
                // Captured before the resets below, because it is what the audit
                // row means by "prior state".
                let prior_latch = self.latch.label();
                self.session = Some(s.to_string());
                self.latch = Latch::Open;
                // #48 (F-23): the latch this described is gone, so the reason it
                // was in that position goes with it. Left set, a `local` latch
                // re-earned by the next conversation's file read would be
                // reported as the user's decision.
                self.local_by_user_flip = false;
                // V32 Phase C: the new conversation gets a fresh budget and a
                // fresh right to report — same scope, same reset.
                self.budget.reset();
                self.latch_flagged = false;
                self.beacon_flagged = false;
                // H-2 (2026-08-08 re-review): `contaminated` is deliberately
                // NOT cleared here — see the field's own doc. A rotation is a
                // claim about a file an attacker can create; it may reopen the
                // latch and refill the budget (both merely permissive, and both
                // re-earned by the next real call), but it may not un-taint a
                // context window.
                //
                // Step 4: unless the USER armed this exact wait. The guard is
                // the whole design — it is checked before anything is cleared,
                // so a rotation into an unarmed tab takes the H-2 path above and
                // nothing else. Deliberately not `if let Some(..) = ..` over
                // `clear_contamination`: that call must not run at all on an
                // unarmed tab, or a later refactor of it could start reaching
                // the bit through this door.
                if !self.awaiting_session_clear {
                    return None;
                }
                let prior = self.clear_contamination()?;
                Some(ClearedOnRotation {
                    prior_latch,
                    prior_session,
                    session: s.to_string(),
                    armed: prior.armed,
                })
            }
            // First sighting: the same scope, only now identified. The latch
            // carries over — calls made before the registry knew the session
            // still happened in this conversation.
            //
            // Not a rotation, so it cannot fire the arm either: "we did not know
            // the id before" is not evidence that the conversation changed. This
            // is the same reading `None` gets above.
            None => {
                self.session = Some(s.to_string());
                None
            }
        }
    }
}

/// What a tab looked like immediately before [`TabLatch::clear_contamination`]
/// released it — the "prior state" every clear's audit row records.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PriorTaint {
    /// [`Latch::label`] at the moment of the clear — i.e. the latch the bit was
    /// released *from*. `clear_contamination` never changes it; on the unlatch
    /// path the caller moves the latch to `Open` immediately afterwards, so this
    /// reads `external`/`local`, which is the state an audit row means by
    /// "prior". It therefore always equals `OverrideOutcome::prior.label()`.
    latch: &'static str,
    /// Whether the one-shot arm was set. False for a false-positive resume of an
    /// un-armed tab; true when the clear is a restore's arm firing.
    armed: bool,
    /// The conversation the tab was in. For the rotation path this is the
    /// *outgoing* session — the contaminated one.
    session: Option<String>,
}

/// A contamination bit cleared inside [`TabLatch::observe`] — the armed
/// one-shot firing on a proved session rotation.
///
/// Returned rather than recorded in place for the reason [`Contamination`] is:
/// the transition happens under the registry mutex and `record_flag` does file
/// I/O. Every caller of `observe` turns this into a
/// [`ContaminationCleared`] with its own [`LatchScope`] — which is also why the
/// scope is not carried here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a contamination clear that is not recorded is an unaudited release of containment"]
struct ClearedOnRotation {
    /// [`Latch::label`] before the rotation reopened it.
    prior_latch: &'static str,
    /// The contaminated conversation that just ended.
    prior_session: String,
    /// The conversation the tab is now in.
    session: String,
    /// Always true — the arm is the only way to reach this type. Carried so the
    /// row builder takes the same `armed` input on both paths.
    armed: bool,
}

/// V32 Phase F: the containment state of one tab, as the badge and the
/// override popover need it. Shared by `/status`, the `latch_status` IPC
/// command and the two Phase F endpoints' replies so all four describe a tab
/// with the same four facts.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatchView {
    /// [`Latch::label`]: `open` / `external` / `local`.
    pub latch: &'static str,
    /// Locked decision 15: has external content entered this conversation at
    /// all? Survives [`LatchOverride::FlipLocal`] and — since H-2 — every
    /// *unarmed* session rotation: the bit is sticky for the tab's registry
    /// entry. It is released only by the three USER actions listed on
    /// [`TabLatch::contaminated`], one of which is
    /// [`LatchOverride::Unlatch`] (2026-08-10 amendment).
    pub contaminated: bool,
    /// Whether "switch to local" applies right now (EXTERNAL-latched only).
    pub can_flip_local: bool,
    /// Whether "restore full access" applies right now (anything but open).
    pub can_unlatch: bool,
    /// Step 4: whether either contamination clear applies right now — i.e.
    /// whether the tab is contaminated at all.
    pub can_clear: bool,
    /// Step 4: whether the user has armed the one-shot clear (they restored a
    /// checkpoint) and cImp is waiting for this tab to start a new harness
    /// session. See [`TabLatch::awaiting_session_clear`].
    ///
    /// Published because the UI has to say *why* a contaminated tab is showing
    /// no "clear now" affordance after a restore, and because step 5's
    /// restore-linked entry point must be able to tell an already-armed tab from
    /// a fresh one without re-deriving the rule.
    pub awaiting_session_clear: bool,
    /// #48 (F-23): whether [`latch`](Self::latch) reads `local` because the USER
    /// flipped it there, rather than because a local-capability tool ran. See
    /// [`TabLatch::local_by_user_flip`].
    ///
    /// Published on `/latch/state` so the OpenCode plugin's web-direction refusal
    /// can serve the constant whose cause it actually checked. It is a fact cImp
    /// recorded when it applied the override, so selecting a message with it is a
    /// lookup — not a message composed from anything a caller supplied.
    ///
    /// `false` for every latch that is not `local`, by construction: the three
    /// writes to the underlying latch keep the two in step.
    pub local_by_user_flip: bool,
}

impl Default for LatchView {
    /// The view of a tab the proxy has never served: nothing latched, nothing
    /// contaminated, no override available.
    fn default() -> Self {
        LatchView {
            latch: Latch::Open.label(),
            contaminated: false,
            can_flip_local: false,
            can_unlatch: false,
            can_clear: false,
            awaiting_session_clear: false,
            local_by_user_flip: false,
        }
    }
}

/// The USER-initiated containment moves — V32 Phase F's two latch moves
/// (locked decision 15), plus step 4's two contamination moves.
///
/// There is still no "latch external": the system does that, and an action that
/// only ever tightens needs no consent surface. What step 4 adds is the *clear*
/// this enum's doc used to say could not exist. H-2's conclusion was that no
/// filesystem-derived **evidence** may un-taint a context window, and that
/// stands; the trust root here is not evidence but **authority** — a human
/// acting in cImp's own UI (see [`TabLatch::contaminated`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatchOverride {
    /// EXTERNAL → Local. Restores the proxied local-capability tools and closes
    /// the external side in the same move.
    FlipLocal,
    /// Anything → Open. Restores both sides; the UI puts a confirmation in
    /// front of it because it recreates the trifecta with injected content
    /// still in the context window.
    ///
    /// **And it clears [`TabLatch::contaminated`]** — decision 15's 2026-08-10
    /// amendment. The click is a verdict, not a workflow step: it already hands
    /// back the strictly more dangerous capability, so quarantining persistent
    /// memory afterwards overruled the user's own decision. On an
    /// uncontaminated tab it clears nothing and is still legal; the clear is a
    /// consequence of the action, not its precondition (contrast
    /// [`ClearContamination`](Self::ClearContamination), whose entire purpose is
    /// the clear, so "nothing to clear" is *its* error).
    Unlatch,
    /// Step 4 — **false-positive resume.** The user has looked at what was
    /// flagged and judged it harmless. Clears the contamination bit now; the
    /// session, the tab and the working tree are untouched (no restart, no
    /// `/clear`, no file written). The UI puts a confirmation in front of it for
    /// the same reason it does for [`Unlatch`](Self::Unlatch): if the judgement
    /// is wrong, a steered model gets its persistence channel back.
    ClearContamination,
    /// Step 4 — **restore.** The user rolled files back to a checkpoint. That
    /// cannot remove injected text from the model's context window, so this
    /// clears **nothing**: it arms
    /// [`TabLatch::awaiting_session_clear`], and the bit lifts only once cImp
    /// observes the tab start a new harness session.
    AwaitSessionClear,
}

impl LatchOverride {
    /// Parse a wire value. An unrecognized action is an **error**, never a
    /// benign default: the actions differ in exactly how much capability they
    /// hand back, so a typo must not pick one.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "flip_local" => Ok(LatchOverride::FlipLocal),
            "unlatch" => Ok(LatchOverride::Unlatch),
            "clear_contamination" => Ok(LatchOverride::ClearContamination),
            "await_session_clear" => Ok(LatchOverride::AwaitSessionClear),
            other => Err(format!(
                "invalid latch override `{other}` — expected one of \"flip_local\", \"unlatch\", \
                 \"clear_contamination\", \"await_session_clear\""
            )),
        }
    }

    /// The canonical wire value, and the `tool` column of the activity row.
    pub fn as_str(self) -> &'static str {
        match self {
            LatchOverride::FlipLocal => "flip_local",
            LatchOverride::Unlatch => "unlatch",
            LatchOverride::ClearContamination => "clear_contamination",
            LatchOverride::AwaitSessionClear => "await_session_clear",
        }
    }
}

/// The result of an applied override: what the latch was, and what the tab
/// looks like now. The prior state is carried because it is the fact the
/// activity row exists to record — "restored full access" means something very
/// different from `external` than from `local`.
#[derive(Debug)]
struct OverrideOutcome {
    prior: Latch,
    /// Step 4: the taint state before the move, for the same reason `prior`
    /// exists — "cleared the contamination flag" is only legible beside what was
    /// there. `Some` **only when this override actually released the bit**:
    /// `clear_contamination`, or an `unlatch` on a contaminated tab (decision
    /// 15's 2026-08-10 amendment). `None` for `flip_local`, for the arm, and for
    /// any move on an uncontaminated tab — which is what
    /// [`unlatch_clear_row`] keys its "write no row" decision on.
    prior_taint: Option<PriorTaint>,
    view: LatchView,
}

/// The result of a native-web beacon (#45): the tab's resulting view, which of
/// the two state changes it caused, and whether it is this tab-session's
/// reportable one (#48).
///
/// `report` is what makes the beacon's audit row bounded, and it is a stored
/// bit ([`TabLatch::beacon_flagged`]) rather than a derived one. A caller that
/// POSTs the route in a loop produces one row per tab-session, not one per
/// request; a feed a caller can flood is a feed that evicts the rows it exists
/// to keep.
///
/// #45 derived that bound from `engaged` alone, which silently dropped a whole
/// class of beacon — see [`contaminated_now`](Self::contaminated_now).
#[derive(Debug, PartialEq, Eq)]
struct BeaconOutcome {
    view: LatchView,
    /// The latch itself MOVED: Open → External. False when it was already
    /// External (sticky, and the fact is unchanged) **and** when the tab was
    /// latched `Local`, where the beacon cannot move it at all.
    engaged: bool,
    /// This beacon is what made the conversation contaminated — the bit went
    /// `false` → `true` here.
    ///
    /// #45 wrote a row only `if engaged`, so a beacon aimed at a `Local`-latched
    /// tab set `contaminated` unconditionally and recorded **nothing**: no row,
    /// no `warn!`, no `info!`. From that moment every `context_note` in the tab
    /// is quarantined and every external result enveloped, with the only
    /// evidence being the quarantine rows of the *later* writes. Locked decision
    /// 15 is unmoved — this records that the bit was SET, and nothing here or
    /// anywhere else clears it.
    contaminated_now: bool,
    /// Whether the handler should write this beacon's `injection_flag` row:
    /// something changed (`engaged || contaminated_now`) **and** this
    /// tab-session has not reported a beacon yet.
    report: bool,
}

impl BeaconOutcome {
    /// Nothing was touched: no policy in force, or no scope to engage.
    fn inert() -> Self {
        BeaconOutcome {
            view: LatchView::default(),
            engaged: false,
            contaminated_now: false,
            report: false,
        }
    }
}

/// Which tool-serving route a gate call is running for. The two differ in one
/// respect only: what an [`ToolClass::External`] classification *means* there.
///
/// `pub(super)` since #48 because the **worker** needs the same distinction and
/// was making the decision without it (review finding A-1). One definition of
/// the rule, in the module that first got it right, rather than a second copy
/// in `agent.rs` that can drift from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LatchRoute {
    /// A proxied `<server>__<tool>` id: `/mcp/call`, and the worker's
    /// MCP-host branch. Every name here is namespaced and therefore EXTERNAL
    /// by the Phase A unknown-⇒-EXTERNAL invariant; this route is the
    /// untrusted-content intake.
    Proxied,
    /// A cImp-native bare name: `/graph_run`'s `graph_*` / `context_*` tools,
    /// and the worker's native dispatch. This route physically cannot serve a
    /// proxied server's content, so a name that classifies EXTERNAL here is not
    /// external content at all: it is a typo or a hallucination that dispatch
    /// will reject as unknown. Letting it *engage* the latch would let one bad
    /// tool name poison a scope for its whole session — so on this route
    /// EXTERNAL neither latches nor is refused, and dispatch answers with its
    /// own error.
    Native,
    /// A **cImp-initiated hook**: the three `/context/*` shim routes (#48,
    /// finding M-7). Like [`LatchRoute::Native`] in what an EXTERNAL
    /// classification means — these routes serve fixed, cImp-owned names and
    /// physically cannot carry a proxied server's content — and different in
    /// exactly one respect, which is the whole reason the variant exists:
    /// **a hook may be REFUSED by a latch but must never MOVE one.**
    ///
    /// The calls arriving here are not tool calls. `PreToolUse`/`PreCompact`/
    /// `PostToolUse` fire automatically, for cImp's own automation, over work
    /// the harness has *already* permitted. Letting them engage would latch
    /// every tab with the read advisor or auto-check enabled to `Local` at its
    /// first read or edit, and every proxied web/MCP tool would be refused from
    /// that moment for a choice the model never made. The latch records what a
    /// CONVERSATION elected to do; cImp advising on a read the model had
    /// already been granted is not the conversation electing anything.
    ///
    /// It still *reads* the latch, because the other direction is the one M-7
    /// is about: under an EXTERNAL latch cImp must not execute the project's
    /// configured checks, or hand back repo source text, on behalf of a
    /// conversation that has ingested untrusted content.
    Hook,
    /// **V39 Phase B: a cross-harness delegation** (`POST /delegate`).
    ///
    /// The third point in the route space, and it needs its own variant
    /// because it is the one combination the other three cannot express:
    ///
    /// | | name comes from | may be REFUSED | may MOVE the latch |
    /// |---|---|---|---|
    /// | [`Proxied`](Self::Proxied) | the model | yes | yes |
    /// | [`Native`](Self::Native) | the model | yes | yes |
    /// | [`Hook`](Self::Hook) | **cImp** | yes | **no** |
    /// | `Delegation` | **cImp** | yes | **yes** |
    ///
    /// *Name from cImp*, like a hook: the model names `delegate_task_<harness>`
    /// on the child, which resolves the harness id and forwards THAT — the
    /// route states its own class-table identity ([`DELEGATE_TOOL`]) and takes
    /// no tool name from the request. So M-2's "in the table but not
    /// dispatchable" wave-through must not apply here, exactly as it must not
    /// on `Hook`: the name is the route, not a hallucination dispatch will
    /// reject.
    ///
    /// *Elective, unlike a hook*: a hook fires automatically over work the
    /// harness already permitted, which is why it must not latch. A
    /// `delegate_task_*` call is the conversation choosing to hand work to a
    /// peer and take its answer back — as elective as `offload_task`, and
    /// latching for the same reason.
    Delegation,
}

impl LatchRoute {
    /// The route a tool name arrives on, by the one convention both dispatchers
    /// use: a namespaced `<server>__<tool>` id is proxied, a bare name is
    /// native (`agent.rs::HostRouter::call`, `mcp_host::call_for_consumer`).
    ///
    /// Never answers [`LatchRoute::Hook`] or [`LatchRoute::Delegation`]: both
    /// are properties of the ROUTE and not of the name, so those handlers state
    /// it themselves.
    pub(super) fn of_tool(name: &str) -> Self {
        if name.contains("__") {
            LatchRoute::Proxied
        } else {
            LatchRoute::Native
        }
    }

    /// Whether an admitted call on this route may **move** the scope's latch.
    ///
    /// `false` only on [`LatchRoute::Hook`] — see that variant. A separate axis
    /// from [`external_is_content`](Self::external_is_content), deliberately: a
    /// hook has to be classified and gated (so it can be refused) without being
    /// elective (so it must not latch). [`LatchRoute::Delegation`] shares the
    /// hook's *name-from-cImp* property and not this one, which is exactly why
    /// it is a fourth variant rather than a reuse of either neighbour.
    pub(super) fn engages(self) -> bool {
        self != LatchRoute::Hook
    }

    /// Whether an [`ToolClass::External`] classification on this route really
    /// means **external content**.
    ///
    /// `false` on [`LatchRoute::Native`], [`LatchRoute::Hook`] and
    /// [`LatchRoute::Delegation`] is the whole rule, and it is not a
    /// weakening of the unknown-⇒-EXTERNAL invariant: every proxied id contains
    /// `__` by construction, so the restrictive default still governs every
    /// name that can carry external content. What it excludes is the name that
    /// cannot — a misspelled `graph_symbols`, which is a hallucination, not a
    /// page.
    pub(super) fn external_is_content(self) -> bool {
        self == LatchRoute::Proxied
    }

    /// **Whether a gated call could actually EXECUTE on this route** — i.e.
    /// whether there is anything for the latch to be about. `false` means the
    /// gate must return without refusing and without moving the latch, and let
    /// the dispatcher answer with its own unknown-tool error.
    ///
    /// Two rules, one predicate, because they are the same principle applied to
    /// the two ways a name can fail to name a tool on a native route (#48,
    /// findings A-1 and M-2):
    ///
    /// 1. **Not in the table** — `class == External` on a route that cannot
    ///    carry a proxied server's content
    ///    ([`external_is_content`](Self::external_is_content)). A misspelled
    ///    `graph_symbols` is a hallucination, not a page.
    /// 2. **In the table but not dispatchable**
    ///    ([`toolclass::dispatchable`]). Six names are classified for reasons
    ///    other than being callable — the three `/context/*` hook routes' fixed
    ///    identities and Claude's own `Edit`/`Write`/`Bash`. Before this, a
    ///    model emitting the bare name `hook_post_edit` or `Bash` on
    ///    [`LatchRoute::Native`] classified LOCAL-CAPABILITY and latched the
    ///    scope to `Local` **before** dispatch rejected the name: the A-1 harm
    ///    (one bad tool name costs a scope the other half of its tools) in the
    ///    direction A-1's fix did not cover.
    ///
    /// Rule 2 is deliberately confined to [`LatchRoute::Native`], the only
    /// route whose name is model-supplied. [`LatchRoute::Hook`]'s name is
    /// composed by cImp and *is* the route's identity — applying rule 2 there
    /// would wave through the three hook routes M-7 exists to gate — and
    /// [`LatchRoute::Proxied`]'s names are the MCP host's to reject.
    ///
    /// This is not a weakening of unknown-⇒-EXTERNAL: it never admits a name
    /// into a *less* restrictive class, it only declines to record taint for a
    /// call that never runs. The containment question — may this class run at
    /// all — is still [`Latch::refusal`]'s, and every name that answers `true`
    /// here still faces it.
    pub(super) fn can_execute(self, name: &str, class: ToolClass) -> bool {
        if class == ToolClass::External && !self.external_is_content() {
            return false;
        }
        self != LatchRoute::Native || toolclass::dispatchable(name)
    }
}

/// What the calling ROUTE knows about a gated call that the registry cannot see
/// (#48, finding F-3): who asked for it, and — when the call is an intake —
/// where the content it is about to bring back is coming from.
///
/// Every field here is one the [`Screen::Contamination`](outbound::Screen)
/// row needs and the [`LatchRegistry`] has no way to derive. The registry owns
/// per-tab state; it does not see request bodies, does not parse tool
/// arguments, and cannot tell an IPC command from a loopback POST.
///
/// **Required at every call site, not defaulted.** The same rule
/// [`outbound::Flag::origin`] is under, for the same reason: #45 found that a
/// provenance column behind a defaulting constructor lets a new call site
/// inherit "cImp decided this" by writing nothing, which is the exact shape of
/// omission these rows exist to prevent. A native route states
/// [`CallProvenance::internal`] — with no URL, because a native route cannot
/// carry a fetched page — as a decision rather than by omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CallProvenance<'a> {
    /// Who asked for the state change a resulting row records.
    origin: outbound::Origin,
    /// The URL the call is fetching, when the route can see one. `/mcp/call`
    /// reads it out of the tool arguments (`detection::origin_of`) — the same
    /// pair its SSRF and detection rows carry, so a contamination row and the
    /// screen rows about the same call name the same page.
    url: Option<&'a str>,
    /// That URL's host — the at-a-glance column.
    host: Option<&'a str>,
    /// #48 F-16: the project this call runs against, in
    /// [`crate::activity::root_key`] form, for a row the registry writes when it
    /// has no scope to take one from ([`unattributed_write`]). `None` where the
    /// route genuinely has no project in view — `/latch/beacon` and the IPC
    /// override are about a tab, not a directory.
    ///
    /// Why the ROUTE and not the registry: [`LatchScope::root`] comes from the
    /// TAB (`tab_root_key`, F-3) precisely so a request body cannot redirect a
    /// contamination row. This field is the case where there IS no tab, and the
    /// only project in view is the one the call is about to write into — the same
    /// one the call's own `kind:"graph"` row files under, resolved by the same
    /// function (`GraphService::graph_root_key`), so the two rows for one call
    /// cannot name different projects.
    root: Option<&'a str>,
}

impl<'a> CallProvenance<'a> {
    /// cImp's own dispatch, executing a call it was already running, with no
    /// fetched content in view and no project in view either. Native routes that
    /// are about a TAB rather than a directory.
    const fn internal() -> Self {
        CallProvenance {
            origin: outbound::Origin::Internal,
            url: None,
            host: None,
            root: None,
        }
    }

    /// cImp's own dispatch on a native route that knows which project the call
    /// runs against (`/graph_run`). See [`Self::root`].
    const fn internal_in(root: &'a str) -> Self {
        CallProvenance {
            origin: outbound::Origin::Internal,
            url: None,
            host: None,
            root: Some(root),
        }
    }

    /// cImp's own dispatch over the proxied intake, naming the page it is
    /// about to read (either half may be absent — a search tool has arguments
    /// but no URL).
    ///
    /// No `root`: a PERSISTENT-WRITE cannot arrive on `/mcp/call` (every
    /// namespaced id classifies EXTERNAL), so the one row that reads
    /// [`Self::root`] is unreachable from here and a root passed in would be
    /// speculative.
    fn intake(url: Option<&'a str>, host: Option<&'a str>) -> Self {
        CallProvenance {
            origin: outbound::Origin::Internal,
            url,
            host,
            root: None,
        }
    }

    /// A loopback POST from a local process — the native-web beacon. Marked
    /// [`outbound::Origin::Http`] because the launch token is readable by
    /// anything running as this user, so a beacon is never evidence that the
    /// user acted (#45).
    const fn http() -> Self {
        CallProvenance {
            origin: outbound::Origin::Http,
            url: None,
            host: None,
            root: None,
        }
    }
}

/// The per-tab-session taint latches for the tools this proxy serves.
///
/// Locked decision 3: consumer enforcement lives here, keyed by V28 tab
/// identity. Two asymmetries with the worker's latch are deliberate:
///
/// - **Refusal, not def removal.** The worker rebuilds its advertised tool list
///   every turn, so decision 2's def removal is available to it. Consumers
///   cache `tools/list` at connect (the long-standing OpenCode behaviour that
///   forces a tab restart after MCP flag changes, and Claude does the same), so
///   removing a def mid-session would not be seen. The fixed-string refusals
///   from [`toolclass`] are the whole enforcement here.
/// - **Only the tools this proxy serves.** Claude's native Read/Bash and
///   OpenCode's bash/write never route through cImp, so no latch of ours can
///   reach them (decision 3's honest limit; OS containment is V33, optional
///   hook gating is Phase E).
#[derive(Default)]
struct LatchRegistry {
    tabs: Mutex<HashMap<(&'static str, String), TabLatch>>,
}

/// V32 Phase G (locked decision 16): the two feature switches one gated call
/// resolves, snapshotted by the handler that owns the settings read.
///
/// They are separate because they *are* separate features and can be switched
/// independently — and the interesting combination is the asymmetric one:
/// latch off + quarantine on still tracks contamination (so a note written
/// after a fetch is still held for review) while refusing nothing. The registry
/// takes them as data rather than reading settings itself, so the whole gate
/// stays a pure decision over one lock and one snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GatePolicy {
    /// [`Feature::TaintLatch`](crate::settings::injection::Feature::TaintLatch)
    /// — engagement, refusals and the latch shown in `/status`.
    latch: bool,
    /// [`Feature::MemoryQuarantine`](crate::settings::injection::Feature::MemoryQuarantine)
    /// — whether a PERSISTENT-WRITE from a contaminated conversation is stored
    /// held-for-review.
    quarantine: bool,
}

impl GatePolicy {
    /// Resolve both switches for one tab scope. `None` scope ⇒ the unknown
    /// caller's answer, the same fail-open reading `Scope::for_tab` takes
    /// (#48 F-35: `Scope::UnknownCaller` is what `Scope::App` was called at this
    /// site before locked decision 36; no behaviour moved).
    fn resolve(settings: &crate::settings::Settings, scope: Option<&LatchScope>) -> Self {
        use crate::settings::injection::{effective, Feature, Scope};
        let s = scope.map_or(Scope::UnknownCaller, LatchScope::injection);
        GatePolicy {
            latch: effective(Feature::TaintLatch, s, settings),
            quarantine: effective(Feature::MemoryQuarantine, s, settings),
        }
    }

    /// Neither control applies — nothing to decide, nothing to record.
    fn inert(self) -> bool {
        !self.latch && !self.quarantine
    }
}

/// V32 Phase G: read the app's live settings from managed state.
///
/// Every gated loopback handler already holds an `AppHandle`; this is the one
/// place that turns it into a `Settings`, so a handler cannot accidentally
/// resolve the hierarchy against a different snapshot than its neighbour. The
/// fallback is `Settings::default()` — all protection ON — because a request
/// arriving before managed state is up must not be the moment containment
/// silently lapses.
pub(crate) fn live_settings(app: &AppHandle) -> crate::settings::Settings {
    app.try_state::<crate::ipc::AppState>()
        .map(|s| s.settings.current())
        .unwrap_or_default()
}

/// One contamination TRANSITION, ready to record — see [`note_contamination`].
///
/// Owned rather than borrowed because the transition is detected under the
/// registry mutex and the row is written after it is dropped: `record_flag`
/// goes through `activity::record_bg`, whose contract is that it does file I/O
/// (inline off a tokio runtime), and holding a lock across that would put the
/// store's I/O on the critical path of every other tab's gated call.
struct Contamination {
    origin: outbound::Origin,
    consumer: &'static str,
    /// `agent:tab` — [`LatchScope::label`], the same convention every other
    /// V32 row uses.
    scope: String,
    /// The conversation, when the registry entry knows it.
    session: Option<String>,
    tool: String,
    url: Option<String>,
    host: Option<String>,
    root: String,
    detail: String,
}

impl Contamination {
    /// Write the row. Fire-and-forget, like every other `record_flag` call on
    /// these paths: recording an event must not be able to fail the call it
    /// observes.
    fn record(self) {
        info!(
            target: "offload",
            consumer = self.consumer,
            scope = %self.scope,
            tool = %self.tool,
            host = self.host.as_deref().unwrap_or(""),
            root = %self.root,
            "loopback: V32 conversation became contaminated"
        );
        outbound::record_flag(outbound::Flag {
            screen: outbound::Screen::Contamination,
            origin: self.origin,
            consumer: self.consumer,
            scope: &self.scope,
            // #48 F-29: derived, because this row's `scope` was built by
            // `LatchScope::label` (or, with no tab identity, by the `/mcp/call`
            // route's honest `"{agent}:(no tab identity)"`) — the two inputs
            // `scope_attribution` is defined over. The struct carries the label
            // rather than the scope, so there is no tab field to read here.
            attribution: outbound::scope_attribution(&self.scope),
            session: self.session.as_deref(),
            tool: &self.tool,
            host: self.host.as_deref(),
            url: self.url.as_deref(),
            resolved_ip: None,
            canary: false,
            root: self.root.clone(),
            detail: &self.detail,
        });
    }
}

/// Step 4: on whose reasoning a contamination bit was released. The row's
/// `basis`, and the word that makes the audit trail legible — "the user said it
/// was a false positive" and "the user restored and then started a new session"
/// are very different claims about what is in the model's context window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClearBasis {
    /// [`LatchOverride::ClearContamination`] — the user judged the flagged
    /// content harmless. Nothing else changed.
    Resume,
    /// [`LatchOverride::AwaitSessionClear`] armed the tab after a restore, and
    /// cImp has now observed a new harness session.
    Restore,
    /// [`LatchOverride::Unlatch`] — decision 15's 2026-08-10 amendment. The user
    /// restored FULL access, and the flag went with it. A third basis rather
    /// than a reuse of [`Resume`](Self::Resume) because the two are different
    /// claims about what the user decided: `Resume` says *"that content was
    /// harmless"*, this says *"I am taking the whole risk knowingly"* — and an
    /// incident reviewer who cannot tell them apart cannot reconstruct the
    /// decision.
    Unlatch,
}

impl ClearBasis {
    /// The row's at-a-glance `tool` column. These rows have no tool call behind
    /// them; what happened is the fact worth reading.
    fn tool(self) -> &'static str {
        match self {
            ClearBasis::Resume => LatchOverride::ClearContamination.as_str(),
            ClearBasis::Restore => "session_clear_observed",
            // `"unlatch"` is also the `tool` column of the tab's
            // `latch_override` row. The two are told apart by `screen`, and
            // `contamination_events()` reads only the two contamination lanes,
            // so nothing joins them by accident — the shared word is a feature.
            ClearBasis::Unlatch => LatchOverride::Unlatch.as_str(),
        }
    }
}

/// One clearing of [`TabLatch::contaminated`], ready to record — the exact
/// counterpart of [`Contamination`], down to being owned rather than borrowed so
/// the row is written after the registry lock is dropped.
///
/// **Both authorised paths build one**, which is what stops the two from
/// describing the same state change differently: the immediate resume in
/// [`LatchRegistry::apply_override`], and the armed rotation firing inside
/// [`TabLatch::observe`] (via [`ClearedOnRotation::into_row`]).
struct ContaminationCleared {
    /// Who acted *now*. [`outbound::Origin::Ipc`] for the resume — a human in
    /// the app's own UI. [`outbound::Origin::Internal`] for the armed rotation:
    /// the authority was the earlier click (which has its own
    /// [`outbound::Screen::LatchOverride`] row), but the act recorded here is
    /// cImp's own observation, and `Ipc` means "a human did *this*".
    origin: outbound::Origin,
    basis: ClearBasis,
    consumer: &'static str,
    /// `agent:tab` — [`LatchScope::label`].
    scope: String,
    /// The conversation the row is filed under: the one contamination was
    /// cleared *for*. For the rotation path that is the OUTGOING session, not
    /// the new one — the new one was never contaminated, and filing it there
    /// would break a join against the `contamination` row that opened the
    /// lifecycle.
    session: Option<String>,
    root: String,
    detail: String,
}

impl ContaminationCleared {
    /// Write the row. Fire-and-forget, like every other `record_flag` call on
    /// these paths.
    fn record(self) {
        warn!(
            target: "offload",
            consumer = self.consumer,
            scope = %self.scope,
            basis = self.basis.tool(),
            origin = self.origin.as_str(),
            root = %self.root,
            "loopback: V32 contamination flag cleared on the user's authority"
        );
        outbound::record_flag(outbound::Flag {
            screen: outbound::Screen::ContaminationCleared,
            origin: self.origin,
            consumer: self.consumer,
            scope: &self.scope,
            // Derived from the label this row was built with — see the
            // contamination row above (#48 F-29).
            attribution: outbound::scope_attribution(&self.scope),
            session: self.session.as_deref(),
            tool: self.basis.tool(),
            host: None,
            url: None,
            resolved_ip: None,
            canary: false,
            root: self.root.clone(),
            detail: &self.detail,
        });
    }

    /// Record a clear that may not have happened. Every `observe` call site
    /// funnels through this, so "the arm fired here" needs no branch of its own
    /// at five sites.
    fn record_from(cleared: Option<ClearedOnRotation>, scope: &LatchScope) {
        if let Some(ev) = cleared {
            ev.into_row(scope).record();
        }
    }
}

/// The sentence an incident reviewer reads when the bit was released —
/// composed **once**, for both paths.
///
/// Written as one function rather than two format strings because the whole
/// point of the pair is that they say the same things about different bases: the
/// prior state, what was and was not restored by the clear, and — the part a
/// reviewer most needs — that quarantined notes are untouched by it.
fn clear_detail(
    basis: ClearBasis,
    origin: outbound::Origin,
    prior_latch: &str,
    prior_session: Option<&str>,
    new_session: Option<&str>,
) -> String {
    let how = match basis {
        ClearBasis::Resume => "the user judged the flagged content harmless and cleared the flag \
                               from the taint popover. The session, the tab and the working tree \
                               were not touched"
            .to_string(),
        ClearBasis::Unlatch => "the user restored FULL access from the taint popover (`unlatch`), \
                                and decision 15's 2026-08-10 amendment releases the flag with it: \
                                that click already hands back the strictly more dangerous \
                                capability — read AND web with the injected content still in the \
                                context window — so quarantining persistent memory afterwards \
                                would overrule the judgement it just asked for. An attacker \
                                cannot click it; the trust root is authority, not evidence. The \
                                session, the tab and the working tree were not touched"
            .to_string(),
        ClearBasis::Restore => format!(
            "the user restored a checkpoint, which armed a ONE-SHOT clear (a restore rolls back \
             files and cannot remove injected text from a context window, so the flag was kept), \
             and cImp has now observed this tab start a new harness session{}. The arm is the \
             authority here; the rotation only answers \"has it happened yet\", which is why a \
             forgeable rotation signal is acceptable for it and for nothing else",
            match new_session {
                Some(s) => format!(" ({s})"),
                None => String::new(),
            }
        ),
    };
    // The one sentence the three bases cannot share: two of them leave the latch
    // exactly where it was, and the third IS a latch move.
    let latch_note = match basis {
        ClearBasis::Unlatch => "The same click also moved the latch to `open` — that is what \
                                released the flag, and it is recorded as its own `latch_override` \
                                row.",
        ClearBasis::Resume | ClearBasis::Restore => {
            "The latch itself is unchanged by this and keeps its own controls."
        }
    };
    format!(
        "CONTAMINATION CLEARED (basis: {}, origin: {}): {how}. Prior state: contaminated=true, \
         latch={prior_latch}, session={}. {latch_note} Memory notes already quarantined STAY \
         quarantined — promoting or discarding them is the Memory view's own review (locked \
         decision 10), a separate consent surface. What changes is that this tab's future \
         persistent writes are stored clean again, and that a fresh contamination will report \
         itself as a new transition.",
        basis.tool(),
        origin.as_str(),
        prior_session.unwrap_or("unknown"),
    )
}

impl ClearedOnRotation {
    /// Turn a lock-side clear into the row its caller's scope can file.
    fn into_row(self, scope: &LatchScope) -> ContaminationCleared {
        debug_assert!(self.armed, "only an armed tab can clear on a rotation");
        ContaminationCleared {
            // NOT `Ipc`: see the field's doc. The click that authorised this
            // happened earlier and was recorded then.
            origin: outbound::Origin::Internal,
            basis: ClearBasis::Restore,
            consumer: scope.agent,
            scope: scope.label(),
            session: Some(self.prior_session.clone()),
            root: scope.root.clone(),
            detail: clear_detail(
                ClearBasis::Restore,
                outbound::Origin::Internal,
                self.prior_latch,
                Some(&self.prior_session),
                Some(&self.session),
            ),
        }
    }
}

/// **The one place a conversation is marked contaminated** (#48, finding F-3).
///
/// Both paths that can set the bit — an admitted proxied EXTERNAL call in
/// [`LatchRegistry::gate`], and the native-web beacon in
/// [`LatchRegistry::beacon`] — flip it *here*, so the transition cannot be set
/// on one path and recorded on the other, and a third path added later gets the
/// row by calling the only function that sets the bit.
///
/// # What it records, and what it deliberately does not
///
/// It records the **transition**, false → true, not every contaminating call.
/// Later EXTERNAL calls restate a fact this row already carries, and each
/// already writes an ordinary proxied-MCP activity row of its own; what had no
/// record at all was the moment the conversation stopped being clean. The
/// `mem::replace` below *is* the claim, so this is self-limiting and needs no
/// separate claim bit of the [`TabLatch::latch_flagged`] kind.
///
/// Because the bit is sticky across session rotations (H-2 — see
/// [`TabLatch::contaminated`]), "once" here means **once per tab**, not once
/// per conversation: a `/clear` in a contaminated tab keeps the taint and
/// writes no second row, and the row's `session` therefore names the
/// conversation contamination started in. A consumer joining these rows to
/// conversation-scoped state has to read them that way.
///
/// It does **not** decide whether the call contaminates. That is the caller's
/// classification (`gate` calls it only for `ToolClass::External` on a route
/// where EXTERNAL means content; the beacon calls it unconditionally, because a
/// beacon *is* the harness reporting that it read a page). Nothing about when
/// contamination is SET changes here — this is observability over an unchanged
/// decision.
///
/// # Why the return value must be used
///
/// The detection happens under the registry mutex and the write happens after
/// it is released, so the two are necessarily separate statements. `#[must_use]`
/// is what keeps them from drifting apart: a path that flips the bit and drops
/// the result fails the build under `-D warnings`, which is the same
/// "compile-time or it will be forgotten" posture `declare_screens!` takes for
/// the retention lane.
#[must_use = "a contamination transition that is detected and not recorded is finding F-3 again — \
              call Contamination::record() after dropping the registry lock"]
fn note_contamination(
    entry: &mut TabLatch,
    scope: &LatchScope,
    tool: &str,
    prov: CallProvenance<'_>,
) -> Option<Contamination> {
    if std::mem::replace(&mut entry.contaminated, true) {
        return None;
    }
    Some(Contamination {
        origin: prov.origin,
        consumer: scope.agent,
        scope: scope.label(),
        // The registry entry's session, not the scope's: `observe` has already
        // run by the time any caller reaches here, so this is the session the
        // latch itself considers current — the one a later join has to match.
        session: entry.session.clone(),
        tool: tool.to_string(),
        url: prov.url.map(str::to_string),
        host: prov.host.map(str::to_string),
        root: scope.root.clone(),
        detail: format!(
            "CONTAMINATED: external content entered this conversation via {tool}{}. Nothing was \
             refused — the call was admitted, and this row records the state change it caused. \
             From here on every persistent memory write from this tab is quarantined for review \
             and every external result keeps its spotlighting envelope (latch={}). No \
             filesystem-derived signal clears the bit (H-2: a new harness session is not proof of \
             a new context window, because the model's own shell can forge one) and no HTTP route \
             can. It is cleared only by the USER, from the taint popover — immediately \
             (`clear_contamination`, \"that content was harmless\"), by restoring FULL access \
             (`unlatch`, which accepts the larger risk deliberately), or after a checkpoint \
             restore (`await_session_clear`, effective once cImp observes a new harness session). \
             The workflow flip (`flip_local`) does NOT clear it. Whichever happens, it writes its \
             own `contamination_cleared` row.",
            match prov.host {
                Some(h) => format!(" from {h}"),
                None => String::new(),
            },
            entry.latch.label(),
        ),
    })
}

/// #48 (2026-08-08 re-review), finding M-19 — what [`LatchRegistry::gate`]
/// hands back to a caller it could not attribute to a tab.
///
/// # The asymmetry this closes
///
/// The identity-less fail-open is locked (F-5/H-8) and load-bearing: a child
/// spawned before `--tab` existed, and the documented headless consumers, must
/// keep their TOOL-SERVING routes. Nothing here touches that — every class
/// except one still leaves this function `Clean`, and no latch row is created
/// for any of them.
///
/// PERSISTENT-WRITE is the exception, and the precedent for treating it as one
/// is already in this codebase, one module over: on the **headless** path a
/// write with no identity is refused outright (`graph::mcp::headless_refusal`,
/// `HEADLESS_WRITE_UNAVAILABLE`), on exactly this reasoning — that path has
/// neither a session identity nor a taint verdict, and a note written blind
/// with neither is *"project-wide, permanent, unattributable AND unquarantined,
/// which is the highest-privilege write the memory surface offers"*. The
/// loopback path reached the identical state and stored the note clean. Two
/// paths, the same two missing facts, opposite answers.
///
/// # Why quarantine and not the headless path's refusal
///
/// Locked decision 10. A refusal on this path throws away the legitimate
/// research conclusion the session existed to produce; the quarantine keeps it,
/// flags it, hides it from `context_recall` / `context_notes` / compaction
/// carry-over / the fact distiller, and hands the user promote-or-discard. (The
/// headless path refuses because there is no running app to review a queue in,
/// not because refusal is the better answer.)
///
/// It is [`WriteTaint::Unattributed`] rather than `Quarantined` so the model is
/// told the actual reason — see that variant.
///
/// # Deliberately not gated on `policy.latch`
///
/// Only on `policy.quarantine`, matching the scoped path: locked decision 16
/// keeps the two switches independent, and this is a quarantine decision, not a
/// latch decision. Nothing here reads or moves a latch — there is no scope to
/// move one for, which is the whole point.
///
/// #48 F-16: `prov` is here for one field — [`CallProvenance::root`]. The
/// finding's own wording, *"`LatchRegistry::gate` has no scope to derive a root
/// from"*, is true of `gate` and **false of the route that calls it**: the only
/// route that can reach this line with a PERSISTENT-WRITE is `/graph_run`, and
/// that handler holds the project the note is about to be written into.
fn unattributed_write(
    policy: GatePolicy,
    route: LatchRoute,
    name: &str,
    prov: CallProvenance<'_>,
) -> WriteTaint {
    let class = toolclass::classify(name);
    if !policy.quarantine || class != ToolClass::PersistentWrite || !route.can_execute(name, class)
    {
        return WriteTaint::Clean;
    }
    warn!(
        target: "offload",
        tool = %name,
        "loopback: persistent memory write held — the caller carries no resolvable tab identity"
    );
    // One row per held note, for the same reason the scoped quarantine writes
    // one: each is a separate item in the user's review queue, and a feed that
    // reported only the first would leave later ones discoverable solely by
    // opening the Memory view. There is no scope, so the columns that name one
    // say so rather than guessing — the `consumer` a request body could have
    // supplied is exactly the field M-19 showed is caller-chosen.
    outbound::record_flag(outbound::Flag {
        screen: outbound::Screen::MemoryQuarantine,
        origin: outbound::Origin::Internal,
        consumer: "unattributed",
        scope: "unattributed",
        // #48 F-29 — the reason this field exists. `"unattributed"` is a
        // description, not a scope, and the old derivation (anything without a
        // `:` ⇒ `Headless`) turned it into the positive claim *"a run with no tab
        // behind it"*. This frame does not know that: it knows only that the
        // caller's tab identity did not resolve, which is what
        // `Attribution::Unattributed` says and what this whole row is about.
        //
        // Not more precise than that ON PURPOSE. The route's `LatchScoping`
        // could distinguish "no id was sent" (`Headless`) from "an id that names
        // no configured tab" (`Unrecognized`), but `gate` receives
        // `Option<&LatchScope>` and both collapse to `None` before this line —
        // the same collapse F-20 fixed one seam over. Recovering it means
        // threading the attribution through `CallProvenance`, a separate change;
        // claiming either half from here would be a guess.
        attribution: crate::activity::Attribution::Unattributed,
        session: None,
        tool: name,
        host: None,
        url: None,
        resolved_ip: None,
        canary: false,
        // #48 F-16: the route's project, not an empty string. There is no scope
        // to take a root from — that is this function's whole premise — but the
        // ROUTE knows which project the note is about to be written into, and
        // that is the project a reviewer filters by. `None` would be a route with
        // no project in view, which cannot reach this line today; the fallback is
        // still empty rather than invented, and `""` is a positive claim of
        // ignorance with a documented meaning (see `ActivityEntry::root`).
        root: prov.root.unwrap_or_default().to_string(),
        detail: toolclass::UNATTRIBUTED_WRITE_NOTICE,
    });
    WriteTaint::Unattributed
}

/// #48 (F-34) — pick the refusal that states the cause the gate **checked**,
/// for the one latch position that has two possible causes.
///
/// # What this is, and what it is deliberately not
///
/// It is a **message selector**, not a gate. Containment is decided entirely by
/// [`Latch::proxy_gate`](toolclass::Latch::proxy_gate) before this runs and is
/// byte-identical to what it always was: `Some(_)` in, `Some(_)` out, and the
/// same calls refused. `local_by_user_flip` never joins the guard — for F-13's
/// reason, an unknown value must be able to cost only the better *message* and
/// never the refusal, so a `false` here simply serves the pre-F-34 constant.
///
/// # Why it is here and not in [`Latch::refusal`](toolclass::Latch::refusal)
///
/// **Locked decision 34 places the choice in `LatchRegistry::gate`, which holds
/// `TabLatch::local_by_user_flip`, and NEVER in `Latch::refusal`** — and that is
/// load-bearing rather than stylistic. `Latch::refusal` is a *pure function over
/// [`Latch`](toolclass::Latch)* that the **offload worker** also calls
/// (`offload::agent`), and the worker has no user-flip concept to thread:
/// migrating this down would either break it or force it to pass a meaningless
/// `false` forever. The rule is a convention, not a type, so it is also guarded
/// by a tripwire in `toolclass`
/// (`the_user_flip_constant_is_never_reachable_from_the_pure_latch_functions`).
///
/// Written as a free function taking the bool rather than a `TabLatch` method so
/// the ONE fact it may consult is visible in its signature: nothing from the
/// caller, the model or the tool arguments can reach the string.
///
/// # The match, and why on the constant
///
/// `REFUSAL_EXTERNAL_BLOCKED` is produced by exactly one state — `Latch::Local`
/// blocking [`ToolClass::External`] — so keying on it is equivalent to keying on
/// that pair, and it cannot silently capture a future refusal that means
/// something else. The other two constants are unreachable under a `Local`
/// latch, so they fall through untouched.
fn user_flip_refusal(refusal: &'static str, local_by_user_flip: bool) -> &'static str {
    if local_by_user_flip && refusal == toolclass::REFUSAL_EXTERNAL_BLOCKED {
        // The user's own IPC flip closed the external side. Saying a tool call
        // did it is F-23's defect on the route that ships ON.
        toolclass::REFUSAL_EXTERNAL_USER_LOCAL
    } else {
        refusal
    }
}

impl LatchRegistry {
    /// Decide one call, and engage the latch when it may proceed.
    ///
    /// The whole check-then-engage runs under one lock and **before** the call
    /// executes — loopback serves concurrent requests, so two simultaneous
    /// calls from one tab must not both observe an open latch. A refused call
    /// never engages or flips anything (same property as Phase A's
    /// `latch_gate`): otherwise a hallucinated call to the blocked side could
    /// redefine which side of the boundary the session is on.
    ///
    /// V32 Phase C2: the success arm now carries a [`WriteTaint`]. It is
    /// [`WriteTaint::Quarantined`] for exactly one case — a PERSISTENT-WRITE
    /// under an EXTERNAL latch, which Phase B refused and locked decision 10
    /// turns into a quarantined write — and `Clean` for everything else. The
    /// caller must thread it into the call it is about to make; ignoring it
    /// would store an externally-influenced note as ordinary memory.
    ///
    /// V32 Phase G: `policy` carries the two feature switches (locked decision
    /// 16). With both off the gate returns immediately without touching any
    /// state — a disabled control must leave no trace, not merely no verdict,
    /// or `/status` would keep showing latches the user turned off.
    ///
    /// #48 (F-3): `prov` is what the calling route knows and the registry
    /// cannot derive — see [`CallProvenance`]. It is used for exactly one thing
    /// here: the [`Screen::Contamination`](outbound::Screen) row, written when
    /// an admitted call is the one that stops this conversation being clean.
    fn gate(
        &self,
        scope: Option<&LatchScope>,
        route: LatchRoute,
        name: &str,
        policy: GatePolicy,
        prov: CallProvenance<'_>,
    ) -> Result<WriteTaint, &'static str> {
        if policy.inert() {
            return Ok(WriteTaint::Clean);
        }
        // Fail-open: no tab identity ⇒ no latch (see [`latch_scope`]) — except
        // for the one class where "we do not know who this is" is itself the
        // hazard. See [`unattributed_write`].
        let Some(scope) = scope else {
            return Ok(unattributed_write(policy, route, name, prov));
        };
        let class = toolclass::classify(name);
        // #48, findings A-1 and M-2: a call that cannot execute on this route
        // is not evidence of anything, so it neither latches nor is refused —
        // see [`LatchRoute::can_execute`] for both rules and why this does not
        // weaken unknown-⇒-EXTERNAL.
        if !route.can_execute(name, class) {
            return Ok(WriteTaint::Clean);
        }
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = tabs.entry(scope.key()).or_insert_with(TabLatch::fresh);
        // Step 4: for a Claude tab this is the usual place an armed one-shot
        // fires — the first cImp tool call of the conversation that followed the
        // user's `/clear`. Recorded on every exit below, before this call's own
        // rows, so the feed reads in the order the state actually moved.
        let rotated = entry.observe(scope.session.as_deref());
        // V32 Phase F: quarantine keys on CONTAMINATION, not on the latch
        // position. `Latch::External` always implies `contaminated` (both are
        // set by the same admitted call, a few lines below), so this only ever
        // WIDENS the pure-latch verdict `proxy_gate` computes — to the one case
        // decision 15 creates, where a user override moved the latch off
        // External on a conversation that has already read external content.
        // The pure function stays the single definition of the latch's own
        // semantics; the bit is layered over it here, at the only site that
        // owns per-conversation state.
        //
        // V32 Phase G layers the two switches over the same expression, and the
        // order is the whole point: the latch's verdict is computed only when
        // the latch feature is on, and the quarantine verdict only when the
        // quarantine feature is on. So "latch off, quarantine on" still holds a
        // note written after a fetch (contamination is tracked below regardless
        // of the latch), and "latch on, quarantine off" refuses the same calls
        // it always did while storing writes clean.
        let latched = if policy.latch {
            entry.latch.proxy_gate(class)
        } else {
            ProxyGate::Proceed(WriteTaint::Clean)
        };
        let decision = match latched {
            ProxyGate::Proceed(WriteTaint::Clean)
                if policy.quarantine
                    && class == ToolClass::PersistentWrite
                    && entry.contaminated =>
            {
                ProxyGate::Proceed(WriteTaint::Quarantined)
            }
            ProxyGate::Proceed(WriteTaint::Quarantined | WriteTaint::Unattributed)
                if !policy.quarantine =>
            {
                ProxyGate::Proceed(WriteTaint::Clean)
            }
            other => other,
        };
        let refusal = match decision {
            // `Unattributed` cannot arrive here — it is [`unattributed_write`]'s
            // answer, returned above this frame's `scope` binding, and
            // `proxy_gate` never produces it. It is bound rather than excluded
            // so that a future path which does route one through holds the note
            // (and explains it correctly) instead of falling into the `Clean`
            // arm below.
            ProxyGate::Proceed(held @ (WriteTaint::Quarantined | WriteTaint::Unattributed)) => {
                // Locked decision 10: store it, flag it, hold it for the user.
                // The write itself never latches (PERSISTENT-WRITE is not a
                // latching class), so nothing about the scope changes here.
                warn!(
                    target: "offload",
                    agent = scope.agent,
                    tab = %scope.tab,
                    tool = %name,
                    latch = entry.latch.label(),
                    "loopback: persistent memory write quarantined by the V32 session taint latch"
                );
                // Unlike the refusal below this is NOT one-row-per-scope: each
                // quarantined note is a separate item in the user's review
                // queue, and a feed that reported only the first would leave
                // later ones discoverable solely by opening the Memory view.
                drop(tabs);
                ContaminationCleared::record_from(rotated, scope);
                outbound::record_flag(outbound::Flag {
                    screen: outbound::Screen::MemoryQuarantine,
                    origin: outbound::Origin::Internal,
                    consumer: scope.agent,
                    scope: &scope.label(),
                    // #48 F-29: the scope is in hand, so the tab id comes from
                    // its field rather than from a re-split label.
                    attribution: scope.attribution(),
                    session: scope.session.as_deref(),
                    tool: name,
                    host: None,
                    url: None,
                    resolved_ip: None,
                    canary: false,
                    root: scope.root.clone(),
                    detail: held
                        .write_notice()
                        .unwrap_or(toolclass::QUARANTINE_WRITE_NOTICE),
                });
                return Ok(held);
            }
            ProxyGate::Proceed(WriteTaint::Clean) => None,
            // #48 (F-34): the message, and ONLY the message. `decision` above is
            // untouched, so what gets refused is byte-identical to what always
            // did — this arm just picks which fixed constant states the cause,
            // from a fact only this frame holds. See [`user_flip_refusal`].
            ProxyGate::Refuse(r) => Some(user_flip_refusal(r, entry.local_by_user_flip)),
        };
        if let Some(refusal) = refusal {
            warn!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %name,
                latch = entry.latch.label(),
                "loopback: tool call refused by the V32 session taint latch"
            );
            // V32 Phase C: Phase B left this refusal without a consumer — the
            // user could only see it as a tool that mysteriously stopped
            // working. One row per scope (see `TabLatch::latch_flagged`).
            let first = !std::mem::replace(&mut entry.latch_flagged, true);
            drop(tabs);
            ContaminationCleared::record_from(rotated, scope);
            if first {
                outbound::record_flag(outbound::Flag {
                    screen: outbound::Screen::LatchRefusal,
                    origin: outbound::Origin::Internal,
                    consumer: scope.agent,
                    scope: &scope.label(),
                    // The scope is in hand — see `LatchScope::attribution`.
                    attribution: scope.attribution(),
                    session: scope.session.as_deref(),
                    tool: name,
                    host: None,
                    url: None,
                    resolved_ip: None,
                    canary: false,
                    root: scope.root.clone(),
                    detail: refusal,
                });
            }
            return Err(refusal);
        }
        // V32 Phase F: the call is admitted, so if it is EXTERNAL its content is
        // about to enter this conversation. Set the contamination bit HERE
        // rather than deriving it from the latch, because the latch is now
        // user-movable and the bit is not. (A refused call never reaches this
        // point, so a hallucinated call to the blocked side cannot contaminate
        // a clean session — the same property `engage` has.)
        //
        // V32 Phase G: tracked whenever EITHER switch is on (an inert policy
        // returned above), because contamination is the quarantine's input as
        // much as the latch's — a user who keeps quarantine but drops the latch
        // still needs "this conversation read a page" to be true.
        //
        // #48 (F-3): and the TRANSITION is recorded, through the one function
        // that owns the bit. This was the finding's whole substance — the line
        // this replaces set the bit silently, and the only trace was the `info!`
        // below, which fires on the *latch* transition. A tab already latched
        // `Local`, or one running with the latch feature off and the quarantine
        // on, contaminated with no timestamp, no tool and no row. The condition
        // is unchanged, deliberately: recording must follow the same rule the
        // bit does, or the switch combination that made it silent still would.
        //
        // Engagement is the LATCH's own state, so it moves only while the latch
        // feature is on: a latch shown as engaged in `/status` while the feature
        // is off would describe a boundary that is not being enforced. It is
        // sequenced ahead of the contamination note for one reporting reason —
        // the row quotes the latch this call leaves the tab in, so a fresh tab's
        // contamination row must say `external` rather than the `open` it was a
        // microsecond earlier. Nothing can observe the entry between the two
        // (both run under the one lock), so the order is a choice about the row,
        // not about the semantics.
        //
        // #48 (M-7): …and only on a route whose calls are ELECTIVE. A
        // [`LatchRoute::Hook`] call is cImp's own automation firing over work
        // the harness already permitted, so it reads the latch (it can be
        // refused, three lines up) and never moves it. `engages()` is checked
        // here rather than inside `Latch::engage` because it is a fact about
        // the route, not about the class.
        let engaged = policy.latch && route.engages() && entry.latch.engage(class);
        let contamination = if class == ToolClass::External {
            note_contamination(entry, scope, name, prov)
        } else {
            None
        };
        let latch = entry.latch.label();
        // Both the log line and the row are written with the lock released —
        // `record_flag` reaches the activity store, which does file I/O.
        // (Step 4's clear row is written first, below, for the same ordering
        // reason the beacon states.)
        drop(tabs);
        ContaminationCleared::record_from(rotated, scope);
        if engaged {
            info!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %name,
                latch,
                "loopback: V32 session taint latch engaged"
            );
        }
        if let Some(contamination) = contamination {
            contamination.record();
        }
        Ok(WriteTaint::Clean)
    }

    /// V32 Phase F (locked decision 14): engage this tab's EXTERNAL latch on
    /// behalf of a HARNESS-NATIVE web tool that never routed through cImp.
    ///
    /// The beacon is the sensor mode's whole mechanism: Claude's `WebFetch` /
    /// `WebSearch` and OpenCode's `webfetch` / `websearch` bypass the proxy, so
    /// without this a tab could read an attacker's page while `/status` still
    /// says `open` and every proxied local-capability tool stays available
    /// beside it. It does exactly what an admitted proxied EXTERNAL call does —
    /// engage the latch, set the contamination bit — and deliberately nothing
    /// more:
    ///
    /// - **No refusal, ever.** The tool has already been permitted by the
    ///   harness by the time the hook runs (and in `deny` mode it never runs at
    ///   all). Returning "blocked" here would be a lie the caller cannot act on.
    /// - **Fail-open on identity**, like every other gate here: a beacon with no
    ///   tab id — or, since #45, with an id that is not a configured tab — has
    ///   no scope to engage.
    ///
    /// It reports what changed ([`BeaconOutcome`]) rather than writing the row
    /// itself: the row's honesty depends on the [`outbound::Origin`] of the
    /// request that caused it, and the registry cannot see that. The handler
    /// owns it (#45).
    ///
    /// The one asymmetry with `gate`: a beacon arriving while the tab is
    /// LOCAL-latched cannot refuse the fetch (it already happened), so it
    /// records the contamination and leaves the latch where it is — the honest
    /// reading of "this conversation has now seen external content, and its
    /// proxied external side stays closed". That case is exactly the one #45
    /// left unaudited; see [`BeaconOutcome::contaminated_now`] (#48).
    ///
    /// V32 Phase G: gated by the same [`GatePolicy`] a proxied call resolves.
    /// An inert policy answers with the default view and records nothing — a
    /// beacon whose latch and quarantine are both off has nothing to report to.
    ///
    /// #48 (F-3): the [`Screen::LatchBeacon`](outbound::Screen) row the handler
    /// writes is unchanged and still says what it always said — *a native web
    /// tool was detected*. The contamination row this method now writes says
    /// something different — *this conversation stopped being clean* — and a
    /// beacon into an already-contaminated tab writes only the first. `prov`
    /// carries the [`outbound::Origin`] for the same reason the handler states
    /// it for the beacon row: over this route it is `Http`, and that is a fact
    /// about the caller, not about the tab.
    fn beacon(
        &self,
        scope: Option<&LatchScope>,
        tool: &str,
        policy: GatePolicy,
        prov: CallProvenance<'_>,
    ) -> BeaconOutcome {
        if policy.inert() {
            return BeaconOutcome::inert();
        }
        let Some(scope) = scope else {
            return BeaconOutcome::inert();
        };
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = tabs.entry(scope.key()).or_insert_with(TabLatch::fresh);
        let cleared = entry.observe(scope.session.as_deref());
        // Unchanged, deliberately: contamination is set on every beacon, and
        // nothing on THIS route can ever clear it (locked decision 15 — step 4's
        // two clears are user actions over IPC, and `observe` above releases the
        // bit only for a tab the user armed). What #48
        // adds is only that the TRANSITION is observable, so it can be recorded
        // — through the SAME function the proxied gate flips the bit with, so
        // the two paths cannot disagree about what a transition is or produce
        // two shapes of row for it. Ordered after the engagement for the same
        // reporting reason `gate` states: the row quotes the latch this beacon
        // leaves the tab in.
        let moved = policy.latch && entry.latch.engage(ToolClass::External);
        let contamination = note_contamination(entry, scope, tool, prov);
        let contaminated_now = contamination.is_some();
        // One row per tab-session over BOTH transitions, rather than one per
        // transition kind: a policy change mid-session could otherwise produce a
        // second row for the same conversation.
        let report =
            (moved || contaminated_now) && !std::mem::replace(&mut entry.beacon_flagged, true);
        let view = entry.view();
        drop(tabs);
        // Step 4: recorded BEFORE the contamination row this beacon may also
        // produce. A beacon arriving on the first call after an armed rotation
        // clears the bit and immediately re-sets it, and the feed has to read in
        // that order to make sense.
        ContaminationCleared::record_from(cleared, scope);
        if moved {
            info!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %tool,
                latch = view.latch,
                "loopback: V32 session taint latch engaged by a native-web beacon"
            );
        } else if contaminated_now {
            // The case #45 left entirely silent: the latch did not move (the tab
            // is latched `Local`, or the latch feature is off) but this
            // conversation is contaminated from here on.
            info!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %tool,
                latch = view.latch,
                "loopback: V32 conversation marked contaminated by a native-web beacon (latch unmoved)"
            );
        }
        if let Some(contamination) = contamination {
            contamination.record();
        }
        BeaconOutcome {
            view,
            engaged: moved,
            contaminated_now,
            report,
        }
    }

    /// V32 Phase H: this tab's current view, **read-only** — the state the
    /// OpenCode plugin's native-tool gate decides against.
    ///
    /// Two properties, both deliberate:
    ///
    /// - **It does not create an entry.** A tab that has never made a gated call
    ///   has nothing to report, and materializing a row for every poll would put
    ///   tabs in `/status` that no tool call ever touched. Absent ⇒
    ///   [`LatchView::default`] ⇒ `open` ⇒ the gate denies nothing. Fail-open by
    ///   construction, not by a branch someone has to remember.
    /// - **It DOES `observe`.** A stale `external` left over from a rotated
    ///   session would deny `read`/`bash` for a whole fresh conversation — a
    ///   false deny of the harness's core tools, which is far worse than the
    ///   read-only purity of not touching the entry. `observe` is the same
    ///   rotation rule `gate` and `beacon` apply, so the three cannot disagree
    ///   about when a conversation ended.
    ///
    /// Step 4: which also means this is one of the places an armed one-shot
    /// fires. For an OpenCode tab it is the *usual* one — the plugin polls
    /// `/latch/state` around the harness's own turns, so a `/clear` after a
    /// restore lifts the bit without waiting for a proxied tool call.
    fn view_for(&self, scope: &LatchScope) -> LatchView {
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let (view, cleared) = match tabs.get_mut(&scope.key()) {
            Some(entry) => {
                let cleared = entry.observe(scope.session.as_deref());
                (entry.view(), cleared)
            }
            None => (LatchView::default(), None),
        };
        drop(tabs);
        ContaminationCleared::record_from(cleared, scope);
        view
    }

    /// V32 Phase F (locked decision 15): apply a USER-initiated latch move.
    ///
    /// Decision 1 rejected automatic resets and still does — an injected context
    /// stays injected, so the latch never *releases itself*. What decision 15
    /// adds is a human, who knows something the system cannot infer: that the
    /// research is done and its output has been read.
    ///
    /// **What enforces "a human" (#45).** This used to claim "nothing the model
    /// can reach may move this", which was false: the same implementation was
    /// also exposed as `POST /latch/override`, behind nothing but the per-launch
    /// bearer token — and that token is readable by any process running as the
    /// user (`.cimp-offload.json`, `.cimp-discovery/<pid>.json`, and the
    /// generated OpenCode plugin inside the project tree). That route is GONE.
    /// What the code now enforces, exactly:
    ///
    /// - **The only caller is the capability-scoped `latch_override` Tauri IPC
    ///   command**, driven by the badge popover. The webview holds no bearer
    ///   token and makes no HTTP call, so this path is not reachable from
    ///   outside the app process.
    /// - **What a shell-capable model CAN still reach is `/latch/beacon`**,
    ///   which only ever tightens (Open → External) and only for a configured
    ///   tab id ([`is_configured_tab`]) — it cannot flip to Local, cannot
    ///   unlatch, and cannot clear contamination.
    /// - **What clears `contaminated`** is three of the four actions —
    ///   `clear_contamination`, `unlatch` (decision 15's 2026-08-10 amendment)
    ///   and, deferred, `await_session_clear` — and nothing else: no automatic
    ///   path, no HTTP path. `flip_local` is the one that does not. See
    ///   [`TabLatch::contaminated`] for why a click is a legitimate trust root
    ///   where a transcript file is not.
    ///
    /// This is not an integrity boundary against native code, and never was —
    /// decision 3 says plainly that a model with a shell already has the
    /// capabilities the latch withholds. It is the difference between an audit
    /// trail that records a user's decision and one that records a POST.
    ///
    /// **The feature switches are deliberately not consulted.** `gate` creates
    /// no entry while [`GatePolicy::inert`], so with both controls off there is
    /// usually nothing here to move and the caller gets the "nothing to
    /// override" error. But an entry created while the controls were ON survives
    /// the user switching them off, and its contamination bit is still what the
    /// badge renders — so refusing to clear it would leave a stale flag the user
    /// cannot reach *because* they disabled the feature. Every action here is
    /// user-initiated and only ever loosens cImp's own bookkeeping; none of them
    /// needs the feature to be armed to be meaningful.
    ///
    /// Errors (rather than silently no-op'ing) when the move does not apply, so
    /// the UI can say why instead of appearing to have worked.
    fn apply_override(
        &self,
        scope: &LatchScope,
        action: LatchOverride,
    ) -> Result<OverrideOutcome, String> {
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(entry) = tabs.get_mut(&scope.key()) else {
            return Err(format!(
                "no taint latch is engaged for {} — nothing to override",
                scope.label()
            ));
        };
        // An armed one-shot can fire right here: the user restored, ran
        // `/clear`, and the first thing to look at the entry afterwards is their
        // next click. Captured rather than dropped, and recorded below on BOTH
        // exits — a refused action must not swallow a clear that already
        // happened.
        let rotated = entry.observe(scope.session.as_deref());
        let prior = entry.latch;
        let mut prior_taint = None;
        // The action's own verdict, computed before the lock is released so an
        // error path can still record `rotated`.
        let applied = match action {
            // The workflow button: research finished, now apply it. EXTERNAL
            // only — from `Open` there is nothing to flip and from `Local` this
            // would be a no-op that reads like an action. At no instant does the
            // session hold web AND local capability: the flip closes the
            // external side in the same assignment that opens the local one.
            LatchOverride::FlipLocal => {
                if prior != Latch::External {
                    Err(format!(
                        "\"switch to local\" applies only to an EXTERNAL-latched tab ({} is {})",
                        scope.label(),
                        prior.label()
                    ))
                } else {
                    entry.latch = Latch::Local;
                    // #48 (F-23): record WHY the latch is `local`, at the only
                    // site that can know. This is the fact the native-web
                    // refusal is selected on — see
                    // [`TabLatch::local_by_user_flip`] — and it is written under
                    // the same lock as the assignment above so the two cannot be
                    // observed apart.
                    entry.local_by_user_flip = true;
                    Ok(())
                }
            }
            // The at-own-risk button: both sides open again. Valid from any
            // state except a latch that is already open, which would be a
            // no-op.
            //
            // Decision 15's 2026-08-10 amendment: **it also clears the
            // contamination bit.** The trust root is the one that closed H-2 —
            // authority, not evidence. An attacker cannot click this; the click
            // already hands back the strictly more dangerous capability (read +
            // web, with the injected content still in the context window) behind
            // the popover's own confirmation; so leaving persistent memory
            // quarantined afterwards overruled a judgement the product had just
            // asked the user to make. `FlipLocal` above keeps the bit precisely
            // because it is a workflow step and not a verdict.
            //
            // Ordering is load-bearing: the clear runs BEFORE `latch = Open`, so
            // `PriorTaint::latch` records the latch the bit was released from
            // (`external`/`local`) rather than the `open` this arm is about to
            // write — which is what keeps it equal to `OverrideOutcome::prior`,
            // the value `override_row` puts in the same sentence.
            LatchOverride::Unlatch => {
                if prior == Latch::Open {
                    Err(format!(
                        "{} is not latched — nothing to unlatch",
                        scope.label()
                    ))
                } else {
                    // `None` here is not an error: the unlatch is legal on its
                    // own terms and the clear is a consequence of it, not its
                    // purpose. An uncontaminated latched tab unlatches and
                    // writes no `contamination_cleared` row — see
                    // `unlatch_clear_row`.
                    prior_taint = entry.clear_contamination();
                    entry.latch = Latch::Open;
                    // #48 (F-23): the web side is open again, so nothing is being
                    // refused for this reason any more. Cleared here rather than
                    // left to the next rotation because the field must never
                    // outlive the latch position it explains.
                    entry.local_by_user_flip = false;
                    Ok(())
                }
            }
            // Step 4, the false-positive resume. Clears the bit and NOTHING
            // else: the latch stays where it is (it has its own buttons), the
            // budget keeps its spend, the session and the tab are not touched,
            // and quarantined notes stay quarantined.
            //
            // It supersedes an arm, which `clear_contamination` drops — there is
            // nothing left to wait for once the bit is gone.
            LatchOverride::ClearContamination => match entry.clear_contamination() {
                Some(p) => {
                    prior_taint = Some(p);
                    Ok(())
                }
                None => Err(format!(
                    "{} is not flagged as contaminated — nothing to clear",
                    scope.label()
                )),
            },
            // Step 4, the restore arm. Clears nothing now, by user decision: a
            // restore rolls back FILES and cannot remove injected text from the
            // model's context window, so this is the case where clearing
            // immediately is least justified.
            LatchOverride::AwaitSessionClear => {
                if !entry.contaminated {
                    Err(format!(
                        "{} is not flagged as contaminated — there is nothing waiting to clear",
                        scope.label()
                    ))
                } else if entry.awaiting_session_clear {
                    // Not a failure so much as an answer, and the popover shows
                    // it verbatim. Still an error rather than a silent success:
                    // a second click that reported "done" would imply something
                    // new happened.
                    Err(format!(
                        "{} is already waiting for a new session — the contamination flag clears \
                         when one is observed",
                        scope.label()
                    ))
                } else {
                    entry.awaiting_session_clear = true;
                    Ok(())
                }
            }
        };
        // Deliberately NOT touched by ANY move here: the session's spent budget.
        // Letting a click refill the fetch budget would make the budget
        // advisory. (Live-verified 2026-08-10: an unlatch does not refill it —
        // recipe 13's web-side leg could not be re-probed for exactly that
        // reason.)
        //
        // And `contaminated` is not touched by `FlipLocal`: the flip changes
        // what the session may reach next and cannot un-read what the model has
        // already read. `Unlatch` DOES release it — decision 15's 2026-08-10
        // amendment, argued in that arm — and `Unlatch` is the only latch move
        // that does.
        let view = entry.view();
        drop(tabs);
        ContaminationCleared::record_from(rotated, scope);
        applied?;
        warn!(
            target: "offload",
            agent = scope.agent,
            tab = %scope.tab,
            action = action.as_str(),
            prior = prior.label(),
            latch = view.latch,
            contaminated = view.contaminated,
            awaiting_session_clear = view.awaiting_session_clear,
            "loopback: V32 containment state moved by explicit user override"
        );
        Ok(OverrideOutcome {
            prior,
            prior_taint,
            view,
        })
    }

    /// Step 4: fold each known tab's CURRENT live session into its entry, so a
    /// rotation the harness has already proved reaches [`TabLatch::observe`]
    /// even when the tab has made no gated call since.
    ///
    /// **Why the read path needs this.** Before step 4, `observe` ran only from
    /// `gate`, `beacon` and `view_for` — i.e. only when the harness did
    /// something. That was fine when a rotation had no user-visible consequence
    /// worth waiting for. It is not fine now: the whole promise of the restore
    /// arm is "run `/clear` and the flag lifts", and a Claude tab has no
    /// `/latch/state` poll, so without this the flag would sit set until the
    /// model happened to call a cImp tool. This is the read the UI already makes
    /// every 4 s ([`latch_snapshot`]) — no second timer, no new schedule.
    ///
    /// **It grants nothing a call would not have granted anyway.** Everything
    /// `observe` resets is permissive state that the very next gated call would
    /// have reset before deciding anything, so doing it at read time is strictly
    /// a matter of *when* the same fact becomes visible.
    ///
    /// Takes resolved scopes rather than an `AppHandle` so the session lookup
    /// (which locks the graph service) happens outside this lock.
    fn observe_all(&self, scopes: &[LatchScope]) -> Vec<ContaminationCleared> {
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let mut cleared = Vec::new();
        for scope in scopes {
            if let Some(entry) = tabs.get_mut(&scope.key()) {
                if let Some(ev) = entry.observe(scope.session.as_deref()) {
                    cleared.push(ev.into_row(scope));
                }
            }
        }
        cleared
    }

    /// Every `(agent, tab)` the registry holds an entry for. Cloned out under
    /// the lock so [`latch_snapshot`] can resolve live sessions without holding
    /// it.
    fn keys(&self) -> Vec<(&'static str, String)> {
        let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        tabs.keys().cloned().collect()
    }

    /// V32 Phase C (locked decision 11): whether this tab's session may make
    /// another EXTERNAL call, and the one-per-scope exhaustion report.
    ///
    /// Runs only on `/mcp/call` — every name that route serves is proxied and
    /// therefore EXTERNAL; `/graph_run` serves cImp-native tools that pull no
    /// external bytes and are not budgeted. Fail-open on a call with no tab
    /// identity, exactly like [`gate`](Self::gate): there is no scope to charge.
    fn budget_gate(
        &self,
        scope: Option<&LatchScope>,
        limits: outbound::BudgetLimits,
        tool: &str,
    ) -> Result<(), &'static str> {
        let Some(scope) = scope else { return Ok(()) };
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(entry) = tabs.get_mut(&scope.key()) else {
            // No entry yet ⇒ `gate` has not run for this tab, so nothing is
            // spent. (In practice `gate` always runs first on this route.)
            return Ok(());
        };
        if !entry.budget.exhausted(limits) {
            return Ok(());
        }
        let first = entry.budget.claim_flag();
        drop(tabs);
        if first {
            warn!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %tool,
                "loopback: external fetch budget exhausted for this session"
            );
            outbound::record_flag(outbound::Flag {
                screen: outbound::Screen::Budget,
                origin: outbound::Origin::Internal,
                consumer: scope.agent,
                scope: &scope.label(),
                // The scope is in hand — see `LatchScope::attribution`.
                attribution: scope.attribution(),
                session: scope.session.as_deref(),
                tool,
                host: None,
                url: None,
                resolved_ip: None,
                canary: false,
                root: scope.root.clone(),
                detail: outbound::REFUSAL_BUDGET,
            });
        }
        Err(outbound::REFUSAL_BUDGET)
    }

    /// Charge one completed EXTERNAL call to this tab's session budget.
    /// Silently no-ops without tab identity (nothing to charge) — the same
    /// fail-open the latch takes.
    fn charge(&self, scope: Option<&LatchScope>, response_bytes: usize) {
        let Some(scope) = scope else { return };
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = tabs.get_mut(&scope.key()) {
            entry.budget.charge(response_bytes);
        }
    }

    /// Charge one **attempted** proxied call, whatever it returned (#48, D-3).
    ///
    /// The charge used to sit on the `Ok` arm only, so a loop of fetches
    /// against a host that 500s advanced neither the byte counter nor the call
    /// counter and never exhausted the budget — while the worker's copy of the
    /// same contract charged both arms (an `Err` there becomes an `ERROR: …`
    /// tool result with `executed = true`). Two paths, one contract, opposite
    /// behaviour.
    ///
    /// A failed fetch charges **zero bytes and one call**: nothing was
    /// ingested, but the request left the machine and `max_calls` is what
    /// exists to stop a loop. Taking the whole decision here — rather than a
    /// `map` at the call site — is what makes it testable: the handler's use is
    /// one unconditional statement above the match it used to be inside.
    /// Generic over the error half since #48 M-17 made it a
    /// `mcp_host::HostError`: this function reads only `is_ok`/`len`, and the byte
    /// charge is unchanged by the error type.
    fn charge_call<E>(&self, scope: Option<&LatchScope>, result: &Result<String, E>) {
        self.charge(scope, result.as_ref().map(|t| t.len()).unwrap_or(0));
    }

    /// Claim one of this tab session's audit-row bits — see
    /// [`outbound::AuditClaims`]. Locks for exactly the length of the claim, so
    /// nothing is held across the SSRF screen's DNS `await`.
    ///
    /// Without a registry entry (no tab identity, or `gate` has not run) there
    /// is no session to attribute a repeat to, so the claim falls back to
    /// `unscoped` — which since #48 F-40 is the identity-less scope's own
    /// process-global ledger ([`outbound::UnscopedAudit`]) and **not** a
    /// constant. The latch and the budget still fail open here; the ROWS no
    /// longer do, because "no session" was never a reason for a caller to be
    /// able to write one row per event into a capped lane.
    ///
    /// `unscoped` is a closure so the fallback ledger is touched only when it is
    /// actually reached, and — load-bearing — so its lock is never taken while
    /// this one is held. The early `return` inside the `if let` is what ends the
    /// registry borrow before that call.
    fn claim<T>(
        &self,
        scope: Option<&LatchScope>,
        claim: impl FnOnce(&mut outbound::Budget) -> T,
        unscoped: impl FnOnce() -> T,
    ) -> T {
        let Some(scope) = scope else { return unscoped() };
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = tabs.get_mut(&scope.key()) {
            return claim(&mut entry.budget);
        }
        drop(tabs);
        unscoped()
    }

    /// The `/status` view: one row per tab the proxy has served, sorted so the
    /// output is stable to eyeball across polls.
    fn snapshot(&self) -> Vec<LatchStatus> {
        let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let mut rows: Vec<LatchStatus> = tabs
            .iter()
            .map(|((agent, tab), st)| LatchStatus {
                consumer: agent,
                tab: tab.clone(),
                session: st.session.clone(),
                view: st.view(),
            })
            .collect();
        rows.sort_by(|a, b| (a.consumer, &a.tab).cmp(&(b.consumer, &b.tab)));
        rows
    }
}

/// One `/status` latch row.
#[derive(Serialize, Debug)]
pub struct LatchStatus {
    pub consumer: &'static str,
    pub tab: String,
    pub session: Option<String>,
    /// V32 Phase F: the latch label plus the contamination bit and per-row
    /// override availability. **Flattened**, so the wire shape is unchanged for
    /// the Phase B readers (`latch` stays a top-level key of the row) and the
    /// new facts sit beside it rather than in a nested object — one row per
    /// tab, as `/status` has always been.
    #[serde(flatten)]
    pub view: LatchView,
}

impl LatchStatus {
    /// [`Latch::label`] for this row: `open` / `external` / `local`.
    ///
    /// Read by the tests (and by anyone holding a snapshot); the wire form goes
    /// through `view`'s flattened `latch` key, so the running app never calls
    /// this — the same `cfg_attr` shape `toolclass::mutates_fs` carried until
    /// V33 Phase F landed its consumer and removed it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn latch(&self) -> &'static str {
        self.view.latch
    }
}

/// The process-wide registry. Latch state is intentionally in-memory and
/// non-durable: it describes *live* conversations, and an app restart
/// necessarily ends every one of them.
///
/// **It is bounded** — one entry per (agent, tab) pair, over the AI tab ids the
/// user has configured, which are reused across restarts, so nothing
/// accumulates over a long-running app. That was asserted here and enforced
/// nowhere until #45: every key arrives in a request body, and the map has no
/// TTL, no cap and no eviction, while every entry is serialized into every
/// `/status` response and every 4 s `latch_status` poll. The bound is now real
/// and tested — [`is_configured_tab`], applied in [`latch_scope`], which is the
/// one funnel through which `gate` and `beacon` (the only two methods that
/// insert) receive a scope at all.
///
/// The caveat that keeps this honest: the bound is `configured AI tabs × 1`
/// only while the settings snapshot is readable. See [`is_configured_tab`]'s
/// empty-list escape.
fn latches() -> &'static LatchRegistry {
    static LATCHES: OnceLock<LatchRegistry> = OnceLock::new();
    LATCHES.get_or_init(LatchRegistry::default)
}

/// One tab session's audit-row claim ledger, as the SSRF chokepoint and the
/// detection boundary see it (#48).
///
/// The ledger itself lives inside the tab's [`outbound::Budget`], which is the
/// only per-conversation state with the right lifetime *and* the right reset
/// rule: `TabLatch::observe` wipes it on a proved session rotation, so a
/// genuinely new conversation is entitled to its own rows. A process-global
/// `HashSet<scope>` was the alternative and is wrong for exactly that reason —
/// proxy scopes are stable `agent:tab` strings, so it would suppress a tab's
/// rows permanently, across every session it ever holds.
///
/// A handle rather than a borrow because the ledger sits behind the registry
/// mutex, which must not be held across the SSRF screen's DNS `await`.
///
/// The second field is the agent whose identity-less ledger a call **without** a
/// scope claims against ([`outbound::UnscopedAudit`], #48 F-40). It is carried
/// rather than derived because at the one construction site the agent is already
/// resolved through `graph::source_for_consumer` — the same normalisation that
/// builds the `agent:(no tab identity)` label the resulting row shows, so the
/// ledger and the label cannot name different things.
struct TabAudit<'a>(Option<&'a LatchScope>, &'static str);

impl TabAudit<'_> {
    /// This call's fallback ledger. One place, so both claims agree on it.
    fn unscoped(&self) -> outbound::UnscopedAudit {
        outbound::UnscopedAudit::for_agent(self.0.map(|s| s.agent).unwrap_or(self.1))
    }
}

impl outbound::ScopeAudit for TabAudit<'_> {
    fn claim_ssrf(&self) -> outbound::DoublingRow {
        latches().claim(self.0, outbound::Budget::claim_ssrf_flag, || {
            self.unscoped().claim_ssrf()
        })
    }
    fn claim_unscreened(&self) -> bool {
        latches().claim(self.0, outbound::Budget::claim_unscreened_flag, || {
            self.unscoped().claim_unscreened()
        })
    }
}

/// V32 Phase F: the `/status` latch rows, read **in process**.
///
/// The per-tab taint badge and its override popover live in the webview, which
/// has no bearer token and no business acquiring one — every loopback route is
/// authenticated precisely so only cImp-spawned children can reach it. The
/// Tauri backend already owns the registry, so the UI goes through an IPC
/// command ([`crate::ipc::commands::latch_status`]) that calls this, and the
/// token never leaves the processes that need it.
///
/// **Step 4: it folds each tab's current live session in first.** See
/// [`LatchRegistry::observe_all`] for why the read path has to — a Claude tab
/// polls no `/latch/state`, so without this an armed one-shot would wait for the
/// model to call a cImp tool rather than for the user's `/clear`. This is the
/// same 4 s read the badge already makes; no second timer is introduced.
pub fn latch_snapshot(app: &AppHandle) -> Vec<LatchStatus> {
    // Resolve scopes with the registry lock NOT held: `latch_scope` locks the
    // graph service for the live-session lookup.
    let settings = live_settings(app);
    let scopes: Vec<LatchScope> = latches()
        .keys()
        .iter()
        .filter_map(|(agent, tab)| {
            latch_scope(app, &settings, agent, Some(tab.as_str())).into_scope()
        })
        .collect();
    for cleared in latches().observe_all(&scopes) {
        cleared.record();
    }
    latches().snapshot()
}

/// The caller-composed parts of one Phase F `injection_flag` row: its
/// provenance, its `tool` column and the prose an incident reviewer reads.
///
/// **They are built together on purpose (#48).** #45 shipped them apart — the
/// detail functions spelled `Origin::Ipc` / `Origin::Http` into their own
/// format strings while `Flag.origin` was set independently at the call site.
/// #47 then made `origin` a required field precisely so provenance could not be
/// taken by omission, but the sentence a human actually reads was still not
/// derived from it: re-expose an HTTP path into the override and the row's
/// `origin` key would say `http` while its text went on asserting that a human
/// clicked, with nothing to catch it. One struct, one origin, both consumers.
struct FlagRow {
    /// Which feed lane the row belongs in. Carried here since step 4, because
    /// one of the four override actions is not a latch move at all — it releases
    /// the contamination bit, and that belongs in
    /// [`outbound::Screen::ContaminationCleared`] beside the row that SET the
    /// bit rather than among the latch moves. Deciding it here keeps the choice
    /// beside the sentence that describes it.
    screen: outbound::Screen,
    /// Copied verbatim into [`outbound::Flag::origin`], and interpolated into
    /// [`detail`](Self::detail) by the same function that received it.
    origin: outbound::Origin,
    /// The row's at-a-glance `tool` column.
    tool: String,
    /// The row's human-readable body.
    detail: String,
}

/// An override's `injection_flag` row (#45), composed from the origin the
/// caller states rather than one baked in here (#48).
///
/// Split out of [`apply_latch_override`] so the row's content is assertable
/// without an `AppHandle`, which this crate has no mock for — every Phase F
/// test called [`LatchRegistry::apply_override`] directly and stopped short of
/// the row, leaving the one artifact an incident review actually reads
/// uncovered.
fn override_row(
    origin: outbound::Origin,
    action: LatchOverride,
    outcome: &OverrideOutcome,
) -> FlagRow {
    // The action is the row's at-a-glance "tool" for the three latch-shaped
    // moves: these rows have no tool call behind them, and what the user DID is
    // the fact worth reading. The clear names its own basis instead.
    let (screen, tool) = match action {
        LatchOverride::ClearContamination => (
            outbound::Screen::ContaminationCleared,
            ClearBasis::Resume.tool().to_string(),
        ),
        _ => (outbound::Screen::LatchOverride, action.as_str().to_string()),
    };
    let detail = match action {
        // Step 4: composed by the SAME function the armed-rotation clear uses,
        // so the two paths cannot describe one state change two ways.
        LatchOverride::ClearContamination => clear_detail(
            ClearBasis::Resume,
            origin,
            outcome
                .prior_taint
                .as_ref()
                .map_or(outcome.prior.label(), |p| p.latch),
            outcome
                .prior_taint
                .as_ref()
                .and_then(|p| p.session.as_deref()),
            None,
        ),
        // Step 4: the arm. It clears nothing, and the row has to say so — a
        // reader who sees "restore" in the feed and no `contamination_cleared`
        // row afterwards must be able to tell "still waiting" from "lost".
        LatchOverride::AwaitSessionClear => format!(
            "USER OVERRIDE (await_session_clear, origin: {}): a checkpoint was restored for this \
             tab, and the contamination flag is deliberately NOT cleared (contaminated={}). \
             Restoring rolls back FILES; it cannot remove injected text from the model's context \
             window, so this is the case where clearing immediately would be least justified. cImp \
             will clear the flag when it observes this tab start a new harness session — run \
             `/clear` in the tab, or restart it. Until then memory writes stay quarantined and \
             external results keep their envelope. Latch unchanged ({}).",
            origin.as_str(),
            outcome.view.contaminated,
            outcome.view.latch,
        ),
        // The flip is a WORKFLOW step, not a verdict, which is the whole reason
        // decision 15's 2026-08-10 amendment narrowed "contamination outlives
        // the override" to this one action. The row is where a reviewer learns
        // which of the two moves they are looking at.
        LatchOverride::FlipLocal => format!(
            "USER OVERRIDE (flip_local, origin: {}): taint latch {} → {}. Contamination is NOT \
             cleared by the flip (contaminated={}): memory writes stay quarantined and external \
             results keep their envelope, because the injected content is still in the \
             conversation and \"switch to local\" says \"research done, now apply it\" — not \"that \
             content was harmless\". Clearing the flag is its own decision with its own three \
             actions: `clear_contamination` (the user judges the content harmless), `unlatch` (the \
             user restores FULL access and accepts the larger risk) and `await_session_clear` \
             (after a restore, effective once a new harness session is observed). No automatic \
             path and no HTTP route can reach any of them.",
            origin.as_str(),
            outcome.prior.label(),
            outcome.view.latch,
            outcome.view.contaminated,
        ),
        // Decision 15's 2026-08-10 amendment. One click, two effects, and the
        // row states both — including whether the second one actually fired: an
        // unlatch on an uncontaminated tab clears nothing, and this sentence
        // must not be readable as evidence that a bit was released.
        LatchOverride::Unlatch => format!(
            "USER OVERRIDE (unlatch, origin: {}): taint latch {} → {} — FULL access restored, \
             which recreates the read+web trifecta with any injected content still in the \
             conversation. {} Memory notes ALREADY quarantined STAY quarantined — promoting or \
             discarding them is the Memory view's own review (locked decision 10), a separate \
             consent surface.",
            origin.as_str(),
            outcome.prior.label(),
            outcome.view.latch,
            match outcome.prior_taint.as_ref() {
                Some(p) => format!(
                    "The contamination flag was cleared by the same click (prior state: \
                     contaminated=true, latch={}, session={}), and it is filed as its own \
                     `contamination_cleared` row beside the row that SET the bit. The trust root \
                     is AUTHORITY, not evidence: an attacker cannot click this, and the click \
                     already handed back the strictly more dangerous capability — so leaving \
                     persistent memory writes quarantined would have overruled a judgement the \
                     product had just asked the user to make. This tab's future `context_note` \
                     writes are stored clean again, and a fresh contamination will report itself \
                     as a new transition.",
                    p.latch,
                    p.session.as_deref().unwrap_or("unknown"),
                ),
                None => format!(
                    "This tab was not flagged as contaminated, so there was nothing to clear \
                     (contaminated={}).",
                    outcome.view.contaminated
                ),
            },
        ),
    };
    FlagRow {
        screen,
        origin,
        tool,
        detail,
    }
}

/// Decision 15's 2026-08-10 amendment: the `contamination_cleared` row a **full
/// unlatch** owes, or `None` when this override released nothing.
///
/// # Why a second row rather than a sentence in the first one
///
/// [`outbound::Screen::ContaminationCleared`] is a retention lane *and* a join
/// key: its own doc says a reviewer filtering the two contamination wire values
/// "gets one tab's whole taint lifecycle", and [`outbound::contamination_events`]
/// queries exactly those two lanes. A release visible only inside a
/// [`outbound::Screen::LatchOverride`] detail string is invisible to that join —
/// the Workbench Timeline would show a `☣` that never closes, for a tab the
/// registry reports clean. That is the "signal with no consumer" class (#48,
/// F-3) reintroduced one amendment later, so the clear is filed where every
/// other clear is filed.
///
/// # Why it is composed here and not in [`LatchRegistry::apply_override`]
///
/// The origin is stated ONCE, by the caller (#48, A2-3): [`apply_latch_override`]
/// is the only path in and it names [`outbound::Origin::Ipc`] for both rows, so
/// the two halves of an override cannot disagree about who acted. Composing it
/// here also makes it assertable without an `AppHandle`, which this crate has no
/// mock for — the same seam [`override_row`] exists for.
///
/// `None` covers the honest case: an unlatch on a tab that was never
/// contaminated. It is not an error, and it must not write a row saying a bit
/// was released.
fn unlatch_clear_row(
    origin: outbound::Origin,
    action: LatchOverride,
    scope: &LatchScope,
    outcome: &OverrideOutcome,
) -> Option<ContaminationCleared> {
    if action != LatchOverride::Unlatch {
        return None;
    }
    let prior = outcome.prior_taint.as_ref()?;
    Some(ContaminationCleared {
        origin,
        basis: ClearBasis::Unlatch,
        consumer: scope.agent,
        scope: scope.label(),
        // The conversation the bit was cleared FOR, exactly as the resume path
        // files it: the one the `contamination` row named.
        session: prior.session.clone(),
        root: scope.root.clone(),
        detail: clear_detail(
            ClearBasis::Unlatch,
            origin,
            prior.latch,
            prior.session.as_deref(),
            None,
        ),
    })
}

/// V32 Phase F (locked decision 15): apply a user-initiated latch move to one
/// tab, write its `injection_flag` row, and return the tab's new view.
///
/// **Reachable from the `latch_override` Tauri IPC command only** — the badge
/// popover, i.e. the user. `POST /latch/override` existed alongside it until
/// #45 "so the same action is reachable from a child or a live-verification
/// script"; that convenience made a capability GRANT drivable by anything
/// holding the launch token, and left the resulting row indistinguishable from
/// a click. There is no HTTP path into this function now, so
/// [`outbound::Origin::Ipc`] on the row is a fact rather than an assumption.
pub fn apply_latch_override(
    app: &AppHandle,
    consumer: &str,
    tab: &str,
    action: &str,
) -> Result<LatchView, String> {
    let action = LatchOverride::parse(action)?;
    let agent = crate::graph::source_for_consumer(consumer);
    // One settings snapshot, shared with the tab-id check inside `latch_scope`.
    let settings = live_settings(app);
    let scope = latch_scope(app, &settings, agent, Some(tab))
        .into_scope()
        .ok_or_else(|| {
            // #45 folded "not a configured tab" into this refusal, so the
            // message has to cover both — a popover that said "needs a tab id"
            // about a tab id it was given would send the user looking in the
            // wrong place.
            format!("a latch override needs a configured tab id (got {tab:?})")
        })?;
    let outcome = latches().apply_override(&scope, action)?;

    // Locked decision 15: "every override writes an `injection_flag` row … so
    // the feed records who opened what." `ok: true` — nothing was denied; this
    // is a capability GRANT, and the feed must show it as the deliberate act it
    // is rather than as a failure. The prior latch is in the detail because
    // "restored full access" from `external` and from `local` are very
    // different events.
    //
    // The origin is stated ONCE, here (#48): `override_row` puts it in the
    // row's `origin` key and in the sentence the reviewer reads, so the two
    // cannot come apart. `Ipc` is the one origin that means a human acted, and
    // it is a fact rather than an assumption only because no HTTP path into
    // this function survives (#45) — re-expose one and this constant is what
    // has to change.
    let row = override_row(outbound::Origin::Ipc, action, &outcome);
    outbound::record_flag(outbound::Flag {
        // Step 4: the row's own screen, not a constant here — a contamination
        // clear is filed beside the row that set the bit, not among the latch
        // moves. See `FlagRow::screen`.
        screen: row.screen,
        origin: row.origin,
        consumer: agent,
        scope: &scope.label(),
        // The scope is in hand — see `LatchScope::attribution`.
        attribution: scope.attribution(),
        session: scope.session.as_deref(),
        tool: &row.tool,
        host: None,
        url: None,
        resolved_ip: None,
        canary: false,
        root: scope.root.clone(),
        detail: &row.detail,
    });
    // Decision 15's 2026-08-10 amendment: a full unlatch also RELEASES the
    // contamination bit, and that release owes the `contamination_cleared` lane
    // its own row — see `unlatch_clear_row` for why it is not folded into the
    // latch move's prose. Written AFTER the override row, in the order the state
    // moved: the latch reopened, and the flag went with it. Same stated origin
    // for both, from the one constant above.
    if let Some(cleared) = unlatch_clear_row(outbound::Origin::Ipc, action, &scope, &outcome) {
        cleared.record();
    }
    Ok(outcome.view)
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
    // V40 review H-1: ONE identity for this call. `proxy_identity` folds an
    // in-app consumer onto `Consumer::conservative_grant` and refuses a token
    // nobody declared — before this, `?consumer=offload` reached the
    // `LatchRoute::Native` gate with an activity source that names no
    // configured tab (so no latch, no attribution) and `?consumer=<garbage>`
    // did the same with nothing refusing it. The FOLDED token goes downstream,
    // so `run_command`'s exposure switch, the memory agent scope, the activity
    // source and the latch all answer about the same harness.
    let Some((resolved, consumer_source)) = proxy_identity(body.consumer.as_deref()) else {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(unknown_consumer_message()),
        };
        return write_json(stream, 400, &r).await;
    };
    let consumer = resolved.token();
    // V28: resolve the calling TAB to the session it currently reports, so the
    // `context_*` memory tools scope to this tab's own session instead of "the
    // most recent session for this agent" (two same-agent tabs used to share —
    // and steal — one memory scope). Fail-open: no `tab`, an unknown key, a
    // different agent's entry, or a TTL-stale one all yield `None`, which is
    // exactly the pre-V28 behavior.
    //
    // V32 Phase B folds that same resolution into the latch scope, so the
    // registry is consulted once and the memory scope and the taint scope can
    // never disagree about which session this call belongs to.
    //
    // V32 Phase G: ONE settings read for the whole call (see the sibling note
    // on `/mcp/call`). #45 pulls it above the scope resolution, because the tab
    // id is now validated against the configured tab list and that check must
    // use the same snapshot as the policy it feeds.
    let settings = live_settings(app);
    // #104: the tools this route serves take a project root — `run_command`
    // creates its marker directory under it and `run_check` runs the project's
    // configured commands from it — so the body's `cwd` is resolved to a real
    // root rather than used as one. A refusal is a tool-level error the model
    // can read and act on, not a silently different project.
    let Some(cwd) = external_project_root(app, &settings, body.tab.as_deref(), body.cwd.as_deref())
    else {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(format!(
                "no project root for {} — a working directory is not a project by \
                 itself; open this project as a cImp tab, or run it from inside a \
                 git repository",
                bounded_id(body.cwd.as_deref().unwrap_or("(none)"))
            )),
        };
        return write_json(stream, 200, &r).await;
    };
    let scoping = latch_scope(app, &settings, consumer_source, body.tab.as_deref());
    // #48 F-20: resolved BEFORE `into_scope()` collapses `Anonymous` and
    // `Unknown` into one `None`. That collapse is right for the latch (both fail
    // open) and wrong for the row, which has to say which of the three this call
    // was. See `LatchScoping::attribution`.
    let tab_attr = scoping.attribution();
    let scope = scoping.into_scope();
    let session = scope.as_ref().and_then(|s| s.session.clone());

    // V32 Phase B: the session taint latch over the tools THIS route serves —
    // the content-bearing graph tools (LOCAL-CAPABILITY), the structural ones
    // and the memory reads (TRUSTED, never gated), and `context_note`
    // (PERSISTENT-WRITE).
    //
    // V32 Phase C2 (locked decision 10): a `context_note` under an EXTERNAL
    // latch is no longer refused. The gate returns `Quarantined` and the write
    // proceeds with that verdict threaded into it — the note is stored with a
    // `tainted` flag, kept out of `context_recall`/`context_notes`/the
    // compaction carry-over/the fact distiller (and so out of auto-injection),
    // and held for explicit user promote-or-discard. That preserves the
    // legitimate research conclusion the Phase B refusal dropped.
    //
    // V32 Phase G: both halves resolve through the three-level hierarchy at this
    // tab's scope, from ONE settings read — so a tab with the latch overridden
    // off still quarantines, and a tab with the master switch off does neither.
    // #48 F-16: resolved here, from the service, so the quarantine row this gate
    // may write and the activity row the dispatch will write name the SAME
    // project. `graph_root_key` is `run_graph_tool`'s own resolution, exposed.
    let call_root = graph.graph_root_key(&cwd);
    let gate_policy = GatePolicy::resolve(&settings, scope.as_ref());
    let taint = match latches().gate(
        scope.as_ref(),
        LatchRoute::Native,
        &body.name,
        gate_policy,
        CallProvenance::internal_in(&call_root),
    ) {
        Ok(t) => t,
        Err(refusal) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(refusal.to_string()),
            };
            // 200, like every other tool-level error here: the child renders
            // `error` as the tool result, which is how the model reads it.
            return write_json(stream, 200, &r).await;
        }
    };

    let r = match graph
        .run_graph_tool(
            &cwd,
            &body.name,
            &body.args,
            consumer,
            session.as_deref(),
            // V32 Phase G: the second resolved verdict this route carries — the
            // memory-read tools it serves (`context_recall` / `context_notes`)
            // are the recall envelope's delivery point, and only this frame
            // knows the tab whose scope decides it.
            toolclass::CallGuards {
                taint,
                spotlight_recall: crate::settings::injection::effective(
                    crate::settings::injection::Feature::Spotlighting,
                    crate::settings::injection::Scope::for_tab(
                        consumer_source,
                        body.tab.as_deref(),
                    ),
                    &settings,
                ),
            },
            tab_attr,
        )
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
/// error → 400). Everything else is validated *after* the parse, by
/// [`audit_admit`], so a bad value becomes a readable tool error rather than a
/// bare 400 the model cannot act on.
#[derive(Deserialize)]
struct AuditRunBody {
    category: crate::audit::adapters::Category,
    /// The agent that triggered the scan, from the child's `--consumer` flag.
    /// It selects which `expose_*` toggle the route re-enforces at run time, so
    /// it is a *capability selector*, not a label — H-8: narrowed to
    /// [`audit_consumers`] by [`audit_consumer`] before it reaches
    /// [`AuditState::consumer_exposed`](crate::audit::AuditState::consumer_exposed).
    #[serde(default)]
    consumer: Option<String>,
    /// The child's working directory (the agent's project), sent for
    /// verification only — the scan always runs against this app's own
    /// launch root. `#[serde(default)]` keeps older children compatible.
    #[serde(default)]
    cwd: Option<String>,
    /// V32 C-1b (2026-08-07 review): the cImp tab this child serves
    /// (`cimp --code-audit-mcp --tab <id>`), resolved to the tab's taint latch
    /// so a contaminated conversation cannot run a local scanner.
    ///
    /// H-8 (2026-08-08 re-review): **required in practice.** It stays
    /// `Option` on the wire — a missing field must produce the route's readable
    /// refusal, not serde's 400 — but [`audit_tab`] refuses a body without it.
    /// The fail-open anonymous scope the other identity-taking routes keep is
    /// not available here: this route's whole gate keys on tab identity, so
    /// "no tab" meant "no containment", silently.
    #[serde(default)]
    tab: Option<String>,
}

/// The consumers that legitimately POST `/audit/run` (H-8).
///
/// Empirically the complete set — there are exactly two spawn sites for the
/// `cimp --code-audit-mcp` child, and no other caller exists:
///
/// - Claude: `tabs::config::build_pre_args` emits
///   `[--code-audit-mcp, --tab, <id>]` with **no** `--consumer`, so the child's
///   own default (`audit::mcp::CONSUMER`, `"claude"`) goes on the wire. Pinned
///   by `tabs::config::tests::the_code_audit_child_carries_its_own_tab_id`.
/// - OpenCode: `tabs::config::build_opencode_config` emits
///   `[<exe>, --code-audit-mcp, --consumer, opencode, --tab, <id>]`.
///
/// `"offload"` is deliberately **absent**, even though
/// [`CodeAuditSettings::expose_offload`](crate::settings::schema::CodeAuditSettings)
/// exists: the offload worker is an *in-process* consumer of the audit surface
/// and never speaks to this route. `offload::tools::audit_tools::execute` calls
/// [`audit::mcp::run_audit`](crate::audit::mcp::run_audit) directly through
/// `audit::global()`, gated by `OffloadService::run_on` (`enabled` AND
/// `expose_offload` AND a local backend) and re-gated by `HostRouter::call`.
/// `CodeAuditSettings::mcp_exposed` states the same split from the other side
/// ("`expose_offload` is deliberately absent: the offload worker runs
/// in-process"). So `expose_offload` — which defaults **true** — was reachable
/// over HTTP only by a caller that no legitimate component ever is.
/// The consumers `/audit/run` serves — **the registry**, not a literal pair
/// (V40 Phase A). A harness added without a line here used to be refused by a
/// route it is entitled to, with a message naming two products it isn't one of.
fn audit_consumers() -> Vec<&'static str> {
    crate::harness::registry::harness_ids()
}

/// H-8: narrow `/audit/run`'s caller-asserted `consumer` to [`audit_consumers`]
/// at the parse boundary, returning the `&'static str` the rest of the route
/// uses. Same discipline `ada4bae` gave `/run`'s `tool` label
/// ([`offload_tool_name`]) and for a stronger reason: `tool` is only a label,
/// whereas this value **selects which `expose_*` toggle is checked**.
///
/// Absent/blank still means `"claude"`, which is the child's own default and
/// the pre-H-8 documented behaviour; only *unrecognized* values are refused.
/// (A child old enough to omit `consumer` predates `--tab` and is already
/// refused by [`audit_tab`], so the default is compatibility, not a hole.)
fn audit_consumer(raw: Option<&str>) -> Result<&'static str, String> {
    let raw = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            crate::harness::DEFAULT_HARNESS
                .id()
                .expect("DEFAULT_HARNESS names a registered harness")
        });
    let lower = raw.to_ascii_lowercase();
    audit_consumers()
        .into_iter()
        .find(|c| *c == lower)
        .ok_or_else(|| {
            format!(
                "code audit does not serve the consumer {raw:?} - this route serves the \
                 cimp-code-audit MCP child only ({})",
                audit_consumers().join(", ")
            )
        })
}

/// H-8: `/audit/run` requires a tab identity — a request without one is
/// **refused**, never treated as clean.
///
/// Both spawn paths have sent `--tab` since V32 C-1b (see [`audit_consumers`]),
/// so the only bodies this rejects are a hand-run child, a forged request, and
/// a *stale* child left over from a pre-C-1b build — which is why the message
/// names the remedy (restart the tab) rather than the symptom.
///
/// Trimming/emptiness is checked here rather than left to [`tab_identity`]
/// because `""` and `"   "` are exactly the shapes a caller would use to opt
/// itself back out of the gate.
fn audit_tab(raw: Option<&str>) -> Result<&str, String> {
    raw.map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| {
        "this code-audit MCP connection carries no cImp tab id, so the scan cannot be checked \
         against that tab's containment latch — restart this tab in cImp (its MCP child is from \
         an older build) and try again."
            .to_string()
    })
}

/// Everything `/audit/run` decides **before** the scan starts — all four
/// refusals, in the one order they may be taken — returning the validated
/// consumer on success. `Err(msg)` is the single `RunResult { ok: false, error }`
/// the route writes over HTTP 200, always *before* [`write_ndjson_head`].
///
/// It is one function, taking its dependencies as arguments, for two reasons:
/// the caller cannot reach [`LatchRegistry::gate`] without passing every check
/// first (so an added refusal cannot be inserted on the wrong side of the
/// gate), and the ordering below is testable without a `TcpStream` or an
/// `AppHandle`.
///
/// **Why this order** (each step's own note says what it decides):
///
/// 1. `consumer` — H-8. A *parse-boundary narrowing*: it must precede step 2
///    because it is what step 2 is keyed by. Engages nothing.
/// 2. `expose` — the per-consumer run-time re-gate. Kept ahead of the identity
///    and containment checks so a consumer the user has opted out still gets
///    the specific "not exposed" error rather than a containment refusal that
///    would not explain its situation. Engages nothing.
/// 3. `cwd` — the wrong-instance guard. Same reasoning: a misrouted request was
///    never going to run here, and its own error is the actionable one.
///    Engages nothing.
/// 4. `tab` — H-8. The identity half of the gate below, so it sits immediately
///    before it and shares its "a request that was never going to run does not
///    engage this tab's latch" property. Engages nothing: the refusal happens
///    before any [`LatchScope`] exists.
/// 5. the taint gate — the only step that may touch the registry, and therefore
///    last, exactly as V32 C-1b established.
fn audit_admit(
    reg: &LatchRegistry,
    body: &AuditRunBody,
    served_root: &Path,
    exposed: impl FnOnce(&str) -> bool,
    scope_of: impl FnOnce(&'static str, &str) -> LatchScoping,
    policy_of: impl FnOnce(Option<&LatchScope>) -> GatePolicy,
) -> Result<&'static str, String> {
    // 1. H-8: the caller-asserted `consumer` is narrowed to one of two known
    //    values before anything reads it — see `audit_consumer`.
    let consumer = audit_consumer(body.consumer.as_deref())?;

    // 2. Re-enforce this consumer's expose toggle at run time (see the route's
    //    doc comment): a still-registered child whose consumer has since been
    //    opted out gets a clean tool error, not a scan.
    if !exposed(consumer) {
        return Err(format!(
            "code audit is not exposed to {consumer} — re-enable it in cImp Settings → Code Audit"
        ));
    }

    // 3. Wrong-instance guard: the scan always runs against THIS app's launch
    //    root, so a child whose cwd falls outside it was misrouted (stale or
    //    foreign discovery entry — possible with several cImp instances off one
    //    install). A clean error beats silently auditing the wrong project.
    //
    //    **This is a routing guard, not a boundary, and must not be read as
    //    one** (V33, recorded so the asymmetry with `/context/post_edit` is not
    //    re-raised): `cwd` is `#[serde(default)]` for older children, so step 3
    //    is skipped outright when the field is absent — a caller holding the
    //    loopback token opts out of it by saying nothing. Nothing is gained by
    //    doing so either: passing this check does not choose what gets scanned;
    //    `served_root` does, and it comes from the app. What the two routes DO
    //    share is the path comparison itself, and its `..` refusal now lives in
    //    `is_ancestor_or_equal` so neither route can drift from the other.
    if let Some(child_cwd) = body.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let served = canon(served_root);
        if !is_ancestor_or_equal(&served, &canon(Path::new(child_cwd))) {
            return Err(format!(
                "this cImp instance serves {} — launch cImp in {} (or close the other instance) to audit it",
                served.display(),
                child_cwd
            ));
        }
    }

    // 4. H-8: identity is REQUIRED here. Before this, `tab` was caller-supplied
    //    and optional, absence resolved to `LatchScoping::Anonymous`, and
    //    `gate(None, ..)` returned `Ok(Clean)` without classifying anything —
    //    i.e. the whole containment gate below was opt-in by the caller it was
    //    meant to contain, and opting out was silent.
    let tab = audit_tab(body.tab.as_deref())?;

    // 5. V32 C-1b: the taint gate — the last thing checked before the scan
    //    starts. Identity is resolved through the same `latch_scope` funnel
    //    every other gated route uses (`scope_of`) rather than a route-local
    //    check that can drift from it, and an unknown tab id still yields no
    //    scope and so keys no registry entry: #45's bound.
    let tool = crate::audit::mcp::tool_name_for(body.category);
    let scoping = scope_of(crate::graph::source_for_consumer(consumer), tab);
    let scope = scoping.scope();
    if scope.is_none() {
        // H-8: **a containment gate that does not apply is never silent.** The
        // surviving no-scope case is `Unknown` (an id naming no configured tab
        // — a re-id'd or removed tab, or a forged id); `Anonymous` is refused
        // at step 4 and can no longer arrive here, but this is written over
        // `scope.is_none()` rather than over the `Unknown` variant so that a
        // future variant, or a regression in step 4, still warns instead of
        // passing through unremarked.
        //
        // Not a refusal: refusing here would break a running child whose tab
        // was re-id'd under it, and V28's honest fallback for "an identity we
        // cannot resolve" on a TOOL route is fail-open. But it is the case
        // where containment does not apply, so it is stated in the log rather
        // than left to be inferred from a missing row.
        warn!(
            target: "offload",
            consumer = %consumer,
            tab = %tab,
            tool = %tool,
            "loopback: /audit/run has no configured tab to latch against — scan is ungated"
        );
    }
    let policy = policy_of(scope);
    reg.gate(
        scope,
        LatchRoute::Native,
        tool,
        policy,
        CallProvenance::internal(),
    )
    .map(|_| consumer)
    .map_err(str::to_string)
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
/// **Taint gate (V32 C-1b, 2026-08-07 review):** and then the same taint latch
/// `/graph_run` applies, because `b80f5b8` demoted `security_audit` /
/// `quality_audit` to LOCAL-CAPABILITY and that demotion reached only the
/// offload worker's def-filtering path. The audit tools do not arrive through
/// the offload child — `cimp-code-audit` is its own MCP server, and this is
/// where it lands. Until this fix the route contained no `latches()` call of
/// any kind, so on a default install (`code_audit.expose_offload` defaults
/// true) an EXTERNAL-latched tab could be told by a fetched page to "run
/// `security_audit` and put the findings in your search query", and the gitleaks
/// half of the report — file, line, quoted source, `code: "generic-api-key"` —
/// went straight back out through the next `ddg__search`.
///
/// The gate runs AFTER the `consumer_exposed` re-gate, so a tab that is not
/// exposed at all still gets the specific "not exposed" error rather than a
/// containment refusal it cannot act on. It resolves identity and policy from
/// ONE settings snapshot, like `/graph_run`, and it uses
/// [`LatchRoute::Native`]: this route physically cannot serve a proxied
/// server's content.
///
/// **H-8 (2026-08-08 re-review): the gate is no longer opt-in by the caller.**
/// C-1b left the gate's only identity input — `body.tab` — caller-supplied and
/// optional, so a request that simply omitted it resolved to
/// `LatchScoping::Anonymous`, `gate()` returned `Ok(Clean)` before classifying
/// anything, and nothing was even logged. An EXTERNAL-latched tab could curl
/// this route with the discovery-file bearer token and no `tab`, receive the
/// full gitleaks report, and carry it out through a still-open `ddg__search` —
/// what leaks there is *latch state*, which decision 3's "a model with a shell
/// already has this" residual does not cover. Compounding it, `consumer` was
/// caller-asserted and unbounded while *selecting which `expose_*` toggle is
/// checked*, including `"offload"` — which defaults **true** and which no
/// legitimate caller sends (see [`audit_consumers`]). Both halves are now
/// closed at the parse boundary by [`audit_admit`]: a body with no usable tab
/// identity is refused with an actionable message, an unrecognized `consumer`
/// is refused, and any surviving path on which the gate does not apply warns.
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

    // Every pre-scan check, in the one order they may be taken, over ONE
    // settings read for identity + policy (the `/mcp/call` discipline) — see
    // [`audit_admit`], which owns the ordering rationale and the four refusal
    // messages.
    let settings = live_settings(app);
    let consumer = match audit_admit(
        latches(),
        &body,
        &state.root(),
        |c| state.consumer_exposed(c),
        |agent, tab| latch_scope(app, &settings, agent, Some(tab)),
        |scope| GatePolicy::resolve(&settings, scope),
    ) {
        Ok(c) => c,
        Err(msg) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(msg),
            };
            // 200 with `ok:false`, like every other tool-level error on this
            // route: the child renders `error` as the tool result. Sent BEFORE
            // `write_ndjson_head`, so this is a plain single-JSON body — which
            // the child's line reader already handles (`parse_result_line` over
            // the unterminated trailing line). A refusal written after the
            // ndjson head would corrupt the stream, so every refusal this route
            // can take is funnelled through this one arm.
            return write_json(stream, 200, &r).await;
        }
    };

    write_ndjson_head(stream, "audit").await?;

    // Run the scan concurrently with the heartbeat interval: whichever branch
    // fires, `run_audit` still owns clearing `scanning`.
    let run_fut = crate::audit::mcp::run_audit(&state, category, consumer, body.tab.as_deref());
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
        // #48 M-6: the report crosses the delivery boundary here — screened,
        // enveloped under the scanner preamble, headered if a layer fired. It is
        // a `RawReport`, not a `String`, so this call cannot be dropped by
        // omission (see `audit::mcp::RawReport`).
        //
        // Settings are re-read rather than reusing the `settings` snapshot
        // `audit_admit` gated on: a scan can run for minutes, and the envelope is
        // resolved **once per delivery** for exactly the reason
        // `spotlight::recall_envelope` is — the posture that applies is the one
        // in force when the text enters the conversation, not the one in force
        // when the scan was admitted.
        Ok(report) => RunResult {
            ok: true,
            text: Some(
                report
                    .deliver(crate::audit::mcp::Delivery {
                        settings: &live_settings(app),
                        scope: crate::settings::injection::Scope::for_tab(
                            crate::graph::source_for_consumer(consumer),
                            body.tab.as_deref(),
                        ),
                    })
                    .await,
            ),
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
pub(crate) struct ContextRetrieveBody {
    /// The calling session's working directory; the project root is resolved
    /// from it (defaults to `.`).
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    /// The user's prompt to rank context against.
    pub(crate) prompt: String,
    /// The agent session id (scopes the working-set boost); optional.
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    /// V13 Phase C: which agent shim is calling — `"claude"` (set by
    /// the Claude `UserPromptSubmit` route) or `"opencode"` (the generated plugin);
    /// absent/`None` for an unrecognized caller. Recorded on the checkpoint
    /// it triggers (see [`WorkbenchService::on_prompt`](crate::workbench::WorkbenchService::on_prompt)),
    /// not otherwise used by context retrieval itself.
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// V33: the cImp TAB this prompt belongs to — `--tab <id>` baked into the
    /// `--context-hook` command at spawn (`tabs::config`), or `CIMP_TAB_ID`
    /// from the generated OpenCode plugin. Recorded on the checkpoint this
    /// prompt triggers so the Timeline can tell two same-agent tabs on one
    /// project root apart; nothing about context retrieval reads it.
    ///
    /// `#[serde(default)]` because a hook shim from an older build sends no
    /// such field, and a prompt must never fail for lack of identity — the
    /// checkpoint is simply written without a tab, exactly as before.
    #[serde(default)]
    pub(crate) tab: Option<String>,
}

/// V33: the conversation identity recorded on the prompt-tap checkpoint this
/// route fires — the join key between a Timeline row and a
/// `Screen::Contamination` activity row.
///
/// **The tab id goes through [`tab_identity`]**, the same #45 narrowing every
/// other identity-taking route uses, so only a *configured AI tab id* is ever
/// recorded. A `tab` naming no configured tab is a forged or stale claim, and
/// writing it into a checkpoint would put a fabricated attribution on a record
/// whose whole purpose is to be trusted after an incident. It degrades to no
/// tab — which reads as "cannot attribute this checkpoint", not as "some other
/// tab". `Anonymous` (a hook shim from a build before `--tab` was baked in, or
/// an OpenCode plugin file not yet regenerated) lands in the same place, which
/// is exactly the pre-V33 row.
///
/// `session_id` and `agent` are recorded as sent. They are equally
/// caller-asserted, but neither can widen anything: they are compared for
/// equality against a contamination row and nothing else, and the framing
/// hazard they carry is handled where it lives — at the commit-trailer write
/// boundary (`workbench::shadow`'s `trailer_identity`).
///
/// Split out of the handler so the narrowing is exercised by a test rather than
/// re-implemented in one: a test that owned its own copy of this mapping would
/// stay green if the handler stopped calling it.
fn checkpoint_origin(
    settings: &crate::settings::Settings,
    body: &ContextRetrieveBody,
) -> crate::workbench::shadow::Origin {
    checkpoint_identity(
        settings,
        body.agent.as_deref(),
        body.session_id.as_deref(),
        body.tab.as_deref(),
    )
}

/// The narrowing itself, shared by the prompt-tap trigger above and V33 Phase
/// F's pre-tool trigger ([`handle_tool_checkpoint`]) — ONE spelling, so the two
/// checkpoint writers cannot come to disagree about which `tab` claims are
/// believed.
fn checkpoint_identity(
    settings: &crate::settings::Settings,
    agent: Option<&str>,
    session_id: Option<&str>,
    tab: Option<&str>,
) -> crate::workbench::shadow::Origin {
    // V33 C5: the id is checked against the tabs of the consumer the body
    // asserts, normalised through the same `hook_agent` funnel the gated hook
    // routes use — a `tab` that names another harness's tab is a forged or
    // stale claim exactly as an invented one is, and lands in the same place.
    let tab = match tab_identity(settings, hook_agent(agent), tab) {
        TabIdentity::Configured(tab) => Some(tab.to_string()),
        TabIdentity::Anonymous | TabIdentity::Unknown(_) => None,
    };
    crate::workbench::shadow::Origin::new(
        agent.map(str::to_string),
        session_id.map(str::to_string),
        tab,
    )
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
    let answer = context_retrieve_core(app, &body).await;
    write_json(stream, 200, &answer).await
}

/// **The context-retrieval budget**: how long [`context_retrieve_core`] waits
/// for a fresh digest before answering without it.
///
/// Two ceilings sit above this number, and it must clear BOTH with margin,
/// because past either one the whole reply — greeting, drained auto-check
/// block, and any parked backlog just TAKEN from the store — is discarded
/// *after* those destructive reads already happened:
///
/// - the Claude harness discards the hook's reply outright at 1 s
///   ([`crate::harness::claude::hook::TIMEOUT_SECS`]);
/// - the OpenCode plugin aborts its `/context/retrieve` fetch at **600 ms**
///   (`AbortSignal.timeout(600)` in `templates/plugin.js` — the five deleted
///   shims' own `context_hook::TIMEOUT` number, "a slow/cold index never
///   delays the prompt").
///
/// 500 ms leaves the tighter (OpenCode) ceiling ~100 ms for composing and
/// writing the reply plus the plugin's own fetch overhead. A budget AT 600
/// would lose that race on exactly the timeout path it exists to serve: the
/// reply would leave at ~600 ms + ε, arrive after the client abort, and a
/// backlog already drained out of the park store would be gone for good —
/// on a chronically slow project the OpenCode transport would deliver
/// nothing, ever, while consuming everything.
///
/// The race exists because the measured cost is not the index: on a project
/// with `semantic_search` on and a remote embedding endpoint,
/// `retrieve_context` spends **0.67–2.5 s** in a blocking embed round trip
/// inside this handler. Before the race the handler lost that reply on
/// essentially every prompt while still having consumed the session's
/// once-per-session greeting, marked the dedup ledger injected and drained the
/// parked auto-check block — spending the state and delivering nothing.
///
/// Over budget the result is not discarded, it is parked for the next prompt
/// (`GraphService::park_injection`), the same bargain
/// [`GraphService::post_edit`](crate::graph::GraphService::post_edit) strikes
/// against `POST_EDIT_BUDGET_MS`. A test pins this below BOTH ceilings — the
/// constants live in different files (one of them in a JS template) and
/// nothing else keeps them ordered.
const RETRIEVE_BUDGET_MS: u64 = 500;

/// Join the pieces of one injection reply with a blank line, skipping any that
/// are empty (or whitespace-only).
///
/// Extracted from [`context_retrieve_core`] so the ORDER — greeting, parked
/// blocks, fresh digest, drained auto-check — is asserted by a test rather than
/// re-read out of a chain of `if`s. Parts are joined verbatim: a block's own
/// content is never rewritten here.
fn merge_injection_blocks(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.trim().is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The reply's `files`: parked first (they were retrieved first), then fresh,
/// de-duplicated preserving that order.
fn merge_files_used(parked: Vec<String>, fresh: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(parked.len() + fresh.len());
    for f in parked.into_iter().chain(fresh) {
        if !out.contains(&f) {
            out.push(f);
        }
    }
    out
}

/// The prompt tap's whole effect, shared by `/context/retrieve` (the CHP body a
/// pre-upgrade shim or the OpenCode plugin posts) and
/// [`crate::harness::claude::hook::ROUTE_USER_PROMPT_SUBMIT`] (the raw `UserPromptSubmit` payload
/// the harness posts since V35 Phase J).
///
/// Extracted rather than duplicated: this is the only place the checkpoint
/// trigger fires, the injection gate is read and the digest is composed, so the
/// two transports cannot come to disagree about what a prompt does. Returns the
/// `/context/retrieve` answer verbatim — the Claude-native handler takes `text`
/// out of it and wraps it in the hook-output envelope.
///
/// **The retrieval itself is raced against [`RETRIEVE_BUDGET_MS`]** (2026-08-17
/// fix). `GraphService::retrieve_context` is sync and blocking — SQLite plus,
/// with semantic search on, a remote embed round trip measured at 0.67–2.5 s —
/// so it runs on `spawn_blocking` and this function answers at the budget
/// whether or not it is done. A digest that misses the budget is PARKED for the
/// next prompt (`GraphService::park_injection`) and the reply carries whatever
/// was parked by an earlier prompt, so nothing computed is thrown away.
///
/// What must NOT be parked rides the immediate reply unconditionally: the
/// once-per-session greeting and the drained auto-check block are destructive
/// reads (consumed exactly once), and both are cheap — no embed, no network —
/// so a slow retrieval can never cost the session its project map.
pub(crate) async fn context_retrieve_core(app: &AppHandle, body: &ContextRetrieveBody) -> serde_json::Value {
    // #104: both consumers below create per-project state — the workbench's
    // `<db_subdir>/shadow.git` and the graph store — so the payload's `cwd` is
    // resolved to a real root first. `None` refuses BOTH: no checkpoint and no
    // retrieval for a directory that is no project (`empty` is this route's
    // established "nothing to say" answer).
    let Some(cwd) = external_project_root(app, &live_settings(app), body.tab.as_deref(), body.cwd.as_deref())
    else {
        return serde_json::json!({ "ok": true, "text": "", "files": [], "tokens_est": 0 });
    };

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
            // V33: the identity the Timeline is joined on. The settings read
            // sits INSIDE the `checkpoints_enabled` gate for FIX 8's reason —
            // a user with checkpoints off pays nothing for this.
            let origin = checkpoint_origin(&live_settings(app), body);
            let prompt_head: String = body.prompt.chars().take(80).collect();
            tauri::async_runtime::spawn(async move {
                workbench.on_prompt(&root, origin, &prompt_head).await;
            });
        }
    }

    let empty = serde_json::json!({ "ok": true, "text": "", "files": [], "tokens_est": 0 });
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return empty;
    };
    let graph = graph.inner().clone();
    // The injection toggle is enforced here (the service's retrieve does not) so
    // the preview surface can reuse the same core while injection is off.
    if !graph.context_injection_enabled() {
        return empty;
    }
    let sid = body.session_id.clone().filter(|s| !s.is_empty());

    // A digest an EARLIER prompt's retrieval finished too late to deliver is
    // part of THIS reply. Taken before the race so that a fresh result which
    // also misses the budget parks BEHIND it rather than racing it — the store
    // is oldest-first and this keeps that ordering true.
    let parked = graph.take_parked_injection(sid.as_deref());

    // The slow part, off the async runtime's worker: `retrieve_context` is
    // blocking (SQLite + a blocking HTTP embed), and blocking a runtime thread
    // for seconds would stall every other loopback route, not just this one.
    let mut handle = {
        let graph = graph.clone();
        let root = cwd.clone();
        let prompt = body.prompt.clone();
        let sid = sid.clone();
        tokio::task::spawn_blocking(move || graph.retrieve_context(&root, &prompt, sid.as_deref()))
    };
    // The deadline is taken HERE, before the cheap work below, so the cheap
    // work is overlapped with the retrieval instead of being added on top of
    // the budget.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(RETRIEVE_BUDGET_MS);

    // V11 Phase B: the once-per-session project map. Done here (the real
    // injection path), not in `retrieve_context`, so the preview surface —
    // which also calls `retrieve_context` — never consumes the once-per-session
    // flag. Synchronous and unraced on purpose: it is a once-per-session
    // destructive read with no embed in it, so it must ride this reply.
    let greeting = graph
        .session_greeting(&cwd, sid.as_deref())
        .unwrap_or_default();
    // V12 Phase F: drain any auto-check block a slow post-edit run parked for
    // this session (see `GraphService::post_edit`'s budget/park path) — a
    // turn is never blocked waiting for a check, but its result still reaches
    // the model on the very next opportunity. Destructive too: never parked,
    // never lost to a slow retrieval.
    let pending_check = graph.drain_auto_check(sid.as_deref()).unwrap_or_default();

    let fresh = tokio::select! {
        res = &mut handle => res.ok(),
        _ = tokio::time::sleep_until(deadline) => {
            match sid.clone() {
                Some(s) => {
                    let graph = graph.clone();
                    tauri::async_runtime::spawn(async move {
                        // Bound the parked run for `post_edit`'s reason: a
                        // wedged embedding endpoint must not leave a task (and
                        // a blocking-pool thread) alive for the process's
                        // lifetime. On timeout we stop waiting — `abort` on a
                        // `spawn_blocking` handle cannot interrupt the closure
                        // itself, so this bounds the reaper, not the blocking
                        // call, which is the part that would otherwise leak.
                        const PARKED_MAX_MS: u64 = 60_000;
                        match tokio::time::timeout(
                            Duration::from_millis(PARKED_MAX_MS),
                            &mut handle,
                        )
                        .await
                        {
                            Ok(Ok(r)) => graph.park_injection(Some(&s), &r.context_md, r.files_used),
                            Ok(Err(_join_err)) => {}
                            Err(_elapsed) => handle.abort(),
                        }
                    });
                }
                // No session id to park under — both real transports always
                // send one, so this is the preview-shaped edge case: drop it.
                None => debug!(
                    target: "offload",
                    "context retrieve missed its budget with no session id to park under"
                ),
            }
            None
        }
    };
    let (fresh_text, fresh_files) = match fresh {
        Some(r) => (r.context_md, r.files_used),
        None => (String::new(), Vec::new()),
    };

    let (parked_text, parked_files) = parked.unwrap_or_default();
    let text = merge_injection_blocks(&[&greeting, &parked_text, &fresh_text, &pending_check]);
    let files = merge_files_used(parked_files, fresh_files);
    // Same char→token estimate as the retrieval core (shared divisor so the two
    // can't drift). Estimated from the FULL injected text (greeting + parked +
    // digest + drained auto-check), not just the digest.
    let tokens_est = crate::graph::est_tokens(text.chars().count());
    serde_json::json!({ "ok": true, "text": text, "files": files, "tokens_est": tokens_est })
}

/// A `POST /workbench/tool_checkpoint` request body — V33 Phase F's two
/// out-of-process fire seams: the Claude `PreToolUse` shim
/// (`crate::checkpoint_beacon`) and the OpenCode `tool.execute.before` plugin
/// hook. The worker seam does NOT come through here; it calls
/// `WorkbenchService::on_tool` directly (`offload::tools::dispatch`).
#[derive(Deserialize)]
struct ToolCheckpointBody {
    /// The calling session's working directory — the project root the shadow
    /// repo lives under. Defaults to `.` like every other hook route.
    #[serde(default)]
    cwd: Option<String>,
    /// Which harness is calling: `"claude"` / `"opencode"`. Normalised through
    /// [`hook_agent`], and it selects WHICH tool vocabulary the name below is
    /// checked against — the two namespaces are disjoint and must not be
    /// crossed.
    #[serde(default)]
    agent: Option<String>,
    /// The cImp TAB, baked into the hook command / the plugin file at spawn.
    /// Narrowed through [`checkpoint_identity`]; an unrecognised id degrades to
    /// "no tab", never to another tab.
    #[serde(default)]
    tab: Option<String>,
    /// The harness's own session id, recorded as sent.
    #[serde(default)]
    session_id: Option<String>,
    /// The tool about to run, in the CALLER's vocabulary (`Bash`, `edit`).
    /// Required — a checkpoint with no tool name is not a Phase F checkpoint.
    #[serde(default)]
    tool: Option<String>,
}

/// **The pre-tool checkpoint budget** (2026-08-13 amendment, locked): how long
/// [`handle_tool_checkpoint`] lets a snapshot run before it abandons it
/// unwritten.
///
/// Deliberately **below** the ~2 s both out-of-process callers wait — the Claude
/// shim's [`checkpoint_beacon::REPLY_TIMEOUT`](crate::checkpoint_beacon) and the
/// OpenCode plugin's `AbortSignal.timeout(2000)`. The ordering is the whole
/// point: the harness starts the tool the instant its hook stops waiting, so if
/// the caller's timer fired first the app would still be staging *into* the tool
/// call while believing it had a valid pre-tool checkpoint. Keeping the app's
/// budget under the caller's makes the app's own answer the one that decides,
/// and leaves ~200 ms for the reply to be written and read.
///
/// Not a per-call latency budget for the *user*: the throttle means most calls
/// never reach a snapshot at all, and this bound is only reached by a
/// `git add -A` over a work tree big enough to take seconds.
/// **V40 Phase C, locked decision 22 — this is no longer a number typed here.**
/// It was `Duration::from_millis(1800)`, hand-computed from two artifacts'
/// timers and asserted against both by a cross-file test, which meant a THIRD
/// harness with a shorter timer would have been silently over-run. Core now
/// derives it as `min(every plugin's declared `hook_reply_timeout`) − margin`
/// ([`crate::harness::ingress::hook_reply_budget`]); the shipped pair still
/// implies 1800 ms and a test pins that, so the behaviour is unchanged and the
/// derivation is what moved.
pub(crate) fn tool_checkpoint_budget() -> Duration {
    crate::harness::ingress::hook_reply_budget()
}

/// V33 Phase F: does `tool` change files on disk, **in `harness`'s own tool
/// vocabulary**?
///
/// Split out of [`handle_tool_checkpoint`] so the namespace selection is
/// exercised by a test rather than re-implemented in one — a test that owned its
/// own copy of this lookup would stay green after the handler stopped calling it.
///
/// **V40 Phase A, locked decision 16.** This was a `match` with `"opencode"` in
/// one arm and Claude's table in the `_` arm, which meant a THIRD harness's
/// `edit` was not rejected but answered out of Claude's vocabulary — `false`,
/// silently, for its whole mutation surface. It is now one registry lookup that
/// fails CLOSED: a token naming no registered harness, and a name the registered
/// plugin does not declare, both answer `true`. A checkpoint nobody needed is a
/// commit into cImp's own shadow repo; a missed one is a destructive call with
/// no way back.
fn tool_checkpoint_is_mutating(harness: &str, tool: &str) -> bool {
    crate::harness::native::mutates_fs(crate::harness::HarnessId::from_id(harness), tool)
}

/// The identity check `POST /workbench/tool_checkpoint` makes before anything is
/// staged — the harness token this call is attributed to, or the refusal.
///
/// **V40 review finding M-6 (parity lens).** The route's own doc claims a
/// forged POST "cannot get a destructive call waved through by naming a harness
/// cImp does not know", and the opposite was true: an unregistered token
/// resolves to `UNKNOWN_SOURCE`, `mutates_fs` fails CLOSED for it — which is
/// right for a name inside a known harness's vocabulary and wrong for a source
/// that has no vocabulary — so EVERY tool name from an unidentified caller was
/// "mutating" and minted a snapshot attributed to `unknown:<whatever>`. Bounded
/// by the throttle and the tree-sha dedup, but a checkpoint is the one record
/// that exists to be trusted after an incident, and an unattributable row in it
/// is worse than no row.
///
/// An ABSENT `agent` is a different question with a different answer: it is a
/// shim from a build before the field existed, and it resolves to
/// [`crate::harness::DEFAULT_HARNESS`] exactly as every other hook body does.
///
/// Split out so the decision is testable without a `TcpStream`.
fn checkpoint_source_admits(agent: Option<&str>) -> Result<&'static str, String> {
    let harness = hook_agent(agent);
    if crate::harness::HarnessId::from_id(harness).is_some() {
        return Ok(harness);
    }
    Err(format!(
        "`agent` names no registered harness ({}), so this call cannot be attributed to one. A          checkpoint is the record a restore is judged against; an unattributable row in it is          worse than no row.",
        crate::harness::registry::harness_ids().join(", ")
    ))
}

/// `POST /workbench/tool_checkpoint` (V33 Phase F): take a Workbench checkpoint
/// **immediately before** a filesystem-mutating tool call, attributed to that
/// exact call.
///
/// # Why identity and the tool name are both re-checked here
///
/// **Identity first** (V40 review M-6): a body whose `agent` names no
/// registered harness is refused with a 400, because `mutates_fs` fails CLOSED
/// for an unknown vocabulary — which is right for a name inside a known
/// harness's table and wrong for a source that has no table, where it made
/// EVERY tool name mutating. See [`checkpoint_source_admits`].
///
/// Both callers pre-filter — Claude's hook is installed with an
/// `Edit|Write|MultiEdit|Bash` matcher, the plugin consults a baked
/// `CIMP_MUTATING_TOOLS` set — but neither is the authority. This route resolves
/// the name against the SENDING harness's own reviewed table, through
/// [`crate::harness::native::mutates_fs`]. A drifted matcher, a shim from a
/// newer build, or a forged POST from any local process therefore cannot mint a
/// checkpoint for a tool that harness declares non-mutating — and cannot get a
/// destructive call waved through by naming a harness cImp does not know.
///
/// **Crossing the two vocabularies would be silent, not loud**: `edit` and
/// `Edit` are unknown in each other's tables, so one crossed lookup would
/// disable a whole harness's seam while every test that only exercised the other
/// stayed green. The registry lookup is what makes crossing them impossible to
/// express.
///
/// # Containment posture
///
/// Behind the same bearer token as every other route, and **it takes no taint
/// gate** — deliberately. A checkpoint is a `git add -A` + `commit-tree` into
/// cImp's OWN shadow repo; it returns no project data to the caller (the reply
/// is `{ok, checkpointed}`, two booleans) and grants no capability, so there is
/// nothing for a latch to refuse. The abuse case for a forged POST is a spurious
/// snapshot, which the per-`(root, tab)` min-gap throttle and the tree-sha dedup
/// both bound — and which, unlike a refusal, costs the user nothing they wanted.
/// Gating it would instead mean that a tab which had touched external content
/// stopped getting checkpoints exactly when they matter most.
///
/// # The pre-tool budget (2026-08-13 amendment, locked)
///
/// **Both callers of this route stop waiting after ~2 s** — the Claude shim on
/// its reply-read timeout, the OpenCode plugin on its `AbortSignal.timeout(2000)`
/// — and the harness runs the tool the moment they do. A snapshot still staging
/// past that point is racing the very edit it exists to precede, so this route
/// hands [`WorkbenchService::on_tool`](crate::workbench::WorkbenchService::on_tool)
/// a deadline of [`tool_checkpoint_budget`] and the snapshot writes **nothing**
/// once it is spent. The alternative — let the caller give up and let the app
/// commit the row anyway — is the failure this amendment exists to close: a
/// checkpoint that sometimes contains the change it claims to predate silently
/// misleads a restore, which is strictly worse than having none.
///
/// The budget is deliberately *under* the callers' 2 s so the app's own answer
/// is what decides, rather than whichever timer happens to fire first.
///
/// The worker seam does not come through here and gets no deadline: it is
/// in-process and waits as long as the snapshot takes.
///
/// # The reply, and what it is not
///
/// `{ "ok": true, "checkpointed": <bool> }`. `checkpointed` deliberately does
/// not return a checkpoint id, because on a dedup hit the id would name another
/// trigger's (possibly another tab's) checkpoint and no caller may claim it (see
/// [`shadow::SnapshotOutcome`](crate::workbench::shadow::SnapshotOutcome)).
///
/// It means *"the trigger settled — nothing about this call is unaccounted
/// for"*: true for a checkpoint created, for a dedup hit, and for a throttled
/// call (whose tab already has a checkpoint newer than `checkpoint_min_gap_s`).
/// **False now also covers the one new case: the snapshot was abandoned against
/// the budget above**, i.e. exactly when no checkpoint can be said to precede
/// this call. Neither caller gates anything on it — the Claude shim reads it
/// only to make its wait mean something, the OpenCode plugin awaits it for the
/// ordering — so the user-facing report of a miss is the Activity row
/// `workbench` / `checkpoint_missed`, not this boolean.
async fn handle_tool_checkpoint(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: ToolCheckpointBody = match serde_json::from_slice(&req.body) {
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
    let Some(tool) = body
        .tool
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return write_json(
            stream,
            400,
            &serde_json::json!({ "ok": false, "error": "missing `tool`" }),
        )
        .await;
    };
    // V40 review M-6: identity BEFORE anything is staged. See
    // `checkpoint_source_admits`.
    if let Err(msg) = checkpoint_source_admits(body.agent.as_deref()) {
        return write_json(
            stream,
            400,
            &serde_json::json!({ "ok": false, "error": msg }),
        )
        .await;
    }
    let checkpointed = tool_checkpoint_core(
        app,
        &live_settings(app),
        body.agent.as_deref(),
        tool,
        body.cwd.as_deref(),
        body.session_id.as_deref(),
        body.tab.as_deref(),
    )
    .await;
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "checkpointed": checkpointed }),
    )
    .await
}

/// **The pre-tool checkpoint itself** — the core both out-of-process fire seams
/// reach: this route's harness-neutral body (the OpenCode plugin) and
/// [`crate::harness::claude::hook::ROUTE_PRE_TOOL_USE_CHECKPOINT`]'s Claude hook payload.
///
/// Split out on 2026-08-17, when the Claude side stopped being a shim POSTing to
/// the route and became a handler beside it. One core, so the two transports
/// cannot come to disagree about the tool-name re-check, the enabled switch, the
/// identity narrowing or the deadline — the property
/// `both_transports_of_a_capability_call_one_core` asserts and the reason the
/// Claude migration is a relocation rather than a second implementation.
///
/// Returns `checkpointed`: the trigger settled and nothing about this call is
/// unaccounted for — true for a checkpoint created, a dedup hit and a throttled
/// call; false for a non-mutating name, checkpoints off, no service, or a
/// snapshot abandoned against [`tool_checkpoint_budget`]. `settings` is passed in
/// rather than read here so a handler resolves identity and policy under ONE
/// snapshot.
pub(crate) async fn tool_checkpoint_core(
    app: &AppHandle,
    settings: &crate::settings::Settings,
    agent: Option<&str>,
    tool: &str,
    cwd: Option<&str>,
    session_id: Option<&str>,
    tab: Option<&str>,
) -> bool {
    // Normalized for the two decisions that must not cross the harnesses' tool
    // vocabularies, and passed on RAW to `checkpoint_identity`, which records the
    // caller's own spelling in the commit trailer exactly as it always has.
    let harness = hook_agent(agent);
    if !tool_checkpoint_is_mutating(harness, tool) {
        // Not an error: a caller whose matcher is wider than the table is
        // over-reporting, and a fail-open sensor must never learn to treat that
        // as a failure. No log line either — this is reachable once per
        // non-mutating matched call and would be unbounded chatter.
        return false;
    }
    let Some(workbench) = app.try_state::<Arc<crate::workbench::WorkbenchService>>() else {
        return false;
    };
    let workbench = workbench.inner().clone();
    if !workbench.checkpoints_enabled() {
        return false;
    }
    // #104: the checkpointer creates `<root>/<db_subdir>/shadow.git`, so a cwd
    // that resolves to no project takes no checkpoint rather than minting a
    // shadow repo inside one.
    let Some(root) = external_project_root(app, settings, tab, cwd) else {
        return false;
    };
    let origin = checkpoint_identity(settings, agent, session_id, tab);
    // `harness:tool_name` — the locked value format. `bounded_id` caps the
    // caller-supplied half before it reaches a commit trailer; `trailer_identity`
    // rejects the framing hazards at the write boundary, and an over-long value
    // there would be dropped WHOLE, losing the harness prefix too.
    let source = format!("{harness}:{}", bounded_id(tool));
    // AWAITED, unlike the prompt-tap trigger's fire-and-forget spawn: the point
    // of this trigger is that the snapshot precedes the mutation, and both fire
    // seams hold the tool call until this returns (the OpenCode plugin awaits
    // its POST inside `tool.execute.before`; a Claude `PreToolUse` http hook
    // blocks the call until the handler answers). The wait is bounded twice over
    // — by the throttle, which admits at most one snapshot per
    // `checkpoint_min_gap_s` per `(root, tab)` and is what makes the common case
    // free, and by the budget below, which stops a slow one from outliving the
    // caller's patience and minting a row for a tool call that has already run.
    workbench
        .on_tool(
            &root,
            origin,
            &source,
            Some(Instant::now() + tool_checkpoint_budget()),
        )
        .await
}

// ── #48, finding M-7: the three `/context/*` hook routes' taint gate ───────
//
// The finding: `/context/post_edit`, `/context/should_read` and
// `/context/compaction` reached local capability with no `latches()` call of
// any kind and no `tab` in their bodies. `post_edit` in particular EXECUTES the
// project's configured check commands. "Only our own shim calls this route" is
// a convention, not a security property: the listener is reachable by any
// process running as this user, and the bearer token is in a discovery file
// that same process can read.
//
// The fix has two halves, and both were needed — a gate with no identity to
// resolve would have been ceremony:
//
// 1. **Identity now rides the wire.** `--tab <id>` is baked into all three hook
//    commands at spawn (`tabs::config::build_pre_args`, the same treatment
//    `--context-hook` and `--taint-beacon` already had), the shims forward it,
//    and the generated OpenCode plugin sends `CIMP_TAB_ID` on its `post_edit`
//    POST.
// 2. **The gate**, below — `/graph_run`'s shape, on [`LatchRoute::Hook`].
//
// **The residual, stated rather than papered over:** a body with no usable
// `tab` still resolves to no scope and is ADMITTED. That is the locked
// fail-open posture every tool-serving route here takes (`latch_scope`), and
// tightening it is the open decision F-5/H-8 tracks, not something to settle
// route-by-route: a shim from a build before this commit sends no `tab`, and
// failing closed would silently disable the read advisor and auto-check for
// every such tab. So a forged POST that simply omits `tab` is ungated here,
// exactly as it is on `/graph_run` and `/mcp/call`.

/// The class-table name `POST /context/post_edit` gates under. See its
/// [`toolclass::TABLE`] row for why it is LOCAL-CAPABILITY.
pub(crate) const HOOK_TOOL_POST_EDIT: &str = "hook_post_edit";
/// The class-table name `POST /context/should_read` gates under.
pub(crate) const HOOK_TOOL_SHOULD_READ: &str = "hook_should_read";
/// The class-table name `POST /context/compaction` gates under. TRUSTED, so
/// this gate admits every call today — see the row.
pub(crate) const HOOK_TOOL_COMPACTION: &str = "hook_compaction";

/// V39 Phase B: the class-table name `POST /delegate` gates under.
///
/// The route's own identity, not a name from the request — the model names
/// `delegate_task_<harness>` on the child, which resolves the harness id and
/// forwards only that. LOCAL-CAPABILITY, so a conversation that has ingested
/// untrusted content is REFUSED here, exactly as it is on `offload_task`
/// (V32 C-1c): both hand a task to a fresh, permissive executor, and this one's
/// executor is a whole peer harness.
const DELEGATE_TOOL: &str = "delegate_task";

/// The taint decision `POST /delegate` takes before any tab is touched — the
/// [`hook_admit`] shape, for the same two reasons: a handler cannot reach the
/// capability without passing through it, and the decision is testable without
/// a `TcpStream` or an `AppHandle`.
///
/// `Err(refusal)` means *this conversation may not delegate*. Unlike a hook,
/// the refusal is returned to the caller verbatim: this IS a tool call the
/// model made, so the model is the right audience for the reason — the same
/// treatment `/run` gives a refused `offload_task`.
///
/// [`LatchRegistry::gate`] writes the `Screen::LatchRefusal` row, so the
/// refusal has its user-visible consumer without this function minting one.
fn delegate_admit(
    reg: &LatchRegistry,
    // The class-table identity to gate under — always `DELEGATE_TOOL`.
    // Passed rather than hardcoded, exactly as `hook_admit` takes its `tool`:
    // the name a route gates under has to be readable AT the route, or "which
    // boundary is this handler behind" becomes a question you answer by
    // following a call.
    tool: &'static str,
    agent: &'static str,
    tab: Option<&str>,
    scope_of: impl FnOnce(&'static str, Option<&str>) -> LatchScoping,
    policy_of: impl FnOnce(Option<&LatchScope>) -> GatePolicy,
) -> Result<(), &'static str> {
    let scoping = scope_of(agent, tab);
    let scope = scoping.scope();
    let policy = policy_of(scope);
    // `CallProvenance::http()`, like the hooks and unlike `/run`: this is a
    // POST from a local process holding a launch token that anything running as
    // this user can read, so it is never evidence that cImp itself decided the
    // call (#45's reasoning for the beacon route). It reaches no row today —
    // provenance is read only for an admitted EXTERNAL call and this name is
    // LOCAL-CAPABILITY — but stating it is how the wrong origin avoids being
    // inherited later.
    reg.gate(
        scope,
        LatchRoute::Delegation,
        tool,
        policy,
        CallProvenance::http(),
    )
    .map(|_| ())
}

/// The taint decision the three `/context/*` hook routes take before they reach
/// [`crate::graph::GraphService`], as one function taking its dependencies as
/// arguments — the [`audit_admit`] shape, for the same two reasons: a handler
/// cannot reach capability without passing through it, and the decision is
/// testable without a `TcpStream` or an `AppHandle` (this crate has no
/// `tauri::test` mock — see [`latch_state_reply`]).
///
/// `Err(refusal)` means *this conversation may not have this*. Every caller
/// answers it with the route's own fail-safe reply — empty text, or a `pass`
/// verdict — and never with the refusal string: these are hooks, and a hook
/// that returns an error perturbs the turn it was supposed to be invisible to.
/// The refusal is not silent even so: [`LatchRegistry::gate`] writes the
/// [`Screen::LatchRefusal`](outbound::Screen) row (once per scope) that gives
/// it a user-visible consumer.
///
/// `agent` is caller-asserted, exactly as `consumer` is on `/graph_run`. It
/// selects which agent's key the scope is built under and nothing else; F-4
/// (`(consumer, tab)` is a verified pair on no route) is unchanged here, not
/// worked around.
/// **The hook gate, as one call a plugin can make** (V40 Phase C).
///
/// [`hook_admit`]'s signature names `LatchRegistry`, `LatchScoping`,
/// `LatchScope` and `GatePolicy` — four private types, three of them closures'
/// arguments. That is the right shape for the callers inside this file and the
/// wrong one for a plugin route: the latch model is core's, and a harness's
/// ingress must be able to ask "may this hook run?" without being handed the
/// machinery that answers.
///
/// Same decision, same ledger, same `CallProvenance::http()`; only the surface
/// is narrower. `false` means refused.
pub(crate) fn hook_gate_admits(
    app: &AppHandle,
    settings: &crate::settings::Settings,
    tool: &'static str,
    agent: Option<&str>,
    tab: Option<&str>,
) -> bool {
    hook_admit(
        latches(),
        tool,
        hook_agent(agent),
        tab,
        |agent, tab| latch_scope(app, settings, agent, tab),
        |scope| GatePolicy::resolve(settings, scope),
    )
    .is_ok()
}

fn hook_admit(
    reg: &LatchRegistry,
    tool: &'static str,
    agent: &'static str,
    tab: Option<&str>,
    scope_of: impl FnOnce(&'static str, Option<&str>) -> LatchScoping,
    policy_of: impl FnOnce(Option<&LatchScope>) -> GatePolicy,
) -> Result<(), &'static str> {
    let scoping = scope_of(agent, tab);
    let scope = scoping.scope();
    let policy = policy_of(scope);
    // `CallProvenance::http()`, not `internal()`: this is a POST from a local
    // process, and the launch token is readable by anything running as this
    // user, so it is never evidence that cImp itself decided the call (#45's
    // reasoning for the beacon route). It reaches no row today — provenance is
    // read only when an admitted call is EXTERNAL, and no name gated here is —
    // but stating it by omission is how the wrong origin gets inherited later.
    reg.gate(
        scope,
        LatchRoute::Hook,
        tool,
        policy,
        CallProvenance::http(),
    )
    .map(|_| ())
}

/// The agent key a hook body's caller-asserted `agent` resolves to. Absent ⇒
/// `claude`: `--precompact-hook` and `--read-hook` are installed only into
/// Claude's settings overlay, and a `post_edit` body with no `agent` is a shim
/// from a build before this field existed, which was a Claude shim.
///
/// **EMPTY counts as absent** (V40 review finding M-4). `identity_of_request`
/// answers `req.cimp.agent.clone().unwrap_or_default()` and
/// `chp::Envelope::agent_token` answers `.unwrap_or("")`, so an artifact from
/// before the discriminator existed arrives here as `Some("")`, not `None`. On
/// develop `source_for_consumer("")` was `"claude"` and the guard never
/// mattered; since V40 it is `UNKNOWN_SOURCE`, which names no configured tab
/// and fails every gate open. Whitespace-only counts as empty, so `" "` cannot
/// walk past it either — but a token that has any content is passed through
/// UNTRIMMED, because `HarnessId::from_consumer`'s no-trim is a locked decision
/// and `" opencode "` must keep answering `unknown`.
fn hook_agent(agent: Option<&str>) -> &'static str {
    crate::graph::source_for_consumer(
        agent
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(crate::harness::DEFAULT_HARNESS.token()),
    )
}

/// The agent key an identity-less body on `route` resolves to — [`hook_agent`]
/// for the routes whose default is the ROUTE's, not the app's.
///
/// One funnel for the three places that read a body-supplied discriminator and
/// fall back to [`crate::harness::ingress::wire_default`] (`/memory/event`,
/// `/latch/state`, and the CHP observer), so the handler and the observer on one
/// request cannot answer differently — the disagreement V40 review M-4 found on
/// `/memory/event`, where the handler read `opencode` and `note_chp` read
/// `unknown` from the same bytes.
///
/// Empty is absent here for the same reason it is in [`hook_agent`]; an
/// unresolvable token stays `UNKNOWN_SOURCE`, which is V40's deliberate
/// narrowing and not what this funnel is about.
fn wire_agent(route: &str, token: Option<&str>) -> &'static str {
    match token.filter(|s| !s.trim().is_empty()) {
        Some(t) => crate::graph::source_for_consumer(t),
        None => crate::graph::source_for_consumer(
            crate::harness::ingress::wire_default(route).token(),
        ),
    }
}

/// A `POST /context/compaction` request body (the Claude `PreCompact` shim).
#[derive(Deserialize)]
pub(crate) struct ContextCompactionBody {
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    /// `"manual"` / `"auto"`; recorded, not currently branched on.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) trigger: Option<String>,
    /// #48 (M-7): which shim is calling. See [`hook_agent`].
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// #48 (M-7): the cImp TAB this hook serves, baked into argv at spawn.
    /// `#[serde(default)]` because a shim from an older build sends none — see
    /// the residual note above.
    #[serde(default)]
    pub(crate) tab: Option<String>,
}

/// `POST /context/compaction` (V11 Phase D): always runs the session's
/// compaction side effects (clear injection dedup, mark post-compaction) and
/// returns a compact working-set/notes block as `{ ok, text }` to carry through
/// the summary. Never blocks — an empty block is returned as empty text.
///
/// #48 (M-7): gated through [`hook_admit`] on [`HOOK_TOOL_COMPACTION`], which
/// classifies TRUSTED — so this gate admits every call today, and the route is
/// inside the mechanism rather than beside it (demoting that one row is all it
/// takes to close the route, and its comment states what else a demotion must
/// do first). The block's content is why: paths, symbol NAMES and memory-note
/// text, no source text, with quarantined notes already excluded.
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
    let settings = live_settings(app);
    if hook_admit(
        latches(),
        HOOK_TOOL_COMPACTION,
        hook_agent(body.agent.as_deref()),
        body.tab.as_deref(),
        |agent, tab| latch_scope(app, &settings, agent, tab),
        |scope| GatePolicy::resolve(&settings, scope),
    )
    .is_err()
    {
        return write_json(stream, 200, &empty).await;
    }
    let block = compaction_block(app, &body);
    write_json(stream, 200, &serde_json::json!({ "ok": true, "text": block })).await
}

/// The compaction carry-over block, **after** the gate — shared by
/// `/context/compaction` and [`crate::harness::claude::hook::ROUTE_PRE_COMPACT`].
///
/// The gate itself deliberately stays in each handler rather than moving in
/// here: the route-enumeration test (`every_loopback_route_declares_what_it_does_
/// about_the_latch`) checks each handler's own body for its `hook_admit(latches(),
/// …)` call, and a gate that a route merely inherits from a helper is a gate a
/// reviewer cannot see at the route.
pub(crate) fn compaction_block(app: &AppHandle, body: &ContextCompactionBody) -> String {
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return String::new();
    };
    let graph = graph.inner().clone();
    // #104: `compaction_context` opens the project's store — resolve, never
    // trust the payload's cwd. No root ⇒ no carry-over block, the route's own
    // fail-safe.
    let Some(root) = external_project_root(app, &live_settings(app), body.tab.as_deref(), body.cwd.as_deref())
    else {
        return String::new();
    };
    graph
        .compaction_context(&root, body.session_id.as_deref())
        .unwrap_or_default()
}

/// A `POST /context/should_read` request body (the Claude `PreToolUse` Read
/// advisor shim).
#[derive(Deserialize)]
pub(crate) struct ShouldReadBody {
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    pub(crate) file_path: String,
    /// 1-based read offset, when the agent asked for a windowed read.
    #[serde(default)]
    pub(crate) offset: Option<u32>,
    /// V17 Phase B: the `Read` line limit, when the agent asked for a slice.
    /// Forwarded so the verdict can tell a full read from a head-peek (a
    /// deliberate slice always passes — Phase C's first-read branch).
    #[serde(default)]
    pub(crate) limit: Option<u32>,
    /// #48 (M-7): which shim is calling. See [`hook_agent`].
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// #48 (M-7): the cImp TAB this hook serves, baked into argv at spawn.
    #[serde(default)]
    pub(crate) tab: Option<String>,
}

/// `POST /context/should_read` (V11 Phase E): the read-advisor verdict for a
/// `Read`. Returns `{ ok, verdict: "pass" }` to let the read through, or
/// `{ ok, verdict: "remind", text }` to deny-with-content. Fails open to `pass`
/// on any missing state — the advisor must never block a legitimate read.
///
/// #48 (M-7): gated through [`hook_admit`] on [`HOOK_TOOL_SHOULD_READ`]
/// (LOCAL-CAPABILITY — the verdict hands back the file's outline, its symbol
/// body, or a unified diff of it, which is repo source text). **This does not
/// weaken the sentence above.** The gate's only reachable effect is to turn a
/// `remind` into a `pass`, because `pass` is the fail-safe every arm of this
/// route falls back to: a latched conversation gets its read through untouched
/// and pays only the tokens the advisor would have saved. The advisor can still
/// never block a legitimate read — after this change it can block strictly
/// fewer of them.
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
    let settings = live_settings(app);
    if hook_admit(
        latches(),
        HOOK_TOOL_SHOULD_READ,
        hook_agent(body.agent.as_deref()),
        body.tab.as_deref(),
        |agent, tab| latch_scope(app, &settings, agent, tab),
        |scope| GatePolicy::resolve(&settings, scope),
    )
    .is_err()
    {
        return write_json(stream, 200, &pass).await;
    }
    match should_read_verdict(app, &body) {
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

/// The read advisor's verdict, **after** the gate — `Some(reminder)` for a
/// `remind`, `None` for a `pass`. Shared by `/context/should_read` and
/// [`crate::harness::claude::hook::ROUTE_PRE_TOOL_USE`]; see [`compaction_block`] for why the
/// gate stays at each route rather than moving in here.
pub(crate) fn should_read_verdict(app: &AppHandle, body: &ShouldReadBody) -> Option<String> {
    let graph = app.try_state::<Arc<crate::graph::GraphService>>()?;
    let graph = graph.inner().clone();
    // #104: the advisor OPENS (and therefore creates) the project's graph store,
    // so it is the route that minted the stray state dirs. The root is resolved,
    // never taken from the payload; no root ⇒ pass the read through, which is
    // this route's fail-safe everywhere else too.
    let root = external_project_root(app, &live_settings(app), body.tab.as_deref(), body.cwd.as_deref())?;
    graph.should_read(
        &root,
        body.session_id.as_deref(),
        &body.file_path,
        body.offset,
        body.limit,
    )
}

/// A `POST /activity/contract_drift` request body (V16 Feature 3): a hook
/// shim reporting a payload that was missing required fields.
#[derive(Deserialize)]
pub(crate) struct ContractDriftBody {
    pub(crate) shim: String,
    #[serde(default)]
    pub(crate) missing: Vec<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
}

/// The one bucket every shim name cImp does not ship shares. Parenthesized like
/// [`outbound::NO_TAB_IDENTITY`], so it cannot be confused with a real name.
const DRIFT_SHIM_UNKNOWN: &str = "(unrecognized shim)";

/// The ledger key for a caller-supplied `shim` string: a token some registered
/// harness declares, or the one shared sentinel.
///
/// Returns `&'static str` and not `String` **on purpose** — that is the bound
/// itself rather than a check that implements it. The ledger's key type makes a
/// caller-supplied string unable to become a key at all, so the key space is
/// `drift_tokens().len() + 1` by construction and cannot drift back.
///
/// **Exact match, never a prefix** (after trimming): `"read_hook-forged"` is the
/// sentinel, not `read_hook`. A prefix or truncation rule here would let an
/// invented name claim a real shim's counter — [`bounded_id`]'s ordering rule,
/// one route over.
///
/// **V40 Phase C, locked decision 22.** The list used to be a
/// `const DRIFT_SHIMS: [&str; 10]` here — one harness's shim-token vocabulary,
/// the whole key space of the drift ledger, in core. It comes from
/// [`crate::harness::ingress::drift_tokens`] now: still `&'static str`, still
/// bounded, but by what the plugins declare. Drift fails SAFE in both
/// directions exactly as before — an undeclared shim shares the sentinel bucket
/// (fewer rows, never more), and a declared name no shim sends is a bucket
/// nothing ever claims.
///
/// The names themselves are unchanged, and deliberately: a tab open across an
/// upgrade still runs the old shim binary and still POSTs these exact strings
/// over the wire, so both paths must land in ONE bucket per capability.
fn drift_shim_key(raw: &str) -> &'static str {
    let raw = raw.trim();
    crate::harness::ingress::drift_tokens()
        .into_iter()
        .find(|shim| *shim == raw)
        .unwrap_or(DRIFT_SHIM_UNKNOWN)
}

/// Rate-limit state for `handle_contract_drift`. A systematically broken payload
/// fires its shim on every hook invocation, and without a ledger one bad session
/// would flood the Activity store's 400-row graph ring.
///
/// **#48 F-37 / locked decision 42 — this used to be a `HashSet<(String,
/// String)>` keyed on the caller's own `shim` and `session_id`.** Both halves of
/// the key came off the wire, so any token-holder could grow it without limit and
/// evict the whole graph lane, taking genuine security rows out of a capped ring
/// with it. The fix is the bar `/activity/discovery_skipped` already meets
/// ([`DISCOVERY_REPORTS`]), in both of its halves:
///
/// * **The key is not the caller's** — it is [`drift_shim_key`]'s classification
///   of it, a `&'static str` from a compile-time list. Ten thousand invented shim
///   names buy **one** bucket.
/// * **Repeats cost `log2(n)` rows** — [`outbound::Doubling`], the same primitive
///   `claim_ssrf` and the discovery report use, and each row states how many
///   reports it stands for so a fold is never a silent drop.
///
/// **`session_id` is deliberately no longer part of the key.** It is
/// caller-supplied with nothing app-side to classify it against — this body
/// carries no tab, and the missing `session_id` is frequently the very drift
/// being reported — so keeping it would have left the unbounded half in place.
/// The cost is the documented "one row per shim per session" becoming "rows at
/// reports 1, 2, 4, 8 … per shim per app run": strictly more rows for a genuinely
/// broken shim, and the consumer is unaffected, since `drift.payload.v1` reads
/// events since process start and de-duplicates by shim
/// (`ipc::commands::advisor_signals`).
///
/// Process lifetime, unchanged and for the same reason as before.
static CONTRACT_DRIFT_SEEN: OnceLock<Mutex<HashMap<&'static str, outbound::Doubling>>> =
    OnceLock::new();

/// Count one drift report against the process ledger. See
/// [`CONTRACT_DRIFT_SEEN`].
pub(crate) fn claim_contract_drift(shim: &'static str) -> outbound::DoublingRow {
    let ledger = CONTRACT_DRIFT_SEEN.get_or_init(Default::default);
    let mut ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
    drift_claim_in(&mut ledger, shim)
}

/// [`claim_contract_drift`] against a caller-owned ledger, so the key-space bound
/// and the doubling are assertable without process-global state (the suite runs
/// cases concurrently in one process). The twin of [`claim_in`].
fn drift_claim_in(
    ledger: &mut HashMap<&'static str, outbound::Doubling>,
    shim: &'static str,
) -> outbound::DoublingRow {
    ledger.entry(shim).or_default().claim()
}

/// How many field names one drift row may list. Every real payload check in this
/// crate has at most five (`read_hook::contract_checks`), so this is slack for a
/// future check rather than a limit anything genuine can reach.
const MAX_DRIFT_MISSING: usize = 12;

/// The caller's `missing` list, bounded in both dimensions before it reaches a
/// row (#48 F-37).
///
/// The list is an arbitrary count of arbitrary strings and it lands in the row's
/// `target` — which the store does **not** truncate (only `request` and
/// `response` are capped) and which `advisor_signals` copies verbatim into a
/// user-facing signal. Every genuine report is byte-identical to the plain
/// `join(", ")` this replaced; only abuse is cut, and the row says it was.
fn bounded_missing(raw: &[String]) -> String {
    let mut out: Vec<String> = raw
        .iter()
        .take(MAX_DRIFT_MISSING)
        .map(|f| bounded_id(f))
        .collect();
    if let Some(extra) = raw.len().checked_sub(MAX_DRIFT_MISSING).filter(|n| *n > 0) {
        out.push(format!("… (+{extra} more)"));
    }
    out.join(", ")
}

/// The activity row one drift report writes, or `None` when the ledger folds it
/// into an earlier one.
///
/// Split from the handler and given its ledger as a closure for the reason
/// [`record_discovery_skipped`] documents, plus one this route owns:
/// `activity::record_bg` has **no `cfg(test)` diversion**, so a row written
/// inside the handler is unobservable to the suite — which is why the pre-F-37
/// behaviour had no row-level test at all. Returning the record makes what a
/// caller can put in the store assertable without touching the global store.
///
/// Nothing here is left at the caller's length: [`bounded_id`] on the shim name
/// and on the session id, [`bounded_missing`] on the field list. The bounds are
/// applied **after** [`drift_shim_key`] has classified, so a truncated name
/// cannot claim a real shim's counter.
pub(crate) fn contract_drift_row(
    body: &ContractDriftBody,
    claim: impl FnOnce(&'static str) -> outbound::DoublingRow,
) -> Option<crate::activity::ActivityRecord> {
    let outbound::DoublingRow::Write { total, suppressed } = claim(drift_shim_key(&body.shim))
    else {
        return None;
    };
    let shim = bounded_id(&body.shim);
    let session = bounded_id(body.session_id.as_deref().unwrap_or_default());
    let missing = bounded_missing(&body.missing);
    Some(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::Graph,
            crate::activity::now_ms(),
            String::new(), // no root — the report is about the harness, not a project
            "harness".to_string(),
            "contract_drift".to_string(),
            format!("{shim}: {missing}"),
            missing.chars().count(),
            0,
            false, // a drift report is never "ok" — it flags the entry in the feed
            // The report is about the harness shim, not a tab's call — but
            // the session it drifted in is known and is the join key.
            //
            // #48 F-20 left this ALONE, and `Unattributed` is honest here:
            // `ContractDriftBody` carries no `tab` field at all, so this
            // writer genuinely does not know. The shim *does* (`--tab {tab}`
            // is baked into its hook command line, `tabs/config.rs`), so the
            // fix is a wire change — `#[serde(default)] tab: Option<String>`
            // on the body plus the shim sending it — and both skew directions
            // degrade safely. That is a shim/app contract change and belongs
            // with F-6's drift-canary work, not here.
            crate::activity::Attribution::Unattributed,
            Some(session.clone()),
            None,
            None,
        ),
        request: format!(
            "shim {shim} payload missing required fields (session {session}) — report {total} \
             from this shim this app run, {suppressed} folded into it"
        ),
        response: missing,
    })
}

/// `POST /activity/contract_drift` (V16 Feature 3): record a shim's
/// payload-drift report as an Activity event (`source: "harness"`,
/// `tool: "contract_drift"`), rate-limited per shim by [`CONTRACT_DRIFT_SEEN`].
/// Always answers `{ok: true}` — the shim is fail-open and fire-and-forget.
///
/// The 400 on a malformed body is this route's own long-standing contract and is
/// **not** the discipline `handle_discovery_skipped` follows (one constant reply
/// on every path, locked decision 37). The difference is deliberate: that route
/// exists so a *child* can report containment and must give a prober no oracle;
/// this one answers a shim of ours that is already misbehaving, and locked
/// decision 42 moved the bound, not the protocol.
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
    if let Some(record) = contract_drift_row(&body, claim_contract_drift) {
        crate::activity::record_bg(record);
    }
    write_json(stream, 200, &ok).await
}

// ── V35 Phase I: CHP — the protocol version, and the hello ──────────────────

/// Observe the `chp` version a routed POST carries, for the stale-artifact
/// report (`harness::chp`, milestone decision 9 / design D5).
///
/// Called once per request from the dispatcher, **beside** the route table
/// rather than inside the handlers. Three properties make that safe, and each is
/// the reason the phase's "zero behavior change" claim is a fact rather than a
/// hope:
///
/// * it **reads only** — no reply, no early return, no error path a route could
///   inherit. A malformed body here is ignored; the route's own handler still
///   owns its 400;
/// * the routes' body types are **untouched**, so every existing
///   deserialization test still pins the same shape;
/// * the tab id is validated against the user's configured AI tabs before
///   anything is stored, exactly as [`handle_latch_beacon`] validates its own —
///   so the peer registry's key space is `configured tabs × 2 agents` and not
///   "whatever a request body said" (#45's rule, one route surface over).
///
/// **Cost on the hot path.** Every prompt and every tool call passes here, so
/// the common case must not clone `Settings`. It does not: the second and every
/// later message from a tab hits [`crate::harness::chp::already_seen`], a single
/// map lookup, and returns. Only a *new* `(agent, tab, chp)` triple — i.e. a tab
/// launching, or a tab's artifact having changed — pays one settings read.
///
/// **V35 Phase J** added the second source. A Claude `type: "http"` hook posts
/// the harness's own payload, which has no room for a CHP envelope, so its
/// identity arrives in `X-CIMP-*` headers instead. Same three properties, same
/// validation, same hot-path shortcut — only the read differs, and which of the
/// two applies is decided by the route rather than by sniffing the body.
///
/// **V35 Phase L** added the second duty: the same one-lookup pass counts the
/// push against the tab's served capabilities and reports any that have gone
/// QUIET. Locked decision 7 — a served capability whose pushes stop arriving
/// must NOT silently fall back to the reader, because falling back would
/// restore the data and hide the breakage, which is the exact silent-drift
/// class this milestone exists to delete. The reader stays suppressed while the
/// hello's claim stands; the silence gets a `drift.payload.v1` row instead,
/// under the same token that capability's payload drift uses.
fn note_chp(app: &AppHandle, route: &str, req: &Request) {
    // **V40 Phase C, locked decision 22.** This used to read
    // `hook::is_hook_route(route)` — core deciding, by naming one
    // harness, where a request's identity lives. The question is now asked of
    // the registry: a plugin whose ingress puts its identity outside the body
    // answers for its own routes, and `None` from all of them means "read the
    // CHP envelope", which is what every ordinary caller sends.
    let (chp, agent_token, tab) = if let Some(id) =
        crate::harness::ingress::identity_of_request(route, req)
    {
        if id.tab.is_empty() {
            return;
        }
        (id.chp, id.agent, id.tab)
    } else {
        let Some((env, tab)) = crate::harness::chp::envelope(route, &req.body) else {
            return;
        };
        (
            env.chp.unwrap_or(crate::harness::chp::PRE_CHP),
            env.agent_token().to_string(),
            tab,
        )
    };
    // V40 review M-4: through the same funnel the HANDLERS use, and empty is
    // absent. Both identity readers answer the empty string rather than `None`
    // for a body with no discriminator (`identity_of_request` is
    // `unwrap_or_default()`, `Envelope::agent_token` is `unwrap_or("")`), so a
    // pre-upgrade artifact resolved to `UNKNOWN_SOURCE` — which fails
    // `is_configured_tab` and silently switched OFF stale-artifact recording and
    // the quiet-capability detector for exactly the artifacts they exist to
    // catch.
    let agent = wire_agent(route, Some(agent_token.as_str()));
    // The quiet pass runs FIRST and on every POST, including the ones the
    // `already_seen` shortcut below returns early from — a tab whose `chp` has
    // not changed is precisely the steady state in which a hook goes silent.
    // It never inserts, so an unvalidated tab still cannot grow the registry:
    // the counters ride an entry only the validated path below creates.
    report_quiet_capabilities(route, agent, &tab);
    if crate::harness::chp::already_seen(agent, &tab, chp) {
        return;
    }
    let settings = live_settings(app);
    if !is_configured_tab(&settings, agent, &tab) {
        return;
    }
    crate::harness::chp::note_push(agent, &tab, chp, crate::activity::now_ms());
}

/// Count one push against this tab's served capabilities and file a drift row
/// for any that have now gone quiet (V35 Phase L, locked decision 7).
///
/// "Demonstrably active" is deliberately the cheapest sound definition:
/// **another push whose arrival proves this one should also have fired**
/// ([`crate::harness::chp::witness_of`]). No timers, no wall-clock thresholds,
/// nothing that fires because a user went to lunch — a `UserPromptSubmit` with
/// no `Stop` behind it three times over is the `Stop` hook having stopped
/// firing, and nothing else.
fn report_quiet_capabilities(route: &str, agent: &'static str, tab: &str) {
    // The route→event join and the capability→token join are both the
    // harness's (locked decision 22): the sending harness is the only thing
    // that knows which of ITS routes feeds a capability, and which ledger
    // bucket that capability's silence belongs in. Core keeps the detector,
    // the ledger and the bound.
    let harness = crate::harness::HarnessId::from_id(agent);
    let Some(event) = harness
        .and_then(|h| h.plugin())
        .and_then(|p| p.chp_event_for_route(route))
        .or_else(|| crate::harness::chp::event_for_route(route))
    else {
        return;
    };
    for capability in crate::harness::chp::note_event(agent, tab, event) {
        let Some(shim) = harness
            .and_then(|h| h.plugin())
            .and_then(|p| p.drift_token_for_capability(&capability))
        else {
            continue;
        };
        warn!(
            target: "offload",
            agent,
            %tab,
            %capability,
            witness = %event,
            "loopback: a SERVED capability has gone quiet — its hook stopped firing while the \
             session kept pushing. The fallback reader stays suppressed on purpose (falling back \
             would hide this); restart the tab to re-declare."
        );
        let body = ContractDriftBody {
            shim: shim.to_string(),
            missing: vec![crate::harness::ingress::MISSING_PUSH.to_string()],
            session_id: None,
        };
        if let Some(record) = contract_drift_row(&body, claim_contract_drift) {
            crate::activity::record_bg(record);
        }
    }
}

/// A `POST /session/hello` body — V35 Phase I, design D3.
///
/// Every field is optional and every one is caller-supplied. The handler bounds
/// each before it can reach a row or the panel; nothing here is believed beyond
/// "this local process said so", which is the standard every `Origin::Http`
/// producer on this listener is held to.
#[derive(Deserialize)]
struct SessionHelloBody {
    /// The protocol version the artifact speaks. Absent ⇒ pre-CHP, never an
    /// error — a hello is exactly the message an old artifact would not send at
    /// all, so tolerating its absence here is belt-and-braces rather than a
    /// live path.
    #[serde(default)]
    chp: Option<u32>,
    /// `claude` / `opencode`, normalized through `source_for_consumer` like
    /// every other route's discriminator.
    #[serde(default)]
    agent: Option<String>,
    /// The cImp tab this artifact was generated for. Required in practice: a
    /// hello with no tab has nothing to key, and one naming an unconfigured tab
    /// is refused (see the handler).
    #[serde(default)]
    tab: Option<String>,
    /// The harness's own version, when it exposes one to its extensions.
    #[serde(default)]
    harness_version: Option<String>,
    /// The CHP events this artifact will actually push, with its per-tab flags
    /// applied.
    #[serde(default)]
    serves: Vec<String>,
    /// …and the rest, each with a reason.
    #[serde(default)]
    cannot: Vec<SessionHelloUnable>,
}

/// One `cannot` entry: a capability this artifact will not serve, and why.
#[derive(Deserialize)]
struct SessionHelloUnable {
    #[serde(default)]
    id: String,
    #[serde(default)]
    why: String,
}

/// How many `serves` / `cannot` entries one hello may declare.
///
/// The live vocabulary is 17 ids (`harness::chp::EVENTS`), so this is slack for
/// a future event rather than a limit anything genuine can reach — the same
/// shape and the same reasoning as [`MAX_DRIFT_MISSING`]. Without it, `serves`
/// is an unbounded list of unbounded strings that reaches an in-memory registry
/// and a Settings panel.
pub(crate) const MAX_HELLO_DECLARATIONS: usize = 32;

/// The doubling ledger for hello rows, keyed on the **resolved** `agent:tab` —
/// which is only ever reached after [`is_configured_tab`] accepted it, so the
/// key space is bounded by the user's own tab list exactly as
/// [`DISCOVERY_REPORTS`]'s is.
///
/// Two gates, not one, and they catch different things: the row is written only
/// when the hello actually CHANGED what cImp knows (a plugin re-loading with the
/// same declaration is silent), and repeats of a genuinely flip-flopping
/// declaration cost `log2(n)` rows. Process lifetime, following its two
/// siblings.
static HELLO_SEEN: OnceLock<Mutex<HashMap<String, outbound::Doubling>>> = OnceLock::new();

/// Count one hello against the process ledger. See [`HELLO_SEEN`].
pub(crate) fn claim_hello(key: &str) -> outbound::DoublingRow {
    let ledger = HELLO_SEEN.get_or_init(Default::default);
    let mut ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
    claim_in(&mut ledger, key)
}

/// The caller's declaration list, bounded in both dimensions before it reaches
/// the peer registry — the [`bounded_missing`] discipline applied to `serves`
/// and to `cannot`.
pub(crate) fn bounded_declarations(raw: &[String]) -> Vec<String> {
    raw.iter()
        .take(MAX_HELLO_DECLARATIONS)
        .map(|s| bounded_id(s))
        .collect()
}

/// The Activity row one hello writes, or `None` when nothing changed.
///
/// Split from the handler for the reason [`contract_drift_row`] documents:
/// `activity::record_bg` has no `cfg(test)` diversion, so a row written inside a
/// handler is unobservable to the suite. Returning the record makes what a
/// caller can put in the store assertable without touching the global store.
pub(crate) fn hello_row(
    agent: &'static str,
    tab: &str,
    chp: u32,
    version: &str,
    serves: &[String],
    cannot: usize,
    claim: impl FnOnce(&str) -> outbound::DoublingRow,
) -> Option<crate::activity::ActivityRecord> {
    let outbound::DoublingRow::Write { total, suppressed } = claim(&format!("{agent}:{tab}")) else {
        return None;
    };
    let version = if version.is_empty() {
        "version not declared".to_string()
    } else {
        format!("v{version}")
    };
    let target = format!(
        "{agent}/{tab}: chp {chp} ({version}) — serves {}, cannot {cannot}",
        serves.len()
    );
    Some(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            // The lane `contract_drift` already uses for harness-contract facts,
            // with the same `source: "harness"`. Deliberately NOT a new
            // retention lane: a hello fires once per tab launch, and the two
            // rows a reader wants side by side ("this plugin introduced itself"
            // / "this shim's payload broke") belong in one feed.
            crate::activity::ActivityKind::Graph,
            crate::activity::now_ms(),
            String::new(), // no root — the hello is about a harness, not a project
            "harness".to_string(),
            "chp_hello".to_string(),
            target,
            serves.len(),
            0,
            // A hello is a normal, healthy event — unlike a drift report, which
            // flags its entry.
            true,
            crate::activity::Attribution::Tab(tab.to_string()),
            None,
            None,
            None,
        ),
        request: format!(
            "serves: {}",
            if serves.is_empty() {
                "(nothing declared)".to_string()
            } else {
                serves.join(", ")
            }
        ),
        response: format!(
            "hello {total} from this tab this app run, {suppressed} folded into it"
        ),
    })
}

/// `POST /session/hello` (V35 Phase I, design D3): a generated harness artifact
/// introducing itself — the protocol version it speaks, the harness version it
/// runs under (when the harness exposes one), and what it will and will not
/// serve.
///
/// # Nothing gates on this, and that is the phase's exit criterion
///
/// `serves` / `cannot` are RECORDED and DISPLAYED, never consulted by a
/// capability. Negotiation becomes load-bearing in Phase L; making it so here
/// would be a behavior change dressed as a declaration — and would hand an
/// artifact the power to switch cImp features off by lying about itself.
///
/// **`serves` is not a trust claim in either direction.** An artifact declaring
/// `tool.gate` has said nothing cImp relies on: the gate's authority is cImp
/// computing the verdict at `/latch/state`, and the artifact's only power is to
/// refuse MORE than it was told to.
///
/// # Auth, and the same honesty clause every route here owes
///
/// Bearer, inherited from the pre-dispatch [`authorized`] check. The launch
/// token is readable by any process running as this user, so "authenticated"
/// means *a local process*, never *cImp's own plugin*. Which is why the tab id
/// is validated ([`is_configured_tab`]) and every string is bounded before it
/// reaches the registry or the Settings panel.
///
/// # Answers
///
/// `200 {ok, chp}` — the ack carries the SERVER's version so a future client can
/// adapt to an older cImp. `400` on a malformed body or an unconfigured tab,
/// following [`handle_latch_beacon`]'s discipline rather than
/// [`handle_discovery_skipped`]'s constant-ack one: this route answers cImp's
/// own generated artifact, which is fail-open and discards the reply, and a
/// rejected tab is a fact worth a log line.
async fn handle_session_hello(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: SessionHelloBody = match serde_json::from_slice(&req.body) {
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
    let agent = crate::graph::source_for_consumer(body.agent.as_deref().unwrap_or(crate::harness::DEFAULT_HARNESS.token()));
    let tab = body.tab.as_deref().map(str::trim).unwrap_or("");
    let settings = live_settings(app);
    if tab.is_empty() || !is_configured_tab(&settings, agent, tab) {
        warn!(
            target: "offload",
            agent,
            tab = %bounded_id(tab),
            "loopback: /session/hello rejected — not a configured tab id"
        );
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(
                "/session/hello accepts configured AI tabs only — a hello with no tab has \
                 nothing to key"
                    .to_string(),
            ),
        };
        return write_json(stream, 400, &r).await;
    }
    let chp = body.chp.unwrap_or(crate::harness::chp::PRE_CHP);
    let version = bounded_id(body.harness_version.as_deref().unwrap_or("").trim());
    let serves = bounded_declarations(&body.serves);
    let cannot: Vec<crate::harness::chp::Unable> = body
        .cannot
        .iter()
        .take(MAX_HELLO_DECLARATIONS)
        .map(|u| crate::harness::chp::Unable {
            id: bounded_id(&u.id),
            why: bounded_id(&u.why),
        })
        .collect();
    let changed = crate::harness::chp::note_hello(
        agent,
        tab,
        chp,
        &version,
        serves.clone(),
        cannot.clone(),
        crate::activity::now_ms(),
    );
    if changed {
        if let Some(record) = hello_row(agent, tab, chp, &version, &serves, cannot.len(), claim_hello)
        {
            crate::activity::record_bg(record);
        }
    }
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "chp": crate::harness::chp::CHP_VERSION }),
    )
    .await
}

// ── V35 Phase L: the read path, pushed (design D2, issue #69) ────────────────
//
// Three capabilities that reached cImp by TAILING AN EMITTED ARTIFACT — Tier C,
// whose whole failure mode is silent zeros — now arrive as documented hook
// payloads. Six routes, three cores:
//
//   `/session/assistant_text`  ← `/claude/hook/stop`                (Stop)
//   `/session/tool_result`     ← `/claude/hook/post_tool_use_result` (PostToolUse, all tools)
//   `/session/subagent`        ← `/claude/hook/subagent`             (SubagentStart/Stop)
//
// **The arbitration rule lives in the cores, not in the handlers**, and it is
// the same predicate the fallback readers ask
// ([`crate::harness::chp::served`]): a capability is served for a tab when
// THAT tab's hello declared it. Both sides consulting one predicate is what
// makes "exactly one path produces this data" a property rather than a
// convention — the two failures that would otherwise be invisible are TTS
// speaking a message twice and one tool result being counted twice.
//
// What is deliberately NOT here:
//
// * **`session.usage`.** No Claude hook payload carries token counts — the
//   common input set is `session_id` / `transcript_path` / `cwd` /
//   `permission_mode` / `hook_event_name`, and `PostCompact` exposes no
//   compaction metrics either. `claude.transcript.usage` therefore stays Tier C
//   on the transcript tail, permanently-until-upstream-changes. The V35
//   milestone's Phase L row lists "usage" among the migrations; that was
//   written before the payload set was checked, and the registry row now
//   records the limitation instead of the intent.
// * **`session.context`.** Same shape of answer: the statusline stdin payload
//   has no hook equivalent.
// * **Sub-agent token usage.** `SubagentStop` carries
//   `last_assistant_message`, not tokens, and there is no sub-agent transcript
//   path in any payload — so `SubagentState::scan`'s sub-agent-lane
//   accounting keeps reading `<session_id>/subagents/agent-*.jsonl`. What
//   migrates is the LIFECYCLE (which drives the avatar), not the spend.

/// A `POST /session/assistant_text` body — one complete assistant message, as
/// prose.
///
/// **Prose, never markup or control** (design § 5.2). The sender is not trusted
/// to segment: `text` goes through `tts::prose::speak_prose`, which strips
/// terminal escapes, reduces markdown and segments app-side exactly as it does
/// for the fallback readers. A plugin controls *what* cImp says out loud, which
/// is why this capability sits in the freely-declarable data tier and the
/// per-tab `tts_injection.enabled` gate still applies.
#[derive(Deserialize)]
struct SessionAssistantTextBody {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    tab: Option<String>,
    #[serde(default)]
    text: String,
}

/// A `POST /session/tool_result` body — one tool result's SIZE.
///
/// `chars` and not the content: the consumer is usage accounting, whose
/// estimated-token proxy has always been a character count
/// (`harness::claude::read::tool_result_chars`). Taking the content here would
/// put an unbounded, model-influenced blob on the wire for a `u32`'s worth of
/// information.
#[derive(Deserialize)]
struct SessionToolResultBody {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    tab: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    chars: u32,
}

/// A `POST /session/subagent` body — one sub-agent lifecycle edge.
///
/// `active` rather than an event-name string, because the only thing the
/// consumer needs is whether this id is now running: an id that started and has
/// not stopped holds the avatar in *Thinking*. A harness that grows a third
/// lifecycle state maps it onto this pair rather than teaching L3 a new word.
#[derive(Deserialize)]
struct SessionSubagentBody {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    tab: Option<String>,
    #[serde(default)]
    agent_id: String,
    #[serde(default)]
    active: bool,
}

/// A `POST /session/output_started` or `/session/output_stopped` body — one
/// turn boundary, reported by the harness itself.
///
/// Identity only: the edge is the message. Which direction it is comes from the
/// ROUTE rather than a body field, for the same reason the two `permission.*`
/// events are two routes — an edge whose direction is a payload value can be
/// dropped by a lenient parser and read as its opposite.
#[derive(Deserialize)]
struct HarnessOutputBody {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    tab: Option<String>,
}

/// A `POST /session/subagents_active` body — the sub-agent COUNT's zero
/// boundary, as the harness sees it.
///
/// Distinct from `session.subagent` (`/session/subagent`), which reports one
/// sub-agent's lifecycle and lets core derive the edge: a harness that already
/// knows "none running / some running" posts this and core keeps no set for it.
#[derive(Deserialize)]
struct SubagentsActiveBody {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    tab: Option<String>,
    #[serde(default)]
    active: bool,
}

/// `POST /session/output_{started,stopped}`: a pushed turn boundary.
///
/// **Gated on the tab's own hello** (`chp::served`), exactly as every other
/// pushed core is, and here that gate is load-bearing for a reason worth
/// stating: core may ALSO be inferring this tab's activity from its terminal
/// (`ActivitySource::TuiMarkers`). Two producers for one avatar is the
/// double-speak V35 Phase L's arbitration exists to prevent, so a harness that
/// pushes these edges must declare them in its hello — at which point its
/// plugin declares `ActivitySource::OutOfBand` and the TUI heuristic never runs
/// for its tabs.
async fn handle_harness_output(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
    started: bool,
) -> AppResult<()> {
    let ok = RunResult {
        ok: true,
        text: None,
        error: None,
    };
    let Ok(body) = serde_json::from_slice::<HarnessOutputBody>(&req.body) else {
        return write_json(stream, 400, &bad_request("bad request body")).await;
    };
    if let Some((agent, tab)) =
        session_push_identity(app, body.agent.as_deref(), body.tab.as_deref())
    {
        harness_output_core(app, agent, &tab, started);
    }
    write_json(stream, 200, &ok).await
}

/// Apply one pushed turn boundary — the `harness.output_*` core.
///
/// Returns whether it acted, the same shape the other pushed cores answer with
/// so the arbitration tests can assert an exact complement.
pub(crate) fn harness_output_core(
    app: &AppHandle,
    agent: &'static str,
    tab: &str,
    started: bool,
) -> bool {
    let event = if started {
        crate::harness::chp::EV_HARNESS_OUTPUT_STARTED
    } else {
        crate::harness::chp::EV_HARNESS_OUTPUT_STOPPED
    };
    if !crate::harness::chp::served(agent, tab, event) {
        return false;
    }
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return false;
    };
    let tab = crate::state::TabId::from_str(tab);
    let signal = if started {
        crate::state::StateSignal::HarnessOutputStarted { tab }
    } else {
        crate::state::StateSignal::HarnessOutputStopped { tab }
    };
    let _ = state.state_signals.try_send(signal);
    true
}

/// `POST /session/subagents_active`: the pushed zero-boundary of a tab's
/// sub-agent count.
async fn handle_subagents_active(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let ok = RunResult {
        ok: true,
        text: None,
        error: None,
    };
    let Ok(body) = serde_json::from_slice::<SubagentsActiveBody>(&req.body) else {
        return write_json(stream, 400, &bad_request("bad request body")).await;
    };
    if let Some((agent, tab)) =
        session_push_identity(app, body.agent.as_deref(), body.tab.as_deref())
    {
        subagents_active_core(app, agent, &tab, body.active);
    }
    write_json(stream, 200, &ok).await
}

/// Apply one pushed sub-agent-count edge — the `subagents.active` core.
///
/// Emits the same `SubagentsActiveChanged` signal [`subagent_core`] derives
/// from individual lifecycles, so the state manager sees one signal shape
/// whichever path produced it.
pub(crate) fn subagents_active_core(
    app: &AppHandle,
    agent: &'static str,
    tab: &str,
    active: bool,
) -> bool {
    if !crate::harness::chp::served(agent, tab, crate::harness::chp::EV_SUBAGENTS_ACTIVE) {
        return false;
    }
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return false;
    };
    let _ = state
        .state_signals
        .try_send(crate::state::StateSignal::SubagentsActiveChanged {
            tab: crate::state::TabId::from_str(tab),
            active,
        });
    true
}

/// Speak one pushed assistant message — the `assistant_text` core.
///
/// Returns whether it acted, which is what the arbitration tests assert on:
/// for one `(agent, tab, capability)` the answer here and the fallback reader's
/// `ctx.pushed(..)` are exact complements.
pub(crate) async fn assistant_text_core(app: &AppHandle, agent: &'static str, tab: &str, text: &str) -> bool {
    if !crate::harness::chp::served(agent, tab, crate::harness::chp::EV_ASSISTANT_TEXT) {
        // Not declared by THIS tab's artifact ⇒ its reader is still speaking,
        // and speaking here too is the double-speak this phase must not ship.
        return false;
    }
    let tab_id = crate::state::TabId::from_str(tab);
    // V39 Phase B — **delegation is a SECOND consumer of this core** (locked
    // decision 16's read half). It sits inside the `served` gate, above the
    // empty-text return and above the TTS toggle, and each of those three
    // positions is deliberate:
    //
    // * inside the gate, because arbitration decides which of the push core
    //   and the fallback reader produces this datum, and both call the same
    //   completion feed — a delegation must be told exactly once;
    // * above the empty-text return, because locked decision 13 needs to tell
    //   "the worker said nothing" (an error, now) from "the worker never
    //   answered" (a timeout, minutes later) — and it can only do that if an
    //   empty turn is reported as a completion;
    // * above the TTS path, because a delegation must complete on a tab with
    //   speech switched off.
    //
    // Additive: nothing below this line changed, so the existing TTS behaviour
    // is exactly what it was.
    crate::delegation::note_assistant_text(&tab_id, text);
    if text.trim().is_empty() {
        return false;
    }
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return false;
    };
    crate::tts::speak_prose(
        &tab_id,
        &state.tts_segments,
        &state.settings,
        None,
        crate::tts::ProseSource::ChpPush,
        text,
    )
    .await;
    true
}

/// Record one pushed tool-result size — the `session.tool_result` core.
///
/// The same `UsageEvent::ToolResult` row the transcript tail writes, into the
/// same graph service, keyed the same way. Nothing downstream can tell which
/// path produced it, which is the point: the migration is of the SOURCE, not of
/// the data model.
pub(crate) fn tool_result_core(
    app: &AppHandle,
    agent: &'static str,
    tab: &str,
    cwd: Option<&str>,
    session_id: &str,
    tool: Option<String>,
    chars: u32,
) -> bool {
    if !crate::harness::chp::served(agent, tab, crate::harness::chp::EV_SESSION_TOOL_RESULT) {
        return false;
    }
    let (Some(cwd), false) = (cwd.filter(|c| !c.trim().is_empty()), session_id.is_empty()) else {
        // No project root or no session ⇒ nothing to attribute the row to. The
        // reader has both by construction (it IS reading a session's file), so
        // this is the push path's own honest floor.
        return false;
    };
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return false;
    };
    // #104: `record_usage` opens the project's store. A sub-agent's cwd is not
    // a root; with no resolvable one there is nothing to attribute the usage to.
    let Some(root) = external_project_root(app, &live_settings(app), Some(tab), Some(cwd)) else {
        return false;
    };
    graph.record_usage(
        &root,
        session_id,
        agent,
        crate::graph::UsageEvent::ToolResult { tool, chars },
    );
    true
}

/// The most sub-agent ids one tab's pushed lifecycle set will hold.
///
/// The set is keyed by `(agent, tab)` — both validated — but the ids inside it
/// come off the wire, and a `SubagentStart` storm with no matching stops would
/// otherwise grow one tab's set without limit. At the cap a further start is
/// counted as "still active" without being remembered individually, which
/// degrades the edge detection to coarse rather than unbounded.
const MAX_PUSHED_SUBAGENTS: usize = 64;

/// Sub-agents currently running per `(agent, tab)`, as declared by pushes.
///
/// In-memory and non-durable for the reason the CHP peer registry is: it
/// describes live tabs, and an app restart ends every one of them. The
/// transcript tail keeps its OWN equivalent set (`update_agents`) for the tabs
/// that do not push, and the two never both drive the avatar for one tab —
/// arbitration decides which.
type SubagentSets = HashMap<(String, String), std::collections::HashSet<String>>;
static PUSHED_SUBAGENTS: OnceLock<Mutex<SubagentSets>> = OnceLock::new();

/// Apply one pushed sub-agent lifecycle edge — the `session.subagent` core.
///
/// Emits `StateSignal::SubagentsActiveChanged` on the empty↔non-empty EDGE only,
/// exactly as `harness::claude::read::update_agents` does, so the state manager
/// sees the same signal shape whichever path produced it.
pub(crate) fn subagent_core(
    app: &AppHandle,
    agent: &'static str,
    tab: &str,
    agent_id: &str,
    active: bool,
) -> bool {
    if !crate::harness::chp::served(agent, tab, crate::harness::chp::EV_SESSION_SUBAGENT) {
        return false;
    }
    let agent_id = bounded_id(agent_id);
    if agent_id.trim().is_empty() {
        // A lifecycle with no key cannot be closed; recording it would wedge the
        // avatar in Thinking forever. `contract_checks` reports the absence.
        return false;
    }
    let key = (agent.to_string(), tab.to_string());
    let registry = PUSHED_SUBAGENTS.get_or_init(Default::default);
    let mut registry = registry.lock().unwrap_or_else(PoisonError::into_inner);
    let set = registry.entry(key).or_default();
    let was_active = !set.is_empty();
    if active {
        if set.len() < MAX_PUSHED_SUBAGENTS {
            set.insert(agent_id);
        }
    } else {
        set.remove(&agent_id);
    }
    let now_active = !set.is_empty();
    drop(registry);
    if was_active == now_active {
        return true; // recorded, but not an edge — no signal.
    }
    if let Some(state) = app.try_state::<crate::ipc::AppState>() {
        let _ = state
            .state_signals
            .try_send(crate::state::StateSignal::SubagentsActiveChanged {
                tab: crate::state::TabId::from_str(tab),
                active: now_active,
            });
    }
    true
}

/// The `(agent, tab)` a harness-neutral Phase L body claims, **validated**.
///
/// One helper for the three routes, because all three carry the same two
/// identity fields and all three must narrow them the same way (#45's rule):
/// `agent` normalizes through `source_for_consumer` like every other route's
/// discriminator, and `tab` must name a configured AI tab for that agent.
fn session_push_identity(
    app: &AppHandle,
    agent: Option<&str>,
    tab: Option<&str>,
) -> Option<(&'static str, String)> {
    let agent = crate::graph::source_for_consumer(agent.unwrap_or(crate::harness::DEFAULT_HARNESS.token()));
    let tab = tab.map(str::trim).unwrap_or("");
    if tab.is_empty() {
        return None;
    }
    let settings = live_settings(app);
    if !is_configured_tab(&settings, agent, tab) {
        return None;
    }
    Some((agent, tab.to_string()))
}

/// `POST /session/assistant_text` — one complete assistant message, spoken.
async fn handle_session_assistant_text(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let ok = RunResult {
        ok: true,
        text: None,
        error: None,
    };
    let Ok(body) = serde_json::from_slice::<SessionAssistantTextBody>(&req.body) else {
        return write_json(stream, 400, &bad_request("bad request body")).await;
    };
    if let Some((agent, tab)) =
        session_push_identity(app, body.agent.as_deref(), body.tab.as_deref())
    {
        assistant_text_core(app, agent, &tab, &body.text).await;
    }
    write_json(stream, 200, &ok).await
}

/// `POST /session/tool_result` — one tool result's size, recorded.
async fn handle_session_tool_result(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let ok = RunResult {
        ok: true,
        text: None,
        error: None,
    };
    let Ok(body) = serde_json::from_slice::<SessionToolResultBody>(&req.body) else {
        return write_json(stream, 400, &bad_request("bad request body")).await;
    };
    if let Some((agent, tab)) =
        session_push_identity(app, body.agent.as_deref(), body.tab.as_deref())
    {
        tool_result_core(
            app,
            agent,
            &tab,
            body.cwd.as_deref(),
            body.session_id.as_deref().unwrap_or(""),
            body.tool.as_deref().map(bounded_tool_name),
            body.chars,
        );
    }
    write_json(stream, 200, &ok).await
}

/// `POST /session/subagent` — one sub-agent lifecycle edge.
async fn handle_session_subagent(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let ok = RunResult {
        ok: true,
        text: None,
        error: None,
    };
    let Ok(body) = serde_json::from_slice::<SessionSubagentBody>(&req.body) else {
        return write_json(stream, 400, &bad_request("bad request body")).await;
    };
    if let Some((agent, tab)) =
        session_push_identity(app, body.agent.as_deref(), body.tab.as_deref())
    {
        subagent_core(app, agent, &tab, &body.agent_id, body.active);
    }
    write_json(stream, 200, &ok).await
}

/// A tool name from the wire, bounded like every other caller-supplied id.
fn bounded_tool_name(raw: &str) -> String {
    bounded_id(raw)
}

fn bad_request(msg: &str) -> RunResult {
    RunResult {
        ok: false,
        text: None,
        error: Some(msg.to_string()),
    }
}

// ── #48 F-32 / locked decision 37: a child reports what containment did ──────

/// A `POST /activity/discovery_skipped` request body — a cImp stdio MCP child
/// saying it skipped one or more candidate discovery entries and reached this
/// instance anyway (#48 F-32).
///
/// Modelled field-for-field on [`LatchBeaconBody`]'s two identity fields,
/// including the `#[serde(default)] Option<String>` spelling and the
/// `source_for_consumer(…unwrap_or(DEFAULT_HARNESS))` normalisation, so one tab is
/// named the same way from every route. **`consumer` is a BODY field**: the
/// query-string form exists only on `/mcp/call`, whose body is not ours — it is
/// MCP JSON-RPC, owned by another protocol — so cImp's transport metadata cannot
/// go in it. Every route whose body cImp defines end to end carries `consumer`
/// here.
///
/// **Three deliberate omissions, each closing an attack:**
///
/// * **No `cwd` and no path of any kind.** `/audit/run` needs the child's cwd
///   for its wrong-instance check; this does not. A body-supplied path would let
///   a caller file a *security* row under a project it is not about. The root is
///   derived app-side from the tab ([`tab_root_key`]) or left honestly empty.
/// * **No free-text field.** `/latch/beacon` needs [`bounded_tool`] purely
///   because it accepts a caller-chosen `tool` string. With no such field there
///   is no truncation and no control-sequence question at all: this row's `tool`
///   column is the fixed literal `"discovery"`.
/// * **No pid, port or root of the skipped entries.** They would be
///   attacker-chosen strings presented to an incident reader as forensic fact.
///   What the app can say about the directory, it observes itself
///   ([`discovery_census`]).
#[derive(Deserialize, Default)]
struct DiscoverySkippedBody {
    /// The cImp tab id the reporting child was spawned for. Absent ⇒
    /// [`crate::activity::Attribution::Unattributed`], never `Headless`.
    #[serde(default)]
    tab: Option<String>,
    /// `claude` / `opencode`, normalized through `source_for_consumer`.
    #[serde(default)]
    consumer: Option<String>,
    /// How many candidates the child says it skipped. **Caller-asserted**, and
    /// the row says so — see [`bounded_skips`] for the bound and
    /// [`discovery_row`] for the honesty clause that states it.
    #[serde(default)]
    skipped: u32,
}

/// The one and only response body this route ever produces.
///
/// A constant rather than a serialized value so *"the response is not the
/// signal"* is a fact about the code and not a claim about it — see
/// [`handle_discovery_skipped`].
const DISCOVERY_ACK: &[u8] = br#"{"ok":true}"#;

/// The row's `tool` column: a fixed literal, because nothing in the body may
/// choose it.
const DISCOVERY_TOOL: &str = "discovery";

/// `POST /activity/discovery_skipped` (#48 F-32, locked decision 37): record
/// that a cImp MCP child skipped candidate discovery entries which did not
/// answer a token-authenticated `GET /health`, and reached this instance anyway.
///
/// **Why a new route rather than an extension of `/activity/contract_drift`.**
/// That route already is a token-authenticated child→app activity-row writer —
/// the finding's claim that none existed was wrong — and reusing it is still
/// wrong, for four reasons: it writes `ActivityKind::Graph`, whose single
/// 400-row lane is shared with every real graph tool call (a security row there
/// is evictable by ordinary work, and a flood there evicts ordinary work); its
/// body carries no `tab`, so its row is honestly `Unattributed` and naming a tab
/// would be the wire change anyway; `activity::record_bg` has **no `cfg(test)`
/// diversion**, which is exactly why F-20's owed test was never written, while
/// `outbound::record_flag` does; and its dedup ledger was keyed on two
/// caller-supplied strings with no bound (F-37, filed separately — since closed
/// by locked decision 42, which gave that ledger this one's discipline and its
/// row a `contract_drift_row` seam for the same reason. The first three reasons
/// are unaffected and this route still stands on its own).
///
/// # Auth
///
/// Bearer, inherited from the pre-dispatch [`authorized`] check — no route-level
/// auth code. And the honesty clause every `Origin::Http` producer owes: **the
/// launch token is readable by any process running as this user** (from
/// `.cimp-offload.json`, from `.cimp-discovery/<pid>.json`, and from the
/// generated OpenCode plugin inside the project tree). "Authenticated" here
/// means *a local process*, never *cImp's own child*.
///
/// # The response is not the signal
///
/// **`200` with the byte-identical body [`DISCOVERY_ACK`] on every single
/// path** — malformed JSON, an empty body, an unknown tab, an anonymous tab,
/// `skipped: 0`, `skipped: 9999`, a row written, a row suppressed by the gate.
/// This function has exactly one exit and nothing before it can return early.
///
/// That **diverges deliberately** from both siblings, and the divergence is the
/// point: `handle_latch_beacon` answers 400 to an unknown tab (a tab-id
/// enumeration oracle in the other direction, moot there because a token-holder
/// can read `settings.json` anyway, but not moot as a precedent) and
/// `handle_contract_drift` answers 400 to a parse error. Locked decision 37
/// requires this route to answer identically on every path. Follow the decision,
/// not the siblings — pinned by
/// `tests::the_discovery_report_answers_identically_on_every_path`.
///
/// The real signal has three consumers, none of them this reply: the activity
/// row (the user consumer F-32 exists to add), a `warn!` on `target: "offload"`
/// (the operator consumer), and the child's own unchanged `eprintln!`.
async fn handle_discovery_skipped(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    // Everything that can vary happens in here and returns `()`. No `?`, no
    // early return, no branch on the outcome.
    note_discovery_skipped(app, &req.body);
    write_simple(
        stream,
        200,
        "application/json; charset=utf-8",
        DISCOVERY_ACK,
    )
    .await
}

/// The app-side facts about a **configured** tab that a discovery row needs and
/// a request body must never be allowed to supply.
struct TabFacts {
    /// [`tab_root_key`] — resolved from settings, never from the wire.
    root: String,
    /// The V28 live-session registry's answer for this tab, never the wire's.
    session: Option<String>,
}

/// [`handle_discovery_skipped`] minus the socket: parse the body and record the
/// row, swallowing everything.
///
/// Split so the route's single exit is structural, and so the half that needs an
/// `AppHandle` (which this crate cannot mock) is one thin frame that injects the
/// two app-derived facts into [`record_discovery_skipped`] as a closure — the
/// same seam `mark_live_session_from_event` uses.
fn note_discovery_skipped(app: &AppHandle, raw: &[u8]) {
    // A parse failure is NOT an error path here: it degrades to a default body,
    // whose `skipped: 0` writes no row. Answering 400 would have been a second
    // response shape, i.e. the oracle this route exists without.
    let body: DiscoverySkippedBody = serde_json::from_slice(raw).unwrap_or_default();
    // ONE settings read for the whole request, the discipline `/mcp/call`
    // documents: the tab-identity check and the root resolution must not run
    // against two snapshots.
    let settings = live_settings(app);
    record_discovery_skipped(
        &settings,
        &body,
        claim_discovery_report,
        |tab, agent| TabFacts {
            root: tab_root_key(app, &settings, tab),
            session: app
                .try_state::<Arc<crate::graph::GraphService>>()
                .and_then(|g| g.live_session_for_tab(tab, agent)),
        },
    );
}

/// The `skipped` count a row may state, decided at the parse boundary.
///
/// * `None` ⇒ **no row at all.** A report of zero skips is not a report — a
///   genuine child returns before posting — and it is also what a malformed or
///   empty body degrades to. So "a caller can make the store write a row" costs
///   it at least a well-formed claim.
/// * `Some((n, clamped))` ⇒ write a row for `n`, and say so if it was clamped.
///
/// The ceiling is [`MAX_DISCOVERY_PROBES`], and it is not a guess: a single
/// resolution cannot skip more than its probe budget — [`Probe::answers`]
/// enforces that — so any larger value is *definitionally* not something a
/// genuine child produced. It is clamped rather than rejected because rejecting
/// would need a second response shape, which is the oracle
/// [`handle_discovery_skipped`] exists without.
fn bounded_skips(raw: u32) -> Option<(u32, bool)> {
    if raw == 0 {
        return None;
    }
    let cap = MAX_DISCOVERY_PROBES as u32;
    Some((raw.min(cap), raw > cap))
}

/// The per-key doubling ledger for discovery reports (#48 F-32).
///
/// **The key space is bounded by something the caller does not control**, which
/// is the property [`CONTRACT_DRIFT_SEEN`] lacked until decision 42 gave it one
/// too (F-37 — its key is a `&'static str` from a fixed list, a stricter bound
/// than this one because that route has no tab list to key on): entries are keyed on
/// the *resolved scope label*, so a configured tab gets its own counter and
/// `Anonymous` + `Unknown(_)` share **one** sentinel bucket per consumer. A
/// caller inventing ten thousand tab ids therefore gets one counter and
/// `log2`-many rows, not ten thousand of each. Map size is bounded by
/// `2 × (configured AI tabs + 1)`.
///
/// Process lifetime, following `CONTRACT_DRIFT_SEEN`'s precedent, and that is a
/// decision rather than an omission: the doubling makes process lifetime cheap,
/// and — unlike a latch or a budget — this is not a per-conversation
/// entitlement, so there is nothing a session rotation should restore.
static DISCOVERY_REPORTS: OnceLock<Mutex<HashMap<String, outbound::Doubling>>> = OnceLock::new();

/// Count one report against the process ledger. See [`DISCOVERY_REPORTS`].
fn claim_discovery_report(key: &str) -> outbound::DoublingRow {
    let ledger = DISCOVERY_REPORTS.get_or_init(Default::default);
    let mut ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
    claim_in(&mut ledger, key)
}

/// [`claim_discovery_report`] against a caller-owned ledger, so the key-space
/// bound and the doubling are assertable without process-global state (the
/// suite runs cases concurrently in one process).
fn claim_in(ledger: &mut HashMap<String, outbound::Doubling>, key: &str) -> outbound::DoublingRow {
    ledger.entry(key.to_string()).or_default().claim()
}

/// What the APP itself currently sees in `.cimp-discovery/`.
///
/// The half of the row a request cannot forge: the app runs on the same machine
/// as the child, so instead of believing a claim about the directory it lists
/// it. Called **only on the write path** (after the doubling gate), so it can
/// never become a filesystem-scan amplifier under a flood.
struct DirCensus {
    /// Parseable per-instance entries present right now.
    entries: usize,
    /// …of which do not belong to this process.
    foreign: usize,
}

fn discovery_census() -> DirCensus {
    let own = std::process::id();
    let all = read_all_discoveries();
    DirCensus {
        entries: all.len(),
        foreign: all.iter().filter(|d| d.pid != own).count(),
    }
}

/// Everything one discovery row states, gathered so [`discovery_row`] can stay
/// pure.
struct DiscoveryReport {
    /// The clamped, caller-asserted skip count.
    skipped: u32,
    /// Whether the caller's number exceeded the probe budget.
    clamped: bool,
    /// Reports this scope has filed (the doubling ledger's `total`).
    total: u32,
    /// How many reports this row stands for beyond itself.
    suppressed: u32,
    /// What the app observed for itself.
    observed: DirCensus,
}

/// Record one discovery report, given a settings snapshot and a way to resolve
/// a configured tab's app-side facts.
///
/// This is where locked decision 37's bar is enforced, clause by clause. A
/// token-holder can cause a row, and cannot:
///
/// * **name a non-configured tab** — the id is re-classified through
///   [`tab_identity`] against the user's own tab list. `Configured` ⇒
///   `Attribution::Tab`; `Unknown` ⇒ `Unrecognized` (bounded, [`bounded_id`],
///   because the id is caller-chosen and unbounded on the wire); `Anonymous` ⇒
///   **`Unattributed`, never `Headless`**. `Headless` is a *positive* claim —
///   "a worker run with no tab behind it" — and a body-supplied tab is
///   indistinguishable from an invented one, so claiming it would be F-20's
///   defect and F-29's, one producer further on. `Attribution::from_child_argv`
///   is forbidden here by its own doc for exactly that reason.
/// * **say anything a genuine row could not** — every remaining field is
///   app-derived: `root` from [`tab_root_key`], `session` from the V28 live
///   registry, `consumer` normalized to one of two words, `tool` a fixed
///   literal, `origin` fixed to `Http`, `skipped` clamped by [`bounded_skips`],
///   and the directory census observed rather than asserted.
/// * **cost another lane a row** — `Screen::DiscoverySkipped` is its own H-9
///   retention lane, so a flood here evicts only discovery rows
///   (`activity::tests::no_screen_can_evict_another_screens_rows` covers it for
///   every screen, this one included, without an edit).
/// * **exceed `log2(n)` rows in its own lane** — [`DISCOVERY_REPORTS`].
/// * **touch any latch** — nothing in this path reaches `latches()`; it holds no
///   registry handle and creates no entry, which
///   `every_loopback_route_declares_what_it_does_about_the_latch` checks against
///   the handler's source rather than believing.
///
/// `claim` and `facts` are injected for the same reason and it is not only
/// testability: the ledger is a process-global map and the facts need an
/// `AppHandle` this crate cannot mock, so a test that had to go through either
/// would be racing its neighbours or unable to run at all. Production wires
/// [`claim_discovery_report`] here — pinned by
/// `tests::the_discovery_report_never_reaches_the_hook_shims_path`, which reads
/// the wiring out of the source rather than trusting it.
fn record_discovery_skipped(
    settings: &crate::settings::Settings,
    body: &DiscoverySkippedBody,
    claim: impl FnOnce(&str) -> outbound::DoublingRow,
    facts: impl FnOnce(&str, &'static str) -> TabFacts,
) {
    let Some((skipped, clamped)) = bounded_skips(body.skipped) else {
        return;
    };
    let agent = crate::graph::source_for_consumer(body.consumer.as_deref().unwrap_or(crate::harness::DEFAULT_HARNESS.token()));
    let identity = tab_identity(settings, agent, body.tab.as_deref());
    // The scope label doubles as the flood key, which is deliberate: both want
    // "the identity this call actually resolved to", and the identity-less cases
    // must collapse onto one bucket rather than onto whatever the caller typed.
    let scope = match identity {
        TabIdentity::Configured(tab) => format!("{agent}:{tab}"),
        TabIdentity::Anonymous | TabIdentity::Unknown(_) => {
            format!("{agent}:{}", outbound::NO_TAB_IDENTITY)
        }
    };
    let outbound::DoublingRow::Write { total, suppressed } = claim(&scope) else {
        return;
    };

    let (attribution, root, session) = match identity {
        TabIdentity::Configured(tab) => {
            let f = facts(tab, agent);
            (
                crate::activity::Attribution::Tab(tab.to_string()),
                f.root,
                f.session,
            )
        }
        TabIdentity::Unknown(tab) => (
            crate::activity::Attribution::Unrecognized(bounded_id(tab)),
            String::new(),
            None,
        ),
        TabIdentity::Anonymous => (
            crate::activity::Attribution::Unattributed,
            String::new(),
            None,
        ),
    };

    let row = discovery_row(
        outbound::Origin::Http,
        &DiscoveryReport {
            skipped,
            clamped,
            total,
            suppressed,
            observed: discovery_census(),
        },
    );
    // The operator consumer. The user consumer is the row below; the child's own
    // stderr line is the third and is unchanged.
    warn!(
        target: "offload",
        agent,
        scope = %scope,
        skipped,
        total,
        suppressed,
        "loopback: /activity/discovery_skipped — a child skipped candidate discovery entries \
         and reached this instance anyway"
    );
    outbound::record_flag(outbound::Flag {
        screen: row.screen,
        origin: row.origin,
        consumer: agent,
        scope: &scope,
        attribution,
        session: session.as_deref(),
        tool: &row.tool,
        host: None,
        url: None,
        resolved_ip: None,
        canary: false,
        root,
        detail: &row.detail,
    });
}

/// A discovery report's `injection_flag` row, composed by a **pure** function so
/// what an incident reader is told is assertable without an `AppHandle` — the
/// same seam [`beacon_row`] and [`override_row`] exist for.
///
/// The prose carries six facts, and none of them is optional:
///
/// 1. what happened (a child skipped N candidate entries that did not answer a
///    token-authenticated `GET /health`);
/// 2. that **containment worked, and this row is the proof** — the child reached
///    *this* instance anyway, which is how the report arrived at all;
/// 3. the benign cause (an unclean shutdown leaves `.cimp-discovery/<pid>.json`
///    behind; removal is graceful-exit only);
/// 4. the hostile cause (this is also exactly what a **planted** entry looks
///    like — #48 F-11/F-28, locked decision 30);
/// 5. what to do (list `.cimp-discovery/` next to the cImp executable), together
///    with what the app observed there itself;
/// 6. the **honesty clause**: an authenticated POST from a local process is not
///    evidence of a user action, the count is caller-asserted, and nothing here
///    moved a latch, contaminated a conversation or refused a call.
fn discovery_row(origin: outbound::Origin, rep: &DiscoveryReport) -> FlagRow {
    let n = rep.skipped;
    let clamped = if rep.clamped {
        format!(
            " (the caller's number exceeded the probe budget of {MAX_DISCOVERY_PROBES} and was \
             clamped)"
        )
    } else {
        String::new()
    };
    let stands_for = if rep.suppressed > 0 {
        format!(
            " This is report {} for this scope and stands for {} further report(s) folded into \
             it (rows are written at 1, 2, 4, 8 … so a loop cannot evict this lane's history).",
            rep.total, rep.suppressed
        )
    } else {
        String::new()
    };
    FlagRow {
        screen: outbound::Screen::DiscoverySkipped,
        origin,
        tool: DISCOVERY_TOOL.to_string(),
        detail: format!(
            "DISCOVERY ENTRY SKIPPED (origin: {}): a cImp MCP child resolved its loopback \
             endpoint and skipped {n} candidate discovery entr(ies) that did not answer a \
             token-authenticated `GET /health`{clamped} — and then reached THIS instance anyway, \
             which is how this report arrived. Containment worked; this row is the proof, not an \
             alarm about a failure. After an unclean cImp shutdown a leftover \
             `.cimp-discovery/<pid>.json` produces exactly this and is harmless (removal is \
             graceful-exit only). It is ALSO what a PLANTED entry looks like: a well-formed file \
             naming a deeper project root and a port nothing serves is how untrusted content \
             steers a child onto a dead endpoint (#48 F-11/F-28, locked decision 30). If you did \
             not expect it, list `.cimp-discovery/` next to the cImp executable — this app sees \
             {} entr(ies) there right now, {} of them not its own process.{stands_for} This row \
             records an authenticated POST from a local process: the launch token is readable by \
             anything running as this user, so it is NOT evidence of a user action, and the count \
             is CALLER-ASSERTED (clamped to the probe budget of {MAX_DISCOVERY_PROBES}; the \
             directory figures above are the app's own observation and are not). Nothing here \
             moved a latch, contaminated a conversation or refused a call.",
            origin.as_str(),
            rep.observed.entries,
            rep.observed.foreign,
        ),
    }
}

// ── NC-2 (issue #5): the neutral half of hook-driven permission detection ────
//
// **V40 Phase C, locked decision 21.** What used to live here was the whole
// chain: Claude's `Notification` payload struct, its marker strings, its
// `IGNORED_NOTIFICATION_TYPES` list transcribed from the hooks guide, the
// classifier that reads `hook_event_name`, and the session-id → transcript-stem
// → cwd resolution that knows what a Claude transcript path looks like. All of
// that is `harness/claude/hook.rs` now.
//
// What stays is the part that is true of prompt detection in general: the tabs
// an edge could belong to, and the signal an edge becomes. The TUI-regex
// detector produces the same [`PermissionEdge`] from a screen scrape, and both
// producers are idempotent at the state manager — which is why a hook and a
// regex match for the same prompt collapse to one edge rather than being two
// features that must agree.

/// One tab a permission edge could belong to: its id, the harness session id it
/// is currently running (from the graph's live-session registry — `None` for a
/// configured-but-not-running tab), and the directory it launches in.
#[derive(Debug, Clone)]
pub(crate) struct PermissionTabCandidate {
    pub(crate) tab: String,
    pub(crate) session_id: Option<String>,
    pub(crate) cwd: PathBuf,
}

/// What one permission payload did: the tab whose state signal was sent, or why
/// nothing was sent.
///
/// The route answers 200 on every arm — the producers are observe-only and must
/// never be given a reason to retry — so this exists to keep the *diagnosis* out
/// of the transport, not to give a caller anything to branch on.
pub(crate) enum PermissionOutcome {
    Mapped(String),
    Unmapped(&'static str),
}

/// Every tab a permission edge could be attributed to, with the session each is
/// currently running.
///
/// Snapshots what is needed from managed state and drops the guards — nothing
/// borrowed from `AppHandle` is held across a response write. An empty answer
/// (no `AppState`, no configured tabs) is not an error: it makes the resolution
/// find nothing, which is the same "drop it rather than guess" outcome an
/// ambiguous match produces.
pub(crate) fn permission_tab_candidates(
    app: &AppHandle,
    harness: crate::harness::HarnessId,
) -> Vec<PermissionTabCandidate> {
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return Vec::new();
    };
    let sessions: Vec<(String, String)> = app
        .try_state::<Arc<crate::graph::GraphService>>()
        .map(|g| g.live_sessions_for(harness))
        .unwrap_or_default();
    crate::tabs::harness_tab_dirs(&state.settings.current(), &state.launch.cwd, harness)
        .into_iter()
        .map(|(tab, dir)| PermissionTabCandidate {
            session_id: sessions
                .iter()
                .find(|(k, _)| *k == tab)
                .map(|(_, s)| s.clone()),
            tab,
            cwd: dir,
        })
        .collect()
}

/// Emit one neutral permission edge for `tab`, returning whether it was sent.
///
/// The SAME `StateSignal`s the TUI-regex detector emits, so the whole downstream
/// pipeline (`awaiting_permission` → TTS enqueue, per-tab badge, avatar) is
/// untouched by which producer found the prompt.
///
/// Edge-triggered and best-effort, exactly like the PTY processor's `try_send`:
/// a full channel means the state manager is saturated, and the regex detector's
/// next scan re-raises the edge anyway.
pub(crate) async fn send_permission_edge(
    app: &AppHandle,
    tab: &str,
    edge: crate::harness::plugin::PermissionEdge,
) -> bool {
    use crate::harness::plugin::PermissionEdge;
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return false;
    };
    let signals = state.state_signals.clone();
    let registry = state.tabs.clone();
    let tab_id = crate::state::TabId::from_str(tab);
    let signal = match edge {
        PermissionEdge::Detected => crate::state::StateSignal::PermissionPromptDetected {
            tab: tab_id.clone(),
        },
        PermissionEdge::Resolved => crate::state::StateSignal::PermissionPromptResolved {
            tab: tab_id.clone(),
        },
    };
    let _ = signals.try_send(signal);
    // M11 (2026-08-05 review): a hook-driven Resolved clears the flag eagerly —
    // a denial from the harness's own auto-classifier can land while a genuine
    // approval prompt is still on screen. The regex fallback cannot recover on
    // its own: `PermissionDetector::check` is edge-triggered on a latched
    // per-kind pattern name, so while that same pattern keeps matching it emits
    // NOTHING. Drop the latch (and re-scan) in the tab's PTY processor so a
    // prompt that is genuinely still up is re-raised immediately. Sent AFTER
    // the Resolved signal so the two land on the state manager in that order.
    if matches!(edge, PermissionEdge::Resolved) {
        registry.lock().await.clear_permission_latch(&tab_id).await;
    }
    true
}

/// **The harness said this tab's assistant turn is over.** Relays it to the
/// state manager as [`crate::state::StateSignal::HarnessTurnEnded`], which
/// re-emits it as `StateEvent::TurnEnded` without touching the avatar.
///
/// Shaped like [`send_permission_edge`] — same lookup, same `try_send`, same
/// "no app state yet ⇒ drop it" answer. A dropped signal costs one missed idle
/// announcement, never correctness: nothing downstream latches on it.
///
/// Only a harness whose plugin declares
/// [`crate::harness::plugin::HarnessPlugin::turn_end_push`] has a producer for
/// this; see that method for why the Idle edge is not the same thing.
pub(crate) async fn send_turn_ended(app: &AppHandle, tab: &str) -> bool {
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return false;
    };
    let signal = crate::state::StateSignal::HarnessTurnEnded {
        tab: crate::state::TabId::from_str(tab),
    };
    let _ = state.state_signals.try_send(signal);
    true
}

/// A `POST /context/post_edit` request body (the Claude `PostToolUse` shim, or
/// the OpenCode plugin's `tool.execute.after` hook).
#[derive(Deserialize)]
pub(crate) struct ContextPostEditBody {
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    #[serde(default)]
    pub(crate) file_path: String,
    /// Recorded for symmetry with the shim's payload; not currently branched
    /// on (the matcher/plugin already scope this to edit-class tools).
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) tool_name: Option<String>,
    /// #48 (M-7): which shim is calling — `"claude"` (the `--postedit-hook`
    /// shim) or `"opencode"` (the generated plugin). See [`hook_agent`].
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// #48 (M-7): the cImp TAB this hook serves — `--tab <id>` from argv on the
    /// Claude side, `CIMP_TAB_ID` on the OpenCode side.
    #[serde(default)]
    pub(crate) tab: Option<String>,
}

/// **V33 C4** — every directory this instance will run the project's configured
/// CHECK COMMANDS in on a hook's behalf: the **served root** (this app's launch
/// directory) plus each **configured AI tab's** working directory, and nothing
/// else. Derived entirely from the app and the settings snapshot; the request
/// body contributes nothing to this list.
///
/// The tab dirs are here because they are not always under the launch root: V13
/// Phase D's "New tab in worktree…" sets `AiToolTabConfig::cwd` to a freshly
/// created git worktree, and a hook firing in that tab legitimately names it.
/// Resolution is [`crate::tabs::ai_tab_dir`], the same call
/// [`build_ai_tool_spec`](crate::tabs::config) makes when it actually spawns the
/// tab, so this list is the set of directories cImp itself launches agents in.
///
/// Every consumer's tabs, not the caller's: these are the operator's own
/// directories either way, and scoping the list by the caller's asserted
/// `agent` would let the assertion move a *capability* boundary — the thing
/// C5 exists to stop it doing to the identity one.
///
/// An empty vec is possible only when managed state is absent AND
/// `current_dir()` fails (a deleted cwd). It denies everything, which is the
/// correct answer: a root that cannot be resolved must read as absent, never as
/// "allow whatever was asked for".
fn hook_exec_roots(app: &AppHandle, settings: &crate::settings::Settings) -> Vec<PathBuf> {
    let launch = app
        .try_state::<crate::ipc::AppState>()
        .map(|s| s.launch.cwd.clone())
        .or_else(|| std::env::current_dir().ok());
    let Some(launch) = launch else {
        return Vec::new();
    };
    let mut roots = vec![launch.clone()];
    for tab in ai_tab_ids(settings) {
        if let Some(dir) = crate::tabs::ai_tab_dir(settings, tab, &launch) {
            if !roots.contains(&dir) {
                roots.push(dir);
            }
        }
    }
    roots
}

/// **V33 C4** — the working directory `POST /context/post_edit` may execute in,
/// or `None` to refuse.
///
/// This is [`audit_admit`]'s step 3 in a second place, deliberately built from
/// the same two helpers ([`canon`] + [`is_ancestor_or_equal`]) rather than from
/// new path logic, so the two routes' notions of "inside a root I serve" cannot
/// drift. What differs is only the answer to a miss: `/audit/run` returns a
/// readable tool error, and this route — a hook that must never perturb an edit
/// — returns its own fail-safe (empty text) with an operator-visible `warn!`.
///
/// Three cases:
///
/// 1. **No `cwd` on the wire** ⇒ the served root. The pre-V33 default was
///    `PathBuf::from(".")`, i.e. the app process's cwd; the served root is that
///    same directory by a route that cannot be moved by a `chdir` and that is
///    stated rather than implied.
/// 2. **A `cwd` at or under one of the roots** ⇒ admitted, and passed through
///    **as written**. The path string keys the single-flight `RootRunner`
///    bucket and the auto-check baseline downstream, so canonicalizing it here
///    would silently re-bucket every existing caller.
/// 3. **Anything else** ⇒ `None`. Including a path containing `..`, which is
///    refused inside [`is_ancestor_or_equal`] rather than here: a component walk
///    cannot resolve a `..`, and [`canon`] only resolves one for a path that
///    EXISTS, so an unresolved `..` reaching a zip-compare reads as a
///    descendant. That refusal is shared with [`audit_admit`] step 3
///    deliberately — see the helper's own note for the measurement behind it,
///    including why the Windows spelling in this comment's first draft
///    (`P:\served\..\..\evil`) was rejected for the wrong reason and
///    `\\?\P:\served\..\..\evil` was not rejected at all. Costs nothing here:
///    every real caller sends the absolute cwd its harness reported.
fn admitted_hook_root(roots: &[PathBuf], requested: Option<&str>) -> Option<PathBuf> {
    let Some(req) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return roots.first().cloned();
    };
    let hint = canon(Path::new(req));
    roots
        .iter()
        .any(|r| is_ancestor_or_equal(&canon(r), &hint))
        .then(|| PathBuf::from(req))
}

/// `POST /context/post_edit` (V12 Phase F): debounce this session's edits, run
/// the project's configured checks single-flight per root, diff against the
/// session's own baseline, and return only NEW/worsened diagnostics (plus an
/// optional auto-impact note) as `{ ok, text }`. Fails open to empty text on
/// any missing state — the hook must never block or perturb an edit.
///
/// #48 (M-7): gated through [`hook_admit`] on [`HOOK_TOOL_POST_EDIT`]. This is
/// the route the finding is really about — it **executes the project's
/// configured check commands**, which is the definition of LOCAL-CAPABILITY
/// under decision 1, and it did so with no `latches()` call at all. A refusal
/// answers with the route's own fail-safe (empty text), so a contaminated
/// conversation loses its auto-check diagnostics and nothing else; the edit
/// itself is never perturbed.
///
/// **V33 C4 closes the directory half.** The `cwd` those commands run in used
/// to come straight out of the request body (defaulting to `"."`) with no
/// ancestor check and no allowlist, so anything holding the loopback token could
/// have the operator's own vetted check commands executed in a directory it
/// named — a cloned repo's `Makefile`, say, reached through a `cargo`/`npm`
/// script the operator configured for their own project. It is now resolved through
/// [`admitted_hook_root`] against [`hook_exec_roots`], which derives from the
/// served root and the configured tabs and **never from the request**. A refusal
/// takes the route's own fail-safe (empty text) and logs; it cannot perturb the
/// edit.
///
/// **The identity half is deliberately untouched** (locked V33 decision 2). A
/// body with no usable `tab` still resolves to no scope and is ADMITTED, exactly
/// as on `/graph_run` and `/mcp/call` — see the residual note above `hook_admit`.
/// The two halves are independent: C4's allowlist is app-derived, so omitting
/// `tab` does not walk around it, which is why the directory half could be
/// closed without settling the identity one.
///
/// **Why the sibling hook routes get no such check** (so the asymmetry is not
/// later read as an oversight): `/context/should_read` and
/// `/context/compaction` take the same caller-supplied `cwd` and share the same
/// identity fail-open, but neither EXECUTES anything with it — it selects which
/// project's index to read, and what a read can hand back is what their
/// [`toolclass::TABLE`] rows and their [`hook_admit`] gate already decide. There
/// is no command to run in a directory a caller names, so there is nothing for
/// a root allowlist to contain. If either ever grows a spawn, it inherits this
/// route's treatment.
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
    let settings = live_settings(app);
    if hook_admit(
        latches(),
        HOOK_TOOL_POST_EDIT,
        hook_agent(body.agent.as_deref()),
        body.tab.as_deref(),
        |agent, tab| latch_scope(app, &settings, agent, tab),
        |scope| GatePolicy::resolve(&settings, scope),
    )
    .is_err()
    {
        return write_json(stream, 200, &empty).await;
    }
    let text = post_edit_diagnostics(app, &settings, &body).await;
    write_json(stream, 200, &serde_json::json!({ "ok": true, "text": text })).await
}

/// The auto-check diff for one edit, **after** the gate — including V33 C4's
/// root admission, which is part of the work rather than part of the latch gate.
/// Shared by `/context/post_edit` and [`crate::harness::claude::hook::ROUTE_POST_TOOL_USE`]; see
/// [`compaction_block`] for why the latch gate stays at each route.
pub(crate) async fn post_edit_diagnostics(
    app: &AppHandle,
    settings: &crate::settings::Settings,
    body: &ContextPostEditBody,
) -> String {
    // V33 C4: decide WHERE before deciding whether there is anything to run —
    // the roots are app-derived, so this cannot be moved by the body.
    let exec_roots = hook_exec_roots(app, settings);
    let Some(cwd) = admitted_hook_root(&hook_exec_roots(app, settings), body.cwd.as_deref()) else {
        // Bounded: the rejected string is caller-chosen and unbounded on the
        // wire, and this is the one place it reaches an operator-facing line.
        warn!(
            target: "offload",
            requested = %bounded_id(body.cwd.as_deref().unwrap_or_default()),
            "loopback: /context/post_edit named a working directory outside this instance's \
             served root and its configured tabs' directories — the project's configured check \
             commands were NOT run there (the edit itself is unaffected)"
        );
        return String::new();
    };
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return String::new();
    };
    // #104: admitted is not the same as *is a project root* — a sub-agent's cwd
    // passes C4's allowlist perfectly well and `post_edit` opens the project's
    // store from whatever it is handed. Resolve it, then **re-apply C4 to the
    // answer**: the walk goes UP, and a resolved root above every served root
    // would run the operator's check commands one directory further out than
    // C4 admits. It never widens; on a miss the route takes its own fail-safe.
    let root = external_project_root(app, settings, body.tab.as_deref(), Some(&cwd.to_string_lossy()));
    let Some(root) = root.filter(|r| {
        let r = canon(r);
        exec_roots.iter().any(|allowed| is_ancestor_or_equal(&canon(allowed), &r))
    }) else {
        warn!(
            target: "offload",
            requested = %bounded_id(&cwd.to_string_lossy()),
            "loopback: /context/post_edit could not resolve a project root at or under \
             this instance's served roots from the working directory it named — the \
             project's configured check commands were NOT run (the edit itself is \
             unaffected)"
        );
        return String::new();
    };
    let graph = graph.inner().clone();
    graph
        .post_edit(&root, body.session_id.as_deref(), &body.file_path)
        .await
        .unwrap_or_default()
}

/// **The live-session write a `/memory/event` body asks for** (V40 Phase D,
/// locked decision 20).
///
/// The body names a SESSION, so the write lands in the session key space —
/// where it cannot name a cImp tab. That is the whole of the C-2 fix now: it
/// used to be `mark_live_session_from_event`, which refused any key that
/// exactly matched a configured AI tab id, because one map held both key spaces
/// and a POST could therefore repoint a running tab's session (flapping the
/// taint latch clear in a loop, with the real tap's re-stamp producing a second
/// rotation that helped the attacker). A check beside the write has to keep a
/// list in step; separate spaces make the collision unrepresentable.
///
/// A harness whose identity is TAB-keyed ([`SessionKey::Tab`] — its session is
/// bound by cImp's own reader) gets **no registry write from a request body at
/// all**: its live session is not something a wire value may claim. An
/// unregistered `agent` likewise writes nothing — fail closed.
///
/// `mark` is the registry write, taken as a parameter rather than reached
/// through a `GraphService` this crate has no `AppHandle` to build — the same
/// reasoning #48 gave for the function this replaces: a bound asserted *beside*
/// its enforcement point survives deleting the call, so the test drives this
/// function and observes whether (and into which space) the write happened.
fn mark_live_session_from_body(
    mark: impl FnOnce(crate::harness::plugin::SessionKey, &str),
    agent: &str,
    session: &str,
) {
    let space = crate::harness::HarnessId::from_id(agent)
        .and_then(|h| h.plugin())
        .map(|p| p.session_key_space());
    match space {
        Some(crate::harness::plugin::SessionKey::Session) => {
            mark(crate::harness::plugin::SessionKey::Session, session);
        }
        Some(crate::harness::plugin::SessionKey::Tab) => debug!(
            target: "offload",
            agent,
            "loopback: /memory/event named a session for a tab-keyed harness; its reader owns \
             that binding, so nothing is written"
        ),
        None => warn!(
            target: "offload",
            agent,
            "loopback: /memory/event from an unregistered harness; no live-session write"
        ),
    }
}

/// `POST /memory/event`: record what a harness's memory-ingress body reports —
/// its tool events, and (V14 Phase C) the usage the same hook is the only
/// source of. Best-effort — an unclassifiable tool or a missing graph service is
/// a silent no-op (200 with the recording skipped), never an error the plugin
/// has to handle.
///
/// **V40 Phase I (issue #107 item 2): the body shape is the harness's.** This
/// function used to declare the wire struct itself and read every field of it —
/// `msg_id`, `in_tok`, `parent_session_id`, `tool`, `args`. It named no harness
/// *id*, so the layering allowlists stayed clean, but the row shape was one
/// plugin's, and a second harness's would have had nowhere to live but here.
/// [`crate::harness::plugin::HarnessPlugin::memory_event`] reads it now and
/// answers a neutral [`crate::harness::plugin::MemoryEvent`]; what stays is the
/// recording, which is cImp's: the `cwd` resolution, the graph writes and the
/// live-session registry.
async fn handle_memory_event(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    use crate::harness::plugin::MemoryEventKind;

    // Which harness is speaking. The `agent` discriminator is CHP's, not any
    // harness's, so core reads that one field itself; everything else in the
    // body belongs to whoever sent it. An identity-less body resolves through
    // `wire_default`, which is this route's compatibility promise to plugins
    // generated before the field existed.
    let asserted = serde_json::from_slice::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v.get("agent").and_then(|a| a.as_str()).map(str::to_string));
    let agent = wire_agent(MEMORY_EVENT_ROUTE, asserted.as_deref());
    let ok = serde_json::json!({ "ok": true });
    // A harness with no memory ingress — or an `agent` naming no registered
    // harness at all — records nothing. Locked decision 2: `None` is a
    // first-class answer here, not a reason to fall back to whichever harness
    // core happens to know the body shape of.
    let Some(parsed) = crate::harness::HarnessId::from_id(agent)
        .and_then(|h| h.plugin())
        .and_then(|p| p.memory_event(&req.body))
    else {
        return write_json(stream, 200, &ok).await;
    };
    let event = match parsed {
        Ok(e) => e,
        Err(why) => {
            return write_json(stream, 400, &serde_json::json!({ "ok": false, "error": why })).await;
        }
    };

    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return write_json(stream, 200, &ok).await;
    };
    let graph = graph.inner().clone();
    // #104: every arm below opens the project's store (memory rows, usage
    // totals), so the plugin-supplied `cwd` is resolved to a real root first.
    // This body carries no `tab` — the memory POST never had one — so an
    // unresolvable cwd has nothing to fall back to and the event is dropped
    // rather than filed against a directory that is not a project.
    let Some(cwd) = external_project_root(app, &live_settings(app), None, event.cwd.as_deref())
    else {
        return write_json(stream, 200, &ok).await;
    };
    // C-2 (2026-08-07 review) used to read settings here, once for the whole
    // request, so the three live-session writes below could refuse a key that
    // named a configured tab. V40 Phase D removed the read with the check: the
    // registry has two key spaces now and `mark_live_session_from_body` decides
    // which one a body-supplied id lands in, which needs no settings at all.
    let mark_live = |target: &str| {
        mark_live_session_from_body(
            |space, k| graph.mark_live_session(space, k, agent, k),
            agent,
            target,
        )
    };

    match event.kind {
        // V24 Phase F: a completed assistant turn's real token totals. The
        // roll-up target and the declared lane are the sending harness's choice
        // (locked decision 19); `record_usage` upserts by `msg_id`, so the
        // plugin's duplicate final emit is harmless.
        MemoryEventKind::Turn {
            target,
            origin,
            msg_id,
            model,
            in_tok,
            out_tok,
            cache_read,
            cache_make,
        } => {
            graph.record_usage(
                &cwd,
                &target,
                agent,
                crate::graph::UsageEvent::Turn {
                    msg_id,
                    model,
                    in_tok,
                    out_tok,
                    cache_read,
                    cache_make,
                    origin: origin.to_string(),
                },
            );
            // Mark the SAME id live: the target is the session row that exists
            // / gets the spend attributed (the parent when a child reports), so
            // that's the row the Sessions list should flag active.
            mark_live(&target);
        }
        // A sub-agent's tool call: recorded against nobody, but the PARENT
        // stays live — the child's activity is the parent still working.
        MemoryEventKind::SubagentTool { parent } => mark_live(&parent),
        MemoryEventKind::Tool { tool, args } => {
            // V40 Phase A, locked decision 16: the memory classification is the
            // SENDING harness's, resolved through the registry. A body whose
            // `agent` names no registered harness records nothing — where the
            // old single `match` would have answered it out of whichever
            // vocabulary happened to contain the name.
            let source = crate::harness::HarnessId::from_id(agent);
            if let Some((kind, arg)) = crate::harness::native::memory_kind(source, &tool) {
                // V40 Phase C, locked decision 16: which KEY carries the target
                // is the sending harness's vocabulary, not core's. This was a
                // chain of four `or_else`s mixing one harness's snake_case with
                // another's camelCase in one lookup — see
                // `HarnessPlugin::memory_arg_keys`.
                let value = crate::harness::native::memory_arg(source, arg, &args);
                let (path, detail) = match arg {
                    crate::harness::plugin::MemArg::Path
                    | crate::harness::plugin::MemArg::Pattern => (value.unwrap_or_default(), None),
                    crate::harness::plugin::MemArg::Command => (
                        String::new(),
                        value.map(|c| c.chars().take(200).collect::<String>()),
                    ),
                };
                // Skip an event with no usable target: an empty path
                // (Path/Pattern) or a Command whose `command` arg was absent
                // (detail is None) — recording it would just evict useful
                // events from the ring.
                let recordable = match arg {
                    crate::harness::plugin::MemArg::Command => detail.is_some(),
                    _ => !path.is_empty(),
                };
                if recordable {
                    graph.record_mem_event(
                        &cwd,
                        &event.session_id,
                        agent,
                        kind,
                        &path,
                        None,
                        None,
                        detail.as_deref(),
                    );
                }
            }

            // V14 Phase C: the usage tap. Unlike the memory recording above,
            // this runs for EVERY tool call, not just ones the native table
            // maps to a filesystem/query target — usage wants the full picture.
            // `chars` is estimated from the tool's serialized INPUT args (its
            // actual output isn't visible to this hook). This path records only
            // tool-result chars, never Turn tokens, so a session that never got
            // a real usage event stays est-only in the X-ray (V24 Phase E
            // derives `est_only` from zero token totals — see
            // `usage_row_for_session`).
            let chars = serde_json::to_string(&args)
                .map(|s| s.chars().count())
                .unwrap_or(0) as u32;
            graph.record_usage(
                &cwd,
                &event.session_id,
                agent,
                crate::graph::UsageEvent::ToolResult {
                    tool: Some(tool),
                    chars,
                },
            );

            // V24 Phase B: this harness has no tab binding on this path, so the
            // live-session registry is keyed by the reporting session id itself;
            // the entry expires by TTL (there is no cancel signal to clear it).
            // C-2: which is exactly why the key must not be allowed to name a
            // TAB — the other half of the same map.
            mark_live(&event.session_id);
        }
        MemoryEventKind::Nothing => {}
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
    // An unrecognised consumer advertises NOTHING rather than inheriting
    // another's grants (locked decision 2). An empty list, not an error: a
    // `tools/list` that 400s would break the child's handshake, while an empty
    // one is the honest answer to "what may this caller reach".
    // V40 review H-1: through the same funnel `/mcp/call` resolves its grant
    // with, so a token that is callable is exactly a token that is listed.
    let tools = match proxy_identity(query_param(&req.path, "consumer")) {
        Some((c, _)) => service.mcp_tool_descriptors(c).await,
        None => Vec::new(),
    };
    write_json(stream, 200, &serde_json::json!({ "tools": tools })).await
}

/// `POST /mcp/call`: run one proxied MCP tool for the requesting consumer.
/// Body `{name, arguments}`; consumer from `?consumer=` (Claude when absent).
/// The service guards the call against any tool not offered by a server
/// exposed to that consumer. 200 even on a tool-level error (the child renders
/// `error` as a tool result).
///
/// V32 Phase B adds the two halves of the consumer-side containment: the tab's
/// session taint latch in front of the call, and the spotlighting envelope
/// around its result. This is the tab's untrusted-content intake — the one
/// route through which a fetched page's bytes reach a Claude/OpenCode session.
async fn handle_mcp_call(
    stream: &mut TcpStream,
    service: &Arc<OffloadService>,
    app: &AppHandle,
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
    // V40 review H-1: the GRANT and the LATCH KEY are resolved together, here,
    // before anything is charged or gated — `proxy_identity` folds an in-app
    // consumer onto `Consumer::conservative_grant` and derives `agent` from the
    // FOLDED consumer, so a request served Claude's server set is judged under
    // Claude's latch. Refused (not degraded) for a token nobody declared:
    // locked decision 2, and refusing here means an unattributable caller
    // cannot spend a tab's budget either.
    let Some((consumer, agent)) = proxy_identity(query_param(&req.path, "consumer")) else {
        return write_json(
            stream,
            400,
            &serde_json::json!({ "error": unknown_consumer_message() }),
        )
        .await;
    };
    // V32 Phase G: ONE settings read here, so the tab-id check, the latch, the
    // budget, detection and the envelope all resolve under the same snapshot —
    // a mid-call settings save must not leave a result screened by one posture
    // and wrapped by another. #45 adds the tab-id check inside `latch_scope` to
    // that list, which is why the read precedes it.
    //
    // Corrected 2026-08-08 (#48, review finding G-4): this comment also named
    // "the SSRF screen", and that one is NOT resolved here. `ServiceHandle::mcp_call`
    // builds `outbound::Policy` from its own independent `self.settings.current()`,
    // so the SSRF guard can be a snapshot behind or ahead of everything above.
    // Benign in practice (sub-millisecond, both postures the user's own) and
    // recorded as an accepted residual in the V32 spec rather than fixed here;
    // the fix is to thread `settings` into `mcp_call`. Do not restore the old
    // wording without making it true.
    let settings = live_settings(app);
    let scoping = latch_scope(app, &settings, agent, body.tab.as_deref());
    // #48 F-20 — see `handle_graph_run`: resolved before the collapse. This is
    // the row that answers "which tab fetched that page", and it is the one the
    // finding says could not.
    let tab_attr = scoping.attribution();
    let scope = scoping.into_scope();
    let inj_scope = crate::settings::injection::Scope::for_tab(agent, body.tab.as_deref());
    let gate_policy = GatePolicy::resolve(&settings, scope.as_ref());
    // V32 Phase C: the flagged row's provenance, read from the arguments before
    // they are moved into the call — the result alone cannot say which page it
    // came from, and that is the first thing a user reads off the row.
    //
    // #48 (F-3): read BEFORE the gate rather than after the budget, because the
    // gate is now also a row writer — this is the route whose admitted call
    // contaminates the conversation, and "from which page" is one of the three
    // facts the contamination row exists to carry. Moving the read up changes
    // nothing else: `origin_of` only inspects the arguments.
    let (flag_url, flag_host) = detection::origin_of(&body.arguments);
    // The gate's V32 Phase C2 `WriteTaint` is discarded here, and can only ever
    // be `Clean`: this route serves proxied `<server>__<tool>` ids, every one of
    // which classifies EXTERNAL by the unknown-⇒-EXTERNAL invariant, so no
    // PERSISTENT-WRITE can arrive on it. Memory writes reach cImp through
    // `/graph_run` alone.
    if let Err(refusal) = latches().gate(
        scope.as_ref(),
        LatchRoute::Proxied,
        &body.name,
        gate_policy,
        CallProvenance::intake(flag_url.as_deref(), flag_host.as_deref()),
    ) {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(refusal.to_string()),
        };
        return write_json(stream, 200, &r).await;
    }
    // V32 Phase C: the session's EXTERNAL budget, checked after the latch (a
    // latched-out call was never going to run, and must not consume the one
    // budget report) and before the call leaves the process.
    if let Err(refusal) = latches().budget_gate(
        scope.as_ref(),
        crate::settings::injection::budget_limits(&settings, inj_scope),
        &body.name,
    ) {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(refusal.to_string()),
        };
        return write_json(stream, 200, &r).await;
    }

    let cwd = body.cwd.map(PathBuf::from);
    // The scope label the SSRF screen's `injection_flag` row carries. Without
    // tab identity we are already fail-open on the latch and the budget; the
    // SSRF screen still runs (it needs no identity), so name the scope honestly
    // rather than inventing one.
    let scope_label = scope
        .as_ref()
        .map(LatchScope::label)
        .unwrap_or_else(|| format!("{agent}:{}", outbound::NO_TAB_IDENTITY));
    let detection_cfg = detection::Config::from_settings(&settings, inj_scope);
    let spotlight_on = crate::settings::injection::effective(
        crate::settings::injection::Feature::Spotlighting,
        inj_scope,
        &settings,
    );
    let root_key = cwd
        .as_deref()
        .map(crate::activity::root_key)
        .unwrap_or_default();
    // #48: the tab session's audit-row claim ledger, threaded to the SSRF
    // chokepoint and to the detection boundary so neither can flood a capped
    // feed on a loop. See `TabAudit`.
    let audit = TabAudit(scope.as_ref(), agent);
    let called = service
        .mcp_call(
            consumer,
            &body.name,
            body.arguments,
            cwd.as_deref(),
            &scope_label,
            body.tab.as_deref(),
            tab_attr,
            &audit,
        )
        .await;
    // V32 Phase C, corrected in #48 (D-3): charge the session's EXTERNAL budget
    // for the call that was just ATTEMPTED — before the match, so it cannot
    // again end up on one arm only. See `LatchRegistry::charge_call`.
    latches().charge_call(scope.as_ref(), &called);
    let r = match called {
        // Locked decisions 5 + 6: detection, the envelope and the warning
        // header all compose here, at the proxy's tool-result boundary, so
        // EVERY consumer gets them identically — and they apply whether or not
        // the call carried tab identity, since none of the three needs it. The
        // same `wrap_external_result` the worker's boundary calls, so the
        // external-only rule and the composition order have one definition.
        //
        // #48 M-17 corrects the sentence that used to end this comment — "Errors
        // are cImp-composed strings, not fetched content, and are never screened
        // or wrapped." The diagnostic half is cImp's; the server's own
        // `error.message` never was, and it reached the model here with no bound,
        // no envelope and no screen. `HostError` keeps the two halves apart and
        // `wrap_remote_error` treats the remote half as what it is.
        Ok(text) => {
            let wrapped = detection::wrap_external_result(
                &body.name,
                text,
                detection::ResultCtx {
                    consumer: agent,
                    scope: &scope_label,
                    root: root_key,
                    url: flag_url,
                    host: flag_host,
                    cfg: detection_cfg,
                    spotlight: spotlight_on,
                    audit: &audit,
                    // #48/M-5: the proxy truncates NOTHING after this point — the
                    // consumer (a Claude/OpenCode tab) receives the whole result.
                    // This is the boundary where the unscreened notice is
                    // load-bearing, and the reason it is derived rather than
                    // deleted.
                    delivered_bytes: usize::MAX,
                },
            )
            .await;
            RunResult {
                ok: true,
                text: Some(wrapped),
                error: None,
            }
        }
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(
                detection::wrap_remote_error(
                    &body.name,
                    e.diagnostic(),
                    e.remote(),
                    detection::ResultCtx {
                        consumer: agent,
                        scope: &scope_label,
                        root: root_key,
                        url: flag_url,
                        host: flag_host,
                        cfg: detection_cfg,
                        spotlight: spotlight_on,
                        audit: &audit,
                        // As above: nothing truncates a proxied error either.
                        delivered_bytes: usize::MAX,
                    },
                )
                .await,
            ),
        },
    };
    write_json(stream, 200, &r).await
}

/// A `POST /latch/beacon` body — V32 Phase F (locked decision 14).
///
/// Posted by the OpenCode plugin's `tool.execute.before` handler when the model
/// reaches for a HARNESS-NATIVE web tool — and, until 2026-08-17, by the
/// `cimp --taint-beacon` Claude shim, which a tab open across that upgrade may
/// still be running. Claude's current path is
/// [`crate::harness::claude::hook::ROUTE_PRE_TOOL_USE_TAINT`], whose handler carries Claude's own
/// hook payload and reaches [`latch_beacon_core`] directly rather than through
/// this body. Every field except `tab` is descriptive; `tab` is the only one the
/// latch actually needs.
#[derive(Deserialize)]
struct LatchBeaconBody {
    /// The cImp tab id the reporting harness was spawned for. Absent ⇒
    /// fail-open, exactly like [`GraphRunBody::tab`].
    #[serde(default)]
    tab: Option<String>,
    /// `claude` / `opencode`, normalized through `source_for_consumer` so one
    /// tab keys the same latch from every route.
    #[serde(default)]
    consumer: Option<String>,
    /// The native tool that is about to run (`WebFetch`, `webfetch`, …). Log
    /// and diagnostics only.
    #[serde(default)]
    tool: Option<String>,
}

/// A `POST /latch/state` body — V32 Phase H (locked decision 17).
///
/// Same two identity fields as [`LatchBeaconBody`] and nothing else: the query
/// is "what is in force for this tab", and the answer must not depend on
/// anything the *caller* claims about the tool it is about to run.
#[derive(Deserialize)]
struct LatchStateBody {
    /// The cImp tab id. Absent ⇒ no scope ⇒ the fail-open answer (`gate:false`).
    #[serde(default)]
    tab: Option<String>,
    /// `claude` / `opencode`, normalized through `source_for_consumer`.
    #[serde(default)]
    consumer: Option<String>,
}

/// `POST /latch/beacon`: engage a tab's EXTERNAL latch because the HARNESS's
/// own web tool is about to run (locked decision 14, sensor mode).
///
/// Behind the same bearer token as every other route — an unauthenticated
/// caller must not be able to latch a tab out of its local tools, which would
/// be a denial-of-service on the user's session dressed as containment.
///
/// **This is the route #45 left reachable**, and the reasoning is the asymmetry
/// between what it can do and what the removed override route could. A beacon
/// only ever TIGHTENS: Open → External, plus the contamination bit. It cannot
/// flip to Local, cannot unlatch, and cannot clear contamination. Its abuse case
/// is therefore a denial of the user's own local tools, recoverable by a tab
/// restart — not an escape from containment. Against that it has a real caller
/// (the Claude `PreToolUse` shim and the OpenCode plugin) with no IPC path
/// available to it, because it fires from a child process. Two hardenings make
/// the residual honest:
///
/// 1. **The `tab` is validated** against the user's configured AI tabs
///    ([`is_configured_tab`]) — the fix for the registry-growth finding, since
///    an unvalidated body-supplied key is the map's whole key space.
/// 2. **An engagement writes an origin-marked row** ([`outbound::Origin::Http`]),
///    so the feed says a local process asserted this rather than implying the
///    user did. Bounded to one row per tab-session by
///    [`BeaconOutcome::engaged`], because the latch is sticky.
///
/// Answers 200 with the tab's resulting view for every beacon it accepts,
/// including one with no tab identity (nothing engaged, `latch: "open"`). The
/// reporter is a fail-open shim that discards the body; the status code exists
/// for a human reading a trace, not for control flow — which is also why a
/// rejected tab id gets a 400 it will never read.
async fn handle_latch_beacon(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: LatchBeaconBody = match serde_json::from_slice(&req.body) {
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
    let agent =
        crate::graph::source_for_consumer(body.consumer.as_deref().unwrap_or(crate::harness::DEFAULT_HARNESS.token()));
    // ONE settings read for the whole request: the tab-id check, `latch_scope`
    // and the policy must not resolve against three different snapshots.
    let settings = live_settings(app);
    // #45: reject an unknown tab id explicitly rather than letting it fall into
    // `latch_scope`'s fail-open `None`. The two are the same for the registry
    // (nothing is created either way), but they are not the same for a reader:
    // "this beacon named a tab that does not exist" is a fact worth a log line,
    // and answering 200 to it would tell a prober that the id was accepted.
    //
    // No activity row here, deliberately. The id is entirely caller-supplied,
    // so a row per rejection is an unbounded write into a capped feed — it would
    // evict the genuine rows this issue exists to preserve. The signal's
    // consumer is the enforcement itself (the request is refused) plus the
    // ABSENCE of the engagement row a real beacon leaves.
    //
    // #48: the check reads the SAME resolution the scope does, rather than a
    // second `is_configured_tab` call beside it — two spellings of one rule are
    // two things to keep in step.
    let tool = bounded_tool(body.tool.as_deref());
    match latch_beacon_core(latches(), app, &settings, agent, body.tab.as_deref(), &tool) {
        Ok(view) => write_json(stream, 200, &serde_json::json!({ "ok": true, "latch": view })).await,
        Err(tab) => {
            warn!(
                target: "offload",
                agent,
                tab = %tab,
                tool = %tool,
                "loopback: /latch/beacon rejected — not a configured tab id"
            );
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!(
                    "unknown tab id {tab:?} — /latch/beacon accepts configured AI tabs only"
                )),
            };
            write_json(stream, 400, &r).await
        }
    }
}

/// **The taint engagement itself** — the core both fire seams reach: this
/// route's harness-neutral body (the OpenCode plugin) and
/// [`crate::harness::claude::hook::ROUTE_PRE_TOOL_USE_TAINT`]'s Claude hook payload.
///
/// Split out on 2026-08-17, when the Claude side stopped being a shim POSTing to
/// the route and became a handler beside it. One core, so the two transports
/// cannot come to disagree about the #45 narrowing, the policy resolution, the
/// provenance or the row the engagement writes.
///
/// `Err(tab)` is the ONE case the two callers answer differently — the id names
/// no configured AI tab — and it is returned rather than handled here because
/// this route 400s a caller that will never read it while a Claude hook must
/// answer `{}` on every path (a `PreToolUse` non-2xx is a non-blocking error the
/// harness logs, and there is nothing to log about a hook with nothing to say).
/// Either way nothing is engaged and no registry entry is created, which is #45's
/// bound.
/// **The taint beacon, as one call a plugin can make** (V40 Phase C).
///
/// The narrow twin of [`latch_beacon_core`], for the same reason
/// [`hook_gate_admits`] is the narrow twin of [`hook_admit`]: the registry the
/// core takes is a private type, and a harness's ingress route must be able to
/// engage the latch without holding the latch machinery. Same core, same row,
/// same #45 narrowing — `Err(tab)` still means "named no configured tab, nothing
/// engaged".
pub(crate) fn latch_beacon_for(
    app: &AppHandle,
    settings: &crate::settings::Settings,
    agent: &'static str,
    tab: Option<&str>,
    tool: &str,
) -> Result<LatchView, String> {
    latch_beacon_core(latches(), app, settings, agent, tab, tool)
}

fn latch_beacon_core(
    reg: &LatchRegistry,
    app: &AppHandle,
    settings: &crate::settings::Settings,
    agent: &'static str,
    tab: Option<&str>,
    tool: &str,
) -> Result<LatchView, String> {
    let scoping = latch_scope(app, settings, agent, tab);
    if let LatchScoping::Unknown(tab) = scoping {
        return Err(tab);
    }
    let scope = scoping.scope();
    let policy = GatePolicy::resolve(settings, scope);
    // `CallProvenance::http()`: both seams are a loopback POST from a local
    // process (a Claude hook's POST is the harness's, which is no better), and
    // the contamination row this may write has to say so for the same reason the
    // beacon row does — the launch token is readable by anything running as this
    // user (#45).
    let out = reg.beacon(scope, tool, policy, CallProvenance::http());
    report_beacon(scope, outbound::Origin::Http, tool, &out);
    Ok(out.view)
}

/// Write the [`Screen::LatchBeacon`](outbound::Screen) row for one beacon, if
/// this beacon is the one that reports.
///
/// Split out of `handle_latch_beacon` (#48, F-3) so the *pair* of rows a beacon
/// produces is assertable: `LatchRegistry::beacon` writes the contamination row
/// itself, this writes the beacon row, and the two say different things — "this
/// conversation stopped being clean" and "a harness-native web tool was
/// detected". A test that only saw one of them could not tell a regression that
/// dropped the other from a design that never had it.
///
/// Keyed on [`BeaconOutcome::report`], not on `engaged`: a beacon that
/// contaminates a `Local`-latched tab moves no latch and used to leave no trace
/// at all, while quarantining every `context_note` the tab made afterwards.
fn report_beacon(
    scope: Option<&LatchScope>,
    origin: outbound::Origin,
    tool: &str,
    out: &BeaconOutcome,
) {
    if !out.report {
        return;
    }
    let Some(scope) = scope else { return };
    let row = beacon_row(origin, tool, out);
    outbound::record_flag(outbound::Flag {
        screen: row.screen,
        origin: row.origin,
        consumer: scope.agent,
        scope: &scope.label(),
        // The scope is in hand — see `LatchScope::attribution`.
        attribution: scope.attribution(),
        session: scope.session.as_deref(),
        tool: &row.tool,
        host: None,
        url: None,
        resolved_ip: None,
        canary: false,
        root: scope.root.clone(),
        detail: &row.detail,
    });
}

/// The upper bound on a caller-supplied tool name before it reaches an activity
/// row, a log line or the TTS surface (#48). Long enough for every real
/// harness tool name (`WebFetch`, `websearch`) with room to spare.
const BEACON_TOOL_MAX: usize = 64;

/// `/latch/beacon`'s `tool`, bounded (#48).
///
/// The field is an arbitrary unbounded string from a request body and it lands
/// in the row's `tool` column and its `detail`. Svelte escapes on render and
/// the row is bounded to one per tab-session, so this is not an injection or a
/// flood — what it is is a caller choosing how many bytes of the feed, the
/// `tracing` output and the TTS surface one beacon occupies. Truncated by
/// **chars**, not bytes, so a multi-byte name cannot be cut mid-codepoint.
///
/// Control-sequence hygiene is a separate concern with its own owner (Phase D,
/// at the surfaces that render); this only bounds length.
pub(crate) fn bounded_tool(raw: Option<&str>) -> String {
    let raw = raw.map(str::trim).filter(|t| !t.is_empty());
    let Some(raw) = raw else {
        return "(native web tool)".to_string();
    };
    bounded_id(raw)
}

/// One caller-supplied identifier, bounded before it reaches an activity row —
/// the truncation half of [`bounded_tool`], shared rather than re-spelled.
///
/// Its second caller is [`record_discovery_skipped`]'s `Unrecognized` arm (#48
/// F-32): a tab id that names no configured tab is an arbitrary unbounded string
/// from a request body, and putting it in a row verbatim would let a caller
/// choose how many bytes of a capped feed one report occupies. **Only ever
/// applied AFTER classification** — truncating first could fold a long invented
/// id onto a configured one, which would turn a bound into a forgery primitive.
///
/// Its third and fourth callers are #48 F-39 and F-37 (locked decision 42), the
/// same string half of the same class: [`LatchScoping::attribution`]'s
/// `Unrecognized` arm — reached by `/graph_run` and `/mcp/call`, and likewise
/// only after [`latch_scope`] classified the full id — and
/// [`contract_drift_row`], where the shim name and the session id a hook shim
/// reports are both arbitrary strings that reach a row.
///
/// Truncated by **chars**, not bytes, so a multi-byte id cannot be cut
/// mid-codepoint. Control-sequence hygiene is a separate concern with its own
/// owner (Phase D, at the surfaces that render); this only bounds length.
pub(crate) fn bounded_id(raw: &str) -> String {
    let mut out: String = raw.chars().take(BEACON_TOOL_MAX).collect();
    if raw.chars().nth(BEACON_TOOL_MAX).is_some() {
        out.push('…');
    }
    out
}

/// A beacon's `injection_flag` row (#45), composed from the origin the caller
/// states rather than one baked in here (#48).
///
/// Pure, so what an incident reader is told is assertable without an
/// `AppHandle` — the same seam [`override_row`] exists for. The text states the
/// origin limit in words rather than leaving it to the `origin` key, because
/// the person reading this after the fact needs to know that "the expected shim
/// sent it" is an assumption, not a finding.
///
/// The first sentence follows the outcome rather than asserting the engagement
/// case (#48): a beacon that contaminates a `Local`-latched tab, or one that
/// arrives with the latch feature off, moves no latch and refuses nothing —
/// saying it did would be the row lying about the one fact it exists to record.
fn beacon_row(origin: outbound::Origin, tool: &str, out: &BeaconOutcome) -> FlagRow {
    let what = if out.engaged {
        "so this tab is now EXTERNAL-latched and its proxied local-capability tools will refuse"
    } else {
        "and this conversation is now CONTAMINATED — the taint latch did not move (it is not \
         Open, or the latch control is off), so nothing is refused, but every memory write from \
         here on is quarantined and every external result keeps its envelope"
    };
    FlagRow {
        screen: outbound::Screen::LatchBeacon,
        origin,
        tool: tool.to_string(),
        detail: format!(
            "NATIVE-WEB BEACON ({tool}, origin: {}): the harness's own web tool is about to run, \
             {what} (latch={}, contaminated={}). This row records an authenticated POST to \
             /latch/beacon from a local process — a cImp-generated artifact is the expected sender, \
             but the launch token is readable by anything running as this user, so this is NOT \
             evidence of a user action. This route only ever TIGHTENS: it cannot unlatch and it \
             cannot clear the contamination flag. Clearing that is a user action in cImp's own UI \
             (step 4), and no HTTP route reaches it.",
            origin.as_str(),
            out.view.latch,
            out.view.contaminated,
        ),
    }
}

/// V32 Phase H (locked decision 17): whether the OpenCode native-tool gate is
/// **in force** for one tab — the single resolved boolean the plugin is told, so
/// no part of the three-level hierarchy has to be reimplemented in JS.
///
/// It is the AND of two features, and the second one is the point:
///
/// - [`Feature::HarnessNativeGate`] — the Phase H switch itself (default off).
/// - [`Feature::TaintLatch`] — because this gate enforces *the latch's*
///   boundary on tools cImp does not route. With the latch feature off the
///   registry stops engaging (see [`GatePolicy`]), so the latch label the plugin
///   would read is not a boundary anyone is maintaining; denying against it
///   would be enforcement without a policy behind it.
///
/// Resolving it here rather than in the plugin is also what keeps the taint
/// latch a LIVE feature: the gate's own flag is spawn-baked into the plugin
/// file, but this AND is recomputed on every query, so switching the latch off
/// stops the denials without a tab restart.
/// Takes the resolved injection scope rather than a [`LatchScope`] (#48), so
/// the app-wide answer — the one a call with no *usable* tab identity gets — is
/// expressible. See [`LatchScoping::injection`].
fn native_gate_verdict(
    settings: &crate::settings::Settings,
    s: crate::settings::injection::Scope<'_>,
) -> bool {
    use crate::settings::injection::{effective, Feature};
    effective(Feature::HarnessNativeGate, s, settings)
        && effective(Feature::TaintLatch, s, settings)
}

/// `POST /latch/state`: the resolved containment state of one tab (V32 Phase H).
///
/// **Why a new route rather than an extension of an existing one.**
/// `/latch/beacon` *mutates* — it engages the EXTERNAL latch — so reusing it for
/// a read would latch a tab every time the model touched a local file.
/// `/status` is the whole-app debug view (every tab, every feature, at every
/// scope): far more than a hook on the hot path should parse, and it answers
/// nothing about *this* tab without the plugin knowing which row is its own.
/// This route answers exactly the two facts the gate needs, for one tab.
///
/// Behind the same bearer check as every other route (it precedes dispatch in
/// [`handle_conn`]), because the reply describes a tab's containment posture.
///
/// **Always 200, and always fail-open in shape**: a tab the proxy has never
/// served, or a body with no identity at all, answer `latch: "open"` — the
/// value that denies nothing. The plugin's own error paths land on the same
/// verdict, so "the app is down" and "the app says no gate" are the same
/// behaviour rather than two.
///
/// **The `gate` half is NOT hard-coded off for an unusable tab id (#48).** It
/// resolves the feature hierarchy at whatever scope the body earns — the tab's
/// own when the id is configured, app-wide otherwise. An id that names no
/// configured tab is very often a real tab that was removed or re-id'd while
/// its per-*directory* OpenCode plugin file kept the old id (the unfixed H-2),
/// and "the gate is switched off" and "the gate cannot find your tab" must not
/// be the same answer.
///
/// Known residual, stated rather than papered over: because an unknown id keys
/// no registry entry (#45's bound, deliberately kept), its `latch` is always
/// `open`, and the plugin denies only on `external`/`local`. So the practical
/// effect for a stale plugin file is still "nothing is refused" — what changes
/// is that the verdict now reflects a decision someone took instead of a
/// collapsed `Option`. Closing it properly needs H-2: a per-tab plugin file.
async fn handle_latch_state(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: LatchStateBody = match serde_json::from_slice(&req.body) {
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
    let agent = wire_agent(LATCH_STATE_ROUTE, body.consumer.as_deref());
    let settings = live_settings(app);
    let scoping = latch_scope(app, &settings, agent, body.tab.as_deref());
    // #48: the verdict comes from the resolved injection scope, which is
    // app-wide for both identity-less cases — NOT a hard `false`. #45 folded
    // "an id that names no configured tab" into `latch_scope`'s `None`, and
    // this arm read that `None` as "off", so a stale plugin file (see
    // `LatchScoping::Unknown`) turned the Phase H gate off with only a `warn!`
    // on the sibling beacon route to say so. The registry is untouched either
    // way: `view_for` needs a real scope and never creates one.
    let view = scoping
        .scope()
        .map_or_else(LatchView::default, |scope| latches().view_for(scope));
    write_json(stream, 200, &latch_state_reply(&settings, &scoping, view)).await
}

/// `POST /latch/state`'s reply body, given a resolved scoping and this tab's
/// view.
///
/// Split out of [`handle_latch_state`] (#48) because the regression this issue
/// fixes lived in a `match` arm *here* — `None => (false, …)` — and this crate
/// has no `tauri::test` `AppHandle` mock, so the handler itself is unreachable
/// from a test. Everything that decides the reply is in this function now; the
/// handler's remaining work is the registry lookup, which needs the process
/// global. Re-adding "an unusable tab id means the gate is off" therefore means
/// writing it where `an_unknown_tab_id_resolves_the_app_wide_gate_verdict_not_a_hard_off`
/// can see it.
fn latch_state_reply(
    settings: &crate::settings::Settings,
    scoping: &LatchScoping,
    view: LatchView,
) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        // The RESOLVED verdict, not the stored switch: the plugin holds no
        // part of the hierarchy. Deliberately NOT branched on whether the
        // scoping named a usable tab — see `LatchScoping::injection`.
        "gate": native_gate_verdict(settings, scoping.injection()),
        // Flattened rather than nested so the hook reads one string. The
        // full view rides along for a human reading a trace.
        "latch": view.latch,
        "contaminated": view.contaminated,
        // #48 (F-23): WHY the latch is where it is, for the one position that has
        // two possible causes. The plugin refuses the same calls either way — this
        // decides only which fixed refusal it serves, so a plugin (or a loopback)
        // that does not know the field loses nothing but the better message.
        "local_by_user_flip": view.local_by_user_flip,
    })
}

/// `GET /status`: the proxy's V32 Phase B debug view — one row per tab it has
/// served, with that tab's resolved session and latch state
/// ([`Latch::label`]). Read by hand (and by the live-verification recipes) to
/// answer "why is this tab being refused?" without turning on trace logging.
///
/// Behind the same bearer token as every other route; it exposes no fetched
/// content, only cImp's own identifiers and three fixed labels.
/// V32 Phase G (locked decision 16) adds the `injection` object: the RESOLVED
/// value of every control at every scope, and which of the three levels decided
/// it. With three levels, "why is this tab not latching?" has to be answerable
/// without reading code — and `/status` is where the live-verification recipes
/// already look.
async fn handle_status(stream: &mut TcpStream, app: &AppHandle) -> AppResult<()> {
    write_json(
        stream,
        200,
        &serde_json::json!({
            // Step 4: through `latch_snapshot`, so a hand-run `/status` and the
            // UI's badge poll see the same freshness rule rather than two.
            "latches": latch_snapshot(app),
            "injection": injection_status(&live_settings(app)),
        }),
    )
    .await
}

/// The `/status` + `latch_status` introspection view of the enable hierarchy:
/// the master switch, whether protection is reduced anywhere, and one row per
/// scope naming every feature's resolved value and deciding level.
///
/// Scopes reported, always all of them: `app` (the app-wide controls), the
/// `offload-worker` pseudo-scope, and every configured AI tab. Reporting a scope
/// even when nothing is overridden there is the point — the question this
/// answers is "what is in force", and an absent row reads as "off" to exactly
/// the user who is trying to find out.
///
/// **#48 F-35 — the `app` row reports [`Scope::UnknownCaller`], not
/// [`Scope::AppWide`], and that is deliberate for now.** Locked decision 36
/// split one `Scope::App` into those two; this row's label says
/// *"Application-wide"* while its numbers are the identity-less caller's — the
/// app-wide baseline PLUS any configured tab's L3 `On` (N-1). That mismatch is
/// pre-existing rather than introduced here: it is exactly what the row has
/// always published, it is what live-verify recipe *"an identity-less call
/// honours a per-tab `On`"* observes through `/status` (the app row reading
/// `decided_by:"scope"` while its own `override_value` is `"inherit"`), and
/// moving it to `AppWide` would change `GET /status` JSON and take that recipe's
/// only observation point away. Repointing it is a behaviour change with its own
/// retest box and is raised as **F-38**, not folded into the split.
///
/// [`Scope::UnknownCaller`]: crate::settings::injection::Scope::UnknownCaller
/// [`Scope::AppWide`]: crate::settings::injection::Scope::AppWide
pub fn injection_status(settings: &crate::settings::Settings) -> serde_json::Value {
    use crate::settings::injection::{self as inj, Scope};
    let mut scopes = vec![
        serde_json::json!({
            "scope": Scope::UnknownCaller.key(),
            "label": "Application-wide",
            "features": inj::report(settings, Scope::UnknownCaller),
        }),
        serde_json::json!({
            "scope": Scope::OffloadWorker.key(),
            "label": "Offload worker",
            "features": inj::report(settings, Scope::OffloadWorker),
        }),
    ];
    for t in &settings.tabs {
        if let crate::settings::TabConfig::AiTool(c) = t {
            let scope = Scope::tab_only(&c.id);
            scopes.push(serde_json::json!({
                "scope": c.id,
                "label": c.name,
                "features": inj::report(settings, scope),
            }));
        }
    }
    serde_json::json!({
        "protection": inj::master_enabled(settings),
        "reduced": inj::protection_reduced(settings),
        "scopes": scopes,
    })
}

/// Read one `?key=value` query parameter off a request path.
///
/// Deliberately no percent-decoding: every value on these routes is composed by
/// cImp itself (consumer names, tab ids — `[a-z0-9-]`), never by a user or a
/// browser. Matches the pre-V30 behaviour of [`consumer_from_token`], which
/// this now backs.
fn query_param<'a>(path: &'a str, key: &str) -> Option<&'a str> {
    let (_, query) = path.split_once('?')?;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Parse a `consumer` discriminator into a [`Consumer`], from wherever the
/// route carries it — the `?consumer=` query string on `/mcp/*`, the request
/// BODY on `/run` and `/graph_run`.
///
/// **Absent** ⇒ [`crate::harness::DEFAULT_HARNESS`]: the pre-V30 child sends no
/// query at all, and that child was Claude's. **Unknown** ⇒ `None`, and every
/// route that asks advertises nothing and refuses — these are grant-bearing
/// questions, and until V40 Phase A a typo'd token was answered with Claude's
/// granted server set.
fn consumer_from_token(token: Option<&str>) -> Option<Consumer> {
    Consumer::parse(token.unwrap_or_else(|| {
        crate::harness::DEFAULT_HARNESS
            .id()
            .expect("DEFAULT_HARNESS names a registered harness")
    }))
}

/// **The single identity resolution for a grant-bearing loopback route** (V40
/// review finding H-1).
///
/// Answers the resolved-and-folded [`Consumer`] the call is judged under AND
/// the `source_for_consumer` token its taint latch, its EXTERNAL budget, its
/// injection scope and its activity attribution key off — from ONE resolution,
/// so the two can never name different harnesses.
///
/// `None` for a token no registered harness and no in-app consumer claims: a
/// grant question, refused rather than degraded. Before this funnel,
/// `?consumer=offload` resolved to Claude's granted server set (via
/// [`Consumer::conservative_grant`]) while its latch key resolved to the
/// activity source `"offload"`, which names no configured tab — so the latch,
/// the budget and the attribution all fell through their documented fail-open
/// while the *grant* stayed Claude's. `?consumer=<garbage>` did the same on
/// `/run` and `/graph_run`, where nothing refused it at all.
fn proxy_identity(token: Option<&str>) -> Option<(Consumer, &'static str)> {
    let consumer = consumer_from_token(token)?.proxied();
    Some((consumer, crate::graph::source_for_consumer(consumer.source())))
}

/// The refusal a grant-bearing route answers a token nobody declared with
/// (locked decision 2). One text, so `/mcp/call`, `/run` and `/graph_run` all
/// name the same registered list.
fn unknown_consumer_message() -> String {
    format!(
        "unknown consumer; this proxy serves {} (plus `offload`, cImp's own in-app consumer). \
         A consumer token decides which MCP servers a caller may reach and which tab's taint \
         latch judges the call, so an unrecognised one is refused rather than defaulted.",
        crate::harness::registry::harness_ids().join(", ")
    )
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
        .unwrap_or(crate::harness::DEFAULT_HARNESS.token())
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

/// A `POST /delegate` body — one cross-harness delegation request (V39
/// Phase B, locked decision 3).
///
/// `harness` rather than a tab id, and that is the whole shape of decision 3:
/// at most one tab per harness holds the Manual role, so the driver names a
/// harness and cImp resolves the tab. A tab argument would let a model drive
/// any tab it could guess the id of.
#[derive(Deserialize)]
struct DelegateBody {
    #[serde(default)]
    harness: String,
    #[serde(default)]
    task: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    timeout_s: Option<u64>,
    /// Which consumer this child serves — cImp-authored argv on the child side.
    #[serde(default)]
    consumer: Option<String>,
    /// The calling tab. Unforgeable in practice (`--tab` is composed by cImp at
    /// spawn), and REQUIRED: the acyclic check and the Events row both need it.
    #[serde(default)]
    tab: Option<String>,
}

/// A `POST /delegate` response — [`RunResult`]'s three fields plus the meta the
/// child renders as the result footer, so a delegation result reads like an
/// `offload_task` one (worker, duration, screening verdict).
#[derive(Serialize)]
struct DelegateResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screened: Option<bool>,
}

impl DelegateResult {
    fn failed(msg: String) -> Self {
        Self {
            ok: false,
            text: None,
            error: Some(msg),
            worker: None,
            duration_ms: None,
            screened: None,
        }
    }
}

/// `POST /delegate` — drive another harness's tab and return its answer.
///
/// The app owns the tabs, so this route is the only way in; the child has no
/// self-contained fallback and says so rather than inventing one.
///
/// **Target resolution is a lookup, not a search** (locked decision 8): the
/// harness id names the one tab whose `delegation_role == Manual` for that
/// harness. If it moved or closed between `tools/list` and this call, the call
/// is refused naming the condition — never silently retargeted.
///
/// Every other condition is the engine's: `delegation::drive` runs the whole of
/// locked decision 12's preflight, so this handler deliberately re-checks
/// nothing it could get wrong on its own.
async fn handle_delegate(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: DelegateBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            let r = DelegateResult::failed(format!("bad request body: {e}"));
            return write_json(stream, 400, &r).await;
        }
    };
    if body.task.trim().is_empty() {
        let r = DelegateResult::failed("`task` must be non-empty".into());
        return write_json(stream, 400, &r).await;
    }

    let settings = live_settings(app);
    let agent = crate::graph::source_for_consumer(body.consumer.as_deref().unwrap_or(crate::harness::DEFAULT_HARNESS.token()));
    // The calling tab must be a CONFIGURED tab of this consumer. An anonymous
    // or unrecognized id is refused rather than fail-open: unlike a latch (where
    // "we do not know who this is" degrades to no containment), delegation has
    // nothing safe to do without a driver — the cycle check and the audit row
    // are both keyed by it.
    let driver = match tab_identity(&settings, agent, body.tab.as_deref()) {
        TabIdentity::Configured(t) => crate::state::TabId::from_str(t),
        TabIdentity::Anonymous | TabIdentity::Unknown(_) => {
            let r = DelegateResult::failed(
                "delegation needs to know which tab is asking, and this request names none that \
                 is configured for this harness"
                    .into(),
            );
            return write_json(stream, 200, &r).await;
        }
    };

    // V39 Phase B: the taint gate, and it sits HERE on purpose — after every
    // parse-boundary rejection (so a malformed request never engages a latch)
    // and before the worker is resolved, the slot is claimed, the read-only
    // lock is engaged or a single byte is typed. A refused delegation must
    // leave the worker tab exactly as it was, and must mint no `start` row for
    // a delegation that never began; both are true by this ordering, and
    // `the_delegate_gate_runs_before_anything_is_driven` pins it.
    if let Err(refusal) = delegate_admit(
        latches(),
        DELEGATE_TOOL,
        agent,
        body.tab.as_deref(),
        |a, t| latch_scope(app, &settings, a, t),
        |scope| GatePolicy::resolve(&settings, scope),
    ) {
        let r = DelegateResult::failed(refusal.to_string());
        return write_json(stream, 200, &r).await;
    }

    let harness = body.harness.trim();
    let Some(worker_id) = manual_tab_for(&settings, harness) else {
        let r = DelegateResult::failed(format!(
            "no tab currently holds the Manual delegation role for `{harness}` — it was set to \
             None, moved, or the tab was closed since this tool was listed"
        ));
        return write_json(stream, 200, &r).await;
    };

    // V39 review R-6: **watch the caller** while the delegation runs, the same
    // way `/run` does and with the same reasoning one step further. A worker
    // tab is single-slot and its keyboard is locked for the whole flight, so a
    // `delegate_task_*` caller that died — a closed session, a killed child —
    // used to hold BOTH for the full `delegation.default_timeout_s` (ten
    // minutes by default) waiting to hand over a reply nobody would read.
    //
    // After the request body a well-behaved client sends nothing and does not
    // half-close its write half until it has the response, so a probe read
    // returning 0 bytes (or erroring) means the connection went away. No
    // heartbeat half: unlike `/run` this route answers with one JSON object and
    // adding a keep-alive stream would change the wire shape for the child.
    //
    // What happens on cancel is `drive_watching`'s, shared with the facade: the
    // engine is TOLD (no key is ever sent — the worker finishes visibly), the
    // flight is awaited rather than dropped so the slot and lock are released
    // by their owner, and a pre-claim abandonment mark (R-8) is dropped after.
    let cancel = CancellationToken::new();
    let drive_req = crate::delegation::DriveRequest {
        worker: crate::state::TabId::from_str(&worker_id),
        driver: Some(driver),
        mode: crate::delegation::DelegationMode::Explicit,
        task: body.task,
        context: body.context,
        timeout_s: body.timeout_s,
        // The explicit tool adds NOTHING (locked decision 2a): what the user
        // asked for is what the worker reads. Only the Phase C facade passes a
        // note, and only because `offload_task`'s `schema` / `profile` have no
        // other way through a PTY.
        format_note: None,
    };
    let reply = {
        let (mut rd, _wr) = stream.split();
        let flight = crate::delegation::drive_watching(app, drive_req, &cancel);
        tokio::pin!(flight);
        loop {
            let mut probe = [0u8; 1];
            tokio::select! {
                biased;
                r = &mut flight => break r,
                read = rd.read(&mut probe) => match read {
                    Ok(0) | Err(_) => {
                        debug!("delegate loopback: caller disconnected mid-flight; cancelling");
                        cancel.cancel();
                        break (&mut flight).await;
                    }
                    // A stray byte before the response is unexpected on this
                    // one-shot protocol; ignore it and keep waiting.
                    Ok(_) => continue,
                },
            }
        }
    };

    let r = match reply {
        Ok(reply) => DelegateResult {
            ok: true,
            text: Some(reply.text),
            error: None,
            worker: Some(reply.worker),
            duration_ms: Some(reply.duration_ms),
            screened: Some(reply.screened),
        },
        // 200 with `ok:false`, like `/run`: a refusal, a timeout and a
        // take-over are all task-level outcomes the model should read and adapt
        // to, not transport errors.
        Err(e) => DelegateResult::failed(e.to_string()),
    };
    write_json(stream, 200, &r).await
}

/// The tab id currently holding the Manual delegation role for `harness`.
///
/// `None` when nothing does — which is a real state, not an error: the role may
/// have been cleared or moved between the `tools/list` that advertised the tool
/// and this call (locked decision 8's move rule makes that a normal event).
fn manual_tab_for(settings: &crate::settings::Settings, harness: &str) -> Option<String> {
    settings.tabs.iter().find_map(|t| match t {
        crate::settings::TabConfig::AiTool(c)
            if c.delegation_role == crate::settings::DelegationRole::Manual
                && crate::tabs::tab_consumer(c) == Some(harness) =>
        {
            Some(c.id.clone())
        }
        _ => None,
    })
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
mod tests;

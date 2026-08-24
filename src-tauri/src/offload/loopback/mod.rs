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
use super::outbound;
use super::toolclass::{self, Profile};
use super::latch::{
    ai_tab_ids, is_configured_tab, latch_scope, latch_snapshot, latches, tab_identity,
    tab_root_key, BeaconOutcome, CallProvenance, FlagRow, GatePolicy, LatchRegistry, LatchRoute,
    LatchScope, LatchScoping, LatchView, TabAudit, TabIdentity,
};
use super::discovery::{
    canon, discovery_path, external_project_root, is_ancestor_or_equal, own_discovery_path,
    read_all_discoveries, read_discovery, sweep_stale_discoveries, Discovery,
    MAX_DISCOVERY_PROBES,
};

// ── V42 R4 (#115): the route families ───────────────────────────────────────
//
// What stays in this file is the LISTENER: the `Loopback` handle, the
// hand-rolled HTTP/1.1 wire and its constant-time token check, the dispatch
// `match`, and the few funnels every family shares — `live_settings` (the one
// `AppHandle` → `Settings` point), `proxy_identity`, the `hook_admit` /
// `delegate_admit` admission pair and the reply writers. Each family below
// owns its own body types, helpers and handlers.
//
// The families are near-independent: the only shared mutable state is
// `latches()` (in `offload::latch`), `live_settings(app)` and the
// `Arc<OffloadService>` four routes take; everything else is resolved from
// `AppHandle::try_state` at request time.
//
// **The source-scanning tests read every file in this list**, not just this
// one — see `tests::LOOPBACK_SRC`. A handler moved between families keeps
// its scanners; a needle that simply moved next door would not.
mod run;
mod graph;
mod audit;
mod context;
mod memory;
mod session;
mod activity_edges;
mod latch_routes;
mod mcp;
mod delegate;
mod events;

// The family items, back in this module's namespace so the dispatch `match`,
// the other families (`use super::*`) and the test module reach them under the
// names they had when this was one file.
//
// A glob per family, so each item keeps EXACTLY the visibility it declares: a
// glob re-export is capped at the item's own, and each line below is written at
// the widest its family actually needs. `pub(super)` therefore stays inside
// `loopback`, `pub(crate)` stays crate-visible — which is what makes
// `offload::loopback::compaction_block` still resolve for `harness::claude::hook`
// and `offload::loopback::injection_status` for `ipc::commands`.
use self::run::*;
use self::graph::*;
use self::audit::*;
pub(crate) use self::context::*;
use self::memory::*;
pub(crate) use self::session::*;
pub(crate) use self::activity_edges::*;
pub(crate) use self::latch_routes::*;
use self::mcp::*;
use self::delegate::*;
pub use self::events::*;

// V42 review (dropped-at-cap): `bounded_id`, its bound, and `live_settings`
// moved UP to `offload` — `offload::latch` imported them from here, which was a
// back-edge from the module V42 R3 (#114) extracted to the module it was
// extracted from. Re-exported so the family files' `use super::*` and
// `harness::claude::hook`'s `use crate::offload::loopback::{..}` still resolve.
pub(crate) use super::{bounded_id, live_settings, BEACON_TOOL_MAX};


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

/// **Every file the route surface lives in**, paired with its source text.
///
/// Test-only, and `pub(crate)` for the same reason [`core_route_paths`] is: the
/// source-scanning tests that pin this module's security properties do not all
/// live in it (`harness::chp`, `offload::toolclass`, `tabs::config` each read it
/// too). V42 R4 (#115) split the routes across a directory, and a scanner that
/// kept reading one file after a handler moved next door would be GREEN about
/// code it no longer covers — which for these tests means silently passing.
///
/// The list is checked against the `mod` declarations above by
/// `tests::the_source_scanners_read_every_route_file`, so a family added later
/// cannot be scanned by nobody.
#[cfg(test)]
pub(crate) const ROUTE_SOURCES: &[(&str, &str)] = &[
    ("offload/loopback/mod.rs", include_str!("mod.rs")),
    ("offload/loopback/run.rs", include_str!("run.rs")),
    ("offload/loopback/graph.rs", include_str!("graph.rs")),
    ("offload/loopback/audit.rs", include_str!("audit.rs")),
    ("offload/loopback/context.rs", include_str!("context.rs")),
    ("offload/loopback/memory.rs", include_str!("memory.rs")),
    ("offload/loopback/session.rs", include_str!("session.rs")),
    ("offload/loopback/activity_edges.rs", include_str!("activity_edges.rs")),
    ("offload/loopback/latch_routes.rs", include_str!("latch_routes.rs")),
    ("offload/loopback/mcp.rs", include_str!("mcp.rs")),
    ("offload/loopback/delegate.rs", include_str!("delegate.rs")),
    ("offload/loopback/events.rs", include_str!("events.rs")),
];

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
    let src = include_str!("mod.rs");
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

/// The taint decision the three `/context/*` hook routes, the pre-mutation
/// checkpoint and `POST /delegate` take before they reach capability, as one
/// function taking its dependencies as arguments — the [`audit_admit`] shape,
/// for the same two reasons: a handler cannot reach capability without passing
/// through it, and the decision is testable without a `TcpStream` or an
/// `AppHandle` (this crate has no `tauri::test` mock — see
/// [`latch_state_reply`]).
///
/// **The `route` is the only difference between the two callers**, and it is
/// the difference that matters: a hook does not move the latch
/// ([`LatchRoute::Hook`]), an elective delegation does
/// ([`LatchRoute::Delegation`]). V42 R22 (#115) folded two byte-identical
/// copies of this body — and two copies of the provenance note below — into
/// one; [`hook_admit`] and [`delegate_admit`] are this function with their
/// route filled in, and they keep their names because the handlers' call sites
/// are what `tests::every_loopback_route_declares_what_it_does_about_the_latch`
/// reads.
///
/// `Err(refusal)` means *this conversation may not have this*, and the two
/// callers answer it differently. A hook's caller answers with the route's own
/// fail-safe reply — empty text, or a `pass` verdict — and never with the
/// refusal string: these are hooks, and a hook that returns an error perturbs
/// the turn it was supposed to be invisible to. `/delegate` returns the refusal
/// VERBATIM: that one IS a tool call the model made, so the model is the right
/// audience for the reason — the same treatment `/run` gives a refused
/// `offload_task`. Neither is silent: [`LatchRegistry::gate`] writes the
/// [`Screen::LatchRefusal`](outbound::Screen) row (once per scope) that gives
/// the refusal a user-visible consumer.
///
/// `agent` is caller-asserted, exactly as `consumer` is on `/graph_run`. It
/// selects which agent's key the scope is built under and nothing else; F-4
/// (`(consumer, tab)` is a verified pair on no route) is unchanged here, not
/// worked around.
fn admit(
    reg: &LatchRegistry,
    route: LatchRoute,
    // The class-table identity to gate under. Passed rather than hardcoded:
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
    // `CallProvenance::http()`, not `internal()` and unlike `/run`: this is a
    // POST from a local process holding a launch token that anything running as
    // this user can read, so it is never evidence that cImp itself decided the
    // call (#45's reasoning for the beacon route). It reaches no row today —
    // provenance is read only when an admitted call is EXTERNAL, and no name
    // gated here is — but stating it is how the wrong origin avoids being
    // inherited later.
    reg.gate(scope, route, tool, policy, CallProvenance::http())
        .map(|_| ())
}

/// [`admit`] on [`LatchRoute::Hook`] — the gate the `/context/*` routes and the
/// pre-mutation checkpoint take. A hook is not an elective call, so it does not
/// move the latch.
fn hook_admit(
    reg: &LatchRegistry,
    tool: &'static str,
    agent: &'static str,
    tab: Option<&str>,
    scope_of: impl FnOnce(&'static str, Option<&str>) -> LatchScoping,
    policy_of: impl FnOnce(Option<&LatchScope>) -> GatePolicy,
) -> Result<(), &'static str> {
    admit(
        reg,
        LatchRoute::Hook,
        tool,
        agent,
        tab,
        scope_of,
        policy_of,
    )
}

/// [`admit`] on [`LatchRoute::Delegation`] — the gate `POST /delegate` takes
/// before any tab is touched. The call is elective, so it MOVES the latch;
/// that is the whole difference from [`hook_admit`], and
/// `tests::the_delegation_route_both_refuses_and_latches` is what holds it.
fn delegate_admit(
    reg: &LatchRegistry,
    tool: &'static str,
    agent: &'static str,
    tab: Option<&str>,
    scope_of: impl FnOnce(&'static str, Option<&str>) -> LatchScoping,
    policy_of: impl FnOnce(Option<&LatchScope>) -> GatePolicy,
) -> Result<(), &'static str> {
    admit(
        reg,
        LatchRoute::Delegation,
        tool,
        agent,
        tab,
        scope_of,
        policy_of,
    )
}

/// **The hook gate, as one call a plugin can make** (V40 Phase C).
///
/// [`hook_admit`]'s signature names `LatchRegistry`, `LatchScoping`,
/// `LatchScope` and `GatePolicy` — four private types, three of them closures'
/// arguments. That is the right shape for the callers inside this module and
/// the wrong one for a plugin route: the latch model is core's, and a harness's
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

/// Decode a route's JSON body, or answer **400 in that route's own shape**.
///
/// The `match serde_json::from_slice … Err(e) => return write_json(.., 400, ..)`
/// preamble was written out at every body-taking route. V42 R22 (#115) folded
/// it here — but NOT the reply, which is why `refusal` is a parameter: the
/// 400 bodies are NOT one shape, and every one of them is read by something
/// (the two MCP children, the hook shims, the generated OpenCode plugin, the
/// delegating harness's child). The pushed `/session/*` routes send no parse
/// detail at all ([`bad_request`]), `/delegate` answers in its own result
/// type, and the rest split between [`bad_body_result`] and
/// [`bad_body_json`]. `tests::every_bad_body_reply_keeps_its_own_bytes` pins
/// each one's bytes, because nothing else did.
///
/// `Ok(None)` means **the route is already answered** and the handler must
/// return — the same `?`-propagating write the preamble did, one frame in.
async fn decode<T, B>(
    stream: &mut TcpStream,
    req: &Request,
    refusal: impl FnOnce(serde_json::Error) -> B,
) -> AppResult<Option<T>>
where
    T: serde::de::DeserializeOwned,
    B: Serialize,
{
    match serde_json::from_slice::<T>(&req.body) {
        Ok(body) => Ok(Some(body)),
        Err(e) => {
            write_json(stream, 400, &refusal(e)).await?;
            Ok(None)
        }
    }
}

/// The 400 body the task-shaped routes send for an unparseable request — the
/// NDJSON pair, `/mcp/call`, the two `/latch/*` routes and `/session/hello`:
/// a [`RunResult`] whose `error` names the parse failure, so the keys come out
/// in the struct's declared order (`{"ok":false,"error":…}`).
///
/// V42 review (dropped-at-cap): this used to build the `RunResult` itself,
/// re-spelling [`bad_request`]'s three fields one function above it. Two
/// literals of one envelope is how a field added to `RunResult` reaches one
/// 400 body and not the other. The DIFFERENCE between them - this one carries
/// the parse detail, `bad_request` deliberately does not - is the message, and
/// that is all this adds.
fn bad_body_result(e: serde_json::Error) -> RunResult {
    bad_request(&format!("bad request body: {e}"))
}

/// The 400 body the hook routes send — `/context/*`,
/// `/workbench/tool_checkpoint`, `/activity/contract_drift`: the same two
/// fields as [`bad_body_result`], built as a bare object rather than through
/// the struct.
///
/// **Kept as its own function even though the bytes coincide today.** They
/// coincide because `serde_json` resolves with `preserve_order` in this tree
/// (a transitive feature, not something either route chose), which makes a
/// `Map` insertion-ordered; without it these keys would sort and this reply
/// would come out `error` first while [`bad_body_result`]'s would not. Each
/// route keeps building the body it has always built rather than depending on
/// that resolution, and
/// `tests::every_bad_body_reply_keeps_its_own_bytes` is what would notice it
/// changing.
fn bad_body_json(e: serde_json::Error) -> serde_json::Value {
    serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") })
}

/// [`claim_discovery_report`] against a caller-owned ledger, so the key-space
/// bound and the doubling are assertable without process-global state (the
/// suite runs cases concurrently in one process).
fn claim_in(ledger: &mut HashMap<String, outbound::Doubling>, key: &str) -> outbound::DoublingRow {
    ledger.entry(key.to_string()).or_default().claim()
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

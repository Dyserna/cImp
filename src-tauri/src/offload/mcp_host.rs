//! V8-03 MCP host — the warm client pool toward the user's tool servers.
//!
//! Completes V8-01's never-built Phase C: an MCP **client** (ccImp is the
//! host) that keeps long-lived connections to each configured tool server
//! (`duckduckgo`, `fetch`, `context7`, `git`, `filesystem`, …) so the
//! offload worker can reach real tools without paying an `npx`/`uvx`
//! cold-start per call.
//!
//! Per server it runs `initialize` + `tools/list`, **namespaces** every
//! tool as `<server>__<tool>`, drops write/destructive tools (read-class
//! only), confines a `filesystem` server to the offload `allowed_roots`,
//! and tracks per-server health. Connections are kept warm across calls
//! and reconciled against config; a hung or crashed server is isolated
//! (its tools vanish from the capability set) without wedging the loop.
//!
//! Transport: stdio (`command`+`args`+`env`) is fully warm — a reader task
//! multiplexes JSON-RPC responses by id over the child's stdout. HTTP
//! (`url`) is best-effort single-POST per request (no warm channel needed;
//! the priority targets are the stdio `npx`/`uvx` servers).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{broadcast, oneshot, Mutex as TokioMutex, RwLock};
use tracing::{debug, info, warn};

use crate::settings::McpServerConfig;

use super::openai::ToolDef;

const PROTOCOL_VERSION: &str = "2025-06-18";
const CLIENT_NAME: &str = "ccimp-offload-host";
/// Per-request timeout for an MCP server call (initialize / tools/list /
/// tools/call). A server that doesn't answer in this window is treated as
/// hung — the call fails and the loop moves on rather than blocking.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
/// Tighter bound on the handshake so a wedged server doesn't stall warm-up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Write/destructive leading verbs. A tool whose leading verb is in this
/// set is filtered out — the offload worker stays read-only even when a
/// server advertises mutating tools (`filesystem` write, `git` commit).
const WRITE_VERBS: &[&str] = &[
    "write", "delete", "remove", "rm", "create", "make", "mkdir", "put", "post",
    "update", "edit", "move", "mv", "rename", "append", "commit", "push", "merge",
    "reset", "drop", "truncate", "insert", "modify", "patch", "set", "unlink",
    "kill", "exec", "execute", "run", "spawn", "install", "uninstall", "publish",
    "send", "add", "copy", "cp", "save", "store", "upload", "mutate", "destroy",
    "clear", "purge", "apply", "checkout", "clone", "stage", "restore", "revert",
];

/// The leading verb of one name segment: the leading lowercase run so
/// camelCase (`searchWeb` → `search`) resolves, else the whole lowercased
/// segment (`Get` → `get`).
fn token_verb(token: &str) -> String {
    let lead: String = token.chars().take_while(|c| c.is_ascii_lowercase()).collect();
    if lead.is_empty() {
        token.to_ascii_lowercase()
    } else {
        lead
    }
}

/// The leading verb of a tool name (first segment). A thin wrapper over
/// [`token_verb`] kept as the readable name for the filter's intent and
/// exercised directly in tests.
#[cfg_attr(not(test), allow(dead_code))]
fn leading_verb(name: &str) -> String {
    let seg = name
        .split(['_', '-', '.', ' ', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or(name);
    token_verb(seg)
}

/// Whether a tool name is read-class (safe to expose). The offload worker is
/// read-only; mutating tools are dropped. A server may carry its verb in the
/// **second** segment (`git_commit`, `git_push`), so a tool is write-class if
/// *either* of its first two segments leads with a mutating verb.
fn is_read_class(name: &str) -> bool {
    name.split(['_', '-', '.', ' ', ':'])
        .filter(|s| !s.is_empty())
        .take(2)
        .all(|seg| !WRITE_VERBS.contains(&token_verb(seg).as_str()))
}

/// One namespaced, read-class tool offered by a server: the [`ToolDef`]
/// advertised to the model plus the raw server-side name to call.
#[derive(Clone)]
struct HostTool {
    def: ToolDef,
    /// The un-namespaced name the server expects in `tools/call`.
    raw_name: String,
}

/// Per-server health row for the Settings status display.
#[derive(Clone, Debug, Serialize)]
pub struct McpServerHealth {
    pub name: String,
    /// `"stdio"` or `"http"`.
    pub transport: String,
    /// A live connection exists (process spawned / URL set).
    pub connected: bool,
    /// Last operation succeeded and tools are available.
    pub healthy: bool,
    /// Number of read-class tools currently exposed.
    pub tool_count: usize,
    /// Short error if the server failed to connect / went unhealthy.
    pub error: Option<String>,
}

/// Shared state a stdio reader task and the request path both touch.
struct StdioConn {
    stdin: TokioMutex<ChildStdin>,
    child: TokioMutex<Child>,
    pending: StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next_id: AtomicU64,
    /// Flipped false by the reader on EOF / fatal error.
    alive: AtomicBool,
}

impl StdioConn {
    /// Send a request and await its response (by id) up to `timeout`.
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err("server connection is closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        {
            let mut stdin = self.stdin.lock().await;
            if let Err(e) = stdin.write_all(frame.as_bytes()).await {
                self.pending.lock().unwrap().remove(&id);
                return Err(format!("write failed: {e}"));
            }
            if stdin.write_all(b"\n").await.is_err() || stdin.flush().await.is_err() {
                self.pending.lock().unwrap().remove(&id);
                return Err("write/flush failed".into());
            }
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err("server connection closed before responding".into()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("server did not respond within {}s", timeout.as_secs()))
            }
        }
    }

    /// Fire a notification (no id, no response).
    async fn notify(&self, method: &str, params: Value) {
        let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
        let mut stdin = self.stdin.lock().await;
        let _ = stdin.write_all(frame.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        let _ = stdin.flush().await;
    }
}

/// Transport for one server.
enum Conn {
    Stdio(Arc<StdioConn>),
    /// HTTP JSON-RPC: best-effort single POST per request.
    Http { url: String, client: reqwest::Client },
}

/// One connected (or failed) MCP tool server.
pub struct McpServer {
    name: String,
    /// Config signature so reconciliation can detect an edited entry.
    sig: String,
    transport_label: &'static str,
    conn: Option<Conn>,
    tools: Vec<HostTool>,
    healthy: AtomicBool,
    error: StdMutex<Option<String>>,
}

impl McpServer {
    fn health_row(&self) -> McpServerHealth {
        McpServerHealth {
            name: self.name.clone(),
            transport: self.transport_label.to_string(),
            connected: self.conn.is_some(),
            healthy: self.is_healthy(),
            tool_count: self.tools.len(),
            error: self.error.lock().unwrap().clone(),
        }
    }

    fn is_healthy(&self) -> bool {
        if !self.healthy.load(Ordering::Relaxed) {
            return false;
        }
        // A stdio server whose reader saw EOF is dead even if it was healthy.
        match &self.conn {
            Some(Conn::Stdio(c)) => c.alive.load(Ordering::Relaxed),
            _ => true,
        }
    }

    fn set_unhealthy(&self, why: impl Into<String>) {
        self.healthy.store(false, Ordering::Relaxed);
        *self.error.lock().unwrap() = Some(why.into());
    }

    /// Namespaced, read-class tool defs for the chat `tools` array — only
    /// when the server is currently healthy.
    fn tool_defs(&self) -> Vec<ToolDef> {
        if !self.is_healthy() {
            return Vec::new();
        }
        self.tools.iter().map(|t| t.def.clone()).collect()
    }

    /// Map a namespaced tool id back to the raw server-side name.
    fn raw_name(&self, namespaced: &str) -> Option<&str> {
        self.tools
            .iter()
            .find(|t| t.def.function.name == namespaced)
            .map(|t| t.raw_name.as_str())
    }

    /// Execute `tools/call` for a tool on this server.
    async fn call(&self, raw_name: &str, args: Value) -> Result<String, String> {
        let params = json!({ "name": raw_name, "arguments": args });
        let result = match &self.conn {
            Some(Conn::Stdio(c)) => c.request("tools/call", params, REQUEST_TIMEOUT).await,
            Some(Conn::Http { url, client }) => {
                http_rpc(client, url, "tools/call", params, REQUEST_TIMEOUT).await
            }
            None => Err("server is not connected".into()),
        };
        match result {
            Ok(v) => Ok(render_tool_result(&v)),
            Err(e) => {
                self.set_unhealthy(format!("call failed: {e}"));
                Err(e)
            }
        }
    }
}

/// The app-owned MCP host: a warm pool of [`McpServer`] connections plus a
/// change notifier the offload service relays as `tools/list_changed`.
pub struct McpHost {
    servers: RwLock<Vec<Arc<McpServer>>>,
    allowed_roots: RwLock<Vec<PathBuf>>,
    change_tx: broadcast::Sender<()>,
}

impl McpHost {
    pub fn new() -> Arc<Self> {
        let (change_tx, _) = broadcast::channel(16);
        Arc::new(Self {
            servers: RwLock::new(Vec::new()),
            allowed_roots: RwLock::new(Vec::new()),
            change_tx,
        })
    }

    /// Subscribe to capability-change pulses (a server connected, dropped,
    /// or flipped health). The offload service relays these to `/events`.
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.change_tx.subscribe()
    }

    fn signal_change(&self) {
        let _ = self.change_tx.send(());
    }

    /// Bring the warm pool in line with `configs`: connect newly-enabled or
    /// edited servers, drop disabled/removed ones, and leave unchanged
    /// healthy servers untouched (the cheap warm path). Connects concurrently
    /// so one slow `npx` server doesn't serialize the others.
    pub async fn reconcile(&self, configs: &[McpServerConfig], allowed_roots: &[PathBuf]) {
        *self.allowed_roots.write().await = allowed_roots.to_vec();

        let desired: Vec<&McpServerConfig> = configs
            .iter()
            .filter(|c| c.enabled && !c.name.trim().is_empty())
            .collect();
        let desired_sigs: HashMap<String, String> = desired
            .iter()
            .map(|c| (c.name.clone(), config_sig(c)))
            .collect();

        // Partition existing servers into keep / drop.
        let mut changed = false;
        {
            let mut servers = self.servers.write().await;
            let mut kept: Vec<Arc<McpServer>> = Vec::new();
            for s in servers.drain(..) {
                match desired_sigs.get(&s.name) {
                    Some(sig) if *sig == s.sig => kept.push(s), // unchanged
                    _ => {
                        // Removed, disabled, or edited — tear it down.
                        s.shutdown().await;
                        changed = true;
                    }
                }
            }
            *servers = kept;
        }

        // Determine which desired servers are not yet connected.
        let have: Vec<String> = {
            let servers = self.servers.read().await;
            servers.iter().map(|s| s.name.clone()).collect()
        };
        let to_connect: Vec<McpServerConfig> = desired
            .iter()
            .filter(|c| !have.contains(&c.name))
            .map(|c| (*c).clone())
            .collect();

        if !to_connect.is_empty() {
            let roots = allowed_roots.to_vec();
            let mut handles = Vec::new();
            for cfg in to_connect {
                let roots = roots.clone();
                handles.push(tauri::async_runtime::spawn(async move {
                    connect_server(&cfg, &roots).await
                }));
            }
            let mut new_servers = Vec::new();
            for h in handles {
                if let Ok(server) = h.await {
                    new_servers.push(Arc::new(server));
                }
            }
            if !new_servers.is_empty() {
                changed = true;
                self.servers.write().await.extend(new_servers);
            }
        }

        if changed {
            self.signal_change();
        }
    }

    /// All healthy servers' namespaced, read-class tool defs, for merging
    /// into the chat `tools` array (the caller then applies the backend's
    /// `ToolScope`).
    pub async fn tool_defs(&self) -> Vec<ToolDef> {
        let servers = self.servers.read().await;
        let mut out = Vec::new();
        for s in servers.iter() {
            out.extend(s.tool_defs());
        }
        out
    }

    /// Route a namespaced `<server>__<tool>` call to its owning server.
    pub async fn call(&self, namespaced: &str, args: Value) -> Result<String, String> {
        let server = {
            let servers = self.servers.read().await;
            let prefix = namespaced.split("__").next().unwrap_or(namespaced);
            servers.iter().find(|s| s.name == prefix).cloned()
        };
        let Some(server) = server else {
            return Err(format!("no MCP server owns tool `{namespaced}`"));
        };
        let Some(raw) = server.raw_name(namespaced).map(|s| s.to_string()) else {
            return Err(format!("server `{}` no longer offers `{namespaced}`", server.name));
        };
        let was_healthy = server.is_healthy();
        let result = server.call(&raw, args).await;
        if was_healthy && !server.is_healthy() {
            self.signal_change(); // a server just went down mid-call
        }
        result
    }

    /// Per-server health rows for the Settings status display.
    pub async fn health(&self) -> Vec<McpServerHealth> {
        let servers = self.servers.read().await;
        servers.iter().map(|s| s.health_row()).collect()
    }

    /// Names of currently-healthy servers (capability registry input).
    pub async fn healthy_names(&self) -> Vec<String> {
        let servers = self.servers.read().await;
        servers
            .iter()
            .filter(|s| s.is_healthy())
            .map(|s| s.name.clone())
            .collect()
    }

    /// Tear down every connection (app exit / offload disabled).
    pub async fn shutdown(&self) {
        let mut servers = self.servers.write().await;
        for s in servers.drain(..) {
            s.shutdown().await;
        }
    }
}

impl McpServer {
    /// Kill the stdio child / drop the HTTP client.
    async fn shutdown(&self) {
        if let Some(Conn::Stdio(c)) = &self.conn {
            c.alive.store(false, Ordering::Relaxed);
            let mut child = c.child.lock().await;
            let _ = child.kill().await;
        }
    }
}

/// A stable signature of a server's connection-relevant config so an edited
/// entry is detected and reconnected.
fn config_sig(c: &McpServerConfig) -> String {
    let mut env: Vec<(&String, &String)> = c.env.iter().collect();
    env.sort();
    format!("{}|{}|{:?}|{}|{:?}", c.command, c.url, c.args, c.enabled, env)
}

/// Connect (or fail-soft) one server from its config. A failure yields an
/// unhealthy [`McpServer`] carrying the error rather than aborting the pool.
async fn connect_server(cfg: &McpServerConfig, allowed_roots: &[PathBuf]) -> McpServer {
    let sig = config_sig(cfg);
    let use_http = cfg.command.trim().is_empty() && !cfg.url.trim().is_empty();
    let label = if use_http { "http" } else { "stdio" };

    let mut server = McpServer {
        name: cfg.name.clone(),
        sig,
        transport_label: label,
        conn: None,
        tools: Vec::new(),
        healthy: AtomicBool::new(false),
        error: StdMutex::new(None),
    };

    let outcome = if use_http {
        connect_http(cfg).await
    } else {
        connect_stdio(cfg, allowed_roots).await
    };

    match outcome {
        Ok((conn, tools)) => {
            let n = tools.len();
            server.conn = Some(conn);
            server.tools = tools;
            server.healthy.store(true, Ordering::Relaxed);
            info!(server = %cfg.name, transport = label, tools = n, "offload mcp host: connected");
        }
        Err(e) => {
            warn!(server = %cfg.name, transport = label, error = %e, "offload mcp host: connect failed");
            *server.error.lock().unwrap() = Some(e);
        }
    }
    server
}

/// Spawn a stdio MCP server, run the handshake + `tools/list`, and return a
/// warm connection plus its namespaced, read-class tools.
async fn connect_stdio(
    cfg: &McpServerConfig,
    allowed_roots: &[PathBuf],
) -> Result<(Conn, Vec<HostTool>), String> {
    let binary = crate::pty::resolve_command(&cfg.command)
        .map_err(|e| format!("resolve `{}`: {e}", cfg.command))?;
    let mut args = cfg.args.clone();
    confine_filesystem(cfg, &mut args, allowed_roots);

    let mut command = tokio::process::Command::new(&binary);
    command
        .args(&args)
        .envs(&cfg.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Suppress the empty console window Windows allocates for each spawned
    // MCP server (CREATE_NO_WINDOW); output is captured over piped fds.
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let mut child = command
        .spawn()
        .map_err(|e| format!("spawn `{}`: {e}", cfg.command))?;

    let stdin = child.stdin.take().ok_or("child has no stdin")?;
    let stdout = child.stdout.take().ok_or("child has no stdout")?;
    if let Some(stderr) = child.stderr.take() {
        let name = cfg.name.clone();
        tauri::async_runtime::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(target: "offload_mcp", server = %name, "{line}");
            }
        });
    }

    let conn = Arc::new(StdioConn {
        stdin: TokioMutex::new(stdin),
        child: TokioMutex::new(child),
        pending: StdMutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        alive: AtomicBool::new(true),
    });

    // Reader task: demux responses by id; drop notifications.
    {
        let conn = conn.clone();
        tauri::async_runtime::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        let Ok(v) = serde_json::from_str::<Value>(line) else {
                            continue;
                        };
                        if let Some(id) = v.get("id").and_then(|x| x.as_u64()) {
                            if let Some(tx) = conn.pending.lock().unwrap().remove(&id) {
                                let res = if let Some(err) = v.get("error") {
                                    Err(err
                                        .get("message")
                                        .and_then(|m| m.as_str())
                                        .unwrap_or("server error")
                                        .to_string())
                                } else {
                                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                                };
                                let _ = tx.send(res);
                            }
                        }
                        // Notifications (no id) are ignored here; the host
                        // re-derives capabilities on reconcile.
                    }
                    _ => break, // EOF or read error
                }
            }
            // Connection ended: fail every pending request and mark dead.
            conn.alive.store(false, Ordering::Relaxed);
            let pending: Vec<_> = conn.pending.lock().unwrap().drain().collect();
            for (_, tx) in pending {
                let _ = tx.send(Err("server connection closed".into()));
            }
        });
    }

    // Handshake.
    let init = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION") }
    });
    conn.request("initialize", init, CONNECT_TIMEOUT).await?;
    conn.notify("notifications/initialized", json!({})).await;

    let list = conn.request("tools/list", json!({}), CONNECT_TIMEOUT).await?;
    let tools = parse_tools(&cfg.name, &list);
    Ok((Conn::Stdio(conn), tools))
}

/// Connect an HTTP MCP server (best-effort: a single POST handshake +
/// `tools/list`; calls POST per request). No warm channel is kept.
async fn connect_http(cfg: &McpServerConfig) -> Result<(Conn, Vec<HostTool>), String> {
    let url = cfg.url.trim_end_matches('/').to_string();
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let init = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION") }
    });
    http_rpc(&client, &url, "initialize", init, CONNECT_TIMEOUT).await?;
    let list = http_rpc(&client, &url, "tools/list", json!({}), CONNECT_TIMEOUT).await?;
    let tools = parse_tools(&cfg.name, &list);
    Ok((Conn::Http { url, client }, tools))
}

/// One JSON-RPC request over HTTP POST.
async fn http_rpc(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let resp = client
        .post(url)
        .json(&body)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("http request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("http status {}", resp.status()));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("http parse failed: {e}"))?;
    if let Some(err) = v.get("error") {
        return Err(err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("server error")
            .to_string());
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// Parse a `tools/list` result into namespaced, read-class [`HostTool`]s.
/// Dropped (write/destructive) tools are logged so the cut isn't silent.
fn parse_tools(server: &str, list: &Value) -> Vec<HostTool> {
    let arr = list.get("tools").and_then(|t| t.as_array());
    let Some(arr) = arr else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut dropped = Vec::new();
    for t in arr {
        let Some(raw_name) = t.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if !is_read_class(raw_name) {
            dropped.push(raw_name.to_string());
            continue;
        }
        let description = t
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        let parameters = t
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" }));
        let namespaced = format!("{server}__{raw_name}");
        out.push(HostTool {
            def: ToolDef::function(namespaced, description, parameters),
            raw_name: raw_name.to_string(),
        });
    }
    if !dropped.is_empty() {
        debug!(
            server = %server,
            dropped = ?dropped,
            "offload mcp host: filtered out non-read-class tools"
        );
    }
    out
}

/// Render an MCP `tools/call` result's `content` array into the plain text
/// the agent loop feeds back to the model. Concatenates text parts; notes
/// non-text parts; honors `isError`.
fn render_tool_result(result: &Value) -> String {
    let is_error = result.get("isError").and_then(|b| b.as_bool()).unwrap_or(false);
    let mut text = String::new();
    if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
        for part in content {
            match part.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(s) = part.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(s);
                    }
                }
                Some(other) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&format!("[{other} content omitted]"));
                }
                None => {}
            }
        }
    }
    if text.is_empty() {
        // Fall back to the raw structured result for servers that don't use
        // the content envelope.
        text = result.to_string();
    }
    if is_error {
        format!("ERROR (tool reported failure): {text}")
    } else {
        text
    }
}

/// Confine a `filesystem` server to the offload `allowed_roots`: append each
/// configured root not already present in the server's args. The standard
/// `@modelcontextprotocol/server-filesystem` takes its allowed directories
/// as trailing CLI args, so this is the confinement seam. No-op for other
/// servers or when no roots are configured.
fn confine_filesystem(cfg: &McpServerConfig, args: &mut Vec<String>, allowed_roots: &[PathBuf]) {
    if cfg.name != "filesystem" || allowed_roots.is_empty() {
        return;
    }
    for root in allowed_roots {
        let root_str = root.to_string_lossy().to_string();
        if !args.iter().any(|a| a == &root_str) {
            args.push(root_str);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_verb_handles_snake_and_camel() {
        assert_eq!(leading_verb("read_file"), "read");
        assert_eq!(leading_verb("searchWeb"), "search");
        assert_eq!(leading_verb("git_log"), "git");
        assert_eq!(leading_verb("list-directory"), "list");
    }

    #[test]
    fn read_class_keeps_reads_drops_writes() {
        for ok in ["read_file", "search", "list_directory", "git_log", "fetch", "get_info", "show_diff", "blame"] {
            assert!(is_read_class(ok), "{ok} should be read-class");
        }
        for bad in ["write_file", "create_directory", "git_commit", "delete_path", "move_file", "git_push", "run_shell"] {
            assert!(!is_read_class(bad), "{bad} should be filtered");
        }
    }

    #[test]
    fn parse_tools_namespaces_and_filters() {
        let list = json!({
            "tools": [
                { "name": "search", "description": "web search", "inputSchema": { "type": "object" } },
                { "name": "write_file", "description": "writes", "inputSchema": { "type": "object" } },
                { "name": "fetch_content", "description": "gets a url" }
            ]
        });
        let tools = parse_tools("ddg", &list);
        let names: Vec<&str> = tools.iter().map(|t| t.def.function.name.as_str()).collect();
        assert!(names.contains(&"ddg__search"));
        assert!(names.contains(&"ddg__fetch_content"));
        assert!(!names.iter().any(|n| n.contains("write_file")));
        // raw name is preserved for the call.
        assert_eq!(tools.iter().find(|t| t.def.function.name == "ddg__search").unwrap().raw_name, "search");
    }

    #[test]
    fn confine_filesystem_appends_roots_once() {
        let cfg = McpServerConfig {
            name: "filesystem".into(),
            command: "npx".into(),
            ..Default::default()
        };
        let mut args = vec!["-y".to_string(), "@modelcontextprotocol/server-filesystem".to_string()];
        let roots = vec![PathBuf::from("/work"), PathBuf::from("/data")];
        confine_filesystem(&cfg, &mut args, &roots);
        assert!(args.contains(&"/work".to_string()));
        assert!(args.contains(&"/data".to_string()));
        // Idempotent.
        let before = args.len();
        confine_filesystem(&cfg, &mut args, &roots);
        assert_eq!(args.len(), before);
    }

    #[test]
    fn confine_skips_non_filesystem() {
        let cfg = McpServerConfig { name: "git".into(), command: "uvx".into(), ..Default::default() };
        let mut args = vec!["mcp-server-git".to_string()];
        confine_filesystem(&cfg, &mut args, &[PathBuf::from("/work")]);
        assert_eq!(args, vec!["mcp-server-git".to_string()]);
    }

    #[test]
    fn render_tool_result_concatenates_text() {
        let v = json!({ "content": [ { "type": "text", "text": "a" }, { "type": "text", "text": "b" } ] });
        assert_eq!(render_tool_result(&v), "a\nb");
    }

    #[test]
    fn render_tool_result_marks_errors() {
        let v = json!({ "isError": true, "content": [ { "type": "text", "text": "boom" } ] });
        assert!(render_tool_result(&v).contains("boom"));
        assert!(render_tool_result(&v).to_lowercase().contains("error"));
    }
}

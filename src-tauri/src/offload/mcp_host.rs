//! V8-03 MCP host — the warm client pool toward the user's tool servers.
//!
//! Completes V8-01's never-built Phase C: an MCP **client** (cImp is the
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
const CLIENT_NAME: &str = "cimp-offload-host";
/// Per-request timeout for an MCP server call (initialize / tools/list /
/// tools/call). A server that doesn't answer in this window is treated as
/// hung — the call fails and the loop moves on rather than blocking.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
/// Tighter bound on the handshake so a wedged server doesn't stall warm-up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// JSON-RPC error code a **modern-only** MCP server answers a legacy client
/// with. The 2026-07-28 spec revision removed `Mcp-Session-Id` and made
/// `Mcp-Method` / `Mcp-Name` required on client POSTs; a server that dropped
/// the compatibility path replies HTTP 400 + this code and does *not* fall
/// forward. cImp still speaks [`PROTOCOL_VERSION`], so the only useful
/// response is to name the cause instead of surfacing a bare `400`.
const ERR_UNSUPPORTED_REVISION: i64 = -32022;

/// User-facing explanation for [`ERR_UNSUPPORTED_REVISION`]. Phrased as a
/// clause so it composes into both the JSON-RPC and the HTTP-status message.
const UNSUPPORTED_REVISION_MSG: &str =
    "server requires a newer MCP revision than this cImp speaks (2025-06-18)";

/// Write/destructive leading verbs. A tool whose leading verb is in this
/// set is filtered out — the offload worker stays read-only even when a
/// server advertises mutating tools (`filesystem` write, `git` commit).
const WRITE_VERBS: &[&str] = &[
    "write",
    "delete",
    "remove",
    "rm",
    "create",
    "make",
    "mkdir",
    "put",
    "post",
    "update",
    "edit",
    "move",
    "mv",
    "rename",
    "append",
    "commit",
    "push",
    "merge",
    "reset",
    "drop",
    "truncate",
    "insert",
    "modify",
    "patch",
    "set",
    "unlink",
    "kill",
    "exec",
    "execute",
    "run",
    "spawn",
    "install",
    "uninstall",
    "publish",
    "send",
    "add",
    "copy",
    "cp",
    "save",
    "store",
    "upload",
    "mutate",
    "destroy",
    "clear",
    "purge",
    "apply",
    "checkout",
    "clone",
    "stage",
    "restore",
    "revert",
    // Common mutating verbs that previously slipped through as "read-class"
    // because they led with no listed verb (e.g. `task_cancel`, `job_abort`,
    // `branch_force`, `repo_sync`). Kept first-two-only because each can also
    // read-ishly appear later in a name.
    "cancel",
    "abort",
    "force",
    "sync",
];

/// Unambiguously-mutating leading verbs that essentially never appear as a noun
/// in a read-only tool's name, so they disqualify a tool as the leading verb of
/// *any* segment — not just the first two. This closes the gap where a mutating
/// verb sits past the second segment (`repo_data_set_value`, `config_apply_patch`)
/// and isn't destructive enough to be in [`HARD_WRITE_VERBS`]. Noun-ish verbs
/// (`commit`, `merge`, `add`, `copy`, …) are deliberately NOT here — they stay
/// first-two-only so reads like `get_latest_commit` aren't over-dropped.
const ANYSEG_WRITE_VERBS: &[&str] = &[
    "create",
    "mkdir",
    "update",
    "edit",
    "insert",
    "modify",
    "patch",
    "apply",
    "append",
    "rename",
    "reset",
    "install",
    "uninstall",
    "publish",
    "upload",
    "mutate",
    "set",
    "put",
    // Unambiguous mutators that never legitimately name a read tool — caught
    // in any segment so `cache_evict`, `state_flush`, `db_upsert`, `git_amend`,
    // `config_persist` can't pass as read-class.
    "evict",
    "flush",
    "upsert",
    "amend",
    "persist",
];

/// The leading verb of one name segment: the leading lowercase run so
/// camelCase (`searchWeb` → `search`) resolves, else the whole lowercased
/// segment (`Get` → `get`).
fn token_verb(token: &str) -> String {
    let lead: String = token
        .chars()
        .take_while(|c| c.is_ascii_lowercase())
        .collect();
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

/// Unambiguously destructive or code-executing verbs that never legitimately
/// appear *anywhere* in a read-only tool's name — unlike noun-ish verbs
/// (`commit`, `set`, `merge`, `add`, `copy`) which show up in plenty of read
/// tools such as `get_latest_commit` or `list_set_members`. These are checked
/// across every segment (and every camelCase sub-word) so a dangerous verb
/// buried past the second segment (`search_and_replace`, `find_and_delete`,
/// `git_force_push`) or hidden behind a leading lowercase run (`shellExec`)
/// can't slip through.
///
/// The execution verbs (`exec`/`run`/`spawn`/`shell`/`eval`/`bash`/`sh`) are
/// the highest-value entries: a tool like `command_run` or `shell_command_exec`
/// hands the local offload worker arbitrary code execution. We deliberately
/// err toward dropping a read tool that merely *contains* one of these words
/// (e.g. a CI `getRunStatus`) over ever exposing an executor — a dropped read
/// tool is harmless; an exposed executor is not.
const HARD_WRITE_VERBS: &[&str] = &[
    "write",
    "delete",
    "remove",
    "rm",
    "unlink",
    "destroy",
    "truncate",
    "drop",
    "purge",
    "replace",
    "overwrite",
    "rename",
    "uninstall",
    "kill",
    "wipe",
    "exec",
    "execute",
    "eval",
    "run",
    "spawn",
    "shell",
    "bash",
    "sh",
];

/// Split one name segment into lowercased word tokens, breaking on camelCase
/// boundaries so `gitPush` → `["git", "push"]` and `shellExec` →
/// `["shell", "exec"]`. Without this, [`token_verb`] only sees the leading
/// lowercase run and a dangerous verb after the first capital hides.
fn segment_words(segment: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_was_lower_or_digit = false;
    for c in segment.chars() {
        if c.is_ascii_uppercase() && prev_was_lower_or_digit && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        cur.push(c.to_ascii_lowercase());
        prev_was_lower_or_digit = c.is_ascii_lowercase() || c.is_ascii_digit();
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Whether a tool name is read-class (safe to expose). The offload worker is
/// read-only; mutating tools are dropped. This is best-effort *defense in
/// depth* — the real safety boundary for the native tools is server-side
/// filesystem confinement; for third-party MCP servers it bounds what we
/// advertise to the local model but a hostile/oddly-named server can still
/// name a write tool to look like a read. Two-tier check:
///
/// 1. No camelCase sub-word of any segment may be a hard-destructive or
///    execution verb ([`HARD_WRITE_VERBS`]) — catches a dangerous verb past
///    the second segment (`git_force_push`) or behind a capital (`shellExec`).
/// 2. Neither of the first two segments may lead with a (possibly noun-ish)
///    write verb — catches category-prefixed names (`git_commit`, `git_push`)
///    without dropping reads like `get_latest_commit` where the noun-verb sits
///    later.
fn is_read_class(name: &str) -> bool {
    let segments: Vec<&str> = name
        .split(['_', '-', '.', ' ', ':', '/'])
        .filter(|s| !s.is_empty())
        .collect();

    let hits_hard_verb = segments
        .iter()
        .flat_map(|seg| segment_words(seg))
        .any(|w| HARD_WRITE_VERBS.contains(&w.as_str()));
    if hits_hard_verb {
        return false;
    }

    // Unambiguous mutation verbs disqualify anywhere. Checked across every
    // camelCase sub-word (like the HARD tier), not just each segment's leading
    // verb — otherwise a camelCase mutator such as `configSet` / `userDataSet`
    // evades the `set` check that the underscore form `config_set` would hit.
    let hits_anyseg = segments
        .iter()
        .flat_map(|seg| segment_words(seg))
        .any(|w| ANYSEG_WRITE_VERBS.contains(&w.as_str()));
    if hits_anyseg {
        return false;
    }

    // Noun-ish write verbs only disqualify in the first two (category) segments,
    // so a noun-verb later in the name (`get_latest_commit`) isn't over-dropped
    // — but across the camelCase sub-words of those segments, so `commitChanges`
    // / `pushTags` are still caught.
    !segments
        .iter()
        .take(2)
        .flat_map(|seg| segment_words(seg))
        .any(|w| WRITE_VERBS.contains(&w.as_str()))
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
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err("server connection is closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            // Insert and re-check liveness *while holding the pending lock*.
            // The reader sets `alive = false` and then drains `pending` under
            // this same lock on EOF. Without the under-lock recheck there is a
            // TOCTOU: the reader could drain between the top-of-fn check and
            // this insert, orphaning our sender so the call blocks for the full
            // timeout instead of failing fast. The mutex establishes the
            // happens-before with the reader's store, so re-reading here is
            // authoritative.
            let mut pending = self.pending.lock().unwrap();
            if !self.alive.load(Ordering::Relaxed) {
                return Err("server connection is closed".into());
            }
            pending.insert(id, tx);
        }
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
                Err(format!(
                    "server did not respond within {}s",
                    timeout.as_secs()
                ))
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
    /// Streamable HTTP (MCP 2025-06-18 transport): one POST per request. The
    /// `Mcp-Session-Id` the server assigns at `initialize` is captured here and
    /// resent on every later call (some servers hard-reject a session-less
    /// `tools/list` with 400), and SSE-framed response bodies are decoded back
    /// to JSON-RPC. The id is interior-mutable so a server that assigns it on a
    /// later response (or rotates it mid-session) refreshes the stored value
    /// instead of wedging subsequent calls with a stale `400`. No warm channel
    /// is kept.
    ///
    /// A **missing** session id is normal, not a fault: a stateless server
    /// (and every server on the 2026-07-28 revision, which removed the header
    /// outright) never assigns one. `None` simply means the header is omitted
    /// on later requests — no warning, no error, no degraded mode.
    Http {
        url: String,
        client: reqwest::Client,
        session_id: StdMutex<Option<String>>,
        /// Revision the server settled on at `initialize` (see
        /// [`negotiated_version`]), echoed as `MCP-Protocol-Version` on every
        /// post-handshake request.
        protocol_version: String,
    },
}

/// Which consumer a tool-defs / tool-call request is filtered for. Each maps
/// to one per-server access flag; the offload worker uses its own backend
/// `ToolScope` on top of `offload_access`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consumer {
    Claude,
    Offload,
    Opencode,
}

impl Consumer {
    /// Parse the `--consumer` discriminator the per-session child is launched
    /// with. Unknown / absent ⇒ Claude (the original, default consumer).
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "opencode" => Consumer::Opencode,
            "offload" => Consumer::Offload,
            _ => Consumer::Claude,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Consumer::Claude => "Claude Code",
            Consumer::Offload => "the offload worker",
            Consumer::Opencode => "OpenCode",
        }
    }

    /// Whether `server` is exposed to this consumer.
    fn wants(self, server: &McpServer) -> bool {
        match self {
            Consumer::Claude => server.claude_access,
            Consumer::Offload => server.offload_access,
            Consumer::Opencode => server.opencode_access,
        }
    }
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
    /// Expose this server's tools to Claude Code (proxied through the child).
    claude_access: bool,
    /// Expose this server's tools to the offload worker. A flag change forces a
    /// reconnect (it's part of `config_sig`), so these are always fresh.
    offload_access: bool,
    /// V19: expose this server's tools to OpenCode (proxied through the
    /// `--consumer opencode` child). Like the others, part of `config_sig`.
    opencode_access: bool,
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
            Some(Conn::Http {
                url,
                client,
                session_id,
                protocol_version,
            }) => {
                let current = session_id.lock().unwrap().clone();
                match http_request(
                    client,
                    url,
                    "tools/call",
                    params,
                    current.as_deref(),
                    Some(protocol_version.as_str()),
                    REQUEST_TIMEOUT,
                )
                .await
                {
                    Ok((new_session, v)) => {
                        // Refresh the stored id if the server rotated/assigned
                        // one on this response, so the next call isn't rejected.
                        if let Some(s) = new_session {
                            *session_id.lock().unwrap() = Some(s);
                        }
                        Ok(v)
                    }
                    Err(e) => Err(e),
                }
            }
            None => Err("server is not connected".into()),
        };
        match result {
            Ok(v) => Ok(render_tool_result(&v)),
            Err(e) => {
                // A single failed call must NOT permanently disable the whole
                // server — that drops *all* its tools (`tool_defs` returns
                // empty once unhealthy) for the app's lifetime, even though a
                // 45s per-call timeout or a JSON-RPC tool-level error leaves a
                // perfectly live stdio process running. Only flip unhealthy
                // when the connection is genuinely dead (reader saw EOF/fatal,
                // so `alive` is false). HTTP calls are independent and
                // reconnect on demand, so a transient failure there leaves
                // health untouched and the next call can succeed.
                if let Some(Conn::Stdio(c)) = &self.conn {
                    if !c.alive.load(Ordering::Relaxed) {
                        self.set_unhealthy(format!("connection lost: {e}"));
                    }
                }
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
            // Skip rows with no endpoint yet (just added in the editor, no
            // command or url typed) — connecting one would route to stdio with
            // an empty command and surface a confusing `resolve ``` error.
            .filter(|c| {
                // Connect if any consumer wants it; all off ⇒ fully disabled.
                (c.claude_access || c.offload_access || c.opencode_access)
                    && !c.name.trim().is_empty()
                    && (!c.command.trim().is_empty() || !c.url.trim().is_empty())
            })
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

    /// Healthy servers' namespaced tool defs, filtered to one consumer's
    /// access flag.
    async fn tool_defs_filtered(&self, consumer: Consumer) -> Vec<ToolDef> {
        let servers = self.servers.read().await;
        let mut out = Vec::new();
        for s in servers.iter() {
            if consumer.wants(s) {
                out.extend(s.tool_defs());
            }
        }
        out
    }

    /// Offload-worker tool defs (servers with `offload_access`), for merging
    /// into the chat `tools` array (the caller then applies the backend's
    /// `ToolScope`).
    pub async fn tool_defs_for_offload(&self) -> Vec<ToolDef> {
        self.tool_defs_filtered(Consumer::Offload).await
    }

    /// Claude-Code tool defs (servers with `claude_access`), proxied to Claude
    /// through the per-session child's `tools/list`.
    pub async fn tool_defs_for_claude(&self) -> Vec<ToolDef> {
        self.tool_defs_filtered(Consumer::Claude).await
    }

    /// V19: OpenCode tool defs (servers with `opencode_access`), proxied to
    /// OpenCode through the `--consumer opencode` child's `tools/list`.
    pub async fn tool_defs_for_opencode(&self) -> Vec<ToolDef> {
        self.tool_defs_filtered(Consumer::Opencode).await
    }

    /// Route a namespaced `<server>__<tool>` call to its owning server, but
    /// only if that server is exposed to `consumer` — a proxied agent must
    /// never reach a server it isn't granted.
    async fn call_for_consumer(
        &self,
        consumer: Consumer,
        namespaced: &str,
        args: Value,
    ) -> Result<String, String> {
        let owns = {
            let servers = self.servers.read().await;
            servers
                .iter()
                .any(|s| consumer.wants(s) && s.raw_name(namespaced).is_some())
        };
        if !owns {
            return Err(format!(
                "tool `{namespaced}` is not available to {} (no {}-enabled MCP server offers it)",
                consumer.label(),
                consumer.label(),
            ));
        }
        self.call(namespaced, args).await
    }

    /// Route a Claude-exposed namespaced call. Claude must never reach an
    /// offload-only server's tools.
    pub async fn call_for_claude(&self, namespaced: &str, args: Value) -> Result<String, String> {
        self.call_for_consumer(Consumer::Claude, namespaced, args)
            .await
    }

    /// V19: route an OpenCode-exposed namespaced call.
    pub async fn call_for_opencode(&self, namespaced: &str, args: Value) -> Result<String, String> {
        self.call_for_consumer(Consumer::Opencode, namespaced, args)
            .await
    }

    /// Route a namespaced `<server>__<tool>` call to its owning server.
    pub async fn call(&self, namespaced: &str, args: Value) -> Result<String, String> {
        // Route by actual ownership (an exact match on the namespaced def
        // name) rather than parsing a `<prefix>__` split — a server or raw
        // tool name that itself contains `__` would make the split route to
        // the wrong/nonexistent server.
        let server = {
            let servers = self.servers.read().await;
            servers
                .iter()
                .find(|s| s.raw_name(namespaced).is_some())
                .cloned()
        };
        let Some(server) = server else {
            return Err(format!("no MCP server owns tool `{namespaced}`"));
        };
        let Some(raw) = server.raw_name(namespaced).map(|s| s.to_string()) else {
            return Err(format!(
                "server `{}` no longer offers `{namespaced}`",
                server.name
            ));
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
    // Include all access flags: a per-consumer toggle (Claude / offload /
    // OpenCode) must still re-key the signature so `warm_host` reconciles and
    // re-emits a capability pulse.
    format!(
        "{}|{}|{:?}|{}|{}|{}|{:?}",
        c.command, c.url, c.args, c.claude_access, c.offload_access, c.opencode_access, env
    )
}

/// A stable signature of the *whole* desired host configuration (every
/// server's connection-relevant config, keyed by name, plus the allowed
/// roots). `warm_host` compares this against the last reconcile so an
/// unchanged config skips the work — and the `host_reconcile_lock` hold —
/// on the per-run hot path.
pub fn host_config_sig(configs: &[McpServerConfig], roots: &[PathBuf]) -> String {
    let mut servers: Vec<String> = configs
        .iter()
        .map(|c| format!("{}={}", c.name, config_sig(c)))
        .collect();
    servers.sort();
    let mut roots: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
    roots.sort();
    format!("{servers:?}|roots:{roots:?}")
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
        claude_access: cfg.claude_access,
        offload_access: cfg.offload_access,
        opencode_access: cfg.opencode_access,
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
    // Backstop: reap this warm MCP-host server via the kill-on-job-close job
    // if cImp dies hard (kill_on_drop only covers a clean exit).
    crate::process_guard::guard_child(&child);

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
            // Loop ends on EOF or a read error.
            while let Ok(Some(line)) = lines.next_line().await {
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
                            Err(jsonrpc_error_text(err))
                        } else {
                            Ok(v.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = tx.send(res);
                    }
                }
                // Notifications (no id) are ignored here; the host re-derives
                // capabilities on reconcile.
            }
            // Connection ended: fail every pending request and mark dead.
            // Flip `alive` and drain under the same lock the request path
            // takes, so a request can't insert into `pending` after we've
            // drained it (which would orphan its sender until timeout).
            let pending: Vec<_> = {
                let mut p = conn.pending.lock().unwrap();
                conn.alive.store(false, Ordering::Relaxed);
                p.drain().collect()
            };
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
    let init_result = conn.request("initialize", init, CONNECT_TIMEOUT).await?;
    // Record the revision the server settled on. stdio frames carry no
    // headers, so there is nothing to echo back — but a server answering with
    // a different revision than we asked for is exactly the signal that
    // explains a later behavioral surprise, so it must not be swallowed.
    let negotiated = negotiated_version(&init_result);
    if negotiated != PROTOCOL_VERSION {
        info!(
            server = %cfg.name,
            requested = PROTOCOL_VERSION,
            negotiated = %negotiated,
            "offload mcp host: server answered with a different protocol revision"
        );
    }
    conn.notify("notifications/initialized", json!({})).await;

    let list = conn
        .request("tools/list", json!({}), CONNECT_TIMEOUT)
        .await?;
    let tools = parse_tools(&cfg.name, &list);
    Ok((Conn::Stdio(conn), tools))
}

/// Connect a Streamable-HTTP MCP server: `initialize` (capturing the assigned
/// session id), the `notifications/initialized` confirmation, then `tools/list`
/// — all carrying the session id. Calls POST per request; no warm channel.
async fn connect_http(cfg: &McpServerConfig) -> Result<(Conn, Vec<HostTool>), String> {
    let url = cfg.url.trim_end_matches('/').to_string();
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // Bound the connection phase tightly so an unreachable host (a LAN box
        // that's powered off and blackholes the SYN) fails fast instead of
        // pinning `host_reconcile_lock` for the full 30s `CONNECT_TIMEOUT` and
        // stalling every concurrent offload's `warm_host`.
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let init = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION") }
    });
    // The session id is assigned on the initialize response and must be echoed
    // back on every subsequent request (some servers — e.g. ddg-search —
    // hard-reject a session-less `tools/list` with 400 "Missing session ID").
    // A server that assigns none is fine: `None` just omits the header.
    let (mut session_id, init_result) = http_request(
        &client,
        &url,
        "initialize",
        init,
        None,
        // The handshake itself predates the negotiation, so it carries no
        // `MCP-Protocol-Version` header — the body's `protocolVersion` is the
        // request.
        None,
        CONNECT_TIMEOUT,
    )
    .await?;
    // Adopt whatever revision the server answered with (see
    // [`negotiated_version`]) and speak that from here on.
    let protocol_version = negotiated_version(&init_result);
    if protocol_version != PROTOCOL_VERSION {
        info!(
            server = %cfg.name,
            requested = PROTOCOL_VERSION,
            negotiated = %protocol_version,
            "offload mcp host: adopting the server's protocol revision"
        );
    }
    // The transport requires the client to confirm initialization before
    // issuing further requests; send it (best-effort) carrying the session id.
    http_notify(
        &client,
        &url,
        "notifications/initialized",
        json!({}),
        session_id.as_deref(),
        Some(protocol_version.as_str()),
    )
    .await;
    let (list_session, list) = http_request(
        &client,
        &url,
        "tools/list",
        json!({}),
        session_id.as_deref(),
        Some(protocol_version.as_str()),
        CONNECT_TIMEOUT,
    )
    .await?;
    // Fall back to a session id assigned on the tools/list response if the
    // initialize response carried none (some servers assign it late).
    if session_id.is_none() {
        session_id = list_session;
    }
    let tools = parse_tools(&cfg.name, &list);
    Ok((
        Conn::Http {
            url,
            client,
            session_id: StdMutex::new(session_id),
            protocol_version,
        },
        tools,
    ))
}

/// The protocol revision to speak after `initialize`. The spec makes the
/// server's echoed `protocolVersion` authoritative: a server may answer with a
/// revision other than the one the client asked for, and the client must then
/// use that one (or disconnect) rather than assume its request was honored.
/// This adopts it — the HTTP transport echoes the result back as
/// `MCP-Protocol-Version` on every later request.
///
/// A missing/blank field means a server that predates the field; fall back to
/// what we requested ([`PROTOCOL_VERSION`]) rather than failing the connect.
fn negotiated_version(init_result: &Value) -> String {
    init_result
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(PROTOCOL_VERSION)
        .to_string()
}

/// Render a JSON-RPC `error` object into the message the host surfaces (health
/// row + tool-call failure). Codes are otherwise opaque, but
/// [`ERR_UNSUPPORTED_REVISION`] is the one a user can act on, so it gets named.
fn jsonrpc_error_text(err: &Value) -> String {
    let msg = err
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("server error");
    match err.get("code").and_then(|c| c.as_i64()) {
        Some(ERR_UNSUPPORTED_REVISION) => {
            format!("{UNSUPPORTED_REVISION_MSG} — JSON-RPC error -32022: {msg}")
        }
        _ => msg.to_string(),
    }
}

/// Render a non-2xx response from an MCP endpoint. A modern-only server
/// rejects a legacy handshake with HTTP 400 carrying JSON-RPC
/// [`ERR_UNSUPPORTED_REVISION`]; recognize that shape explicitly, and treat a
/// bare `400` on `initialize` as *possibly* the same cause (some servers send
/// a plain-text 400) instead of leaking an unexplained status code.
fn http_error_text(status: u16, method: &str, body: &str) -> String {
    let excerpt: String = body.chars().take(300).collect();
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.get("code")?.as_i64());
    if code == Some(ERR_UNSUPPORTED_REVISION) {
        return format!("{UNSUPPORTED_REVISION_MSG} — HTTP {status}, JSON-RPC error -32022");
    }
    if status == 400 && method == "initialize" {
        return format!(
            "MCP handshake rejected with HTTP {status} — possibly the {UNSUPPORTED_REVISION_MSG}; response: {excerpt}"
        );
    }
    format!("http status {status}: {excerpt}")
}

/// Core Streamable-HTTP request: POST one JSON-RPC frame and return the
/// `Mcp-Session-Id` the server assigned (if any) plus the JSON-RPC `result`.
/// Sends the dual `Accept` the 2025 transport mandates (a server rejects a
/// client that doesn't accept `text/event-stream` with 406), resends a prior
/// `session_id` (when the server assigned one) and the negotiated
/// `MCP-Protocol-Version`, and decodes an SSE-framed response body back to
/// JSON.
///
/// NOTE (2026-07-28 revision, not implemented): that revision drops
/// `Mcp-Session-Id` and requires `Mcp-Method` and `Mcp-Name` headers on every
/// client POST. They would be added right beside the `Accept` header below,
/// gated on the negotiated `protocol_version` — cImp still requests
/// [`PROTOCOL_VERSION`], so a modern-only server is *detected*
/// ([`ERR_UNSUPPORTED_REVISION`]) rather than spoken to.
async fn http_request(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
    session_id: Option<&str>,
    protocol_version: Option<&str>,
    timeout: Duration,
) -> Result<(Option<String>, Value), String> {
    // Unique per-call id (JSON-RPC ids must be unique within a session; some
    // servers reject a repeated id even on a stateless POST).
    static HTTP_RPC_ID: AtomicU64 = AtomicU64::new(1);
    let id = HTTP_RPC_ID.fetch_add(1, Ordering::Relaxed);
    let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let mut req = client
        .post(url)
        .timeout(timeout)
        .header("Accept", "application/json, text/event-stream")
        .json(&body);
    // Omitted entirely when the server never assigned one — a stateless server
    // is a normal server, not a degraded one.
    if let Some(s) = session_id {
        req = req.header("Mcp-Session-Id", s);
    }
    if let Some(v) = protocol_version {
        req = req.header("MCP-Protocol-Version", v);
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| format!("http request failed: {e}"))?;
    let status = resp.status();
    let new_session = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(http_error_text(status.as_u16(), method, &body));
    }
    // For an SSE-framed body, read incrementally and return on the first frame
    // carrying a JSON-RPC result/error — a server that streams progress events
    // before the result must not block us until it closes the stream. A plain
    // JSON body is read whole.
    let v = if is_event_stream(&content_type) {
        read_sse_result(&mut resp).await?
    } else {
        let text = resp
            .text()
            .await
            .map_err(|e| format!("http body read failed: {e}"))?;
        serde_json::from_str::<Value>(&text).map_err(|e| format!("http parse failed: {e}"))?
    };
    if let Some(err) = v.get("error") {
        return Err(jsonrpc_error_text(err));
    }
    Ok((new_session, v.get("result").cloned().unwrap_or(Value::Null)))
}

/// Fire a JSON-RPC notification over HTTP (no id, no result expected). Servers
/// answer `202 Accepted` with an empty body; failures are non-fatal here.
async fn http_notify(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
    session_id: Option<&str>,
    protocol_version: Option<&str>,
) {
    let body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    let mut req = client
        .post(url)
        .timeout(CONNECT_TIMEOUT)
        .header("Accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(s) = session_id {
        req = req.header("Mcp-Session-Id", s);
    }
    if let Some(v) = protocol_version {
        req = req.header("MCP-Protocol-Version", v);
    }
    let _ = req.send().await;
}

/// True for a `text/event-stream` content type, case-insensitively and
/// tolerant of parameters (`; charset=utf-8`). HTTP media types are
/// case-insensitive, so a server sending `Text/Event-Stream` must still be
/// routed through the SSE decoder rather than parsed as plain JSON.
fn is_event_stream(content_type: &str) -> bool {
    content_type
        .to_ascii_lowercase()
        .contains("text/event-stream")
}

/// A single SSE `data:` frame parsed to JSON, kept only if it carries a
/// JSON-RPC `result` or `error` (so non-response events — pings, progress —
/// are skipped).
fn sse_frame(data: &str) -> Option<Value> {
    serde_json::from_str::<Value>(data)
        .ok()
        .filter(|v| v.get("result").is_some() || v.get("error").is_some())
}

/// Incremental SSE event assembler, shared by the streaming reader and the
/// buffered [`decode_jsonrpc_body`] so both honor identical framing rules.
/// Feed it one line at a time; it accumulates an event's `data:` lines and,
/// on the blank line that ends the event, yields the JSON-RPC frame if that
/// event carried a `result`/`error`.
#[derive(Default)]
struct SseAssembler {
    data: String,
}

impl SseAssembler {
    /// Feed one (newline-stripped) line. Returns `Some(frame)` when this line
    /// closed an event whose data is a JSON-RPC response.
    fn push_line(&mut self, line: &str) -> Option<Value> {
        if let Some(rest) = line.strip_prefix("data:") {
            // SSE spec: a single leading space after the colon is stripped;
            // multiple `data:` lines in one event join with a newline.
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            None
        } else if line.is_empty() {
            // A truly empty line ends the event (a whitespace-only line is a
            // data continuation, not a boundary).
            self.finish()
        } else {
            // Other SSE fields (`event:`, `id:`, `:comment`) are ignored.
            None
        }
    }

    /// Flush the current event (e.g. a final one not terminated by a blank
    /// line), clearing it. Returns the frame if it is a JSON-RPC response.
    fn finish(&mut self) -> Option<Value> {
        if self.data.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.data);
        sse_frame(&data)
    }
}

/// Read an SSE-framed response incrementally, returning as soon as a frame
/// carrying a JSON-RPC `result`/`error` arrives. Lines are assembled from raw
/// chunks (decoded at line granularity, so a multibyte char split across two
/// chunks isn't corrupted), so we never wait for the server to close a stream
/// that keeps emitting progress notifications after the result.
async fn read_sse_result(resp: &mut reqwest::Response) -> Result<Value, String> {
    // Bound the unframed accumulation: complete lines are drained below, so this
    // caps a SINGLE newline-less line. Without it a server that streams bytes
    // without a newline grows `buf` until OOM (the caller's timeout is the only
    // other bound).
    const MAX_SSE_BYTES: usize = 16 * 1024 * 1024;
    let mut asm = SseAssembler::default();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(bytes)) => {
                buf.extend_from_slice(&bytes);
                if buf.len() > MAX_SSE_BYTES {
                    return Err(format!(
                        "SSE response exceeded {MAX_SSE_BYTES} bytes without a complete line"
                    ));
                }
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line_bytes);
                    if let Some(v) = asm.push_line(line.trim_end_matches(['\n', '\r'])) {
                        return Ok(v);
                    }
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("http body read failed: {e}")),
        }
    }
    // Stream ended: feed any unterminated trailing line, then flush.
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf);
        if let Some(v) = asm.push_line(line.trim_end_matches(['\n', '\r'])) {
            return Ok(v);
        }
    }
    asm.finish()
        .ok_or_else(|| "no JSON-RPC message found in SSE response".into())
}

/// Decode a fully-buffered Streamable-HTTP response body into the JSON-RPC
/// message. Used for plain `application/json` bodies and as the buffered
/// counterpart to [`read_sse_result`] (kept for the unit tests). A
/// `text/event-stream` body is SSE-framed; a plain body is parsed directly.
#[cfg(test)]
fn decode_jsonrpc_body(content_type: &str, body: &str) -> Result<Value, String> {
    if !is_event_stream(content_type) {
        return serde_json::from_str::<Value>(body).map_err(|e| format!("http parse failed: {e}"));
    }
    let mut asm = SseAssembler::default();
    for line in body.lines() {
        if let Some(v) = asm.push_line(line) {
            return Ok(v);
        }
    }
    asm.finish()
        .ok_or_else(|| "no JSON-RPC message found in SSE response".into())
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
    let is_error = result
        .get("isError")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
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

/// Whether this config is the standard filesystem MCP server, by name *or*
/// by the package it launches. Keying on the configured `name` alone is
/// fragile: a user who names the server `fs` or `local-files` would silently
/// bypass confinement, exposing the whole filesystem to the offload model.
fn is_filesystem_server(cfg: &McpServerConfig, args: &[String]) -> bool {
    if cfg.name.eq_ignore_ascii_case("filesystem") {
        return true;
    }
    const PKG: &str = "server-filesystem";
    cfg.command.contains(PKG) || args.iter().any(|a| a.contains(PKG))
}

/// Confine a filesystem server to the offload `allowed_roots`: append each
/// configured root not already present in the server's args. The standard
/// `@modelcontextprotocol/server-filesystem` takes its allowed directories
/// as trailing CLI args, so this is the confinement seam. No-op for other
/// servers or when no roots are configured.
fn confine_filesystem(cfg: &McpServerConfig, args: &mut Vec<String>, allowed_roots: &[PathBuf]) {
    if !is_filesystem_server(cfg, args) || allowed_roots.is_empty() {
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

    /// A fake healthy server (no real connection) carrying one namespaced tool,
    /// for exercising the per-consumer filtering without spawning an MCP server.
    fn fake_server(
        name: &str,
        claude: bool,
        offload: bool,
        opencode: bool,
        namespaced: &str,
    ) -> McpServer {
        let raw = namespaced
            .split("__")
            .nth(1)
            .unwrap_or(namespaced)
            .to_string();
        McpServer {
            name: name.into(),
            sig: String::new(),
            transport_label: "http",
            conn: None,
            tools: vec![HostTool {
                def: ToolDef::function(namespaced, "", json!({ "type": "object" })),
                raw_name: raw,
            }],
            healthy: AtomicBool::new(true),
            error: StdMutex::new(None),
            claude_access: claude,
            offload_access: offload,
            opencode_access: opencode,
        }
    }

    #[tokio::test]
    async fn tool_defs_and_calls_partition_by_access_flag() {
        let host = McpHost::new();
        host.servers.write().await.extend([
            Arc::new(fake_server("alpha", true, false, false, "alpha__x")), // Claude-only
            Arc::new(fake_server("beta", false, true, false, "beta__y")),   // offload-only
            Arc::new(fake_server("gamma", false, false, true, "gamma__z")), // OpenCode-only
        ]);

        let claude = host.tool_defs_for_claude().await;
        assert_eq!(claude.len(), 1);
        assert_eq!(claude[0].function.name, "alpha__x");

        let offload = host.tool_defs_for_offload().await;
        assert_eq!(offload.len(), 1);
        assert_eq!(offload[0].function.name, "beta__y");

        let opencode = host.tool_defs_for_opencode().await;
        assert_eq!(opencode.len(), 1);
        assert_eq!(opencode[0].function.name, "gamma__z");

        // Claude must not be able to invoke the offload-only server's tool.
        let err = host
            .call_for_claude("beta__y", json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("not available to Claude"), "got: {err}");
        // OpenCode must not reach the Claude-only server's tool.
        let err2 = host
            .call_for_opencode("alpha__x", json!({}))
            .await
            .unwrap_err();
        assert!(err2.contains("not available to OpenCode"), "got: {err2}");
    }

    #[test]
    fn leading_verb_handles_snake_and_camel() {
        assert_eq!(leading_verb("read_file"), "read");
        assert_eq!(leading_verb("searchWeb"), "search");
        assert_eq!(leading_verb("git_log"), "git");
        assert_eq!(leading_verb("list-directory"), "list");
    }

    #[test]
    fn read_class_keeps_reads_drops_writes() {
        for ok in [
            "read_file",
            "search",
            "list_directory",
            "git_log",
            "fetch",
            "get_info",
            "show_diff",
            "blame",
        ] {
            assert!(is_read_class(ok), "{ok} should be read-class");
        }
        for bad in [
            "write_file",
            "create_directory",
            "git_commit",
            "delete_path",
            "move_file",
            "git_push",
            "run_shell",
        ] {
            assert!(!is_read_class(bad), "{bad} should be filtered");
        }
    }

    #[test]
    fn read_class_catches_buried_destructive_verbs() {
        // A hard-destructive verb past the second segment must still drop.
        for bad in [
            "search_and_replace",
            "find_and_delete",
            "list_then_remove",
            "scan_and_wipe",
        ] {
            assert!(!is_read_class(bad), "{bad} should be filtered");
        }
        // ...but a noun-ish verb in the 3rd+ segment must NOT over-drop a read
        // (these are only checked in the first two segments, unchanged).
        for ok in [
            "get_latest_commit",
            "get_repo_merge_status",
            "list_all_user_sets",
        ] {
            assert!(is_read_class(ok), "{ok} should be read-class");
        }
        // An unambiguous mutation verb past the second segment must drop, even
        // though it isn't destructive enough to be a HARD verb.
        for bad in [
            "repo_data_set_value",
            "config_apply_patch",
            "db_record_update",
            "file_meta_rename",
        ] {
            assert!(!is_read_class(bad), "{bad} should be filtered");
        }
        // camelCase mutators must drop too: the ANYSEG/WRITE tiers split
        // camelCase sub-words (not just the leading lowercase run), so these
        // can't evade the way `configSet` once did.
        for bad in [
            "configSet",
            "userDataSet",
            "applyPatch",
            "recordUpdate",
            "metaRename",
        ] {
            assert!(!is_read_class(bad), "{bad} should be filtered");
        }
        // ...without over-dropping a camelCase read whose noun merely contains a
        // verb-like plural ("sets" != "set").
        for ok in ["listAllSets", "getResultSets"] {
            assert!(is_read_class(ok), "{ok} should be read-class");
        }
    }

    #[test]
    fn filesystem_detected_by_package_not_just_name() {
        let cfg = McpServerConfig {
            name: "my-files".into(),
            command: "npx".into(),
            ..Default::default()
        };
        let args = vec![
            "-y".to_string(),
            "@modelcontextprotocol/server-filesystem".to_string(),
        ];
        assert!(is_filesystem_server(&cfg, &args));
        // A genuinely unrelated server is not confined.
        let git = McpServerConfig {
            name: "git".into(),
            command: "uvx".into(),
            ..Default::default()
        };
        assert!(!is_filesystem_server(&git, &["mcp-server-git".to_string()]));
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
        assert_eq!(
            tools
                .iter()
                .find(|t| t.def.function.name == "ddg__search")
                .unwrap()
                .raw_name,
            "search"
        );
    }

    #[test]
    fn confine_filesystem_appends_roots_once() {
        let cfg = McpServerConfig {
            name: "filesystem".into(),
            command: "npx".into(),
            ..Default::default()
        };
        let mut args = vec![
            "-y".to_string(),
            "@modelcontextprotocol/server-filesystem".to_string(),
        ];
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
        let cfg = McpServerConfig {
            name: "git".into(),
            command: "uvx".into(),
            ..Default::default()
        };
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

    #[test]
    fn decode_plain_json_body() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let v = decode_jsonrpc_body("application/json", body).unwrap();
        assert_eq!(v["result"]["ok"], json!(true));
    }

    #[test]
    fn decode_sse_body_extracts_jsonrpc() {
        // The exact shape ddg-search / Context7 return: an `event:` line then a
        // single `data:` line carrying the JSON-RPC response, ended by a blank.
        let sse =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let v = decode_jsonrpc_body("text/event-stream", sse).unwrap();
        assert_eq!(v["result"]["ok"], json!(true));
    }

    #[test]
    fn decode_sse_skips_non_response_events_and_keeps_error() {
        // A leading non-response event (a notification) is skipped; the frame
        // carrying `error` is the one returned.
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n\
                   event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-1,\"message\":\"boom\"}}\n\n";
        let v = decode_jsonrpc_body("text/event-stream", sse).unwrap();
        assert_eq!(v["error"]["message"], json!("boom"));
    }

    #[test]
    fn decode_sse_no_response_frame_errors() {
        let sse = "event: ping\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n\n";
        assert!(decode_jsonrpc_body("text/event-stream", sse).is_err());
    }

    #[test]
    fn decode_charset_suffixed_event_stream() {
        // Content-Type may carry a charset/boundary suffix — substring match.
        let sse = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"v\":7}}\n\n";
        let v = decode_jsonrpc_body("text/event-stream; charset=utf-8", sse).unwrap();
        assert_eq!(v["result"]["v"], json!(7));
    }

    #[test]
    fn is_event_stream_is_case_insensitive() {
        // HTTP media types are case-insensitive — an uppercased Content-Type
        // must still route through the SSE decoder, not the plain-JSON branch.
        assert!(is_event_stream("text/event-stream"));
        assert!(is_event_stream("Text/Event-Stream"));
        assert!(is_event_stream("TEXT/EVENT-STREAM; charset=utf-8"));
        assert!(!is_event_stream("application/json"));
    }

    #[test]
    fn sse_assembler_skips_progress_and_returns_first_result_frame() {
        // The streaming reader feeds the assembler one line at a time and stops
        // at the first JSON-RPC result/error frame — a progress notification
        // emitted before the result must not block or be mistaken for it.
        let mut asm = SseAssembler::default();
        let lines = [
            "event: message",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}",
            "",
            "event: message",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}",
            "",
        ];
        let mut got = None;
        for l in lines {
            if let Some(v) = asm.push_line(l) {
                got = Some(v);
                break;
            }
        }
        assert_eq!(got.unwrap()["result"]["ok"], json!(true));
    }

    #[test]
    fn negotiated_version_adopts_the_servers_answer() {
        // The server's echoed revision is authoritative — even when it differs
        // from (or predates) the one we requested.
        let older = json!({ "protocolVersion": "2025-03-26", "capabilities": {} });
        assert_eq!(negotiated_version(&older), "2025-03-26");
        let newer = json!({ "protocolVersion": "2026-07-28" });
        assert_eq!(negotiated_version(&newer), "2026-07-28");
        // Same-as-requested round-trips unchanged.
        let same = json!({ "protocolVersion": PROTOCOL_VERSION });
        assert_eq!(negotiated_version(&same), PROTOCOL_VERSION);
    }

    #[test]
    fn negotiated_version_falls_back_when_absent_or_blank() {
        // A server that omits the field (or sends junk) must not fail the
        // connect — we keep speaking what we asked for.
        assert_eq!(negotiated_version(&json!({})), PROTOCOL_VERSION);
        assert_eq!(
            negotiated_version(&json!({ "protocolVersion": "  " })),
            PROTOCOL_VERSION
        );
        assert_eq!(
            negotiated_version(&json!({ "protocolVersion": 7 })),
            PROTOCOL_VERSION
        );
        assert_eq!(negotiated_version(&Value::Null), PROTOCOL_VERSION);
    }

    #[test]
    fn jsonrpc_error_names_the_unsupported_revision() {
        let err =
            json!({ "code": ERR_UNSUPPORTED_REVISION, "message": "unsupported protocol version" });
        let text = jsonrpc_error_text(&err);
        assert!(text.contains("newer MCP revision"), "got: {text}");
        assert!(text.contains("-32022"), "got: {text}");
        // The server's own wording is preserved alongside the explanation.
        assert!(text.contains("unsupported protocol version"), "got: {text}");
    }

    #[test]
    fn jsonrpc_error_passes_other_codes_through() {
        // Ordinary tool-level errors must keep their plain message — the
        // revision hint would be actively misleading there.
        let err = json!({ "code": -32602, "message": "invalid params" });
        assert_eq!(jsonrpc_error_text(&err), "invalid params");
        let bare = json!({ "code": -1 });
        assert_eq!(jsonrpc_error_text(&bare), "server error");
    }

    #[test]
    fn http_error_names_the_unsupported_revision() {
        // The modern-only shape: HTTP 400 + JSON-RPC -32022, no fall-forward.
        let body =
            r#"{"jsonrpc":"2.0","error":{"code":-32022,"message":"protocol revision retired"}}"#;
        let text = http_error_text(400, "initialize", body);
        assert!(text.contains("newer MCP revision"), "got: {text}");
        assert!(text.contains("-32022"), "got: {text}");
        // Recognized by code, not by status — a server using another status
        // for the same refusal is still explained.
        let other = http_error_text(426, "tools/list", body);
        assert!(other.contains("newer MCP revision"), "got: {other}");
    }

    #[test]
    fn http_error_hints_on_bare_handshake_400_only() {
        // A plain-text 400 on `initialize` gets the "possibly" hint...
        let at_handshake = http_error_text(400, "initialize", "Bad Request");
        assert!(at_handshake.contains("handshake"), "got: {at_handshake}");
        assert!(
            at_handshake.contains("newer MCP revision"),
            "got: {at_handshake}"
        );
        assert!(at_handshake.contains("Bad Request"), "got: {at_handshake}");
        // ...but a 400 on a later call, or any other status, stays generic —
        // a missing session id and a bad argument both land here.
        let later = http_error_text(400, "tools/call", "Missing session ID");
        assert!(!later.contains("newer MCP revision"), "got: {later}");
        assert!(later.contains("http status 400"), "got: {later}");
        let five_oh_three = http_error_text(503, "initialize", "upstream down");
        assert!(
            !five_oh_three.contains("newer MCP revision"),
            "got: {five_oh_three}"
        );
        assert!(
            five_oh_three.contains("http status 503"),
            "got: {five_oh_three}"
        );
    }

    #[test]
    fn http_error_truncates_long_bodies() {
        let body = "x".repeat(1000);
        let text = http_error_text(500, "tools/call", &body);
        assert!(
            text.len() < 400,
            "body should be excerpted, got {}",
            text.len()
        );
    }

    #[test]
    fn host_config_sig_detects_changes_and_is_stable() {
        let a = McpServerConfig {
            name: "ddg".into(),
            url: "http://x/mcp".into(),
            offload_access: true,
            ..Default::default()
        };
        let roots = vec![PathBuf::from("/work")];
        let s1 = host_config_sig(std::slice::from_ref(&a), &roots);
        // Stable for identical input (the warm_host skip relies on this).
        assert_eq!(s1, host_config_sig(std::slice::from_ref(&a), &roots));
        // Changes when a server field changes (access toggle, url, …).
        let b = McpServerConfig {
            offload_access: false,
            ..a.clone()
        };
        assert_ne!(s1, host_config_sig(std::slice::from_ref(&b), &roots));
        // Changes when the allowed roots change.
        assert_ne!(
            s1,
            host_config_sig(std::slice::from_ref(&a), &[PathBuf::from("/other")])
        );
    }
}

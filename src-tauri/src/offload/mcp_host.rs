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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{broadcast, oneshot, Mutex as TokioMutex, RwLock};
use tracing::{debug, info, warn};

use crate::settings::{McpActivation, McpCategory, McpServerConfig};

use super::openai::ToolDef;
use super::outbound;

/// V37 contract C4 — the stable substring every disabled-server refusal
/// carries, so a consumer (or a test, or a live-verify) can recognize the class
/// without matching cImp's prose. The two level tails below say *which* toggle
/// did it, and they are the same bytes [`EnableVerdict::refusal`] composes the
/// message from — a marker only ever asserted about, never used to build the
/// string it names, is a marker free to drift.
pub const REFUSAL_DISABLED: &str = "is disabled (";
/// Tail of the CATEGORY-level refusal (contract C4), after the category name.
pub const REFUSAL_DISABLED_BY_CATEGORY: &str = " is off)";
/// Tail of the SERVER-level refusal (contract C4).
pub const REFUSAL_DISABLED_BY_SERVER: &str = "server toggle)";

/// V37 contract C3 — the verdict of the ONE effective-enable predicate.
///
/// Not a `bool`, because contract C4's refusal must name the *level* that did
/// it: "you turned this server off" and "the category it sits in is off" are
/// different user mistakes with different fixes, and a `bool` would force the
/// dispatch path to re-derive the reason with a second, drifting copy of the
/// rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnableVerdict {
    /// Effectively enabled: advertise, connect, dispatch.
    Enabled,
    /// The server's own toggle is off — globally, or by a project overlay
    /// entry in `activation.servers`.
    ServerOff,
    /// The server's own toggle is on, but every category containing it is off.
    /// Carries the FIRST containing category in registry order, purely for the
    /// refusal wording — with several off categories any of them is a true
    /// answer, and registry order makes the choice deterministic.
    CategoriesOff(String),
}

impl EnableVerdict {
    /// Whether this verdict means "the server exists right now".
    pub fn is_enabled(&self) -> bool {
        matches!(self, EnableVerdict::Enabled)
    }

    /// The contract-C4 refusal text for a call that reached a disabled server.
    /// `Enabled` has no refusal and yields `None`.
    fn refusal(&self, server: &str) -> Option<String> {
        match self {
            EnableVerdict::Enabled => None,
            EnableVerdict::ServerOff => Some(format!(
                "server `{server}` {REFUSAL_DISABLED}{REFUSAL_DISABLED_BY_SERVER}"
            )),
            EnableVerdict::CategoriesOff(cat) => Some(format!(
                "server `{server}` {REFUSAL_DISABLED}category `{cat}`{REFUSAL_DISABLED_BY_CATEGORY}"
            )),
        }
    }
}

/// V37 contract C3 — **the** effective-enable predicate. One function, one
/// owner; both advertisement ([`McpHost::tool_defs_filtered`]) and dispatch
/// ([`McpHost::call_for_consumer`]) read this and nothing else, so the two can
/// never disagree about whether a server exists.
///
/// ```text
/// enabled(server) :=
///   (server.enabled, overridden by activation.servers[name] if present)
///   AND ( no category contains the server
///         OR at least one containing category is effectively enabled,
///            where a category's effective state = category.enabled
///            overridden by activation.categories[name] if present )
/// ```
///
/// Two rules earn their keep:
///
/// * **Uncategorized servers ride the server toggle alone.** This is what makes
///   the C2 migration invariant hold: a pre-v32 file has no categories, so
///   every server's verdict is exactly its (defaulted-`true`) own toggle.
/// * **One enabled category is enough.** Categories are an OR, not an AND — a
///   server the user filed under both "research" and "web" stays available
///   while either group is on. A server whose categories are *all* off is off
///   even with its own toggle on.
///
/// # The activation maps are already effective
///
/// By the time settings reach the host, `persistence`'s `deep_merge` has folded
/// the project overlay into the `Settings` snapshot. So `activation` here is the
/// *composed* result, not "the overlay" — this function must never read an
/// overlay file, and `Some(v)` in either map simply wins over the global flag
/// while absence inherits it.
pub fn effective_enable(
    server: &McpServerConfig,
    categories: &[McpCategory],
    activation: &McpActivation,
) -> EnableVerdict {
    let server_on = activation
        .servers
        .get(&server.name)
        .copied()
        .unwrap_or(server.enabled);
    if !server_on {
        return EnableVerdict::ServerOff;
    }
    let mut first_containing: Option<&str> = None;
    for c in categories
        .iter()
        .filter(|c| c.servers.iter().any(|s| s == &server.name))
    {
        let on = activation
            .categories
            .get(&c.name)
            .copied()
            .unwrap_or(c.enabled);
        if on {
            return EnableVerdict::Enabled;
        }
        if first_containing.is_none() {
            first_containing = Some(c.name.as_str());
        }
    }
    match first_containing {
        // At least one category contains it and none of them is on.
        Some(name) => EnableVerdict::CategoriesOff(name.to_string()),
        // Uncategorized: the server toggle (already checked) is the whole rule.
        None => EnableVerdict::Enabled,
    }
}

/// Boolean shorthand over [`effective_enable`] for the callers that only need
/// the yes/no (reconcile's desired-set filter, the signature).
pub fn server_enabled(
    server: &McpServerConfig,
    categories: &[McpCategory],
    activation: &McpActivation,
) -> bool {
    effective_enable(server, categories, activation).is_enabled()
}

/// A configured server that the C3 predicate turned OFF, retained by the host
/// across reconciles.
///
/// Contract C4 needs this: a disabled server is treated as **absent for
/// connection purposes** (never connected, torn down if it was), so the routing
/// table — which is built from live connections' advertised tools — knows
/// nothing about it. Without this list, a stale call to a just-disabled server
/// would come back as "no server offers that tool", i.e. *disabled* would be
/// indistinguishable from *never existed*, and the user would have no way to
/// learn that a toggle they flipped is the cause.
#[derive(Clone, Debug)]
pub struct DisabledServer {
    /// [`McpServerConfig::name`] — the id (contract C1) and the namespace
    /// prefix of every tool this server would have advertised.
    pub name: String,
    /// Which level turned it off, for the refusal wording.
    pub verdict: EnableVerdict,
    /// Expose to Claude Code — copied from the config row.
    ///
    /// V37 Phase B (seam finding F2): the C4 refusal is a statement that the
    /// server EXISTS, and it must only be made to a consumer that could have
    /// reached the server had it been enabled. A consumer without the grant
    /// never saw these tools and never would have, so it keeps the pre-V37
    /// unknown-tool wording — otherwise the refusal becomes an existence
    /// oracle for servers the caller was never granted.
    pub claude_access: bool,
    /// Expose to the offload worker — see [`Self::claude_access`].
    pub offload_access: bool,
    /// Expose to OpenCode — see [`Self::claude_access`].
    pub opencode_access: bool,
}

/// V37 contract C5 — a hash of the tool surface **each consumer can currently
/// see**, one field per consumer.
///
/// # Not the same question as [`host_config_sig`]
///
/// The two hashes are deliberately separate and must not be merged:
///
/// * [`host_config_sig`] answers *"should I reconcile?"* — it is computed from
///   the DESIRED config (every server row, the categories, the activation maps,
///   the allowed roots) before anything is connected, and `warm_host` compares
///   it to decide whether to do the work at all.
/// * this answers *"did the advertised surface actually move?"* — it is
///   computed from the RESULT (what [`McpHost::advertised`] returns after the
///   disabled filter, the access filter and the health filter) and the service
///   compares it to decide whether a change pulse is worth emitting.
///
/// They disagree constantly, and that is the point. Editing a server's
/// `auth_token` moves the config signature and reconnects, but if the server
/// comes back offering the same tools no consumer's surface moved and no agent
/// needs a `tools/list_changed`. Conversely a server dying mid-call moves no
/// config at all, while its whole namespace vanishes from every surface.
///
/// # Computed from the output, not a parallel predicate
///
/// Each field hashes the sorted `(server, tool)` pairs that
/// [`McpHost::advertised`] produced for that consumer — the same values the
/// consumer's `tools/list` is built from. There is no second copy of the
/// "is this advertised" rule to drift: if the filter changes, this changes with
/// it. Sorted because the warm pool's order is an artefact of connect timing
/// (reconcile appends), and a reordered pool is not a moved surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McpSurfaceFingerprint {
    claude: u64,
    opencode: u64,
    offload: u64,
}

impl McpSurfaceFingerprint {
    /// The fingerprint of a host advertising nothing to anyone — the honest
    /// seed for the pulse gate, which starts life alongside an empty
    /// [`McpHost`]. Computed rather than a hand-written constant, so it stays
    /// equal to `McpHost::new().surface_fingerprint()` whatever
    /// [`surface_digest`] does (a test pins exactly that).
    pub fn empty() -> Self {
        let none: Vec<(String, ToolDef)> = Vec::new();
        McpSurfaceFingerprint {
            claude: surface_digest(&none),
            opencode: surface_digest(&none),
            offload: surface_digest(&none),
        }
    }
}

/// Hash one consumer's advertised surface: the sorted `server` + namespaced
/// tool-name pairs. Names only — a description or schema edit on the same tool
/// is not a surface *membership* change, and V37's propagation is about which
/// tools exist. (Phase E drops screened tools outright, which changes the name
/// set, so it is covered.)
///
/// Non-cryptographic: this only ever answers "did it move", exactly like
/// [`token_fp`].
fn surface_digest(rows: &[(String, ToolDef)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut keys: Vec<(&str, &str)> = rows
        .iter()
        .map(|(server, def)| (server.as_str(), def.function.name.as_str()))
        .collect();
    keys.sort_unstable();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    keys.hash(&mut h);
    h.finish()
}

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

/// Hard cap on remote-authored bytes cImp will re-emit from a failed MCP
/// request — a server's `error.message` or a non-2xx response body.
///
/// #48 M-17: [`jsonrpc_error`] had NO bound at all (it returned `error.message`
/// verbatim) and [`http_error`] had a local `take(300)`. One const for both,
/// because the two are the same population — bytes a server we do not control
/// chose — and a per-site bound is a bound one site can forget.
const MAX_REMOTE_ERROR_CHARS: usize = 200;

/// The note appended when [`HostError::with_remote`] cut the remote half.
const REMOTE_TRUNCATED_NOTE: &str = " …(truncated)";

/// A failed MCP request, split by **who authored which bytes**.
///
/// # Why a type and not a `String`
///
/// #48 M-17: [`jsonrpc_error`]/[`http_error`] interpolated a remote server's
/// `error.message` straight into cImp's own diagnostic, and the resulting
/// `String` reached both models as a tool result with no bound, no spotlighting
/// envelope and no detection pass — while comments at BOTH boundaries
/// (`agent.rs::HostRouter::call`, `loopback.rs::handle_mcp_call`) asserted these
/// were cImp-composed strings. Once the two halves are one `String` no
/// downstream layer can tell them apart, and any marker it could look for is a
/// marker a hostile server can print. So they are never joined until something
/// states which reader it is joining them for.
///
/// [`remote`](Self::remote) is the ONLY way to the remote bytes, and its only
/// caller is `detection::wrap_remote_error`, which envelopes and screens them.
/// [`Display`](std::fmt::Display) renders the HUMAN form (bounded excerpt, no
/// envelope) for the Settings health row and the log lines, which is why it is
/// not the form the model gets.
///
/// Same shape as M-20's `NoteText`: no `Deref`, no `AsRef<str>`, one named
/// accessor per half.
///
/// `pub` only because it appears in the signatures of `McpHost::call*` and
/// `OffloadService::mcp_call`; `offload` is a private module, so the real reach is
/// the crate. The **accessors** stay `pub(super)` — that is the containment that
/// matters, and it is what keeps `remote()`'s caller list to one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    /// cImp's own sentence about what went wrong. Safe to interpolate anywhere.
    diagnostic: String,
    /// Bytes an MCP server we do not control put on the wire, bounded at
    /// [`MAX_REMOTE_ERROR_CHARS`] the moment they were captured. `None` for an
    /// error cImp raised by itself.
    remote: Option<String>,
}

impl HostError {
    /// An error cImp composed entirely itself: no server bytes in it.
    pub(super) fn cimp(diagnostic: impl Into<String>) -> Self {
        HostError {
            diagnostic: diagnostic.into(),
            remote: None,
        }
    }

    /// cImp's diagnostic plus bytes the remote server supplied. `raw` is bounded
    /// HERE, at capture, rather than at render: a 4 MiB `error.message` must not
    /// sit in memory or in a log line waiting for someone to remember.
    fn with_remote(diagnostic: impl Into<String>, raw: &str) -> Self {
        let mut remote: String = raw.chars().take(MAX_REMOTE_ERROR_CHARS).collect();
        if remote.chars().count() < raw.chars().count() {
            remote.push_str(REMOTE_TRUNCATED_NOTE);
        }
        HostError {
            diagnostic: diagnostic.into(),
            remote: Some(remote),
        }
    }

    /// cImp's half. Always safe to place outside an envelope.
    pub(super) fn diagnostic(&self) -> &str {
        &self.diagnostic
    }

    /// The remote bytes, for the ONE caller that envelopes and screens them
    /// (`detection::wrap_remote_error`). Named `remote` rather than `text` so a
    /// new call site cannot claim it did not know what it was holding.
    pub(super) fn remote(&self) -> Option<&str> {
        self.remote.as_deref()
    }
}

/// The HUMAN form: cImp's diagnostic plus a bounded, unenveloped excerpt.
///
/// For the Settings health row and `tracing` — readers who need the server's
/// wording and are not an LLM. **Never the form a model receives**; that is
/// `detection::wrap_remote_error`.
impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.remote {
            Some(r) => write!(f, "{} — server said: {r}", self.diagnostic),
            None => write!(f, "{}", self.diagnostic),
        }
    }
}

// The two `From` impls keep every existing `"…".into()` / `format!(…).into()`
// site compiling unchanged — and every one of them IS cImp-composed, which is
// exactly what `cimp` claims.
impl From<String> for HostError {
    fn from(s: String) -> Self {
        HostError::cimp(s)
    }
}
impl From<&str> for HostError {
    fn from(s: &str) -> Self {
        HostError::cimp(s)
    }
}

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
    pending: StdMutex<HashMap<u64, oneshot::Sender<Result<Value, HostError>>>>,
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
    ) -> Result<Value, HostError> {
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
                return Err(HostError::cimp(format!("write failed: {e}")));
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
                Err(HostError::cimp(format!(
                    "server did not respond within {}s",
                    timeout.as_secs()
                )))
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
        /// V33 Phase E: the configured bearer token, or `None` for none.
        /// Carried on the connection so every later `tools/call` sends it, not
        /// just the handshake. `None` ⇒ no `Authorization` header at all.
        auth_token: Option<String>,
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

    /// The activity-feed `source` badge for this consumer — the same
    /// `claude`/`opencode`/`offload` vocabulary graph entries use.
    ///
    /// `pub(crate)` since V32 Phase G: `OffloadService::mcp_call` keys the
    /// injection scope off the same vocabulary the latch registry uses, and
    /// re-deriving the agent name from anything else would let a tab's latch and
    /// its override row disagree.
    pub(crate) fn source(self) -> &'static str {
        match self {
            Consumer::Claude => "claude",
            Consumer::Offload => "offload",
            Consumer::Opencode => "opencode",
        }
    }

    /// Whether `server` is exposed to this consumer.
    fn wants(self, server: &McpServer) -> bool {
        self.granted(
            server.claude_access,
            server.offload_access,
            server.opencode_access,
        )
    }

    /// V37 F2: whether a *disabled* server would have been exposed to this
    /// consumer. Same three flags, read off the retained
    /// [`DisabledServer`] instead of a live connection — a disabled server has
    /// none. Kept as a second method rather than a generic over both types so
    /// no call site can accidentally ask the question about the wrong one.
    fn wants_disabled(self, server: &DisabledServer) -> bool {
        self.granted(
            server.claude_access,
            server.offload_access,
            server.opencode_access,
        )
    }

    /// The grant test itself, over the three flags in their canonical order.
    /// One body, so `wants` and `wants_disabled` cannot drift apart.
    fn granted(self, claude: bool, offload: bool, opencode: bool) -> bool {
        match self {
            Consumer::Claude => claude,
            Consumer::Offload => offload,
            Consumer::Opencode => opencode,
        }
    }
}

/// The at-a-glance `target` column for an MCP activity row: the argument
/// that best headlines the call. MCP tool schemas are arbitrary, so this is
/// heuristic — try the common primary-argument names first, then fall back
/// to the first string-valued property. Capped so a prompt-sized argument
/// can't blow up the list feed (the full args are in the recorded request).
fn mcp_target(args: &Value) -> String {
    const PREFERRED: [&str; 10] = [
        "query", "url", "path", "file", "prompt", "question", "name", "id", "topic", "text",
    ];
    const CAP: usize = 160;
    let Some(obj) = args.as_object() else {
        return String::new();
    };
    let picked = PREFERRED
        .iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str))
        .or_else(|| obj.values().find_map(Value::as_str))
        .unwrap_or("");
    let mut out: String = picked.chars().take(CAP).collect();
    if picked.chars().count() > CAP {
        out.push('…');
    }
    out
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
    async fn call(&self, raw_name: &str, args: Value) -> Result<String, HostError> {
        let params = json!({ "name": raw_name, "arguments": args });
        let result = match &self.conn {
            Some(Conn::Stdio(c)) => c.request("tools/call", params, REQUEST_TIMEOUT).await,
            Some(Conn::Http {
                url,
                client,
                session_id,
                protocol_version,
                auth_token,
            }) => {
                let current = session_id.lock().unwrap().clone();
                match http_request(
                    client,
                    url,
                    "tools/call",
                    params,
                    HttpHeaders {
                        session_id: current.as_deref(),
                        protocol_version: Some(protocol_version.as_str()),
                        auth_token: auth_token.as_deref(),
                    },
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
                        // #48 M-17: `{e}` is `HostError`'s HUMAN form — bounded,
                        // carrying the server's own wording, and deliberately
                        // unenveloped. This is where the author-split earns its
                        // keep: the Settings health row's reader is a person, and
                        // an envelope there would be noise.
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
    /// V37 C4: the configured-but-disabled servers from the last
    /// [`reconcile`](McpHost::reconcile) — see [`DisabledServer`] for why the
    /// host keeps knowing about a server it deliberately does not connect.
    ///
    /// Lock order: `disabled` before `servers` wherever both are held, so the
    /// read paths can never invert against reconcile's writes.
    disabled: RwLock<Vec<DisabledServer>>,
    allowed_roots: RwLock<Vec<PathBuf>>,
    change_tx: broadcast::Sender<()>,
}

impl McpHost {
    pub fn new() -> Arc<Self> {
        let (change_tx, _) = broadcast::channel(16);
        Arc::new(Self {
            servers: RwLock::new(Vec::new()),
            disabled: RwLock::new(Vec::new()),
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
    ///
    /// V37 contract C3: `categories` + `activation` are the registry context the
    /// effective-enable predicate needs. A server the predicate turns off is
    /// treated as **absent** here — never connected, and torn down if it was
    /// connected when the toggle flipped — while its name and the reason are
    /// remembered in [`Self::disabled`] so dispatch can tell *disabled* from
    /// *unknown* (contract C4).
    ///
    /// `activation` is already the project-composed map (see
    /// [`effective_enable`]); this function never reads an overlay file.
    pub async fn reconcile(
        &self,
        configs: &[McpServerConfig],
        categories: &[McpCategory],
        activation: &McpActivation,
        allowed_roots: &[PathBuf],
    ) {
        *self.allowed_roots.write().await = allowed_roots.to_vec();

        // Record the disabled set BEFORE any teardown, so there is no window in
        // which a call to a just-disabled server sees neither a connection nor
        // a disabled entry and gets the misleading "unknown tool" refusal.
        // A row with no name yet (just added in the editor) is not "disabled",
        // it is incomplete — it never appears here.
        *self.disabled.write().await = configs
            .iter()
            .filter(|c| !c.name.trim().is_empty())
            .filter_map(|c| {
                let verdict = effective_enable(c, categories, activation);
                (!verdict.is_enabled()).then(|| DisabledServer {
                    name: c.name.clone(),
                    verdict,
                    // F2: carried so the refusal can be scoped to the consumers
                    // that would actually have had this server.
                    claude_access: c.claude_access,
                    offload_access: c.offload_access,
                    opencode_access: c.opencode_access,
                })
            })
            .collect();

        let desired: Vec<&McpServerConfig> = configs
            .iter()
            // Skip rows with no endpoint yet (just added in the editor, no
            // command or url typed) — connecting one would route to stdio with
            // an empty command and surface a confusing resolve error.
            .filter(|c| {
                // Connect if any consumer wants it; all off => fully disabled.
                (c.claude_access || c.offload_access || c.opencode_access)
                    // V37 C3: and the registry says it exists at all.
                    && server_enabled(c, categories, activation)
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
    /// access flag — and, since V37 (contract C3), to the effective-enable
    /// predicate's verdict.
    ///
    /// The second filter is belt-and-braces on the warm path: reconcile already
    /// refuses to connect a disabled server, so in a settled host the disabled
    /// set and the connection set are disjoint. It matters in the window
    /// between a toggle landing in [`Self::disabled`] and the teardown
    /// completing — advertisement is a courtesy (contract C4), but a courtesy
    /// that lies for a few milliseconds costs a model one refused call.
    async fn tool_defs_filtered(&self, consumer: Consumer) -> Vec<ToolDef> {
        self.advertised(consumer)
            .await
            .into_iter()
            .map(|(_, def)| def)
            .collect()
    }

    /// The advertised surface for one consumer, each tool paired with the
    /// server that owns it. **The** traversal: [`Self::tool_defs_filtered`]
    /// drops the owner column and [`Self::surface_fingerprint`] hashes it, so
    /// the pulse's notion of "what is advertised" is the output of the same
    /// filter the advertisement itself is, not a second predicate that could
    /// drift from it.
    ///
    /// Ownership comes from the pool, never from splitting the namespaced name
    /// (this file has documented that hazard since V8-03).
    ///
    /// Lock order: `disabled` before `servers` (see [`McpHost::disabled`]).
    async fn advertised(&self, consumer: Consumer) -> Vec<(String, ToolDef)> {
        let disabled = self.disabled.read().await;
        let servers = self.servers.read().await;
        let mut out = Vec::new();
        for s in servers.iter() {
            if consumer.wants(s) && !disabled.iter().any(|d| d.name == s.name) {
                out.extend(s.tool_defs().into_iter().map(|d| (s.name.clone(), d)));
            }
        }
        out
    }

    /// V37 contract C5 — the fingerprint of the surface each consumer can
    /// currently see. See [`McpSurfaceFingerprint`] for why it is a different
    /// hash from [`host_config_sig`].
    pub async fn surface_fingerprint(&self) -> McpSurfaceFingerprint {
        McpSurfaceFingerprint {
            claude: surface_digest(&self.advertised(Consumer::Claude).await),
            opencode: surface_digest(&self.advertised(Consumer::Opencode).await),
            offload: surface_digest(&self.advertised(Consumer::Offload).await),
        }
    }

    /// V37 C4: the disabled server that owns `namespaced` **for `consumer`**,
    /// if any — `None` means "let dispatch continue".
    ///
    /// # A live owner settles it (Phase B seam finding F1)
    ///
    /// The prefix match below is the one place this file deviates from its own
    /// route-by-ownership rule (`call`), and it has to: a disabled server has no
    /// connection, therefore no advertised tool list to match exactly against.
    /// But a prefix is only evidence when nothing better exists, and a LIVE
    /// server that exactly owns the tool is better evidence — `git` disabled and
    /// `git__extra` enabled both prefix-match `git__extra__log`, and the pre-F1
    /// code refused a call that a healthy, enabled server was serving.
    ///
    /// So the dispatch order is:
    ///
    /// 1. a live server exactly owns the tool and is NOT in `disabled` => `None`,
    ///    and the call proceeds to its real owner;
    /// 2. the live owner IS in `disabled` => refuse with ITS verdict. This is
    ///    the toggle-to-teardown window: `reconcile` writes `disabled` before it
    ///    tears connections down, precisely so this window refuses instead of
    ///    dispatching into a server the user just turned off;
    /// 3. no live owner at all => the prefix match, longest name first so a
    ///    `git`/`git__extra` pair resolves deterministically.
    ///
    /// # Scoped to the consumer (Phase B seam finding F2)
    ///
    /// Only servers `consumer` would have been granted are candidates. For any
    /// other consumer the tool falls through to the pre-V37 "not available to X"
    /// wording, which is the truth for it and does not disclose that a server it
    /// was never granted exists.
    ///
    /// Lock order: `disabled` before `servers` (see [`McpHost::disabled`]).
    async fn disabled_owner(
        &self,
        consumer: Consumer,
        namespaced: &str,
    ) -> Option<(String, EnableVerdict)> {
        let disabled = self.disabled.read().await;
        let live_owner = {
            let servers = self.servers.read().await;
            servers
                .iter()
                .find(|s| s.raw_name(namespaced).is_some())
                .map(|s| s.name.clone())
        };
        // Ownership is a routing fact, so the live lookup is NOT grant-filtered:
        // whoever actually serves the tool is who this call is about. The grant
        // filter applies to the refusal below, which is the disclosure.
        if let Some(name) = live_owner {
            return disabled
                .iter()
                .find(|d| d.name == name && consumer.wants_disabled(d))
                .map(|d| (d.name.clone(), d.verdict.clone()));
        }
        disabled
            .iter()
            .filter(|d| consumer.wants_disabled(d))
            .filter(|d| {
                namespaced.len() > d.name.len() + 2
                    && namespaced.starts_with(&d.name)
                    && namespaced[d.name.len()..].starts_with("__")
            })
            .max_by_key(|d| d.name.len())
            .map(|d| (d.name.clone(), d.verdict.clone()))
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
    ///
    /// # V37 contract C4 — enforcement is here
    ///
    /// Advertisement is a courtesy; THIS is the boundary. Propagation to a live
    /// agent is eventually consistent (Claude sees a new surface next turn,
    /// OpenCode on its own refresh), so a call against a stale surface is
    /// normal, not exceptional — and it must come back saying *which toggle*
    /// made the tool vanish, not "no such tool". The disabled check therefore
    /// runs BEFORE the `owns` check: `owns` is computed from live connections,
    /// which a disabled server by definition has none of.
    ///
    /// # What a toggle does to a call already in flight (corrected, F3)
    ///
    /// The DISPATCH DECISION is never re-checked mid-call: this function reads
    /// `disabled` once, before it routes, and a call already inside
    /// [`Self::call`] is never re-evaluated against a newer verdict. That is not
    /// the same as "the call survives". `reconcile` tears the connection down as
    /// part of applying the toggle, and for a stdio server teardown kills the
    /// child process — so an in-flight request on that transport can still come
    /// back as a transport failure (`connection lost: ...`) rather than a
    /// result. HTTP calls, having no warm channel to kill, do run to completion.
    ///
    /// The distinction matters because the wording differs: a call the toggle
    /// aborted mid-transport reports a lost connection, not [`REFUSAL_DISABLED`]
    /// — only the NEXT call gets the honest disabled refusal.
    async fn call_for_consumer(
        &self,
        consumer: Consumer,
        namespaced: &str,
        args: Value,
    ) -> Result<String, HostError> {
        if let Some((name, verdict)) = self.disabled_owner(consumer, namespaced).await {
            // `refusal` yields `None` only for `Enabled`, which
            // `disabled_owner` never returns.
            if let Some(msg) = verdict.refusal(&name) {
                return Err(HostError::cimp(msg));
            }
        }
        let owns = {
            let servers = self.servers.read().await;
            servers
                .iter()
                .any(|s| consumer.wants(s) && s.raw_name(namespaced).is_some())
        };
        if !owns {
            return Err(HostError::cimp(format!(
                "tool `{namespaced}` is not available to {} (no {}-enabled MCP server offers it)",
                consumer.label(),
                consumer.label(),
            )));
        }
        self.call(namespaced, args).await
    }

    /// Route a namespaced `<server>__<tool>` call to its owning server.
    pub async fn call(&self, namespaced: &str, args: Value) -> Result<String, HostError> {
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
            return Err(HostError::cimp(format!(
                "no MCP server owns tool `{namespaced}`"
            )));
        };
        let Some(raw) = server.raw_name(namespaced).map(|s| s.to_string()) else {
            return Err(HostError::cimp(format!(
                "server `{}` no longer offers `{namespaced}`",
                server.name
            )));
        };
        let was_healthy = server.is_healthy();
        let result = server.call(&raw, args).await;
        if was_healthy && !server.is_healthy() {
            self.signal_change(); // a server just went down mid-call
        }
        result
    }

    /// [`call_for_consumer`](Self::call_for_consumer) plus a Tool Activity
    /// record (`kind: "mcp"`, source = the consumer's badge) — the one entry
    /// point every live MCP dispatch goes through: the loopback `/mcp/call`
    /// route (Claude/OpenCode) via `OffloadService::mcp_call`, and the offload
    /// worker's `HostRouter`. Recording here (not in `call`) keeps the
    /// unattributed primitive available without double-recording. `root` is
    /// the calling session's project root when known (`None` ⇒ empty, like
    /// offload runs with no session cwd).
    ///
    /// # V32 Phase C — this is the SSRF chokepoint
    ///
    /// Because both consumers' calls (loopback `/mcp/call` → `mcp_call`) and
    /// the worker's calls (`agent.rs::HostRouter::call`) converge here, and
    /// nowhere earlier, the outbound URL screen ([`outbound::screen_urls`])
    /// lives here — once, rather than re-implemented on each path with the
    /// inevitable drift. `scope` names the contaminated scope for the activity
    /// row; `policy` carries the user's own configured endpoints, the only
    /// carve-outs from the range check.
    ///
    /// The screen runs **before** [`call_for_consumer`](Self::call_for_consumer)
    /// and therefore before any byte leaves the machine: a denied call never
    /// reaches the MCP server, which is the point (the server is a separate
    /// process on another host and would do the fetch for real).
    ///
    /// # The denial's audit row is per-scope, not per denial (#48)
    ///
    /// `audit` is the calling scope's claim ledger. Every denial used to write
    /// an `injection_flag` row with no dedup at all — unlike the budget and
    /// latch-refusal rows, which have had a claim bit since Phase C — and that
    /// feed was one capped window evicted oldest-first *within a kind*. So a
    /// model looping denied URLs did not merely make noise: it evicted the
    /// `Canary`, `LatchBeacon` and `MemoryQuarantine` rows that are the only
    /// forensic record of an attack that got through. `269daf2` made it cheaper
    /// still, by turning 25 previously-allowed call shapes into a denial each.
    ///
    /// #48 finding H-9 moved the cross-screen half of that guarantee into the
    /// store itself (`activity::Lane`: one retention window per [`Screen`], so
    /// no screen's volume can cost another screen its rows). What this ledger
    /// still buys is the SSRF screen's OWN window: without it, denial 1 — the
    /// interesting one — is evicted by denial 65.
    ///
    /// The refusal served to the model is [`outbound::REFUSAL_SSRF`] whatever
    /// the ledger says — locked decision 11 fixes that string precisely so a
    /// caller cannot learn which address it hit, and the claim must not become
    /// a side channel that tells it whether this denial was its first.
    // Each argument is one leg of the chokepoint's contract (who is calling,
    // for which project, under which policy, into which ledger) and is
    // documented above; bundling them into a struct would move the same list
    // one indirection away from the enforcement that reads it.
    #[allow(clippy::too_many_arguments)]
    pub async fn call_recorded(
        &self,
        consumer: Consumer,
        root: Option<&Path>,
        namespaced: &str,
        args: Value,
        scope: &str,
        // #48 F-20: which tab this proxied call belongs to. The comment that used
        // to stand at the row below — "the proxied MCP route knows its consumer
        // but the tab is not threaded to this frame yet" — was the whole finding:
        // the SSRF and detection `injection_flag` rows this same function writes
        // name the tab (they derive it from `scope`), while the row for the call
        // itself said `unattributed`. So the row that answers "which tab fetched
        // that page?" was the one that could not.
        //
        // An `Attribution` rather than an id: the loopback route's id came out of
        // a request body and is classified by `LatchScoping::attribution`; the
        // worker has no tab and says so with `Headless`. Neither caller can pass
        // the other's reading by accident.
        tab: crate::activity::Attribution,
        policy: &outbound::Policy,
        audit: &dyn outbound::ScopeAudit,
    ) -> Result<String, HostError> {
        let started = crate::activity::now_ms();
        let target = mcp_target(&args);
        let request = serde_json::to_string_pretty(&args).unwrap_or_default();
        // Only EXTERNAL calls carry an outbound channel worth screening. Every
        // name routed here is namespaced and therefore EXTERNAL under the
        // Phase A unknown-⇒-EXTERNAL invariant, but the classification is read
        // rather than assumed so a future promotion in the class table cannot
        // leave a stale assumption behind.
        if super::toolclass::classify(namespaced) == super::toolclass::ToolClass::External {
            if let Err(denial) = outbound::screen_urls(&args, policy).await {
                warn!(
                    target: "offload",
                    tool = %namespaced,
                    host = %denial.host,
                    resolved = %denial.ip,
                    "offload: outbound fetch refused by the V32 SSRF screen"
                );
                // The claim is taken on EVERY denial (it is what counts them);
                // only the row is conditional. The enforcement above is
                // untouched by it — see the function docs.
                let row = audit.claim_ssrf();
                if let outbound::DoublingRow::Write { .. } = row {
                    outbound::record_flag(outbound::Flag {
                        screen: outbound::Screen::Ssrf,
                        origin: outbound::Origin::Internal,
                        consumer: consumer.source(),
                        scope,
                        // #48 F-29: derived, because `scope` here is the label
                        // the calling route built from its `LatchScope` (or the
                        // worker's task id) — a real scope, which is what
                        // `scope_attribution` is defined over.
                        attribution: outbound::scope_attribution(scope),
                        session: None,
                        tool: namespaced,
                        host: Some(&denial.host),
                        url: Some(&denial.url),
                        resolved_ip: Some(&denial.ip),
                        canary: false,
                        root: root.map(crate::activity::root_key).unwrap_or_default(),
                        detail: &outbound::ssrf_flag_detail(row),
                    });
                }
                // cImp's own fixed refusal — no remote half, nothing to envelope.
                return Err(HostError::cimp(outbound::REFUSAL_SSRF));
            }
        }
        let result = self.call_for_consumer(consumer, namespaced, args).await;
        crate::activity::record_bg(crate::activity::ActivityRecord {
            entry: crate::activity::ActivityEntry::new(
                crate::activity::ActivityKind::Mcp,
                started,
                root.map(crate::activity::root_key).unwrap_or_default(),
                consumer.source().to_string(),
                namespaced.to_string(),
                target,
                result.as_ref().map(|t| t.chars().count()).unwrap_or(0),
                crate::activity::now_ms().saturating_sub(started),
                result.is_ok(),
                tab,
                None,
            ),
            request,
            response: match &result {
                Ok(text) => text.clone(),
                // The activity detail pane's reader is a HUMAN, so this is
                // `HostError`'s `Display` — bounded, with the server's wording,
                // and deliberately unenveloped (#48 M-17). The MODEL's copy is
                // composed at the two boundaries by
                // `detection::wrap_remote_error`.
                Err(e) => format!("[error] {e}"),
            },
        });
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
    ///
    /// V37 F4: this also clears [`Self::disabled`]. That list is the *last
    /// reconcile's* verdict, and after a shutdown there is no reconcile behind
    /// it — leaving it populated would let a host that is holding nothing keep
    /// answering "server `X` is disabled" for a registry it no longer reflects,
    /// and would shadow a server a later reconcile connects before it rewrites
    /// the list. Lock order: `disabled` before `servers`.
    /// V37 C5: a teardown that actually dropped something is a surface change
    /// like any other, so it pulses. Before this, `warm_host` tearing the host
    /// down (the user cleared the last access flag) emptied every consumer's
    /// surface in silence, and a live child kept advertising tools that now
    /// answer "no MCP server owns tool X". The service's gate suppresses the
    /// pulse if the surface was already empty, so the app-exit call is free.
    pub async fn shutdown(&self) {
        // Lock order: `disabled` before `servers`, and both released before the
        // pulse so the gate's fingerprint read cannot contend with this call.
        let had_disabled = {
            let mut disabled = self.disabled.write().await;
            let had = !disabled.is_empty();
            disabled.clear();
            had
        };
        let had_servers = {
            let mut servers = self.servers.write().await;
            let had = !servers.is_empty();
            for s in servers.drain(..) {
                s.shutdown().await;
            }
            had
        };
        if had_servers || had_disabled {
            self.signal_change();
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
    //
    // V33 Phase E — `auth_token` is connection-relevant and MUST be here. This
    // list is explicit, and it feeds `host_config_sig`, which `warm_host`
    // compares to decide whether to reconnect at all: a token omitted here
    // means the user edits the key in Settings, the signature does not move,
    // no server is reconnected, and the old credential keeps being sent. The
    // edit would appear to do nothing, with no error anywhere.
    //
    // As a FINGERPRINT, not cleartext: unlike `env` (whose values this line has
    // always carried, and which is the stdio transport's own secret channel),
    // there is no reason for the token's plaintext to sit in a `String` held
    // for the process lifetime on every `McpServer`. `token_fp`'s rationale in
    // `service.rs` is the same one — the signature only ever needs to detect
    // *change*.
    // V37: `enabled` and `origin` join the list.
    //
    // `enabled` because it decides membership of reconcile's DESIRED set: a
    // server toggled off must leave the pool, and the only thing that makes
    // `warm_host` call `reconcile` at all is `host_config_sig` moving.
    //
    // `origin` because V37 Phase E screens tool DESCRIPTIONS on `external`
    // servers only, once per connect. If flipping a server to `external` did
    // not re-key the signature, its already-warm connection would keep serving
    // an unscreened surface for the app's lifetime — the same shape of bug the
    // `auth_token` note above records.
    format!(
        "{}|{}|{:?}|{}|{}|{}|{:?}|{}|{}|{:?}",
        c.command,
        c.url,
        c.args,
        c.claude_access,
        c.offload_access,
        c.opencode_access,
        env,
        token_fp(&c.auth_token),
        c.enabled,
        c.origin
    )
}

/// Non-cryptographic fingerprint of a token, for change detection only — never
/// stored, logged or transmitted. Twin of `offload::service::token_fp`; kept
/// local so `config_sig` has no cross-module dependency for one hash.
fn token_fp(token: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut h);
    h.finish()
}

/// A stable signature of the *whole* desired host configuration (every
/// server's connection-relevant config, keyed by name, plus the allowed
/// roots). `warm_host` compares this against the last reconcile so an
/// unchanged config skips the work — and the `host_reconcile_lock` hold —
/// on the per-run hot path.
pub fn host_config_sig(
    configs: &[McpServerConfig],
    categories: &[McpCategory],
    activation: &McpActivation,
    roots: &[PathBuf],
) -> String {
    let mut servers: Vec<String> = configs
        .iter()
        .map(|c| format!("{}={}", c.name, config_sig(c)))
        .collect();
    servers.sort();
    // V37: the registry context is part of the DESIRED host config, because the
    // C3 predicate reads it. A category's membership or its `enabled` flipping
    // changes which servers should be connected without any server row moving,
    // so without this the toggle would be a no-op until something else happened
    // to re-key the signature.
    let mut cats: Vec<String> = categories
        .iter()
        .map(|c| {
            let mut members: Vec<&str> = c.servers.iter().map(String::as_str).collect();
            members.sort_unstable();
            format!("{}={}:{:?}", c.name, c.enabled, members)
        })
        .collect();
    cats.sort();
    // Both activation halves are `BTreeMap`s, so their `Debug` is already in a
    // stable key order — no sort needed, and none must be added later either.
    let act = format!("{:?}/{:?}", activation.categories, activation.servers);
    let mut roots: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
    roots.sort();
    format!("{servers:?}|cats:{cats:?}|act:{act}|roots:{roots:?}")
}

/// A fake healthy server (no real connection) carrying one namespaced tool,
/// for exercising the per-consumer filtering without spawning an MCP server.
/// At module scope rather than inside `mod tests` because `service`'s pulse-gate
/// tests drive a real [`McpHost`] through it — the gate asks the host for a
/// fingerprint, and every other way of populating the pool opens a socket.
#[cfg(test)]
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

#[cfg(test)]
impl McpHost {
    /// Push a [`fake_server`] into the warm pool — the only way a test outside
    /// this module can move the advertised surface.
    pub(super) async fn insert_fake_server(
        &self,
        name: &str,
        claude: bool,
        offload: bool,
        opencode: bool,
        namespaced: &str,
    ) {
        self.servers
            .write()
            .await
            .push(Arc::new(fake_server(name, claude, offload, opencode, namespaced)));
    }
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
    // Through the spawn gate like every other cImp spawn — see `spawn_gate`.
    let mut child = crate::spawn_gate::spawn_tokio(&mut command)
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
                            Err(jsonrpc_error(err))
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
    // #48 M-17: a CONNECT failure is surfaced to a human (the Settings health
    // row), so `HostError`'s `Display` — the bounded, unenveloped form — is the
    // right rendering here. The MODEL never reads a connect error.
    let init_result = conn
        .request("initialize", init, CONNECT_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
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
        .await
        .map_err(|e| e.to_string())?;
    let tools = parse_tools(&cfg.name, &list);
    Ok((Conn::Stdio(conn), tools))
}

/// Connect a Streamable-HTTP MCP server: `initialize` (capturing the assigned
/// session id), the `notifications/initialized` confirmation, then `tools/list`
/// — all carrying the session id. Calls POST per request; no warm channel.
async fn connect_http(cfg: &McpServerConfig) -> Result<(Conn, Vec<HostTool>), String> {
    let url = cfg.url.trim_end_matches('/').to_string();
    // V33 Phase E: read once here and carried on the `Conn` below, so the
    // handshake and every later `tools/call` present the same credential.
    // Empty ⇒ `None` ⇒ no `Authorization` header (pre-V33 behaviour).
    let auth_token = (!cfg.auth_token.is_empty()).then(|| cfg.auth_token.clone());
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
        HttpHeaders {
            session_id: None,
            // The handshake itself predates the negotiation, so it carries no
            // `MCP-Protocol-Version` header — the body's `protocolVersion` is
            // the request.
            protocol_version: None,
            auth_token: auth_token.as_deref(),
        },
        CONNECT_TIMEOUT,
    )
    .await
    // #48 M-17: a connect failure's reader is the Settings health row — a human —
    // so the bounded, unenveloped `Display` form is the right one here.
    .map_err(|e| e.to_string())?;
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
        HttpHeaders {
            session_id: session_id.as_deref(),
            protocol_version: Some(protocol_version.as_str()),
            auth_token: auth_token.as_deref(),
        },
    )
    .await;
    let (list_session, list) = http_request(
        &client,
        &url,
        "tools/list",
        json!({}),
        HttpHeaders {
            session_id: session_id.as_deref(),
            protocol_version: Some(protocol_version.as_str()),
            auth_token: auth_token.as_deref(),
        },
        CONNECT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;
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
            auth_token,
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

/// Render a JSON-RPC `error` object into the failure the host surfaces (health
/// row + tool-call failure). Codes are otherwise opaque, but
/// [`ERR_UNSUPPORTED_REVISION`] is the one a user can act on, so it gets named.
///
/// #48 M-17: the server's `message` is REMOTE-authored — it is carried on
/// [`HostError::remote`], bounded, and never concatenated into the diagnostic.
/// It used to be returned verbatim and unbounded, and it reached both models as
/// a tool result.
fn jsonrpc_error(err: &Value) -> HostError {
    let msg = err.get("message").and_then(|m| m.as_str());
    let diagnostic = match err.get("code").and_then(|c| c.as_i64()) {
        Some(ERR_UNSUPPORTED_REVISION) => {
            format!("{UNSUPPORTED_REVISION_MSG} — JSON-RPC error -32022")
        }
        Some(c) => format!("the MCP server returned JSON-RPC error {c}"),
        None => "the MCP server returned a JSON-RPC error".to_string(),
    };
    match msg {
        // "server error" was the old placeholder for an error object with no
        // `message`. It is cImp's word, not the server's, so the diagnostic above
        // stands alone and carries no remote half.
        None => HostError::cimp(diagnostic),
        Some(m) => HostError::with_remote(diagnostic, m),
    }
}

/// Render a non-2xx response from an MCP endpoint. A modern-only server
/// rejects a legacy handshake with HTTP 400 carrying JSON-RPC
/// [`ERR_UNSUPPORTED_REVISION`]; recognize that shape explicitly, and treat a
/// bare `400` on `initialize` as *possibly* the same cause (some servers send
/// a plain-text 400) instead of leaking an unexplained status code.
///
/// #48 M-17: the response BODY is remote-authored and rides
/// [`HostError::remote`] under the one shared bound, replacing the local
/// `take(300)`.
fn http_error(status: u16, method: &str, body: &str) -> HostError {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.get("code")?.as_i64());
    if code == Some(ERR_UNSUPPORTED_REVISION) {
        // Recognized by CODE, so nothing remote needs re-emitting.
        return HostError::cimp(format!(
            "{UNSUPPORTED_REVISION_MSG} — HTTP {status}, JSON-RPC error -32022"
        ));
    }
    if status == 400 && method == "initialize" {
        return HostError::with_remote(
            format!(
                "MCP handshake rejected with HTTP {status} — possibly the \
                 {UNSUPPORTED_REVISION_MSG}"
            ),
            body,
        );
    }
    HostError::with_remote(format!("http status {status}"), body)
}

/// The three transport headers a Streamable-HTTP call carries beside its
/// JSON-RPC body. A struct rather than three positional parameters because
/// they are all `Option<&str>`: transposing two of them compiles cleanly and
/// fails only against a live server (the same hazard `shadow::Origin::new`
/// documents), and V33 Phase E's `auth_token` took `http_request` over
/// clippy's argument-count bar anyway.
///
/// Every field is "omit the header entirely" when `None`:
/// * `session_id` — a stateless server assigns none; absence is normal.
/// * `protocol_version` — the `initialize` handshake predates negotiation.
/// * `auth_token` — **an empty bearer is worse than no bearer**, so an empty
///   string is treated as absent at the two send sites, and an unauthenticated
///   server keeps seeing exactly the pre-V33 request.
#[derive(Clone, Copy, Default)]
struct HttpHeaders<'a> {
    session_id: Option<&'a str>,
    protocol_version: Option<&'a str>,
    auth_token: Option<&'a str>,
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
///
/// No header value ever reaches an error string: the two failure renderers
/// ([`http_error`] and the transport `map_err` below) carry only the status,
/// the method name and the server's own body, so the bearer token cannot land
/// in a log line, a Settings health row or an activity row.
async fn http_request(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
    headers: HttpHeaders<'_>,
    timeout: Duration,
) -> Result<(Option<String>, Value), HostError> {
    let HttpHeaders {
        session_id,
        protocol_version,
        auth_token,
    } = headers;
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
    if let Some(t) = auth_token.filter(|t| !t.is_empty()) {
        req = req.bearer_auth(t);
    }
    // reqwest's `e` is cImp/reqwest-composed, not server-authored: a transport
    // failure means no server bytes arrived at all.
    let mut resp = req
        .send()
        .await
        .map_err(|e| HostError::cimp(format!("http request failed: {e}")))?;
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
        return Err(http_error(status.as_u16(), method, &body));
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
            .map_err(|e| HostError::cimp(format!("http body read failed: {e}")))?;
        serde_json::from_str::<Value>(&text)
            .map_err(|e| HostError::cimp(format!("http parse failed: {e}")))?
    };
    if let Some(err) = v.get("error") {
        return Err(jsonrpc_error(err));
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
    headers: HttpHeaders<'_>,
) {
    let HttpHeaders {
        session_id,
        protocol_version,
        auth_token,
    } = headers;
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
    // Same rule as `http_request`: a server that requires auth would answer
    // this notification with a 401, and since the result is discarded the
    // failure would be invisible — so it authenticates too.
    if let Some(t) = auth_token.filter(|t| !t.is_empty()) {
        req = req.bearer_auth(t);
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
async fn read_sse_result(resp: &mut reqwest::Response) -> Result<Value, HostError> {
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
                    return Err(HostError::cimp(format!(
                        "SSE response exceeded {MAX_SSE_BYTES} bytes without a complete line"
                    )));
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
            Err(e) => return Err(HostError::cimp(format!("http body read failed: {e}"))),
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
fn decode_jsonrpc_body(content_type: &str, body: &str) -> Result<Value, HostError> {
    if !is_event_stream(content_type) {
        return serde_json::from_str::<Value>(body)
            .map_err(|e| HostError::cimp(format!("http parse failed: {e}")));
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
            .call_for_consumer(Consumer::Claude, "beta__y", json!({}))
            .await
            .unwrap_err();
        assert!(
            err.diagnostic().contains("not available to Claude"),
            "got: {err:?}"
        );
        // #48 M-17: a cImp-composed refusal has no remote half.
        assert_eq!(err.remote(), None);
        // OpenCode must not reach the Claude-only server's tool.
        let err2 = host
            .call_for_consumer(Consumer::Opencode, "alpha__x", json!({}))
            .await
            .unwrap_err();
        assert!(
            err2.diagnostic().contains("not available to OpenCode"),
            "got: {err2:?}"
        );
        // The offload worker must not reach a server without offload_access
        // (the guard `HostRouter` now goes through via `call_recorded`).
        let err3 = host
            .call_for_consumer(Consumer::Offload, "alpha__x", json!({}))
            .await
            .unwrap_err();
        assert!(
            err3.diagnostic().contains("not available to the offload worker"),
            "got: {err3:?}"
        );
    }

    #[test]
    fn mcp_target_prefers_primary_keys_and_caps() {
        // A preferred key wins over other string fields.
        assert_eq!(
            mcp_target(&json!({ "max_results": "5", "query": "rust async traits" })),
            "rust async traits"
        );
        // No preferred key: fall back to the first string value.
        assert_eq!(mcp_target(&json!({ "n": 3, "lang": "rust" })), "rust");
        // Non-object args (or no strings at all) yield an empty target.
        assert_eq!(mcp_target(&json!(null)), "");
        assert_eq!(mcp_target(&json!({ "n": 3 })), "");
        // Oversized values are cut with a marker.
        let long = "x".repeat(500);
        let t = mcp_target(&json!({ "query": long }));
        assert_eq!(t.chars().count(), 161);
        assert!(t.ends_with('…'));
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

    /// #48 M-17 rewrote these five against [`HostError`]'s two halves rather than
    /// one flat string. The diagnostic is cImp's and the `message` is the
    /// server's, and after this pass nothing can join them without saying which
    /// reader it is joining them for.
    #[test]
    fn jsonrpc_error_names_the_unsupported_revision() {
        let err =
            json!({ "code": ERR_UNSUPPORTED_REVISION, "message": "unsupported protocol version" });
        let e = jsonrpc_error(&err);
        assert!(e.diagnostic().contains("newer MCP revision"), "{e:?}");
        assert!(e.diagnostic().contains("-32022"), "{e:?}");
        // The server's own wording is still available — on the remote half, where
        // a downstream layer can see it is the server's.
        assert_eq!(e.remote(), Some("unsupported protocol version"));
        assert!(
            !e.diagnostic().contains("unsupported protocol version"),
            "remote bytes must not be concatenated into cImp's diagnostic: {e:?}"
        );
    }

    #[test]
    fn jsonrpc_error_passes_other_codes_through() {
        // Ordinary tool-level errors keep their message — as the REMOTE half, and
        // the revision hint (which would be misleading here) stays absent.
        let err = json!({ "code": -32602, "message": "invalid params" });
        let e = jsonrpc_error(&err);
        assert_eq!(e.remote(), Some("invalid params"));
        assert!(!e.diagnostic().contains("newer MCP revision"), "{e:?}");
        assert!(e.diagnostic().contains("-32602"), "{e:?}");
        // An error object with no `message` has nothing remote in it at all: the
        // old "server error" placeholder was cImp's word, never the server's.
        let bare = jsonrpc_error(&json!({ "code": -1 }));
        assert_eq!(bare.remote(), None);
        assert!(bare.diagnostic().contains("-1"), "{bare:?}");
    }

    #[test]
    fn http_error_names_the_unsupported_revision() {
        // The modern-only shape: HTTP 400 + JSON-RPC -32022, no fall-forward.
        let body =
            r#"{"jsonrpc":"2.0","error":{"code":-32022,"message":"protocol revision retired"}}"#;
        let e = http_error(400, "initialize", body);
        assert!(e.diagnostic().contains("newer MCP revision"), "{e:?}");
        assert!(e.diagnostic().contains("-32022"), "{e:?}");
        // Recognized by CODE, so nothing remote needs re-emitting at all.
        assert_eq!(e.remote(), None);
        // Recognized by code, not by status — a server using another status
        // for the same refusal is still explained.
        let other = http_error(426, "tools/list", body);
        assert!(other.diagnostic().contains("newer MCP revision"), "{other:?}");
    }

    #[test]
    fn http_error_hints_on_bare_handshake_400_only() {
        // A plain-text 400 on `initialize` gets the "possibly" hint...
        let at_handshake = http_error(400, "initialize", "Bad Request");
        assert!(at_handshake.diagnostic().contains("handshake"));
        assert!(at_handshake.diagnostic().contains("newer MCP revision"));
        // ...and the body is the server's, on the remote half.
        assert_eq!(at_handshake.remote(), Some("Bad Request"));
        // ...but a 400 on a later call, or any other status, stays generic —
        // a missing session id and a bad argument both land here.
        let later = http_error(400, "tools/call", "Missing session ID");
        assert!(!later.diagnostic().contains("newer MCP revision"));
        assert!(later.diagnostic().contains("http status 400"));
        assert_eq!(later.remote(), Some("Missing session ID"));
        let five_oh_three = http_error(503, "initialize", "upstream down");
        assert!(!five_oh_three.diagnostic().contains("newer MCP revision"));
        assert!(five_oh_three.diagnostic().contains("http status 503"));
    }

    /// #48 M-17: the bound is at CAPTURE and covers both producers. The JSON-RPC
    /// half had none at all — `error.message` was returned verbatim — so a hostile
    /// server had an unbounded channel into both models.
    #[test]
    fn remote_error_text_is_bounded_at_capture_on_both_producers() {
        let long = "x".repeat(10_000);
        let e = jsonrpc_error(&json!({ "code": -1, "message": long.clone() }));
        let remote = e.remote().expect("the server's message is remote-authored");
        assert!(
            remote.chars().count()
                <= MAX_REMOTE_ERROR_CHARS + REMOTE_TRUNCATED_NOTE.chars().count(),
            "got {} chars",
            remote.chars().count()
        );
        assert!(remote.ends_with(REMOTE_TRUNCATED_NOTE), "{remote}");
        // cImp's own diagnostic is never inside the remote half.
        assert!(!remote.contains("JSON-RPC error"));

        let h = http_error(500, "tools/call", &long);
        let hr = h.remote().expect("the body is remote-authored");
        assert!(
            hr.chars().count() <= MAX_REMOTE_ERROR_CHARS + REMOTE_TRUNCATED_NOTE.chars().count()
        );
        assert!(hr.ends_with(REMOTE_TRUNCATED_NOTE), "{hr}");

        // A short message is not decorated — the note means something.
        let short = jsonrpc_error(&json!({ "code": -1, "message": "nope" }));
        assert_eq!(short.remote(), Some("nope"));
    }

    /// #48 M-17: there is NO way to a `String` containing remote bytes except
    /// [`HostError::remote`], whose one caller envelopes them. `Display` is the
    /// HUMAN form: bounded, carrying the server's wording, and unenveloped — the
    /// Settings health row's reader is a person, not a model.
    #[test]
    fn the_human_form_is_bounded_and_carries_no_envelope() {
        let long = "y".repeat(5_000);
        let e = http_error(500, "tools/call", &long);
        let shown = e.to_string();
        assert!(shown.starts_with("http status 500"), "{shown}");
        assert!(shown.contains("server said:"), "{shown}");
        assert!(
            shown.chars().count() < 400,
            "the human form is bounded too, got {}",
            shown.chars().count()
        );
        // No envelope, no preamble, no markers: those are the model's form.
        assert!(!shown.contains("UNTRUSTED-DATA"), "{shown}");
        assert!(
            !shown.contains(crate::offload::spotlight::REMOTE_ERROR_PREAMBLE),
            "{shown}"
        );
    }

    /// An error cImp raised itself has no remote half, so the boundary passes it
    /// through untouched — the property the old comments claimed for ALL errors.
    #[test]
    fn a_cimp_composed_error_has_no_remote_half() {
        for e in [
            HostError::cimp("server is not connected"),
            HostError::from("write/flush failed".to_string()),
            HostError::from(outbound::REFUSAL_SSRF),
            jsonrpc_error(&json!({ "code": ERR_UNSUPPORTED_REVISION })),
        ] {
            assert_eq!(e.remote(), None, "{e:?}");
            // With no remote half the human form IS the diagnostic, unadorned.
            assert_eq!(e.to_string(), e.diagnostic());
        }
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
        let none: Vec<McpCategory> = Vec::new();
        let act = McpActivation::default();
        let s1 = host_config_sig(std::slice::from_ref(&a), &none, &act, &roots);
        // Stable for identical input (the warm_host skip relies on this).
        assert_eq!(
            s1,
            host_config_sig(std::slice::from_ref(&a), &none, &act, &roots)
        );
        // Changes when a server field changes (access toggle, url, …).
        let b = McpServerConfig {
            offload_access: false,
            ..a.clone()
        };
        assert_ne!(
            s1,
            host_config_sig(std::slice::from_ref(&b), &none, &act, &roots)
        );
        // Changes when the allowed roots change.
        assert_ne!(
            s1,
            host_config_sig(
                std::slice::from_ref(&a),
                &none,
                &act,
                &[PathBuf::from("/other")]
            )
        );
    }

    /// V37 contract C3/C5: `warm_host` only calls `reconcile` when
    /// `host_config_sig` moves, so every registry input that changes the
    /// DESIRED set must be in it. Three inputs are new in v32 and none of them
    /// touches a server row: the server's own `enabled`, a category's
    /// membership or `enabled`, and an activation entry. A missing one is a
    /// toggle that silently does nothing until an unrelated edit re-keys the
    /// signature.
    #[test]
    fn host_config_sig_moves_on_every_registry_change() {
        let srv = McpServerConfig {
            name: "ddg".into(),
            url: "http://x/mcp".into(),
            offload_access: true,
            ..Default::default()
        };
        let roots = vec![PathBuf::from("/work")];
        let none: Vec<McpCategory> = Vec::new();
        let act = McpActivation::default();
        let base = host_config_sig(std::slice::from_ref(&srv), &none, &act, &roots);

        // a) the server's own toggle.
        let off = McpServerConfig {
            enabled: false,
            ..srv.clone()
        };
        assert_ne!(
            base,
            host_config_sig(std::slice::from_ref(&off), &none, &act, &roots)
        );

        // b) a category appears, then loses the member, then flips enabled.
        let cat = |members: &[&str], enabled: bool| {
            vec![McpCategory {
                name: "research".into(),
                servers: members.iter().map(|s| (*s).to_string()).collect(),
                enabled,
            }]
        };
        let with_cat = host_config_sig(
            std::slice::from_ref(&srv),
            &cat(&["ddg"], true),
            &act,
            &roots,
        );
        assert_ne!(base, with_cat);
        assert_ne!(
            with_cat,
            host_config_sig(std::slice::from_ref(&srv), &cat(&[], true), &act, &roots),
            "membership change must move the signature"
        );
        assert_ne!(
            with_cat,
            host_config_sig(
                std::slice::from_ref(&srv),
                &cat(&["ddg"], false),
                &act,
                &roots
            ),
            "category enabled flip must move the signature"
        );

        // c) an activation entry at either level.
        let mut server_override = McpActivation::default();
        server_override.servers.insert("ddg".into(), false);
        assert_ne!(
            base,
            host_config_sig(std::slice::from_ref(&srv), &none, &server_override, &roots)
        );
        let mut cat_override = McpActivation::default();
        cat_override.categories.insert("research".into(), false);
        assert_ne!(
            with_cat,
            host_config_sig(
                std::slice::from_ref(&srv),
                &cat(&["ddg"], true),
                &cat_override,
                &roots
            )
        );
    }

    /// **V33 Phase E, the trap.** `config_sig` lists its fields explicitly and
    /// feeds `host_config_sig`, which `warm_host` compares to decide whether to
    /// reconnect. A token missing from that list means editing the key in
    /// Settings changes nothing observable: no reconnect, the stale credential
    /// keeps going out, and there is no error to notice.
    #[test]
    fn an_auth_token_change_moves_the_host_config_sig() {
        let none = McpServerConfig {
            name: "ddg".into(),
            url: "http://172.21.1.11:17201/mcp".into(),
            offload_access: true,
            ..Default::default()
        };
        let roots = vec![PathBuf::from("/work")];
        let sig = |c: &McpServerConfig| {
            host_config_sig(
                std::slice::from_ref(c),
                &[],
                &McpActivation::default(),
                &roots,
            )
        };

        let first = McpServerConfig {
            auth_token: "sk-first".into(),
            ..none.clone()
        };
        let rotated = McpServerConfig {
            auth_token: "sk-second".into(),
            ..none.clone()
        };
        // Adding a token, and rotating one non-empty value to another (the
        // real-world key-rotation case), both move the signature.
        assert_ne!(sig(&none), sig(&first));
        assert_ne!(sig(&first), sig(&rotated));
        // …and it is still stable for identical input.
        assert_eq!(sig(&first), sig(&first.clone()));

        // The signature carries a fingerprint, never the secret itself — it is
        // held per-server for the process lifetime.
        assert!(!sig(&first).contains("sk-first"), "{}", sig(&first));
    }

    // --- V37 Phase A: registry, activation, enforcement --------------------

    fn cfg(name: &str, enabled: bool) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            url: "http://x/mcp".into(),
            offload_access: true,
            enabled,
            ..Default::default()
        }
    }

    /// A disabled-server record granted to EVERY consumer — the default for
    /// tests that are not about the F2 grant scoping.
    fn disabled(name: &str, verdict: EnableVerdict) -> DisabledServer {
        DisabledServer {
            name: name.into(),
            verdict,
            claude_access: true,
            offload_access: true,
            opencode_access: true,
        }
    }

    fn category(name: &str, enabled: bool, servers: &[&str]) -> McpCategory {
        McpCategory {
            name: name.into(),
            servers: servers.iter().map(|s| (*s).to_string()).collect(),
            enabled,
        }
    }

    fn activation(cats: &[(&str, bool)], servers: &[(&str, bool)]) -> McpActivation {
        let mut a = McpActivation::default();
        for (k, v) in cats {
            a.categories.insert((*k).to_string(), *v);
        }
        for (k, v) in servers {
            a.servers.insert((*k).to_string(), *v);
        }
        a
    }

    /// The contract-C3 truth table, in full. This predicate is the single owner
    /// of "does this server exist right now" for BOTH advertisement and
    /// dispatch, so every row here is a behaviour two call sites share.
    #[test]
    fn effective_enable_truth_table() {
        let no_cats: Vec<McpCategory> = Vec::new();
        let neutral = McpActivation::default();

        // Uncategorized: the server toggle is the whole rule.
        assert_eq!(
            effective_enable(&cfg("ddg", true), &no_cats, &neutral),
            EnableVerdict::Enabled
        );
        assert_eq!(
            effective_enable(&cfg("ddg", false), &no_cats, &neutral),
            EnableVerdict::ServerOff
        );

        // One category, server toggle on.
        let on = vec![category("research", true, &["ddg"])];
        let off = vec![category("research", false, &["ddg"])];
        assert_eq!(
            effective_enable(&cfg("ddg", true), &on, &neutral),
            EnableVerdict::Enabled
        );
        assert_eq!(
            effective_enable(&cfg("ddg", true), &off, &neutral),
            EnableVerdict::CategoriesOff("research".into())
        );
        // The server toggle wins outright: an ON category cannot resurrect it,
        // and the verdict names the SERVER level, not the category.
        assert_eq!(
            effective_enable(&cfg("ddg", false), &on, &neutral),
            EnableVerdict::ServerOff
        );

        // A category that does not contain the server is irrelevant.
        let elsewhere = vec![category("web", false, &["fetch"])];
        assert_eq!(
            effective_enable(&cfg("ddg", true), &elsewhere, &neutral),
            EnableVerdict::Enabled
        );

        // Multi-category: categories OR, they do not AND.
        let one_on = vec![
            category("research", false, &["ddg"]),
            category("web", true, &["ddg"]),
        ];
        assert_eq!(
            effective_enable(&cfg("ddg", true), &one_on, &neutral),
            EnableVerdict::Enabled
        );
        let all_off = vec![
            category("research", false, &["ddg"]),
            category("web", false, &["ddg"]),
        ];
        assert_eq!(
            effective_enable(&cfg("ddg", true), &all_off, &neutral),
            // The FIRST containing category in registry order, deterministically.
            EnableVerdict::CategoriesOff("research".into())
        );
    }

    /// The overlay half of C3, in both directions and at both levels. The maps
    /// reaching this function are already project-composed, so an entry is an
    /// override of the global flag — never a copy of it.
    #[test]
    fn activation_overrides_both_levels_in_both_directions() {
        let no_cats: Vec<McpCategory> = Vec::new();

        // Server level: overlay turns a globally-ON server off …
        assert_eq!(
            effective_enable(
                &cfg("ddg", true),
                &no_cats,
                &activation(&[], &[("ddg", false)])
            ),
            EnableVerdict::ServerOff
        );
        // … and a globally-OFF server on.
        assert_eq!(
            effective_enable(
                &cfg("ddg", false),
                &no_cats,
                &activation(&[], &[("ddg", true)])
            ),
            EnableVerdict::Enabled
        );

        // Category level: overlay turns a globally-ON category off …
        let on = vec![category("research", true, &["ddg"])];
        assert_eq!(
            effective_enable(
                &cfg("ddg", true),
                &on,
                &activation(&[("research", false)], &[])
            ),
            EnableVerdict::CategoriesOff("research".into())
        );
        // … and a globally-OFF category on.
        let off = vec![category("research", false, &["ddg"])];
        assert_eq!(
            effective_enable(
                &cfg("ddg", true),
                &off,
                &activation(&[("research", true)], &[])
            ),
            EnableVerdict::Enabled
        );

        // An entry naming something that does not exist is inert, not fatal —
        // a renamed server/category leaves stale overlay keys behind (C1).
        assert_eq!(
            effective_enable(
                &cfg("ddg", true),
                &on,
                &activation(&[("gone", false)], &[("also-gone", false)])
            ),
            EnableVerdict::Enabled
        );
    }

    /// V37 C3: a disabled server is not advertised to ANY consumer. This is the
    /// courtesy half; `call_for_consumer` below is the enforcement half.
    #[tokio::test]
    async fn tool_defs_exclude_disabled_servers() {
        let host = McpHost::new();
        host.servers.write().await.extend([
            Arc::new(fake_server("alpha", true, true, true, "alpha__x")),
            Arc::new(fake_server("beta", true, true, true, "beta__y")),
        ]);
        // `beta` is off at the category level.
        *host.disabled.write().await =
            vec![disabled("beta", EnableVerdict::CategoriesOff("research".into()))];

        for defs in [
            host.tool_defs_for_claude().await,
            host.tool_defs_for_offload().await,
            host.tool_defs_for_opencode().await,
        ] {
            assert_eq!(defs.len(), 1, "got: {defs:?}");
            assert_eq!(defs[0].function.name, "alpha__x");
        }
    }

    /// V37 contract C4. The refusal must name the disabled state AND the level,
    /// because "you turned the server off" and "the category it is in is off"
    /// are different fixes — and it must stay distinguishable from the
    /// unknown-tool refusal, which is what tells the user whether a toggle is
    /// even the cause.
    #[tokio::test]
    async fn call_for_consumer_refuses_disabled_servers_by_level() {
        let host = McpHost::new();
        // `alpha` stays live so the unknown-tool path is still reachable.
        host.servers
            .write()
            .await
            .push(Arc::new(fake_server("alpha", true, true, true, "alpha__x")));
        *host.disabled.write().await = vec![
            disabled("beta", EnableVerdict::ServerOff),
            disabled("gamma", EnableVerdict::CategoriesOff("research".into())),
        ];

        let err = host
            .call_for_consumer(Consumer::Claude, "beta__y", json!({}))
            .await
            .unwrap_err();
        let d = err.diagnostic();
        assert!(d.contains(REFUSAL_DISABLED), "got: {d}");
        assert!(d.contains(REFUSAL_DISABLED_BY_SERVER), "got: {d}");
        assert!(d.contains("beta"), "got: {d}");
        // #48 M-17: a cImp-composed refusal has no remote half.
        assert_eq!(err.remote(), None);

        let err = host
            .call_for_consumer(Consumer::Opencode, "gamma__z", json!({}))
            .await
            .unwrap_err();
        let d = err.diagnostic();
        assert!(d.contains(REFUSAL_DISABLED), "got: {d}");
        assert!(d.contains(REFUSAL_DISABLED_BY_CATEGORY), "got: {d}");
        assert!(d.contains("research"), "got: {d}");
        assert!(!d.contains(REFUSAL_DISABLED_BY_SERVER), "got: {d}");

        // Disabled != unknown: a tool from no configured server at all keeps
        // the pre-V37 wording, with no claim about a toggle.
        let err = host
            .call_for_consumer(Consumer::Claude, "nowhere__t", json!({}))
            .await
            .unwrap_err();
        let d = err.diagnostic();
        assert!(d.contains("not available to Claude"), "got: {d}");
        assert!(!d.contains(REFUSAL_DISABLED), "got: {d}");

        // An enabled, granted server is unaffected by the new check. (It fails
        // later, on the absent connection — the point is only that the disabled
        // refusal did not fire.)
        let err = host
            .call_for_consumer(Consumer::Claude, "alpha__x", json!({}))
            .await
            .unwrap_err();
        assert!(!err.diagnostic().contains(REFUSAL_DISABLED));
    }

    /// The prefix match `disabled_owner` uses is the one deviation from this
    /// file's route-by-ownership rule (a disabled server has no tool list to
    /// match against), so it must be deterministic: longest name wins, and a
    /// name that is only a prefix of a DIFFERENT server's namespace does not
    /// steal the refusal.
    #[tokio::test]
    async fn disabled_owner_prefers_the_longest_namespace_match() {
        let host = McpHost::new();
        *host.disabled.write().await = vec![
            disabled("git", EnableVerdict::ServerOff),
            disabled("git__extra", EnableVerdict::CategoriesOff("vcs".into())),
        ];
        let owner = |t: &'static str| host.disabled_owner(Consumer::Claude, t);
        assert_eq!(owner("git__extra__log").await.unwrap().0, "git__extra");
        assert_eq!(owner("git__log").await.unwrap().0, "git");
        // The separator is required: `github__x` is not `git`'s.
        assert!(owner("github__x").await.is_none());
        // A bare server name with no tool half owns nothing.
        assert!(owner("git").await.is_none());
        assert!(owner("git__").await.is_none());
    }

    /// V37 C3: reconcile must treat a disabled server as ABSENT — never
    /// connected — while still recording it, so dispatch keeps the level
    /// information. (No endpoint is reachable in a unit test, so this asserts
    /// the bookkeeping, which is the part with the contract on it.)
    #[tokio::test]
    async fn reconcile_records_disabled_servers_and_never_connects_them() {
        let host = McpHost::new();
        let servers = vec![
            cfg("ddg", true),
            cfg("beta", false),
            cfg("gamma", true),
            // A half-typed row: no name yet. Not "disabled" — incomplete.
            McpServerConfig {
                name: "  ".into(),
                ..cfg("", false)
            },
        ];
        let cats = vec![category("research", false, &["gamma"])];
        host.reconcile(&servers, &cats, &McpActivation::default(), &[])
            .await;

        let disabled = host.disabled.read().await;
        assert_eq!(disabled.len(), 2, "got: {disabled:?}");
        assert_eq!(disabled[0].name, "beta");
        assert_eq!(disabled[0].verdict, EnableVerdict::ServerOff);
        assert_eq!(disabled[1].name, "gamma");
        assert_eq!(
            disabled[1].verdict,
            EnableVerdict::CategoriesOff("research".into())
        );
        // F2: the grants ride along, so the refusal can be scoped to the
        // consumers that would have had the server (`cfg` sets offload only).
        assert!(disabled[0].offload_access);
        assert!(!disabled[0].claude_access);
        assert!(!disabled[0].opencode_access);
        drop(disabled);

        // Nothing disabled ended up in the pool.
        let pool = host.servers.read().await;
        assert!(pool.iter().all(|s| s.name != "beta" && s.name != "gamma"));
    }
    // ── V37 Phase B ──────────────────────────────────────────────────────────

    /// F1: a LIVE server that exactly owns the tool outranks a disabled server
    /// that merely prefixes it. Before this, `git` being off refused every
    /// `git__extra__*` call that the enabled `git__extra` server was serving —
    /// a namespace collision turning one toggle into an outage for another
    /// server.
    #[tokio::test]
    async fn live_owner_outranks_a_disabled_prefix() {
        let host = McpHost::new();
        host.servers
            .write()
            .await
            .push(Arc::new(fake_server(
                "git__extra",
                true,
                true,
                true,
                "git__extra__log",
            )));
        *host.disabled.write().await = vec![disabled("git", EnableVerdict::ServerOff)];

        // The live owner settles it: no refusal, dispatch continues.
        assert!(host
            .disabled_owner(Consumer::Claude, "git__extra__log")
            .await
            .is_none());
        let err = host
            .call_for_consumer(Consumer::Claude, "git__extra__log", json!({}))
            .await
            .unwrap_err();
        assert!(
            !err.diagnostic().contains(REFUSAL_DISABLED),
            "an enabled server's tool must not be refused as disabled: {err}"
        );

        // A tool with NO live owner still falls back to the prefix match.
        assert_eq!(
            host.disabled_owner(Consumer::Claude, "git__log")
                .await
                .unwrap()
                .0,
            "git"
        );

        // The toggle-to-teardown window stays closed: `reconcile` writes
        // `disabled` before it tears connections down, so a live connection
        // whose OWN name is disabled must still refuse.
        *host.disabled.write().await = vec![disabled("git__extra", EnableVerdict::ServerOff)];
        let err = host
            .call_for_consumer(Consumer::Claude, "git__extra__log", json!({}))
            .await
            .unwrap_err();
        assert!(err.diagnostic().contains(REFUSAL_DISABLED), "got: {err}");
        assert!(err.diagnostic().contains("git__extra"), "got: {err}");
    }

    /// F2: the C4 refusal states that a server EXISTS, so it is only served to
    /// consumers that would have been granted that server. Everyone else keeps
    /// the pre-V37 unknown-tool wording.
    #[tokio::test]
    async fn disabled_refusal_only_reaches_granted_consumers() {
        let host = McpHost::new();
        // Disabled, and only ever exposed to OpenCode.
        *host.disabled.write().await = vec![DisabledServer {
            name: "beta".into(),
            verdict: EnableVerdict::ServerOff,
            claude_access: false,
            offload_access: false,
            opencode_access: true,
        }];

        let err = host
            .call_for_consumer(Consumer::Opencode, "beta__y", json!({}))
            .await
            .unwrap_err();
        assert!(err.diagnostic().contains(REFUSAL_DISABLED), "got: {err}");

        for consumer in [Consumer::Claude, Consumer::Offload] {
            let err = host
                .call_for_consumer(consumer, "beta__y", json!({}))
                .await
                .unwrap_err();
            let d = err.diagnostic();
            assert!(
                !d.contains(REFUSAL_DISABLED),
                "an ungranted consumer must not learn `beta` exists: {d}"
            );
            assert!(d.contains("is not available to"), "got: {d}");
        }
    }

    /// F4: `shutdown` holds nothing, so it must claim nothing. A stale
    /// `disabled` list would keep refusing on behalf of a registry the host no
    /// longer reflects.
    #[tokio::test]
    async fn shutdown_clears_the_disabled_set() {
        let host = McpHost::new();
        *host.disabled.write().await = vec![disabled("beta", EnableVerdict::ServerOff)];
        host.shutdown().await;
        assert!(host.disabled.read().await.is_empty());
        let err = host
            .call_for_consumer(Consumer::Claude, "beta__y", json!({}))
            .await
            .unwrap_err();
        assert!(!err.diagnostic().contains(REFUSAL_DISABLED), "got: {err}");
    }

    /// C5: the fingerprint is per consumer and tracks the OUTPUT of the same
    /// filter advertisement uses — access flags, the disabled set and health all
    /// move it, and only for the consumers actually affected.
    #[tokio::test]
    async fn surface_fingerprint_tracks_each_consumer_separately() {
        let host = McpHost::new();
        assert_eq!(
            host.surface_fingerprint().await,
            McpSurfaceFingerprint::empty(),
            "an empty host must equal the seed the pulse gate starts from"
        );

        // Claude-only server: Claude's surface moves, the other two do not.
        host.insert_fake_server("alpha", true, false, false, "alpha__x")
            .await;
        let one = host.surface_fingerprint().await;
        let seed = McpSurfaceFingerprint::empty();
        assert_ne!(one.claude, seed.claude);
        assert_eq!(one.offload, seed.offload);
        assert_eq!(one.opencode, seed.opencode);

        // Recomputing an unchanged host is stable (a pulse would be suppressed).
        assert_eq!(host.surface_fingerprint().await, one);

        // Disabling it takes Claude's surface back to empty.
        *host.disabled.write().await = vec![disabled("alpha", EnableVerdict::ServerOff)];
        assert_eq!(host.surface_fingerprint().await, seed);
    }

    /// Pool ORDER is an artefact of connect timing (reconcile appends as
    /// connections complete). Two hosts advertising the same tools must
    /// fingerprint identically, or every reconnect would emit a spurious pulse.
    #[tokio::test]
    async fn surface_fingerprint_ignores_pool_order() {
        let a = McpHost::new();
        a.insert_fake_server("alpha", true, true, true, "alpha__x")
            .await;
        a.insert_fake_server("beta", true, true, true, "beta__y")
            .await;
        let b = McpHost::new();
        b.insert_fake_server("beta", true, true, true, "beta__y")
            .await;
        b.insert_fake_server("alpha", true, true, true, "alpha__x")
            .await;
        assert_eq!(a.surface_fingerprint().await, b.surface_fingerprint().await);
    }
}

//! V8-01 local task offload.
//!
//! Lets the main cloud Opus session hand a self-contained subtask to a
//! local LLM (`llama-server` serving Qwen3.6-35B-A3B) via an
//! `offload_task` MCP tool and get back only the synthesized result —
//! the token-heavy churning (searching, reading, summarizing,
//! web-fetching) stays local and Opus's context grows by a paragraph
//! instead of a megabyte.
//!
//! Layers:
//! - [`server`] — the `LlamaServer` supervisor: the single Local
//!   [`Backend`] impl. Parses the user's `server_command`, tracks HTTP
//!   health + the discovered context window, and gates concurrent loops
//!   to the server's slot count. The process itself is owned by the
//!   supervisor (surfaced in the read-only Offload server dashboard,
//!   Tool Activity tab).
//! - `agent` (Phase B) — the OpenAI-compatible agent loop.
//! - `tools` (Phase B/C) — native baseline tools (`read_file`,
//!   `code_search`, `run_command`).
//! - `mcp` (Phase B) — the stdio JSON-RPC MCP server toward Claude
//!   (`cimp --offload-mcp`).
//! - `mcp_host` (Phase C) — the MCP client aggregating the user's tool
//!   servers.
//! - [`harness_tab`] (V39 Phase C) — the facade [`Backend`]: an open AI tab
//!   driven by the delegation engine, which the router cannot tell from a
//!   LAN server.
//! - [`toolclass`] (V32 Phase A) — the tool-class taxonomy + taint latch:
//!   the single source of truth deciding which tools a contaminated task or
//!   session may still reach.
//! - [`spotlight`] (V32 Phase B) — the nonced data-not-instructions envelope
//!   wrapped around every EXTERNAL tool result, at both the worker's and the
//!   proxy's tool-result boundary.
//! - [`outbound`] (V32 Phase C) — the SSRF, fetch-budget and canary screens on
//!   what an EXTERNAL call sends *out*.
//! - [`detection`] (V32 Phase C) — the YARA signature + Prompt Guard classifier
//!   screens on what an EXTERNAL call brings *back*, composing the warning
//!   header with [`spotlight`]'s envelope at the same two boundaries.
//!
//! The whole stack sits behind the minimal [`Backend`] seam (one Local
//! impl today) so V8-02 can add remote/cloud backends + capability-aware
//! routing without re-architecting the loop.

pub mod agent;
pub mod backend_gate;
pub mod detection;
pub mod discovery;
pub mod harness_tab;
pub mod host;
pub mod latch;
pub mod loopback;
pub mod mcp;
pub mod mcp_host;
pub mod metrics;
pub mod openai;
pub mod outbound;
pub mod remote;
pub mod router;
pub mod server;
pub mod service;
pub mod spotlight;
pub mod supervisor;
pub mod toolclass;
pub mod tools;

pub use remote::RemoteBackend;
pub use service::OffloadService;
pub use supervisor::{OffloadState, OffloadSupervisor};

use crate::settings::{BackendTier, ToolScope};

/// Minimal seam over an offload model endpoint. V8-01 has one impl
/// ([`LlamaServer`], a local `llama-server`); V8-02 adds Remote/cloud
/// impls and a router that selects one [`Backend`] per task. The
/// accessors are sync reads (state lives behind atomics) so the router
/// and agent loop can poll them without `.await`.
///
/// Lifecycle (spawn/restart) is intentionally *not* on the trait — only
/// a Local backend owns a process, so it stays inherent to
/// [`LlamaServer`]. This matches V8-02's planned trait surface.
///
/// `name`/`tier` round out the seam for the warm-pool target design (the
/// router currently reads these from config via [`router::BackendView`]),
/// so they're allowed to be unused today.
pub trait Backend: Send + Sync {
    /// Stable display/routing name (`main`, `lan-3070`, `cloud`).
    fn name(&self) -> &str;
    /// HTTP origin to reach the server, e.g. `http://127.0.0.1:8080`
    /// (callers append `/health`, `/props`, `/v1/chat/completions`).
    fn base_url(&self) -> String;
    /// Whether the last health check observed a ready server.
    fn is_ready(&self) -> bool;
    /// The authoritative context window discovered from `/props`
    /// (`n_ctx`), or the configured `declared_context` for endpoints that
    /// don't expose `/props`, or `None` before the first successful probe.
    ///
    /// Always **per slot** — what one in-flight request may occupy. Split-KV
    /// llama-servers report that directly; a `--kv-unified` one reports the
    /// shared window, which the impl divides by its slot count before
    /// answering here (see
    /// [`server::per_slot_n_ctx`](server::per_slot_n_ctx)).
    fn n_ctx(&self) -> Option<u32>;
    /// Parallel slots (`-np`/`--parallel`). The window divides across
    /// these, so each in-flight request gets `n_ctx / slots` tokens.
    fn slots(&self) -> u32;
    /// Offload loops currently holding a slot.
    fn in_flight(&self) -> u32;
    /// Which capability tier this backend serves (router bias).
    fn tier(&self) -> BackendTier;
    /// This backend's allow-list over the global tool pool — the surface
    /// of tools that may be placed in the `tools` array sent to its model.
    fn tool_scope(&self) -> &ToolScope;
}

// -- Helpers the whole subsystem shares --------------------------------------
//
// V42 review (dropped-at-cap): both of these lived in `offload::loopback` and
// were imported by `offload::latch` — a BACK-EDGE, since V42 R3 (#114) pulled
// latch OUT of loopback precisely so the containment state machine would not be
// a routing concern. Neither helper is about routing: one bounds a
// caller-supplied string, the other turns an `AppHandle` into a `Settings`.
// They belong to the module above both, so `latch` depends on its parent and
// nothing depends sideways. `loopback` re-exports them for the `use super::*`
// its family files do.

/// The upper bound on a caller-supplied tool name before it reaches an activity
/// row, a log line or the TTS surface (#48). Long enough for every real
/// harness tool name (`WebFetch`, `websearch`) with room to spare.
pub(crate) const BEACON_TOOL_MAX: usize = 64;

/// One caller-supplied identifier, bounded before it reaches an activity row —
/// the truncation half of `bounded_tool`, shared rather than re-spelled.
///
/// Its second caller is `record_discovery_skipped`'s `Unrecognized` arm (#48
/// F-32): a tab id that names no configured tab is an arbitrary unbounded string
/// from a request body, and putting it in a row verbatim would let a caller
/// choose how many bytes of a capped feed one report occupies. **Only ever
/// applied AFTER classification** — truncating first could fold a long invented
/// id onto a configured one, which would turn a bound into a forgery primitive.
///
/// Its third and fourth callers are #48 F-39 and F-37 (locked decision 42), the
/// same string half of the same class: `LatchScoping::attribution`'s
/// `Unrecognized` arm — reached by `/graph_run` and `/mcp/call`, and likewise
/// only after `latch_scope` classified the full id — and `contract_drift_row`,
/// where the shim name and the session id a hook shim reports are both
/// arbitrary strings that reach a row.
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

/// V32 Phase G: the app's live settings — **still the one point**, now over an
/// injected handle.
///
/// The property this exists for is unchanged: there is exactly ONE place a
/// request turns into a `Settings`, so two gated neighbours cannot resolve the
/// three-level hierarchy against different snapshots. So is the fallback —
/// `Settings::default()`, all protection ON — because a request arriving before
/// the tab layer is up must not be the moment containment silently lapses.
///
/// V42 Phase A2 moved the *implementation* to
/// [`RouteCtx::settings`](crate::offload::host::RouteCtx::settings), which
/// reads the settings handle it was given rather than fishing `AppState` out of
/// the managed-state table. This wrapper stays because the callers that still
/// hold only an `AppHandle` — `harness::claude::hook`'s plugin routes and
/// `offload::latch` — are #114/#115's seam, not this phase's; it delegates, so
/// there is one implementation and not two spellings of one rule.
pub(crate) fn live_settings(app: &tauri::AppHandle) -> crate::settings::Settings {
    crate::offload::host::RouteCtx::from_app(app).settings()
}

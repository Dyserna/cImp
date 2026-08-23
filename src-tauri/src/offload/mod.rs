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

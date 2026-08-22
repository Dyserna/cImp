//! V9-01 — per-project **code knowledge graph** (CKG).
//!
//! An on-disk graph of a project's code (files, symbols, references, calls,
//! imports) and its documentation (markdown + doc-comments), built and
//! maintained inside the cImp binary and stored at
//! `<project>/<db_subdir>/graph.db`. It is queried by two consumers through
//! the surfaces V8 already established: the cloud Opus session (MCP tools on
//! the `--offload-mcp` server) and the local offload worker (native tools).
//!
//! Layering (see `docs/MILESTONE-V9-01-code-knowledge-graph.md`):
//! - **extraction** — `builder` parses each file with tree-sitter into the
//!   language-independent IR in [`model`];
//! - **storage/query** — `index`/`schema` persist the IR into an embedded
//!   CozoDB store and `query` answers Datalog queries over it;
//! - **service** — `GraphService` owns one warm index per project root.
//!
//! Built incrementally; this module currently lands the IR (`model`). The
//! parser (tree-sitter), store (CozoDB), query API, watcher, embedding
//! pipeline, and monitor event bus arrive in later stages.

mod builder;
mod context;
mod embed;
mod gitcmd;
mod gitmeta;
mod impact;
mod index;
mod mcp;
mod memory;
mod model;
mod schema;
// V32 Phase C2 (#48): the write-time credential screen for `context_note`.
// `pub(crate)` since V35 Phase H: `processing::sanitize` reaches in for the
// compiled rule set (see `secrets::credential_rules`) so the capture corpus
// redacts against the same curated patterns memory quarantines against, rather
// than growing a second corpus of its own.
pub(crate) mod secrets;
mod service;
// V17 Phase B: strict whole-file-read command parser, shared by the read-hook
// shim (`crate::read_hook`) and the bypass tap (`service::check_bypass`).
pub(crate) mod shellread;
mod tags;
mod watcher;

pub use builder::parse_file;
pub use context::{est_tokens, RetrieveResult};
pub use index::GraphIndex;
pub use memory::{classify_tool, MemArg, MemorySnapshot, ProjectFact, UsageEvent, UsageOrigin};
// V14 Phase D: only `UsageSnapshot` itself is named by qualified path outside
// this module (the `graph_usage` IPC handler's return type). Its nested
// field types (`Effectiveness`/`SessionUsage`/`SessionUsageRow`/`ToolUsage`/
// `TurnUsage`/`UsageTotals`) are used structurally, never referenced by their
// own `crate::graph::…` path — same posture as `MemorySnapshot`'s own nested
// `WorkingSetEntry`/`MemNote`/`SessionInfo`, which aren't re-exported here
// either. V24 Phase B: `SessionUsageDetail` is likewise named by qualified
// path (the `graph_session_usage` handler's return type); its own nested
// `ModelUsage`/`OriginSplit` stay structural.
pub use mcp::{
    handle_call as handle_mcp_call, lean_filter, native_surface_sig, offload_query,
    offload_run_check, run_check_spec, semantic_code_spec, semantic_spec, source_for_consumer,
    // V38 F-3: the consumer-aware builder replaced the blind one here. `tools()`
    // itself stays (the app's own surface measurement calls it), it is simply no
    // longer something a consumer-serving caller may reach for by accident.
    surface_stats, tool_specs, tools_for as mcp_tools_for, LEAN_HIDDEN, UNKNOWN_SOURCE,
};
pub use memory::{SessionUsageDetail, UsageSnapshot};
pub use model::*;
pub use service::{EmbedderProbe, GraphService, GraphStatus, LangCensus, RebuildOrigin};

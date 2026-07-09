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

mod activity;
mod builder;
mod context;
mod embed;
mod gitmeta;
mod impact;
mod index;
mod mcp;
mod memory;
mod model;
mod schema;
mod service;
mod tags;
mod watcher;

pub use activity::{snapshot as graph_history, GraphCall};
pub use builder::parse_file;
pub use context::RetrieveResult;
pub use index::{GraphIndex, GraphStats, SymbolHit};
pub use memory::{classify_tool, MemArg, MemorySnapshot, ProjectFact};
pub use mcp::{
    handle_call as handle_mcp_call, offload_query, semantic_code_spec, semantic_spec, tool_specs,
    tools as mcp_tools, GraphToolSpec,
};
pub use model::*;
pub use service::{EmbedderProbe, GraphService, GraphStatus, LangCensus};

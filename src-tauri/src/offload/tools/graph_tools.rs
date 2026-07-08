//! V9-01 — the code-knowledge-graph tools exposed to the **offload worker**
//! (the local model running the agent loop), the second consumer of the graph
//! after the cloud Opus session. The tool set + JSON schema come from the
//! single source of truth in [`crate::graph::tool_specs`], so the worker's
//! `ToolDef`s can't drift from the MCP descriptors. Execution resolves the
//! graph store from the worker's confinement roots and runs read-only.
//!
//! Whether these are offered at all is decided by the caller (the service):
//! the local worker always gets them when the graph is enabled; a *remote*
//! worker only when the user opts in (`graph.allow_remote_worker_access`),
//! because a remote backend — LAN or cloud — would receive the project's code
//! structure. See `OffloadService::run_on`.

use crate::offload::openai::ToolDef;

use super::ToolCtx;

/// The graph `ToolDef`s, built from the shared specs. Callers gate *whether*
/// to include these (feature enabled + local/remote opt-in); this just renders
/// them.
pub fn defs() -> Vec<ToolDef> {
    let mut specs = crate::graph::tool_specs();
    // Advertise semantic search to the worker only when it's enabled (it
    // degrades to full-text at runtime if the embedder is down).
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if crate::settings::load_readonly(&cwd).graph.semantic_search {
        specs.push(crate::graph::semantic_spec());
    }
    specs
        .into_iter()
        .map(|s| ToolDef::function(s.name, s.description, s.parameters))
        .collect()
}

/// Execute a `graph_*` tool for the worker against the graph store under one of
/// the confinement roots. Read-only; the result is plain token-bounded text.
pub async fn dispatch(
    name: &str,
    args: serde_json::Value,
    ctx: &ToolCtx,
) -> Result<String, String> {
    crate::graph::offload_query(&ctx.allowed_roots, name, &args).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defs_mirror_the_shared_specs() {
        let specs = crate::graph::tool_specs();
        let defs = defs();
        // `defs()` is the shared base set plus an optional semantic tool when
        // it's enabled in live settings, so assert the base set is covered
        // (subset) rather than exact equality — the count is settings-dependent.
        assert!(!defs.is_empty());
        assert!(defs.len() >= specs.len());
        for spec in &specs {
            let def = defs
                .iter()
                .find(|d| d.function.name == spec.name)
                .unwrap_or_else(|| panic!("base graph tool `{}` missing from defs()", spec.name));
            assert_eq!(def.function.parameters, spec.parameters);
        }
        for def in &defs {
            // V10 adds the `context_*` memory tools to the shared spec set
            // alongside the `graph_*` tools.
            assert!(
                def.function.name.starts_with("graph_") || def.function.name.starts_with("context_"),
                "unexpected tool name `{}`",
                def.function.name
            );
        }
    }
}

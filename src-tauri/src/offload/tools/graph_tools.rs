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
    let g = crate::settings::load_readonly(&cwd).graph;
    if g.semantic_search {
        specs.push(crate::graph::semantic_spec());
    }
    if g.embed_code_bodies {
        specs.push(crate::graph::semantic_code_spec());
    }
    // V17 Phase E: hide the cold-tail tools from the ADVERTISED worker surface
    // when `lean_tools` is on. Dispatch (`offload_query` → `run_tool`) is
    // name-driven and unaffected, so a hidden name still answers.
    crate::graph::lean_filter(specs, g.lean_tools)
        .into_iter()
        // Pure lookups: the code graph is a snapshot built before the run and
        // not rebuilt mid-run, so an identical `graph_*` query can be served
        // from the call cache (its answer can't have changed). File/process
        // tools (read_file/run_command/…) are stateful and re-execute instead.
        .map(|s| ToolDef::function(s.name, s.description, s.parameters).pure())
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
        // `defs()` reads live settings for its optional semantic tools AND the
        // V17 `lean_tools` filter; in the test environment settings default to
        // `lean_tools=false` (no `settings.json` beside the test binary), so the
        // worker surface here is the full, unfiltered set — the comparison stays
        // meaningful. The lean-filtering itself is pinned in
        // `lean_filter_applies_to_worker_defs` below via the pre-filter helper.
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
                def.function.name.starts_with("graph_")
                    || def.function.name.starts_with("context_"),
                "unexpected tool name `{}`",
                def.function.name
            );
        }
    }

    /// V17 Phase E: the worker's `defs()` builds its `ToolDef`s through the same
    /// `lean_filter` as the MCP surface, so `lean_tools=true` drops exactly the
    /// hidden five from the worker surface too. Pinned via the pre-filter helper
    /// so it's independent of ambient settings.
    #[test]
    fn lean_filter_applies_to_worker_defs() {
        let names: Vec<String> = crate::graph::lean_filter(crate::graph::tool_specs(), true)
            .iter()
            .map(|s| s.name.to_string())
            .collect();
        for h in crate::graph::LEAN_HIDDEN {
            assert!(
                !names.iter().any(|n| n == h),
                "`{h}` should be hidden from the worker"
            );
        }
        assert_eq!(
            names.len(),
            crate::graph::tool_specs().len() - crate::graph::LEAN_HIDDEN.len()
        );
    }
}

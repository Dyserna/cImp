//! V26 — the **code-audit tools** exposed to the offload worker (the local model
//! running the agent loop), the third consumer of the audit surface after Claude
//! Code and OpenCode (both of which reach it through the `cimp --code-audit-mcp`
//! stdio child). The tool set + JSON schema come from the single source of truth
//! in [`crate::audit::mcp::tool_descriptors`], so the worker's `ToolDef`s can't
//! drift from the MCP descriptors the other two consumers see — exactly the
//! `graph_tools`/`crate::graph::tool_specs` arrangement, one milestone over.
//!
//! Execution reaches the app's live [`AuditState`] through the process-global
//! [`crate::audit::global`] handle (the seam `crate::graph::offload_query` is for
//! the graph): the worker runs in-process but *outside* any Tauri command
//! context, so it can't reach the `manage`d state the way an IPC command does.
//! A scan the worker triggers streams live into the Code Audit tab just like a
//! UI- or Claude-triggered one, because every consumer funnels through the same
//! [`crate::audit::mcp::run_audit`].
//!
//! Whether these are offered at all is decided by the caller (the service):
//! `code_audit.enabled` AND `expose_offload` AND a **local** backend. The scan
//! always runs locally inside the app regardless of where the worker lives, but
//! the report it returns — repo file:line paths plus scanner messages that can
//! quote the offending code — is local data, so it is never offered to (and,
//! via the router's `allow_audit` re-gate, never runs for) a remote/LAN/cloud
//! backend: the same boundary the graph tools enforce. See
//! `OffloadService::run_on` and `HostRouter::call`.

use crate::audit::adapters::Category;
use crate::offload::openai::ToolDef;

use super::ToolCtx;

/// The two code-audit `ToolDef`s, rendered from the shared MCP descriptors so the
/// worker surface can never drift from what Claude/OpenCode see. The MCP shape
/// carries the schema under `inputSchema`; [`ToolDef::function`] wants it as
/// `parameters`, so that one field is remapped — everything else is copied
/// verbatim.
///
/// Note there is **no** `.pure()` call here, and that is deliberate: unlike the
/// `graph_*` lookups (which query an immutable snapshot built before the run),
/// an audit *runs live scanners over the working tree*, so re-running it after
/// the model has edited files can legitimately yield different findings. The
/// stateful default is exactly right — the [`CallCache`] must re-execute an audit
/// every time and must never serve a stale earlier result.
///
/// [`CallCache`]: crate::offload::agent
pub fn defs() -> Vec<ToolDef> {
    crate::audit::mcp::tool_descriptors()
        .into_iter()
        .map(|d| {
            // `tool_descriptors()` is the internal source of truth, so these
            // fields are always present and correctly shaped; fall back defensively
            // rather than panic if that ever changes.
            let name = d
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let description = d
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let parameters = d
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            ToolDef::function(name, description, parameters)
        })
        .collect()
}

/// Execute a code-audit tool for the worker. Maps the tool name to its
/// [`Category`], resolves the app's live [`AuditState`] through
/// [`crate::audit::global`], and runs the scan to completion via
/// [`crate::audit::mcp::run_audit`], returning the same free-text report the MCP
/// consumers get.
///
/// `args`/`ctx` are unused — both tools are zero-argument and the scan targets
/// the app's own project root, not the worker's confinement roots — but the
/// signature stays parallel to the sibling executors so `dispatch` can route
/// uniformly.
///
/// `global()` returning `None` means the runner was never built in this process
/// (e.g. a headless subcommand, or a unit test with no `main.rs`); that surfaces
/// as a clean tool error the model can read, never a panic.
pub async fn execute(
    name: &str,
    _args: serde_json::Value,
    _ctx: &ToolCtx,
) -> Result<String, String> {
    let category = match name {
        "security_audit" => Category::Security,
        "quality_audit" => Category::Quality,
        // Unreachable via `dispatch` (which only routes these two names here),
        // but keep `execute` total rather than panic on a stray call.
        other => return Err(format!("unknown audit tool: {other}")),
    };
    let state = crate::audit::global()
        .ok_or_else(|| "code audit is unavailable in this process".to_string())?;
    crate::audit::mcp::run_audit(&state, category).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worker's `defs()` are a faithful render of the shared MCP descriptors:
    /// same names, same descriptions, and the `inputSchema` copied over verbatim
    /// as `parameters`. This is the anti-drift pin — if the descriptors change,
    /// the worker surface follows automatically and this stays green.
    #[test]
    fn defs_mirror_the_descriptors() {
        let descriptors = crate::audit::mcp::tool_descriptors();
        let defs = defs();
        assert_eq!(defs.len(), 2, "exactly the two audit tools");
        assert_eq!(defs.len(), descriptors.len());
        for (def, desc) in defs.iter().zip(descriptors.iter()) {
            assert_eq!(
                def.function.name,
                desc.get("name").and_then(|v| v.as_str()).unwrap()
            );
            assert_eq!(
                def.function.description,
                desc.get("description").and_then(|v| v.as_str()).unwrap()
            );
            // The `inputSchema` → `parameters` remap must be an exact copy.
            assert_eq!(def.function.parameters, descriptors_schema(desc));
        }
        // The two expected names, order-independent.
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"security_audit"));
        assert!(names.contains(&"quality_audit"));
    }

    fn descriptors_schema(desc: &serde_json::Value) -> serde_json::Value {
        desc.get("inputSchema").cloned().unwrap()
    }

    /// Audits are stateful (never cache-served): re-running after edits can yield
    /// different findings, so `defs()` must NOT mark them pure. This pins the
    /// "no `.pure()`" decision so a later refactor can't silently make audits
    /// cacheable.
    #[test]
    fn audit_defs_are_stateful_not_pure() {
        for def in defs() {
            assert!(
                def.stateful,
                "`{}` must stay stateful so the call cache never serves a stale audit",
                def.function.name
            );
        }
    }

    /// In a unit test there is no `main.rs` to call `set_global`, so
    /// `crate::audit::global()` is `None` and every `execute` takes the
    /// unavailable-in-this-process path. Confirms both names hit it (rather than
    /// the unknown-name arm) and that the error is the exact user-facing string.
    #[tokio::test]
    async fn execute_without_global_is_unavailable() {
        let ctx = ToolCtx::new(
            vec![std::env::current_dir().unwrap()],
            vec![],
            vec![],
            &std::env::current_dir().unwrap(),
        );
        for name in ["security_audit", "quality_audit"] {
            let err = execute(name, serde_json::json!({}), &ctx)
                .await
                .expect_err("no global set in a unit test → error");
            assert_eq!(err, "code audit is unavailable in this process");
        }
    }

    /// An unknown name is total (an error), not a panic — even though `dispatch`
    /// never routes one here.
    #[tokio::test]
    async fn execute_unknown_name_errors() {
        let ctx = ToolCtx::new(
            vec![std::env::current_dir().unwrap()],
            vec![],
            vec![],
            &std::env::current_dir().unwrap(),
        );
        let err = execute("bogus_audit", serde_json::json!({}), &ctx)
            .await
            .expect_err("unknown name → error");
        assert!(err.contains("unknown audit tool"), "{err}");
    }
}

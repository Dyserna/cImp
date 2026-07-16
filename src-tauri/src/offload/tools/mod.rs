//! V8-01 native baseline offload tools — built into cImp, zero
//! external deps, so offload works before any MCP server is installed:
//!
//! - [`read_file`] — bounded line/byte reads within an `allowed_root`.
//! - [`code_search`] — literal/substring search across an `allowed_root`
//!   (the deep-search case that motivated this milestone).
//! - [`run_command`] — allowlisted, read-only command execution.
//!
//! Dispatch is function-based ([`dispatch`]) rather than a trait so the
//! agent loop can route a model `tool_call` to its owner without a
//! `dyn`-async dance. Each tool module exposes a `def()` ([`ToolDef`])
//! and an `execute`. All file access is confined to [`ToolCtx::allowed_roots`].

use std::path::{Path, PathBuf};

use crate::offload::openai::ToolDef;
use crate::settings::{CommandPolicy, OffloadToolToggles};

pub mod audit_tools;
pub mod code_search;
pub mod graph_tools;
pub mod list_dir;
pub mod read_file;
pub mod run_check;
pub mod run_command;

/// Shared execution context for native tools: the roots file access is
/// confined to and the allowlist `run_command` is gated by.
#[derive(Clone, Debug)]
pub struct ToolCtx {
    /// Roots that `read_file`/`code_search`/`run_command` are confined
    /// to. Guaranteed non-empty by the constructor (falls back to the
    /// launch project root).
    pub allowed_roots: Vec<PathBuf>,
    /// Programs `run_command` may execute (matched by program name).
    /// Empty = nothing runnable (deny by default).
    pub command_allowlist: Vec<String>,
    /// Per-program security policies `run_command` enforces on top of the
    /// allowlist (denied flags/subcommands + spawn env). See [`CommandPolicy`].
    pub command_policies: Vec<CommandPolicy>,
}

impl ToolCtx {
    /// Build a context, falling back to `launch_root` when the
    /// configured `allowed_roots` is empty (the documented default).
    pub fn new(
        mut allowed_roots: Vec<PathBuf>,
        command_allowlist: Vec<String>,
        command_policies: Vec<CommandPolicy>,
        launch_root: &Path,
    ) -> Self {
        if allowed_roots.is_empty() {
            allowed_roots.push(launch_root.to_path_buf());
        }
        Self {
            allowed_roots,
            command_allowlist,
            command_policies,
        }
    }

    /// Resolve a model-supplied path and confine it to `allowed_roots`.
    /// Returns the canonical path on success, or an error string the
    /// loop feeds back to the model so it can correct itself. The path
    /// must exist (we canonicalize); for not-yet-existing paths this is
    /// the right behavior — offload is read-only.
    pub fn confine(&self, requested: &str) -> Result<PathBuf, String> {
        let raw = PathBuf::from(requested);
        // Candidate locations: an absolute request as-is, else the request
        // resolved against EACH root — a relative path may legitimately live
        // under any configured root, not just the first.
        let candidates: Vec<PathBuf> = if raw.is_absolute() {
            vec![raw]
        } else {
            self.allowed_roots.iter().map(|r| r.join(&raw)).collect()
        };
        // Collect every distinct in-root resolution. A relative path can
        // resolve under more than one root when roots overlap/nest; silently
        // returning the first is order-dependent and surprising, so flag the
        // ambiguity instead. The per-root canonicalize + boundary check is the
        // shared [`crate::fsutil::confine_existing`] core (target must exist —
        // offload is read-only); the multi-root/ambiguity policy stays here.
        // (An absolute request has a single candidate and can never be
        // ambiguous here.)
        let mut matches: Vec<PathBuf> = Vec::new();
        for cand in candidates {
            for root in &self.allowed_roots {
                if let Ok(canon) = crate::fsutil::confine_existing(root, &cand) {
                    if !matches.contains(&canon) {
                        matches.push(canon);
                    }
                    // This candidate is confined; don't double-count it across
                    // overlapping roots (it canonicalizes to one real path).
                    break;
                }
            }
        }
        match matches.len() {
            0 => Err(format!(
                "`{requested}` is outside the allowed roots ({} configured)",
                self.allowed_roots.len()
            )),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => Err(format!(
                "`{requested}` is ambiguous — it resolves to {n} different files across \
                 the configured roots. Pass an absolute path to disambiguate."
            )),
        }
    }
}

/// The [`ToolDef`]s for the native tools enabled by `toggles`. Fed into
/// the chat request's `tools` array alongside any MCP-server tools.
///
/// `run_check` (V21 F6) additionally requires the project to have configured
/// `checks` — read live here, gated identically to the MCP surface
/// (`graph/mcp.rs`), so a fresh project sees no `run_check` on either side.
pub fn enabled_defs(toggles: &OffloadToolToggles) -> Vec<ToolDef> {
    let checks_configured = {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        !crate::settings::load_readonly(&cwd).checks.is_empty()
    };
    enabled_defs_inner(toggles, checks_configured)
}

/// The pure toggle→def mapping, split from the live `checks` read so the
/// `run_check` advertisement gate is testable without touching disk settings.
fn enabled_defs_inner(toggles: &OffloadToolToggles, checks_configured: bool) -> Vec<ToolDef> {
    let mut defs = Vec::new();
    if toggles.read_file {
        defs.push(read_file::def());
    }
    if toggles.list_dir {
        defs.push(list_dir::def());
    }
    if toggles.code_search {
        defs.push(code_search::def());
    }
    if toggles.run_command {
        defs.push(run_command::def());
    }
    // V21 F6: advertised only when checks are configured for the project root —
    // the tool can't do anything useful otherwise, and the gate matches the MCP
    // surface so exposure is consistent across both consumers.
    if toggles.run_check && checks_configured {
        defs.push(run_check::def());
    }
    defs
}

/// Route a native `tool_call` to its executor. `args` is the parsed
/// arguments object. Returns the tool result (or an error string the
/// loop surfaces to the model as a `role: tool` message — never a panic).
pub async fn dispatch(
    name: &str,
    args: serde_json::Value,
    ctx: &ToolCtx,
) -> Result<String, String> {
    match name {
        "read_file" => read_file::execute(args, ctx).await,
        "list_dir" => list_dir::execute(args, ctx).await,
        "code_search" => code_search::execute(args, ctx).await,
        "run_command" => run_command::execute(args, ctx).await,
        // V21 F6: worker-native `run_check` — routes to the SAME checks entry
        // point the MCP handler uses (via `crate::graph::offload_run_check`),
        // beside the `graph_` route below because both share the graph module's
        // project-root resolution + activity recording.
        "run_check" => run_check::execute(args, ctx).await,
        // V26 code-audit tools — the two fixed names route to the same executor
        // (which maps each to its category). Advertised only when the service
        // decided to offer them (enabled + `expose_offload` + local backend)
        // and re-gated in the router's `call` via `allow_audit`, exactly like
        // the graph tools — the scan runs locally either way, but its report
        // is local data that must not reach an opted-out or remote backend.
        "security_audit" | "quality_audit" => audit_tools::execute(name, args, ctx).await,
        // V9-01 graph tools (advertised only when the service decided to offer
        // them — feature on + local-or-opted-in remote — and re-gated in the
        // router's `call`).
        n if n.starts_with("graph_") => graph_tools::dispatch(name, args, ctx).await,
        other => Err(format!("unknown native tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> ToolCtx {
        let cwd = std::env::current_dir().unwrap();
        ToolCtx::new(vec![cwd.clone()], vec![], vec![], &cwd)
    }

    /// V26: both audit tool names route into `audit_tools::execute` rather than
    /// falling through to the unknown-tool arm. In a unit test the audit global
    /// is never set, so a correctly-routed call errors — all this test pins is
    /// that it is NOT the unknown-tool error (the exact executor message is
    /// owned and pinned by `audit_tools`'s own test, not double-pinned here).
    #[tokio::test]
    async fn dispatch_routes_the_audit_tools() {
        for name in ["security_audit", "quality_audit"] {
            let err = dispatch(name, serde_json::json!({}), &test_ctx())
                .await
                .expect_err("no audit global in a unit test → error");
            assert!(
                !err.contains("unknown native tool"),
                "`{name}` must route to audit_tools::execute, got: {err}"
            );
        }
    }
}

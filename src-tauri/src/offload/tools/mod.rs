//! V8-01 native baseline offload tools — built into ccImp, zero
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
use crate::settings::OffloadToolToggles;

pub mod code_search;
pub mod read_file;
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
}

impl ToolCtx {
    /// Build a context, falling back to `launch_root` when the
    /// configured `allowed_roots` is empty (the documented default).
    pub fn new(
        mut allowed_roots: Vec<PathBuf>,
        command_allowlist: Vec<String>,
        launch_root: &Path,
    ) -> Self {
        if allowed_roots.is_empty() {
            allowed_roots.push(launch_root.to_path_buf());
        }
        Self {
            allowed_roots,
            command_allowlist,
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
        for cand in candidates {
            let canon = match cand.canonicalize() {
                Ok(c) => c,
                Err(_) => continue,
            };
            for root in &self.allowed_roots {
                if let Ok(root_canon) = root.canonicalize() {
                    if canon.starts_with(&root_canon) {
                        return Ok(canon);
                    }
                }
            }
        }
        Err(format!(
            "`{requested}` is outside the allowed roots ({} configured)",
            self.allowed_roots.len()
        ))
    }
}

/// The [`ToolDef`]s for the native tools enabled by `toggles`. Fed into
/// the chat request's `tools` array alongside any MCP-server tools.
pub fn enabled_defs(toggles: &OffloadToolToggles) -> Vec<ToolDef> {
    let mut defs = Vec::new();
    if toggles.read_file {
        defs.push(read_file::def());
    }
    if toggles.code_search {
        defs.push(code_search::def());
    }
    if toggles.run_command {
        defs.push(run_command::def());
    }
    defs
}

/// Route a native `tool_call` to its executor. `args` is the parsed
/// arguments object. Returns the tool result (or an error string the
/// loop surfaces to the model as a `role: tool` message — never a panic).
pub async fn dispatch(name: &str, args: serde_json::Value, ctx: &ToolCtx) -> Result<String, String> {
    match name {
        "read_file" => read_file::execute(args, ctx).await,
        "code_search" => code_search::execute(args, ctx).await,
        "run_command" => run_command::execute(args, ctx).await,
        other => Err(format!("unknown native tool: {other}")),
    }
}

//! Graph tool surface shared by both consumers:
//! - the **cloud Opus session**, via the `cimp --offload-mcp` server
//!   ([`tools_for`] descriptors + [`handle_call`]); and
//! - the **local offload worker**, via [`offload_query`] (wired into the
//!   offload native-tool router).
//!
//! **V42 R8** split what used to be one 5,700-line `mcp.rs` by what the code is
//! about, not by where it is served from:
//! - [`tools`] — the `graph_*` / `context_*` specs, the MCP adapter
//!   ([`handle_call`], [`dispatch_recorded`]) and the dispatch+format core;
//! - [`checks_tools`] — `run_check` / `run_command`, which need a project root
//!   and no index and read [`crate::checks`], not the graph;
//! - [`surface`] — the process-wide measurement of what is advertised.
//!
//! This file keeps what more than one of them needs (settings + project-root
//! resolution, the consumer→source mapping, the activity response shape) and
//! re-exports the surface `graph/mod.rs` publishes, so the split is invisible
//! outside this directory.

mod checks_tools;
mod surface;
mod tools;

pub use checks_tools::{offload_run_check, run_check_spec};
pub use surface::{native_surface_sig, surface_stats, SurfaceStats};
pub use tools::{
    handle_call, lean_filter, offload_query, semantic_code_spec, semantic_spec, tool_specs,
    tools_for, LEAN_HIDDEN,
};
pub(crate) use checks_tools::dispatch_rootless;
pub(crate) use tools::{dispatch_recorded, ToolCall};

use std::path::{Path, PathBuf};

/// The activity/memory **source** string for a consumer name — the value carried
/// through the activity ring and used to scope the `context_*` memory tools to
/// the calling agent.
///
/// A registered harness resolves to its own id; cImp's own in-app consumers
/// keep their names. **V40 Phase A: anything else is [`UNKNOWN_SOURCE`], not
/// Claude** (locked decision 2). This used to be `if opencode { opencode } else
/// { claude }`, so a forged or hand-run child asserting any consumer at all got
/// Claude's activity badge and Claude's memory scope — a misattribution in the
/// view whose entire job is attribution, and a read of another agent's sessions.
/// The unknown source filters to no sessions, which is the fail-closed answer.
///
/// An ABSENT consumer is a different question and has a different answer: it is
/// resolved by the caller to [`crate::harness::DEFAULT_HARNESS`], which is a
/// documented wire-compatibility promise rather than a guess.
pub fn source_for_consumer(consumer: &str) -> &'static str {
    if let Some(id) = crate::harness::HarnessId::from_consumer(consumer).and_then(|h| h.id()) {
        return id;
    }
    match consumer.trim().to_ascii_lowercase().as_str() {
        // cImp's OWN consumers. Neither is a harness and neither has a
        // `harness/<id>/` directory; both are names the activity feed already
        // shows, so they are spelled here beside the registry lookup rather
        // than smuggled into it.
        "offload" => "offload",
        "audit" => "audit",
        _ => UNKNOWN_SOURCE,
    }
}

/// The activity source for a caller whose asserted consumer names nothing cImp
/// serves. Its own token so a row can say "cImp does not know who this was"
/// instead of naming a product that did not make the call.
pub const UNKNOWN_SOURCE: &str = "unknown";

/// The response payload captured for the activity detail popup: the tool's
/// text on success, the error message (marked) on failure.
fn activity_response(result: &Result<String, String>) -> String {
    match result {
        Ok(text) => text.clone(),
        Err(msg) => format!("[error] {msg}"),
    }
}

// ── project resolution + settings helpers ────────────────────────────────

fn current_settings() -> crate::settings::Settings {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::settings::load_readonly(&cwd)
}

fn limits(settings: &crate::settings::Settings) -> (usize, usize) {
    (
        settings.graph.max_rows_per_query.max(1) as usize,
        settings.graph.max_snippet_bytes.max(40) as usize,
    )
}

fn db_subdir(settings: &crate::settings::Settings) -> String {
    settings.graph.effective_db_subdir()
}

/// The project root for a call arriving on the **headless** path — the stdio
/// MCP child running with the app unreachable, whose `cwd` is its own process
/// working directory.
///
/// #104: that cwd is the sub-agent's shell cwd, not a project. The app-side
/// routes resolve theirs through `discovery::external_project_root`, which can
/// also consult the calling tab's configured directory; here there is no app to
/// ask, so the answer is the marker walk alone
/// ([`crate::fsutil::find_project_root`] — `.git` beats an existing `<sub>`,
/// nearest wins), then an existing `graph.db` above, then the cwd itself.
///
/// The last fallback is kept because the tools reached from here (`run_check`,
/// `run_command`) must still work in a project with no VCS and no index yet,
/// which is the same "the tab's own directory is a root" allowance the app-side
/// resolver makes at its step 3. What it no longer does is let a *sub-directory*
/// of a marked project become one.
fn headless_project_root(cwd: &Path, sub: &str) -> PathBuf {
    crate::fsutil::find_project_root(cwd, sub)
        .map(|p| p.root)
        .or_else(|| find_graph_root(cwd, sub))
        .unwrap_or_else(|| cwd.to_path_buf())
}

/// The project root for a call arriving on a **warm app-side** route — an
/// existing `graph.db` above `cwd`, else `cwd` itself.
///
/// Deliberately NOT [`headless_project_root`]'s marker walk. The routes that
/// reach here resolved the calling tab's project first (through
/// `discovery::external_project_root`, which can consult the tab's configured
/// directory), so a second, different walk here would be free to disagree with
/// the answer the caller already has; the headless child has no app to ask,
/// which is why it does walk. Naming both puts the difference in one place as a
/// documented choice instead of two copies of an expression.
///
/// Shared by `GraphService::run_graph_tool` and `GraphService::graph_root_key`,
/// which MUST agree: the loopback writes its unattributed-write row at gate
/// time, before dispatch has resolved anything, and two resolutions would file
/// one call under two projects (#48 F-16).
pub(crate) fn warm_project_root(cwd: &Path, sub: &str) -> PathBuf {
    find_graph_root(cwd, sub).unwrap_or_else(|| cwd.to_path_buf())
}

/// Walk up from `start` looking for an ancestor containing `<sub>/graph.db`.
pub(crate) fn find_graph_root(start: &Path, sub: &str) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if dir.join(sub).join("graph.db").is_file() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

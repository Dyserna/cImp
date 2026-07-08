//! V10 — session / action memory data types.
//!
//! A per-project rolling record of what each agent session read, edited, and
//! queried, plus free-text notes it chose to remember. Stored in the same
//! `graph.db` as the code graph but in **separate relations** that are ensured
//! independently of [`super::schema::RELATIONS`] and therefore survive a full
//! index rebuild (which `reset()`s the derived relations). The storage methods
//! live on [`super::index::GraphIndex`]; these are the plain, serializable
//! shapes returned to the service / IPC layer.

use serde::Serialize;

/// Newest-session cap: sessions beyond this (by `last_ms`) are evicted, cascading
/// their events and unpinned notes.
pub const MAX_SESSIONS_PER_ROOT: usize = 20;
/// Per-session event ring cap: only the newest this-many `mem_event`s are kept.
pub const MAX_EVENTS_PER_SESSION: i64 = 500;

/// One file in a session's working set, aggregated from its events and scored
/// `recency × frequency × kind_weight` by the ranker.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkingSetEntry {
    pub path: String,
    /// Number of events touching this path in the session.
    pub touches: u32,
    /// The most recent event kind for this path (`read`/`edit`/`query`).
    pub last_kind: String,
    pub last_ms: i64,
    /// Distinct symbols touched on this path (most recent first, bounded).
    pub top_symbols: Vec<String>,
}

/// A remembered note (a decision or fact) for a session. Pinned notes survive
/// their session's eviction and show project-wide.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct MemNote {
    pub note_id: String,
    pub session_id: String,
    pub text: String,
    pub ts_ms: i64,
    pub pinned: bool,
}

/// A session summary row for the Memory UI's "recent sessions" list.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: String,
    pub agent: String,
    pub started_ms: i64,
    pub last_ms: i64,
    pub events: u32,
}

/// The full memory readout for a project (drives the Memory section + IPC).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct MemorySnapshot {
    /// The session with the most recent activity (what `context_recall` scopes
    /// to), or `None` when the project has no memory yet.
    pub current_session: Option<String>,
    /// The current session's ranked working set.
    pub working_set: Vec<WorkingSetEntry>,
    /// The current session's notes plus every pinned note in the project.
    pub notes: Vec<MemNote>,
    /// All known sessions, newest activity first.
    pub sessions: Vec<SessionInfo>,
}

/// Map an agent tool name/id to a memory event `kind` + the argument key that
/// carries the path/target. Shared by the Claude transcript tap and the
/// OpenCode plugin's `/memory/event` ingress so both classify identically.
/// Returns `None` for tools that shouldn't be recorded (Task, TodoWrite, our
/// own graph/offload tools — already captured by the activity ring).
pub fn classify_tool(tool: &str) -> Option<(&'static str, MemArg)> {
    match tool {
        // Reads.
        "Read" | "NotebookRead" | "read" => Some(("read", MemArg::Path)),
        // Edits / writes.
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "edit" | "write" | "patch" => {
            Some(("edit", MemArg::Path))
        }
        // Structural / content queries.
        "Grep" | "Glob" | "grep" | "glob" | "list" => Some(("query", MemArg::Pattern)),
        "Bash" | "bash" => Some(("query", MemArg::Command)),
        _ => None,
    }
}

/// Which argument of a classified tool carries the recorded target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemArg {
    /// `file_path` — a concrete file (recorded as `path`).
    Path,
    /// `pattern`/`path` of a search (recorded as `path`, best-effort).
    Pattern,
    /// `command` of a shell call (recorded into `detail`, no path).
    Command,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_kinds_and_ignores_meta_tools() {
        assert_eq!(classify_tool("Read"), Some(("read", MemArg::Path)));
        assert_eq!(classify_tool("Edit"), Some(("edit", MemArg::Path)));
        assert_eq!(classify_tool("Write"), Some(("edit", MemArg::Path)));
        assert_eq!(classify_tool("Grep"), Some(("query", MemArg::Pattern)));
        assert_eq!(classify_tool("Bash"), Some(("query", MemArg::Command)));
        // OpenCode lowercase ids.
        assert_eq!(classify_tool("edit"), Some(("edit", MemArg::Path)));
        // Not recorded: sub-agents, todos, and our own graph/offload tools.
        assert_eq!(classify_tool("Task"), None);
        assert_eq!(classify_tool("TodoWrite"), None);
        assert_eq!(classify_tool("mcp__cimp-offload__graph_find_symbol"), None);
    }
}

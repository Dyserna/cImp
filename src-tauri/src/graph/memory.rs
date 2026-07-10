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
/// `frequency × kind_weight` by the ranker, with recency as the tie-break
/// (see `GraphIndex::mem_working_set` — recency is deliberately not a score
/// factor, it only orders equal scores).
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

// ── V12 Phase E: memory distillation (durable project facts) ─────────────

/// Cap on **live** (non-archived) project facts. Inserting past the cap
/// archives the oldest UNPINNED live fact first; a pinned fact is never
/// auto-archived (if every live fact is pinned, the cap is simply exceeded —
/// no data loss, just a soft overrun).
pub const MAX_LIVE_PROJECT_FACTS: usize = 100;

/// A durable project fact — either distilled from a session's working
/// set/notes by the local-only offload path, or added manually via the Facts
/// UI. Survives its source session's eviction (unlike `mem_event`/unpinned
/// `mem_note`), which is the whole point: session memory is a ring buffer,
/// project facts are the sediment it leaves behind.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ProjectFact {
    pub fact_id: String,
    pub text: String,
    /// The session that produced this fact (`"manual"` for a UI-added fact).
    pub source_session: String,
    pub ts_ms: i64,
    pub pinned: bool,
    /// Archived facts are excluded from `context_recall` / promotion / the
    /// live-facts cap, but kept (not deleted) so a bad archival is undoable
    /// and the Facts UI can still show provenance if it chooses to.
    pub archived: bool,
}

/// Distilled-fact line count cap: the distiller prompt asks for "AT MOST 3"
/// facts, and a run producing more is treated as a bad generation.
pub const MAX_DISTILLED_FACTS: usize = 3;
/// Per-fact character cap, matching the distiller prompt's "<=200 chars".
pub const MAX_FACT_CHARS: usize = 200;

/// Parse + validate the distiller's raw model output into fact text lines.
/// Pure and DB/network-free so it's unit-testable without a live offload
/// backend.
///
/// The prompt demands "plain text, no numbering, no preamble", but small
/// local models routinely disobey — so each line is normalized before
/// validation (these facts are persisted verbatim into the user-visible
/// Facts UI otherwise): code-fence marker lines and preamble/header lines
/// ending in `:` are dropped, and leading bullet (`-`/`*`/`•`) or numbering
/// (`1.`/`1)`) markers are stripped. The surviving set must be non-empty, at
/// most [`MAX_DISTILLED_FACTS`] lines, each at most [`MAX_FACT_CHARS`]
/// characters. `None` on any violation — a bad generation is skipped
/// (session still marked distilled by the caller), never retried in a loop.
pub fn parse_distilled_facts(raw: &str) -> Option<Vec<String>> {
    let lines: Vec<String> = raw
        .lines()
        .map(str::trim)
        // Code-fence markers around the output are wrapper, not facts.
        .filter(|l| !l.starts_with("```") && !l.starts_with("~~~"))
        // A line ending in `:` is a preamble/header ("Here are the facts:"),
        // never a complete one-line fact.
        .filter(|l| !l.ends_with(':'))
        .map(strip_list_marker)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if lines.is_empty() || lines.len() > MAX_DISTILLED_FACTS {
        return None;
    }
    if lines.iter().any(|l| l.chars().count() > MAX_FACT_CHARS) {
        return None;
    }
    Some(lines)
}

/// Strip one leading markdown list marker — bullet (`- `, `* `, `• `) or
/// number (`1. `, `12) `) — from a trimmed line, returning the re-trimmed
/// remainder. A line that is only a marker collapses to `""` (dropped by the
/// caller). Non-list lines pass through unchanged.
fn strip_list_marker(line: &str) -> &str {
    if matches!(line, "-" | "*" | "•") {
        return ""; // marker-only line — nothing behind it.
    }
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).or_else(|| line.strip_prefix("• ")) {
        return rest.trim_start();
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')) {
            if rest.is_empty() || rest.starts_with(' ') {
                return rest.trim_start();
            }
        }
    }
    line
}

// ── V14 Phase C: usage / cost accounting ──────────────────────────────────
//
// A second per-session ring, stored in its own `usage_stat` relation
// alongside `mem_event` (additive, survives a graph rebuild, evicted with the
// session — see `GraphIndex::record_usage_event` / `prune_sessions_in_tx`).
// Two kinds of row: "turn" (one assistant message's token usage, UPSERTED by
// `msg_id` so a streamed message that firms up its `usage` block updates in
// place rather than duplicating) and "tool_result" (one resolved tool call,
// sized in estimated chars — no exact token count exists for tool output).

/// Per-session `usage_stat` ring cap — deeper than [`MAX_EVENTS_PER_SESSION`]
/// since usage rows are written far more often (every turn AND every tool
/// result, vs. one `mem_event` per *classified* tool call).
pub const MAX_USAGE_PER_SESSION: i64 = 2000;

/// One usage/cost event fed to [`super::service::GraphService::record_usage`].
/// Timestamped internally (same posture as `record_mem_event`'s `ts_ms`
/// argument — callers don't carry a clock through the tap).
#[derive(Clone, Debug, PartialEq)]
pub enum UsageEvent {
    /// One assistant message's token usage. `msg_id` is the UPSERT key: a
    /// streamed transcript can carry the same id across multiple lines as its
    /// `usage` block fills in, and only the last one should survive as a row.
    Turn {
        msg_id: String,
        model: Option<String>,
        in_tok: u32,
        out_tok: u32,
        cache_read: u32,
        cache_make: u32,
    },
    /// One resolved tool call's result size, in characters (estimated tokens
    /// = chars / 4 is a UI-layer concern, not stored here). `tool` is `None`
    /// when the id → name join missed (e.g. the ring evicted it).
    ToolResult { tool: Option<String>, chars: u32 },
}

/// Summed token totals across a session's "turn" rows ("tool_result" rows
/// carry chars, not tokens, so they don't contribute).
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct UsageTotals {
    pub in_tok: u64,
    pub out_tok: u64,
    pub cache_read: u64,
    pub cache_make: u64,
}

/// One turn's token breakdown, plus the (estimated) tool-result characters
/// that arrived since the PREVIOUS turn — i.e. the tool output this turn's
/// assistant message actually read as input context. See
/// [`super::index::GraphIndex::usage_turn_series`].
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TurnUsage {
    pub msg_id: String,
    pub model: Option<String>,
    pub in_tok: u64,
    pub out_tok: u64,
    pub cache_read: u64,
    pub cache_make: u64,
    pub tool_chars: u64,
    pub ts_ms: i64,
}

/// One session's row for the project-wide usage totals table.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SessionUsageRow {
    pub session_id: String,
    pub agent: String,
    pub totals: UsageTotals,
    /// Total estimated tool-result chars for the session (sum of
    /// `usage_per_tool`'s values).
    pub tool_chars: u64,
    /// `cache_read / (cache_read + in_tok)`; `0.0` when there's no
    /// denominator (no turns recorded yet).
    pub cache_hit_ratio: f64,
    /// True when this session has no exact `usage` data — currently every
    /// non-Claude agent (see the OpenCode C3 spike note atop `oob/opencode.rs`).
    pub est_only: bool,
}

// ── V14 Phase D: Usage section assembly ────────────────────────────────────
//
// None of the structs below are stored relations — they're assembled on
// demand by `GraphService::usage_snapshot` from `usage_stat` (Phase C above),
// the V11-C injection/dedup in-memory accounting, and the V11-E read-advisor
// Activity events. See `GraphService::usage_snapshot`/`effectiveness_totals`.

/// One tool's ranked contribution to a session's context (the Usage
/// section's "top consumers" table — e.g. "`Read` of `foo.rs` cost 18k
/// twice"). `est_tokens` is the same `chars / 4` estimate used everywhere
/// else in the graph's honest-accounting posture; `calls` is an exact row
/// count. Distinct from [`GraphIndex::usage_per_tool`](super::index::GraphIndex::usage_per_tool)'s
/// bare char sums (which feed `SessionUsageRow.tool_chars` and don't carry a
/// call count) — the UI table wants both "how much" and "how many times".
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ToolUsage {
    pub tool: String,
    pub est_tokens: u64,
    pub calls: u64,
}

/// The current (most-recently-active) session's usage readout: the per-turn
/// series driving the stacked-bar chart, its summed totals, and the ranked
/// top-tools table.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SessionUsage {
    pub session_id: String,
    pub turns: Vec<TurnUsage>,
    pub totals: UsageTotals,
    pub top_tools: Vec<ToolUsage>,
}

/// The Effectiveness panel's three measured counters. `injected_chars` /
/// `deduped_chars` come from the V11-C in-memory injection/dedup accounting
/// (summed across every session currently resident there by
/// `GraphService::effectiveness_totals` — process-wide, not persisted, lost
/// on restart); `advisor_displaced_chars` comes from the V11-E read-advisor
/// Activity events (the small `graph::activity` ring), deliberately NOT from
/// `usage_stat` — `usage_stat` only knows resolved tool-result sizes, never
/// what a reminder *avoided* sending. All three are measured characters, not
/// fabricated "savings" — the UI still labels every derived (chars→tokens)
/// number `est.`.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Effectiveness {
    pub injected_chars: u64,
    pub deduped_chars: u64,
    pub advisor_displaced_chars: u64,
}

/// The Usage section's full IPC payload (`graph_usage`).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct UsageSnapshot {
    pub current: Option<SessionUsage>,
    pub sessions: Vec<SessionUsageRow>,
    pub effectiveness: Effectiveness,
    /// Completed `offload_task` runs served by a **local** backend — the
    /// milestone's "N tasks served locally" pointer to the Offload Server
    /// tab. Filled in by the `graph_usage` IPC handler (this module has no
    /// dependency on `OffloadService`), `0` when offload is off/unused.
    pub offload_local_tasks: u64,
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

    // ── V12 Phase E: distiller output validation ──────────────────────────

    #[test]
    fn parse_distilled_facts_accepts_one_to_three_clean_lines() {
        assert_eq!(
            parse_distilled_facts("uses FNV hashing for stability\nretry cap is 30s"),
            Some(vec![
                "uses FNV hashing for stability".to_string(),
                "retry cap is 30s".to_string(),
            ])
        );
        assert_eq!(
            parse_distilled_facts("only one fact here"),
            Some(vec!["only one fact here".to_string()])
        );
    }

    #[test]
    fn parse_distilled_facts_trims_and_drops_blank_lines() {
        assert_eq!(
            parse_distilled_facts("  fact one  \n\n\nfact two\n"),
            Some(vec!["fact one".to_string(), "fact two".to_string()])
        );
    }

    #[test]
    fn parse_distilled_facts_rejects_more_than_three_lines() {
        let raw = "one\ntwo\nthree\nfour";
        assert_eq!(parse_distilled_facts(raw), None);
    }

    #[test]
    fn parse_distilled_facts_rejects_an_oversized_line() {
        let long = "x".repeat(MAX_FACT_CHARS + 1);
        assert_eq!(parse_distilled_facts(&long), None);
        // A line right AT the cap is fine.
        let exact = "y".repeat(MAX_FACT_CHARS);
        assert_eq!(parse_distilled_facts(&exact), Some(vec![exact]));
    }

    #[test]
    fn parse_distilled_facts_rejects_empty_or_blank_output() {
        assert_eq!(parse_distilled_facts(""), None);
        assert_eq!(parse_distilled_facts("   \n\n  "), None);
    }

    // Legacy sweep session 5: small models routinely wrap output in
    // preambles/bullets/fences despite the prompt; those used to be persisted
    // verbatim as user-visible project facts.

    #[test]
    fn parse_distilled_facts_drops_preamble_and_strips_bullets() {
        let raw = "Here are the facts:\n- uses FNV hashing for stability\n- retry cap is 30s";
        assert_eq!(
            parse_distilled_facts(raw),
            Some(vec![
                "uses FNV hashing for stability".to_string(),
                "retry cap is 30s".to_string(),
            ])
        );
    }

    #[test]
    fn parse_distilled_facts_strips_numbering_and_fences() {
        let raw = "```\n1. offload uses a warm pool\n2) espeak fallback needs LLVM\n```";
        assert_eq!(
            parse_distilled_facts(raw),
            Some(vec![
                "offload uses a warm pool".to_string(),
                "espeak fallback needs LLVM".to_string(),
            ])
        );
        // A decimal number is NOT numbering — the fact passes through whole.
        assert_eq!(
            parse_distilled_facts("3.5s is the startup budget"),
            Some(vec!["3.5s is the startup budget".to_string()])
        );
    }

    #[test]
    fn parse_distilled_facts_rejects_wrapper_only_output() {
        // Nothing but wrapper noise must still read as a bad generation.
        assert_eq!(parse_distilled_facts("Here are the facts:\n```\n```"), None);
        assert_eq!(parse_distilled_facts("- \n* "), None);
    }

    #[test]
    fn parse_distilled_facts_counts_facts_after_normalization() {
        // Preamble + 3 real facts: 4 raw lines used to be rejected wholesale;
        // now the preamble is dropped and the 3 facts survive.
        let raw = "Key facts:\n- a\n- b\n- c";
        assert_eq!(
            parse_distilled_facts(raw),
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
        // …but 4 real facts still exceed the cap.
        assert_eq!(parse_distilled_facts("- a\n- b\n- c\n- d"), None);
    }
}

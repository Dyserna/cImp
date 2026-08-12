//! V10 — session / action memory data types.
//!
//! A per-project rolling record of what each agent session read, edited, and
//! queried, plus free-text notes it chose to remember. Stored in the same
//! `graph.db` as the code graph but in **separate relations** that are ensured
//! independently of [`super::schema::RELATIONS`] and therefore survive a full
//! index rebuild (which `reset()`s the derived relations). The storage methods
//! live on [`super::index::GraphIndex`]; these are the plain, serializable
//! shapes returned to the service / IPC layer.

use serde::{Deserialize, Serialize};

use crate::offload::outbound::Screen;
use crate::offload::toolclass::WriteTaint;

/// Newest-session cap: sessions beyond this (by `last_ms`) are evicted, cascading
/// their events and unpinned notes.
pub const MAX_SESSIONS_PER_ROOT: usize = 20;

/// Idle-session retention, in days: a session whose `last_ms` is older than
/// this is dropped (with its `usage_stat` + `mem_event` rows) on every store
/// open. A hard constant, NOT a setting — the Sessions list's cost scales with
/// the rows kept and month-old session history has no consumer. Independent of
/// [`MAX_SESSIONS_PER_ROOT`]: that cap bounds the count, this bounds the age.
pub const SESSION_RETENTION_DAYS: u64 = 30;
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

/// #48, F-24 — **why** one note is quarantined, published with the note.
///
/// # The finding
///
/// All three causes already composed a sentence naming themselves, and all three
/// sent it to the **model**: the secret screen's `secrets::write_notice(hits)`
/// and the two latch notices went into the tool result and into an
/// `injection_flag` activity row. The human — who is the only one that can
/// promote or discard — got text, a timestamp and two buttons. For a
/// secret-screen hold that inverts locked decision 22 outright: the note text
/// *is* the credential, so the card displayed the value and withheld the rule.
///
/// # Why it travels with the note and not by a join (locked decision 10, as
/// amended)
///
/// The obvious alternative — leave it in the `injection_flag` activity row and
/// join at render time — fails four ways, and the third is fatal on its own:
///
/// 1. Those rows carry no `note_id`, so there is no key to join on.
/// 2. The activity store is a capped per-lane ring, and its highest-volume lane
///    is `MemoryQuarantine` (one row per held write) — so the OLDEST unreviewed
///    notes, the ones most needing a reason, would be the first to lose it.
/// 3. The feed is user-clearable and per-row deletable. A reason a user can
///    delete without deleting the note is a reason that silently becomes
///    "Reason not recorded".
/// 4. Decision 22's action is literally *store*-and-quarantine. The reason is
///    part of what was stored, so it belongs in the stored row.
///
/// # Set at the STORE boundary
///
/// Built by [`Self::for_write`], which
/// [`GraphIndex::mem_add_note`](crate::graph::index::GraphIndex::mem_add_note)
/// calls on the facts it was handed — never by a caller. A caller-side
/// composition would re-open exactly the gap F-15 closes: a second write path
/// that stores a held note with no reason, or with the wrong one. It is also
/// what makes `tainted` and this field impossible to disagree — the store
/// derives *both* from this one value.
///
/// Mirrored in TypeScript as `NoteQuarantine` in `src/lib/graph.ts`, field for
/// field; `tests::the_quarantine_wire_shape_matches_the_frontend` is what keeps
/// the two from drifting.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoteQuarantine {
    /// The screen that held the write — the `outbound::Screen` slug. Always
    /// `memory_quarantine` today: all three causes are that one screen, which is
    /// why a note held by TWO of them needs no choice made here (see
    /// [`Self::for_write`]).
    pub screen: String,
    /// The rule identifiers that matched, e.g. `secret_aws_access_key_id`.
    ///
    /// **Legitimately EMPTY for the two latch causes**, which match no rule at
    /// all — so it is not the field the frontend tests for substantiveness, and
    /// nothing here may treat an empty `rules` as an absent reason.
    pub rules: Vec<String>,
    /// One sentence naming the cause, in the user's words — the field a human
    /// acts on, and therefore the one that must never arrive blank. Two
    /// sentences when two causes fired.
    pub reason: String,
}

impl NoteQuarantine {
    /// Compose the record for one write, or `None` when the note is not held.
    ///
    /// **Both causes are recorded when both fired.** `mcp::run_tool` used to
    /// collapse them into one `bool` (`tainted || !secrets.is_empty()`), so a
    /// note held by the latch *and* the credential screen lost one of the two —
    /// the same defect as F-9a (merging outcomes and discarding the
    /// incompleteness), in the one place that exists so a human can see why. The
    /// `reason` therefore carries both sentences, in cause order (the writing
    /// conversation's state first, then the note's own content), and `rules`
    /// carries the screen's hits regardless of the latch verdict.
    ///
    /// `screen` needs no choice made about it: all three causes are
    /// [`Screen::MemoryQuarantine`](crate::offload::outbound::Screen), which is
    /// the accurate name for every one of them (a memory write was held), so the
    /// singular field is not lossy.
    pub fn for_write(taint: WriteTaint, rules: &[String]) -> Option<Self> {
        // The gate is the verdict's own `is_quarantined` — the same predicate C2
        // wrote into the column — widened by the screen's hits. Not
        // `reasons.is_empty()`: that would make a verdict whose reason string went
        // missing silently *un*-quarantine the note, and of the two ways to be
        // wrong here only one is a security regression. `WriteTaint::hold` is what
        // makes the two impossible to disagree.
        if !taint.is_quarantined() && rules.is_empty() {
            return None;
        }
        let mut reasons: Vec<&str> = Vec::new();
        if let Some(r) = taint.review_reason() {
            reasons.push(r);
        }
        if !rules.is_empty() {
            reasons.push(super::secrets::SECRET_REVIEW_REASON);
        }
        Some(NoteQuarantine {
            screen: Screen::MemoryQuarantine.as_str().to_string(),
            rules: rules.to_vec(),
            reason: reasons.join(" "),
        })
    }
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
    /// V32 Phase C2 (locked decision 10): the note was written while its
    /// session was EXTERNAL-latched, so it is **quarantined** — stored, but
    /// excluded from every read path (`context_recall`, `context_notes`, the
    /// compaction carry-over, the distiller, and therefore auto-injection)
    /// until a human promotes it in the Memory UI.
    ///
    /// Orthogonal to `pinned` and to the V28 per-tab session scope: quarantine
    /// says *whether* the note may be read at all, the other two say where.
    /// Pre-C2 rows migrate as `false` (`GraphIndex::migrate_mem_note_tainted`).
    pub tainted: bool,
    /// #48, F-24: why this note is held, for the human who must decide about it.
    /// See [`NoteQuarantine`].
    ///
    /// `None` means *we cannot tell you why*, and the Memory view renders it as
    /// exactly that — never as "there was no reason". Three things produce it and
    /// all three are honest: a clean note (nothing to explain), a row written
    /// before this field existed (`GraphIndex::migrate_mem_note_quarantine`
    /// migrates those to `None` rather than synthesizing a cause it does not
    /// know), and a stored record that could not be read back.
    ///
    /// Always `None` on the clean-read path (`GraphIndex::mem_notes`), which is
    /// not a claim about the row but about the query: that path returns only
    /// `tainted == false` notes, and an unheld note has no hold to explain.
    pub quarantine: Option<NoteQuarantine>,
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
    /// **Quarantined notes are not here** — they are in [`Self::quarantined`].
    pub notes: Vec<MemNote>,
    /// V32 Phase C2: every quarantined note in the project, newest first —
    /// project-wide and session-independent, unlike [`Self::notes`], because
    /// the review queue must not hide a note behind "which session is current".
    /// This is the ONE read path a tainted note is allowed to reach, and its
    /// consumer is the Memory UI's promote-or-discard affordance.
    pub quarantined: Vec<MemNote>,
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
    if let Some(rest) = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("• "))
    {
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
/// result, vs. one `mem_event` per *classified* tool call), and deeper again
/// since sub-agent transcripts feed the same session's ring (V17.1): one
/// orchestration session fanning out to sub-agents can carry several hundred
/// turn rows plus their tool results (a real 8-agent milestone build measured
/// ~800 turns), and pruning here silently under-counts the session's spend.
pub const MAX_USAGE_PER_SESSION: i64 = 6000;

/// V24 Phase A: whether a recorded turn came from the main session transcript
/// (`Session`) or a sub-agent transcript (`Agent`) — the tap knows which
/// (sub-agent lines arrive via `<sid>/subagents/*.jsonl` or as inline
/// `isSidechain:true` lines), and this preserves that fact so the Usage chart
/// can show where agent fan-out spend went. Serialized as the wire strings
/// `"session"` / `"agent"` (the `usage_stat.origin` column and the `TurnUsage`
/// IPC mirror in `graph.ts`). Forward-only: pre-V24 rows migrate as `Session`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageOrigin {
    Session,
    Agent,
}

impl UsageOrigin {
    /// The wire string stored in `usage_stat.origin` and read back on load.
    /// Kept in lockstep with the `Serialize` `rename_all = "lowercase"` above
    /// (both feed the same `"session"`/`"agent"` contract).
    pub fn as_str(self) -> &'static str {
        match self {
            UsageOrigin::Session => "session",
            UsageOrigin::Agent => "agent",
        }
    }

    /// Parse the stored column back. Anything unexpected — including a legacy
    /// row that the migration defaulted to `"session"` — reads as `Session`.
    pub fn from_wire(s: &str) -> Self {
        match s {
            "agent" => UsageOrigin::Agent,
            _ => UsageOrigin::Session,
        }
    }
}

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
        /// Session (parent transcript) vs. Agent (sub-agent transcript / an
        /// inline `isSidechain:true` line). Set at the tap; `ToolResult` rows
        /// carry no origin (they're sized in chars, not attributed per turn).
        origin: UsageOrigin,
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
    /// Whether this turn was the main session or a sub-agent (V24 Phase A).
    /// Pre-V24 rows read `Session` (migrated default).
    pub origin: UsageOrigin,
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
    /// True when this session recorded no real Turn tokens at all (all four
    /// token totals are zero) — the table's "est" badge. V24 Phase E: derived
    /// from the totals, not the agent name, so a token-less pre-V24 OpenCode
    /// session keeps the badge while any session with real tokens loses it.
    pub est_only: bool,
    /// Session start / last-activity timestamps (epoch ms), from the
    /// `session` relation via [`SessionInfo`].
    pub started_ms: i64,
    pub last_ms: i64,
    /// Distinct model ids seen across the session's turns, descending by
    /// total tokens attributed to each (`"<synthetic>"` rows excluded).
    /// Empty when no turn carried a model (e.g. tool-result-only sessions).
    pub models: Vec<String>,
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

/// V24 Phase B: total tokens (all four categories summed) attributed to each
/// [`UsageOrigin`] within one model's spend in a session — how much was the
/// main session vs. sub-agent fan-out. Feeds the Cost card's per-model S/A
/// share line.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct OriginSplit {
    pub session_tok: u64,
    pub agent_tok: u64,
}

/// V24 Phase B: one model's contribution to a session — its summed token
/// totals plus the session/agent origin split. Ordered by total tokens
/// descending in [`SessionUsageDetail::per_model`], so a mixed-model session
/// (e.g. a Fable main + Opus sub-agents) is priced per model instead of at one
/// blended rate.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ModelUsage {
    pub model: String,
    pub totals: UsageTotals,
    pub origins: OriginSplit,
}

/// V24 Phase B: full drill-in detail for ONE session (the `graph_session_usage`
/// command) — ANY session, not just the current one. `row` is the same shape
/// the Sessions list shows; `turns`/`top_tools` reuse the current-session
/// queries parameterized by id; `per_model` prices mixed-model sessions
/// honestly. An unknown session id yields [`Self::empty`] (all-zero row, no
/// turns/tools/models) — never an error.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SessionUsageDetail {
    pub row: SessionUsageRow,
    pub turns: Vec<TurnUsage>,
    pub top_tools: Vec<ToolUsage>,
    pub per_model: Vec<ModelUsage>,
}

impl SessionUsageDetail {
    /// The empty detail for an unknown/absent session: an all-zero row that
    /// echoes the requested `session_id` (the IPC contract keeps `row`
    /// non-optional), no turns, no tools, no models. Callers read emptiness
    /// from the zero totals + empty vecs.
    pub fn empty(session_id: &str) -> Self {
        SessionUsageDetail {
            row: SessionUsageRow {
                session_id: session_id.to_string(),
                agent: String::new(),
                totals: UsageTotals::default(),
                tool_chars: 0,
                cache_hit_ratio: 0.0,
                est_only: true,
                started_ms: 0,
                last_ms: 0,
                models: Vec::new(),
            },
            turns: Vec::new(),
            top_tools: Vec::new(),
            per_model: Vec::new(),
        }
    }
}

/// The Effectiveness panel's three measured counters. `injected_chars` /
/// `deduped_chars` come from the V11-C in-memory injection/dedup accounting
/// (summed across every session currently resident there by
/// `GraphService::effectiveness_totals` — process-wide, not persisted, lost
/// on restart); `advisor_displaced_chars` comes from the V11-E read-advisor
/// Activity events in the `crate::activity` store, deliberately NOT from
/// `usage_stat` — `usage_stat` only knows resolved tool-result sizes, never
/// what a reminder *avoided* sending. NOTE: that store PERSISTS across
/// restarts (JSONL mirror), so the since-restart semantics of this counter
/// hinge on `effectiveness_totals`' `ts_ms >= process_start_ms()` filter —
/// do not remove that filter as "redundant". All three are measured
/// characters, not fabricated "savings" — the UI still labels every derived
/// (chars→tokens) number `est.`.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Effectiveness {
    pub injected_chars: u64,
    pub deduped_chars: u64,
    pub advisor_displaced_chars: u64,
    /// V16 Feature 4: WHOLE-FILE chars of reminded files the agent re-read
    /// via the shell anyway (`bypass` Activity events, est.) — what the
    /// bypasses actually re-spent. Display/audit only; NOT the netting
    /// subtrahend (different unit from `advisor_displaced_chars`, which
    /// sums reminder text).
    pub bypassed_chars: u64,
    /// V16 Feature 4 (review fix): reminder-TEXT chars of bypassed
    /// reminders — the like-for-like amount the UI subtracts from
    /// `advisor_displaced_chars` (same unit: both sum reminder text). Kept
    /// separate from `bypassed_chars` so the subtraction is visible AND
    /// unit-consistent.
    pub bypassed_advice_chars: u64,
    /// V16 Feature 9: the compounding readout — displaced chars re-counted
    /// on every subsequent retrieve turn (content kept out of context is
    /// saved again as a cache read each later turn). Root-scoped via the
    /// same session-map filtering as the other in-memory counters.
    pub compounded_chars: u64,
}

/// The Usage section's full IPC payload (`graph_usage`).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct UsageSnapshot {
    pub current: Option<SessionUsage>,
    pub sessions: Vec<SessionUsageRow>,
    pub effectiveness: Effectiveness,
    /// Completed `offload_task` runs served by a **local** backend — the
    /// milestone's "N tasks served locally" pointer to the Offload server
    /// dashboard. Filled in by the `graph_usage` IPC handler (this module has no
    /// dependency on `OffloadService`), `0` when offload is off/unused.
    pub offload_local_tasks: u64,
    /// V17 Phase E: the advertised tool-surface size for both consumers
    /// (MCP + offload worker), measured post-`lean_tools`-filter. Like
    /// `offload_local_tasks`, this is a cross-cutting field filled in by the
    /// `graph_usage` IPC handler (`crate::graph::surface_stats`), not by
    /// `GraphService` — it depends on live settings, not the index.
    pub surface: super::mcp::SurfaceStats,
    /// V24 Phase B: session ids that are live right now — the decided "open
    /// tabs + recency" set. A session qualifies when a live-session registry
    /// entry for it was refreshed within the registry TTL (a Claude tab still
    /// ticking, an OpenCode session still reporting) OR its last recorded
    /// activity falls within the recency window. Deduped. Drives the Sessions
    /// list's active markers; unlike `current` (a single most-recent session),
    /// this marks EVERY live session (a Claude tab and an OpenCode tab on the
    /// same project both show).
    pub active_session_ids: Vec<String>,
    /// The store error that made `sessions` empty, or `None` when the list is
    /// honest. Empty-is-not-absent: a project with no sessions yet and a
    /// project whose `graph.db` could not be opened/queried both render an
    /// empty list, and only this field tells them apart — the Overview shows
    /// "no sessions yet" for `None` and surfaces the failure otherwise, rather
    /// than reporting a swallowed error as "0 sessions". Wire contract:
    /// `store_error: string | null` (serde emits the field name as-is).
    pub store_error: Option<String>,
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

    /// The frontend's hand-mirror of these wire types, embedded at compile time —
    /// the same mechanism `settings::frontend_mirrors` uses, and for the same
    /// reason: nothing in `cargo` can see across the language boundary.
    const GRAPH_TS: &str = include_str!("../../../src/lib/graph.ts");

    /// #48, F-24 — `NoteQuarantine` and `MemNote.quarantine` must reach the
    /// frontend under the names the frontend reads.
    ///
    /// Only half of this is a source scan, and it is the half that has to be: the
    /// TypeScript side is parsed out of `graph.ts`, but the **Rust** side is taken
    /// from `serde_json` rather than from the struct's source text — so a
    /// `#[serde(rename)]`, a `skip_serializing_if`, or a field quietly removed is
    /// caught by what actually goes over the wire, not by a regex over the
    /// declaration.
    ///
    /// Two self-guards, both of which fail loudly rather than skipping:
    /// the interface must be found in `graph.ts` (a rename or a move panics
    /// instead of vacuously passing), and its field set must be non-empty.
    ///
    /// F-24 is the finding where the backend held the reason and the human never
    /// saw it. The failure mode this guards is the near-miss version: publishing a
    /// reason under a key the card does not read, which renders as *"Reason not
    /// recorded"* — indistinguishable, from the user's chair, from not publishing
    /// it at all.
    #[test]
    fn the_quarantine_wire_shape_matches_the_frontend() {
        /// The field names declared in `export interface <name> { … }`, in
        /// declaration order: every `ident:` at the top level of the block.
        fn ts_interface_fields(name: &str) -> Vec<String> {
            let decl = format!("export interface {name} {{");
            let at = GRAPH_TS.find(&decl).unwrap_or_else(|| {
                panic!(
                    "`{decl}` is not declared in src/lib/graph.ts — the mirror moved or was \
                     renamed, and this guard is now watching nothing. Point it at the new name."
                )
            });
            let body_start = at + decl.len();
            let body_len = GRAPH_TS[body_start..]
                .find("\n}")
                .unwrap_or_else(|| panic!("`{decl}`'s body is never closed"));
            let body = &GRAPH_TS[body_start..body_start + body_len];
            let mut out = Vec::new();
            for line in body.lines() {
                let line = line.trim();
                // Skip doc comments (`///`) and blank lines; take `name?: type;`.
                if line.starts_with("//") || line.is_empty() {
                    continue;
                }
                if let Some((lhs, _)) = line.split_once(':') {
                    let field = lhs.trim().trim_end_matches('?');
                    if !field.is_empty() && field.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        out.push(field.to_string());
                    }
                }
            }
            assert!(!out.is_empty(), "parsed no fields out of `{decl}`");
            out
        }

        /// The keys `serde` actually emits for a value.
        fn wire_keys(v: &serde_json::Value) -> Vec<String> {
            v.as_object()
                .expect("a struct serializes as an object")
                .keys()
                .cloned()
                .collect()
        }

        let q = NoteQuarantine {
            screen: "memory_quarantine".to_string(),
            rules: vec!["secret_aws_access_key_id".to_string()],
            reason: "Held by the write-time credential screen.".to_string(),
        };
        assert_eq!(
            wire_keys(&serde_json::to_value(&q).unwrap()),
            ts_interface_fields("NoteQuarantine"),
            "NoteQuarantine's wire shape and `src/lib/graph.ts`'s `NoteQuarantine` \
             have drifted — the Memory view reads the TypeScript names"
        );

        let note = MemNote {
            note_id: "n1".to_string(),
            session_id: "s1".to_string(),
            text: "held".to_string(),
            ts_ms: 1,
            pinned: false,
            tainted: true,
            quarantine: Some(q),
        };
        let v = serde_json::to_value(&note).unwrap();
        assert_eq!(
            wire_keys(&v),
            ts_interface_fields("MemNote"),
            "MemNote's wire shape and `src/lib/graph.ts`'s `MemNote` have drifted"
        );
        // The nested record survives serialization as an object under the key the
        // card reads, and a clean note's field is `null` — which `quarantineReason`
        // collapses to "no reason to show", never to "there was no reason".
        assert!(v.get("quarantine").unwrap().is_object());
        let clean = MemNote {
            quarantine: None,
            ..note
        };
        assert!(serde_json::to_value(&clean)
            .unwrap()
            .get("quarantine")
            .unwrap()
            .is_null());
    }

    /// #48, F-24 — the record is composed from the two causes, both of them, and
    /// only when the note is actually held.
    ///
    /// The reason a `for_write` unit test exists on top of the store's own
    /// end-to-end one: this is the function that decides `tainted`, so "held with
    /// no reason" and "a reason with no hold" are both single-expression mistakes
    /// here, and both are invisible at a call site.
    #[test]
    fn for_write_records_every_cause_and_nothing_when_clean() {
        use crate::offload::toolclass::{
            QUARANTINE_REVIEW_REASON, UNATTRIBUTED_REVIEW_REASON,
        };
        let aws = vec!["secret_aws_access_key_id".to_string()];

        // Not held: no verdict, no hits, no record.
        assert!(NoteQuarantine::for_write(WriteTaint::Clean, &[]).is_none());

        // The screen alone.
        let only = NoteQuarantine::for_write(WriteTaint::Clean, &aws).expect("held");
        assert_eq!(only.screen, "memory_quarantine");
        assert_eq!(only.rules, aws);
        assert_eq!(only.reason, super::super::secrets::SECRET_REVIEW_REASON);

        // The latch alone: a real reason with an EMPTY `rules`, which is the
        // legitimate empty the frontend is careful not to test for.
        let latch = NoteQuarantine::for_write(WriteTaint::Quarantined, &[]).expect("held");
        assert!(latch.rules.is_empty());
        assert_eq!(latch.reason, QUARANTINE_REVIEW_REASON);
        let un = NoteQuarantine::for_write(WriteTaint::Unattributed, &[]).expect("held");
        assert_eq!(un.reason, UNATTRIBUTED_REVIEW_REASON);

        // BOTH: both sentences, in cause order (the conversation's state, then
        // the note's own content), and the rules kept. Collapsing these into one
        // cause is the defect this function exists to prevent.
        let both = NoteQuarantine::for_write(WriteTaint::Quarantined, &aws).expect("held");
        assert_eq!(both.rules, aws);
        assert_eq!(
            both.reason,
            format!(
                "{QUARANTINE_REVIEW_REASON} {}",
                super::super::secrets::SECRET_REVIEW_REASON
            )
        );

        // Never blank, for any held combination — a blank reason renders as
        // "Reason not recorded", i.e. as this whole fix not having shipped.
        for (taint, rules) in [
            (WriteTaint::Quarantined, &[][..]),
            (WriteTaint::Unattributed, &[][..]),
            (WriteTaint::Clean, &aws[..]),
            (WriteTaint::Quarantined, &aws[..]),
        ] {
            let q = NoteQuarantine::for_write(taint, rules).expect("held");
            assert!(!q.reason.trim().is_empty(), "{taint:?}/{rules:?}");
            assert!(!q.screen.trim().is_empty(), "{taint:?}/{rules:?}");
        }
    }

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

    // ── V24 Phase A: origin wire-string tripwire ──────────────────────────
    // The `usage_stat.origin` column, the `UsageOrigin` serde form, and the TS
    // `TurnUsage.origin: 'session' | 'agent'` mirror (`src/lib/graph.ts`) must
    // all agree on these exact strings. Pin them so a rename can't silently
    // desync the wire.
    #[test]
    fn usage_origin_wire_strings_are_stable() {
        // Exhaustive match: adding a variant forces this test to be revisited.
        fn _exhaustive(o: UsageOrigin) {
            match o {
                UsageOrigin::Session | UsageOrigin::Agent => {}
            }
        }
        // serde (the `Turn` payload / `TurnUsage` IPC) and `as_str` (the stored
        // column) must produce the same strings.
        assert_eq!(
            serde_json::to_value(UsageOrigin::Session).unwrap(),
            serde_json::json!("session")
        );
        assert_eq!(
            serde_json::to_value(UsageOrigin::Agent).unwrap(),
            serde_json::json!("agent")
        );
        assert_eq!(UsageOrigin::Session.as_str(), "session");
        assert_eq!(UsageOrigin::Agent.as_str(), "agent");
        // Column round-trip; anything unexpected (incl. a migrated legacy row)
        // reads as `Session`.
        assert_eq!(UsageOrigin::from_wire("session"), UsageOrigin::Session);
        assert_eq!(UsageOrigin::from_wire("agent"), UsageOrigin::Agent);
        assert_eq!(UsageOrigin::from_wire("whatever"), UsageOrigin::Session);

        // `TurnUsage` serializes the origin under the `origin` key.
        let tu = TurnUsage {
            msg_id: "m".into(),
            model: None,
            in_tok: 1,
            out_tok: 0,
            cache_read: 0,
            cache_make: 0,
            tool_chars: 0,
            ts_ms: 0,
            origin: UsageOrigin::Agent,
        };
        let v = serde_json::to_value(&tu).unwrap();
        assert_eq!(v.get("origin").unwrap(), &serde_json::json!("agent"));
    }
}

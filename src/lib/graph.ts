// V9-01 code knowledge graph — frontend IPC wrappers for the Settings panel.
// The backend `GraphService` owns the on-disk index; these drive a rebuild and
// read its status. The live `graph-status` event (per-root build transitions)
// is consumed by the Phase-I monitor tab, not here.

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

/// Mirror of Rust `graph::index::LangCount` — indexed file count for one
/// language. Only languages with at least one file are present.
export interface LangCount {
  lang: string;
  files: number;
}

/// Mirror of Rust `graph::GraphStatus`.
export interface GraphStatus {
  root: string;
  state: 'idle' | 'building' | 'ready' | 'error';
  building: boolean;
  files_indexed: number;
  files: number;
  symbols: number;
  edges: number;
  // Indexed files grouped by language, biggest first.
  langs: LangCount[];
  last_error: string | null;
  // Whether file-watch re-indexing is currently paused (global toggle).
  watch_paused: boolean;
  // Phase G semantic-search status.
  semantic_enabled: boolean;
  embedder_configured: boolean;
  embedder_ready: boolean;
  embed_state: 'off' | 'idle' | 'embedding' | 'degraded' | 'error';
  embedded: number;
  embed_total: number;
  embed_pending: number;
  // V11 Phase G/F: code-embedding coverage + cached-digest count.
  code_embedded: number;
  code_embed_total: number;
  digests: number;
  embed_error: string | null;
}

/// Known per-root graph statuses (empty before the first build).
export function graphStatus(): Promise<GraphStatus[]> {
  return invoke<GraphStatus[]>('graph_status');
}

/// Trigger a full rebuild of the project's graph. `root` defaults (backend
/// side) to the app's launch directory. Returns immediately; progress lands on
/// the `graph-status` event.
export function graphRebuild(root?: string): Promise<void> {
  return invoke<void>('graph_rebuild', { root: root ?? null });
}

/// Force a full re-embed of the project's doc chunks (Phase G). No-op when
/// semantic search is off.
export function graphRebuildEmbeddings(root?: string): Promise<void> {
  return invoke<void>('graph_rebuild_embeddings', { root: root ?? null });
}

/// Open a native file/folder picker and return a project-relative
/// gitignore-style glob for the selection (anchored `/…`, trailing `/` for
/// folders), or null when the user cancels.
export function graphIgnorePick(folder: boolean): Promise<string | null> {
  return invoke<string | null>('graph_ignore_pick', { folder });
}

/// Pause/resume the incremental fs-watcher re-indexing. Returns the new state.
export function graphSetWatchPaused(paused: boolean): Promise<boolean> {
  return invoke<boolean>('graph_set_watch_paused', { paused });
}

/// Mirror of Rust `graph::LangCensus` — one language present in the project.
/// `supported && enabled` = green (indexed); `supported && !enabled` = yellow
/// (indexable but off); `!supported` = red (known-unsupported language, or the
/// `"other"` catch-all bucket).
export interface LangCensus {
  key: string;
  label: string;
  files: number;
  supported: boolean;
  enabled: boolean;
}

/// The project's language census for the Graph index sub-tab's language buttons (Tool Activity).
/// Walks the tree fresh, so call it on tab open and after a rebuild — not on a
/// tight poll. `root` defaults (backend side) to the launch directory.
export function graphLanguageCensus(root?: string): Promise<LangCensus[]> {
  return invoke<LangCensus[]>('graph_language_census', { root: root ?? null });
}

/// Add (`enabled=true`) or remove (`false`) a language from the graph's index
/// set, then kick a rebuild so it's indexed/embedded (or its rows dropped).
/// Only supported language tags are accepted. `root` defaults to the launch dir.
export function graphSetLanguageEnabled(
  lang: string,
  enabled: boolean,
  root?: string,
): Promise<void> {
  return invoke<void>('graph_set_language_enabled', { lang, enabled, root: root ?? null });
}

/// Result of an on-demand embedder reachability probe. Mirror of Rust
/// `graph::EmbedderProbe`.
export interface EmbedderProbe {
  ok: boolean;
  dim: number | null;
  message: string;
}

/// Probe the configured embedding endpoint without running a backfill — drives
/// the Graph index sub-tab's "Test connection" action. Always resolves; `ok` is false
/// (with `message` set) when the endpoint is unreachable or misconfigured.
export function graphTestEmbedder(): Promise<EmbedderProbe> {
  return invoke<EmbedderProbe>('graph_test_embedder');
}

/// One recorded graph tool call — a kind=graph projection of the persistent
/// activity store (Rust `activity::ActivityEntry`). The Tool Activity tab
/// uses the full store via `activity.ts`.
export interface GraphCall {
  /// Stable id in the activity store (survives restarts).
  id: number;
  ts_ms: number;
  /// Canonicalized project root the call ran against.
  root: string;
  /// Who made the call: a registry harness id (`harness_list`), or one of
  /// cImp's own services. A bare `string` since V40 Phase F — the harness half
  /// of this union was the frontend re-declaring the roster, and a call from a
  /// harness this build has not heard of must render, not fail to type-check.
  source: string;
  tool: string;
  target: string;
  chars: number;
  ms: number;
  ok: boolean;
}

/// Recent graph tool calls (harness sessions + offload worker), newest first.
/// The store spans every indexed root; pass `scoped: true` (with an optional
/// `root`, default the launch directory) to see one project's calls only.
/// `sinceTs` trims the response to entries newer than the caller's
/// high-water mark, so a steady poll isn't re-fetching hundreds of rows it
/// already has.
export function graphHistory(opts?: {
  root?: string;
  scoped?: boolean;
  sinceTs?: number;
}): Promise<GraphCall[]> {
  return invoke<GraphCall[]>('graph_history', {
    root: opts?.root ?? null,
    scoped: opts?.scoped ?? false,
    sinceTs: opts?.sinceTs ?? null,
  });
}

/// Subscribe to live per-root status transitions (emitted as one status at a
/// time). Returns an unlisten fn.
export function onGraphStatus(cb: (status: GraphStatus) => void): Promise<UnlistenFn> {
  return listen<GraphStatus>('graph-status', (e) => cb(e.payload));
}

/// V12 Phase F (6c): the analyses-auto trigger's live counts, emitted only
/// when they changed since the last completed index pass.
export interface GraphAnalyses {
  root: string;
  dead_exports: number;
  import_cycles: number;
}

/// Subscribe to `graph-analyses` (fires only on a count change, per-root).
/// Drives the Analyses section's "+N since last pass" badges.
export function onGraphAnalyses(cb: (a: GraphAnalyses) => void): Promise<UnlistenFn> {
  return listen<GraphAnalyses>('graph-analyses', (e) => cb(e.payload));
}

/// V10 (Analyses): one candidate dead export. Mirror of Rust `DeadExportRow`.
export interface DeadExportRow {
  name: string;
  kind: string;
  file: string;
  line: number;
  signature: string;
}

/// Candidate unused public symbols (public/exported defs with no reference and
/// no inbound call edge). Candidates only — dynamic dispatch, an external API,
/// macros, or reflection can produce false positives. `root` defaults to the
/// launch directory. On-demand (no polling).
export function graphDeadExports(root?: string): Promise<DeadExportRow[]> {
  return invoke<DeadExportRow[]>('graph_dead_exports', { root: root ?? null });
}

/// Import cycles between files — each entry is a loop of two or more files that
/// transitively import one another. `root` defaults to the launch directory.
export function graphCycles(root?: string): Promise<string[][]> {
  return invoke<string[][]>('graph_cycles', { root: root ?? null });
}

/// V12 Phase B (Analyses): one symbol changed since HEAD. Mirror of Rust
/// `ChangedSymbolRow`.
export interface ChangedSymbolRow {
  name: string;
  kind: string;
  file: string;
  line: number;
}

/// V12 Phase B (Analyses): one transitive dependent of a changed symbol.
/// Mirror of Rust `DependentRow`. `approx` is always true today — the call
/// graph is name-keyed, not id-resolved (same honesty convention as
/// `graph_references`).
export interface DependentRow {
  name: string;
  kind: string;
  file: string;
  line: number;
  depth: number;
  approx: boolean;
  /// V15 Feature 3: weakest edge confidence along the discovery chain
  /// (`extracted` | `inferred` | `ambiguous`).
  confidence: string;
}

/// V12 Phase B (Analyses): the working-tree diff's blast radius. Mirror of
/// Rust `ImpactResult`.
export interface ImpactResult {
  changed: ChangedSymbolRow[];
  dependents: DependentRow[];
  unindexed: string[];
}

/// "What does my current working-tree change affect?" — diff mode only (vs
/// HEAD). `root` defaults to the launch directory. Rejects with a
/// "not a git repository" message when `root` isn't a git repo.
export function graphImpact(root?: string): Promise<ImpactResult> {
  return invoke<ImpactResult>('graph_impact', { root: root ?? null });
}

/// V10 (Memory): one file in a session's working set. Mirror of Rust
/// `graph::memory::WorkingSetEntry`.
export interface WorkingSetEntry {
  path: string;
  touches: number;
  last_kind: string;
  last_ms: number;
  top_symbols: string[];
}

/// Why one note is quarantined — the screen that held it, the rules that
/// matched, and one sentence a human can act on.
///
/// Mirror of Rust `graph::memory::NoteQuarantine`, which is what publishes it.
///
/// **#48, F-24.** The Memory view's review queue used to show text + time +
/// Promote/Discard and nothing about the cause, which inverts locked decision 22:
/// for a secret-screen hold the note TEXT is the credential, so the card
/// displayed the secret value and withheld the rule name. The reason existed even
/// then, but it went into the tool result and an `injection_flag` activity row —
/// i.e. to the model, which cannot act on it, and not to the human, who must. It
/// now travels on the note row itself, so it cannot age out of a capped activity
/// lane (whose highest-volume lane is this very screen) and the user cannot clear
/// it without deleting the note.
///
/// # Composed at the STORE boundary, never by a caller
///
/// `NoteQuarantine::for_write(taint, rules)` builds it and
/// `GraphIndex::mem_add_note` calls it on the facts it was handed. No write path
/// composes its own, and that placement is the whole point: a caller-side version
/// re-opens exactly the gap F-15 closes — a second write path that stores a note
/// unscreened, or held with no reason, or held with the wrong one. It is also why
/// `MemNote.tainted` and this record cannot disagree: the store derives both from
/// this one value.
///
/// # What therefore arrives here
///
/// - **Both causes when both fired.** A note held by the session taint latch
///   *and* the credential screen carries BOTH sentences in `reason` — the latch's
///   first (the writing conversation's state, then the note's own content) — and
///   the screen's hits in `rules` regardless of the latch verdict. Collapsing the
///   two into a single `bool` was the original defect; nothing on this side may
///   re-collapse them, so render `reason` whole rather than parsing it.
/// - **`screen` is singular and not lossy.** All three causes are the one
///   `memory_quarantine` screen — the accurate name for every one of them (a
///   memory write was held) — so no choice is being hidden by the single field.
/// - **A pre-migration row arrives as `null`,** and must render as the
///   placeholder. `GraphIndex::migrate_mem_note_quarantine` migrates rows written
///   before the column existed to no-reason rather than synthesizing a cause it
///   does not know.
///
/// Still deliberately NOT reconstructed on this side when it is absent: the
/// activity rows carry no `note_id`, so a join would be a guess, and the note
/// text cannot be re-screened here — a plausible-but-wrong cause in a security
/// UI is worse than a blank field (#48, F-23 is that exact mistake elsewhere).
/// [`quarantineReason`] below is the one predicate for "is there a reason to
/// show", and it collapses missing / `null` / blank toward that blank.
export interface NoteQuarantine {
  /// The screen that held the write — the `outbound::Screen` slug, e.g.
  /// `memory_quarantine`.
  screen: string;
  /// The rule identifiers that matched, e.g. `secret_aws_access_key_id`. EMPTY
  /// for the two latch causes, which match no rule at all — a legitimate empty,
  /// which is why it is not the field [`quarantineReason`] tests for
  /// substantiveness.
  rules: string[];
  /// One sentence naming the cause, in the user's words. The field a human acts
  /// on, so it is the one that must never arrive blank.
  reason: string;
}

/// A remembered note. Mirror of Rust `graph::memory::MemNote`.
export interface MemNote {
  note_id: string;
  session_id: string;
  text: string;
  ts_ms: number;
  pinned: boolean;
  /// V32 Phase C2: written while its session was externally tainted, so it is
  /// quarantined — stored but excluded from every read path (recall, listings,
  /// the compaction carry-over, the fact distiller and therefore auto-injection)
  /// until promoted here. Always `false` for entries in `MemorySnapshot.notes`
  /// and `true` for entries in `MemorySnapshot.quarantined`.
  tainted: boolean;
  /// #48, F-24: why this note is held, for the human who must decide about it —
  /// see [`NoteQuarantine`]. Published with the note; the backend emits the key
  /// on every note and sends `null` rather than omitting it, so the `?` here is
  /// for the older backends that omit it entirely.
  ///
  /// Absent or `null` means *we cannot tell you why*, and it is never a claim
  /// that there was no reason. Three things produce it, all honest: a clean note
  /// (no hold to explain, which is every note in `MemorySnapshot.notes`), a row
  /// written before the column existed, and a stored record that could not be
  /// read back. Read it through [`quarantineReason`], never directly.
  quarantine?: NoteQuarantine | null;
}

/// The reason to SHOW for a quarantined note, or `null` when there is none to
/// show.
///
/// **"Empty is not absent" applied, and the direction chosen deliberately**
/// (#48, F-24). Three inputs collapse to `null` here — the field missing (an
/// older backend), the field `null`, and the field present with a blank or
/// whitespace-only `reason` — and the card renders that one `null` as *"Reason
/// not recorded"*, never as "this note had no reason". Collapsing them is safe
/// only because they are collapsed toward the HONEST end: every one of them
/// means *we cannot tell you why*, and none of them may render as an
/// explanation. A present-but-blank `reason` is a backend defect, and treating
/// it as absent is what makes it visible instead of invisible.
///
/// `rules` is NOT part of the predicate: the latch and unattributed-write causes
/// legitimately match no rule, so requiring one would suppress a real reason.
export function quarantineReason(n: MemNote): NoteQuarantine | null {
  const q = n.quarantine;
  if (!q) return null;
  return q.reason.trim() === '' ? null : q;
}

/// A session summary row. Mirror of Rust `graph::memory::SessionInfo`.
export interface SessionInfo {
  session_id: string;
  agent: string;
  started_ms: number;
  last_ms: number;
  events: number;
}

/// The project's full memory readout. Mirror of Rust
/// `graph::memory::MemorySnapshot`.
export interface MemorySnapshot {
  current_session: string | null;
  working_set: WorkingSetEntry[];
  /// Clean notes only — quarantined ones are in `quarantined`.
  notes: MemNote[];
  /// V32 Phase C2: the project-wide quarantine review queue. This UI is the only
  /// reader allowed to see these notes; promote or discard each one.
  quarantined: MemNote[];
  sessions: SessionInfo[];
}

/// The project's session/action memory. `root` defaults to the launch directory.
export function graphMemory(root?: string): Promise<MemorySnapshot> {
  return invoke<MemorySnapshot>('graph_memory', { root: root ?? null });
}

// ── V15 Feature 1: path tracing ──────────────────────────────────────────

/// One node on a traced path. Mirror of Rust `PathNodeRow`. `edge_to_next` /
/// `confidence` describe the edge leaving this node toward the next; both are
/// null on the final node. `line` is 0 for a file node (`kind === 'file'`).
export interface PathNodeRow {
  id: string;
  label: string;
  file: string;
  line: number;
  kind: string;
  edge_to_next: string | null;
  confidence: string | null;
}

/// The result of a `graph_path` trace. Mirror of Rust `PathResult`.
export interface PathResult {
  found: boolean;
  nodes: PathNodeRow[];
  hops: number;
  equal_alternatives: number;
}

/// V15 Feature 1: trace the shortest path between two entities (`from`/`to` are
/// a symbol name, `file:line`, or file path). `kinds` restricts the edge types
/// traversed (subset of `call`/`import`/`contains`; default all). `symmetric`
/// walks edges undirected. `root` defaults to the launch directory.
export function graphPath(
  from: string,
  to: string,
  opts?: { kinds?: string[]; symmetric?: boolean; root?: string },
): Promise<PathResult> {
  return invoke<PathResult>('graph_path', {
    root: opts?.root ?? null,
    from,
    to,
    kinds: opts?.kinds ?? null,
    symmetric: opts?.symmetric ?? null,
  });
}

// ── V15 Feature 2: architecture overview ─────────────────────────────────

/// A hub in the architecture overview. Mirror of Rust `GodNodeRow`.
export interface GodNodeRow {
  id: string;
  label: string;
  file: string;
  kind: string;
  degree: number;
}

/// A subsystem (file community). Mirror of Rust `SubsystemRow`. `files` is a
/// bounded sample of members; `name` is the derived common-prefix label.
export interface SubsystemRow {
  name: string;
  size: number;
  files: string[];
  hub: string;
}

/// An edge crossing subsystem boundaries. Mirror of Rust `SurprisingRow`.
export interface SurprisingRow {
  from: string;
  to: string;
  kind: string;
  from_subsystem: string;
  to_subsystem: string;
}

/// The architecture overview. Mirror of Rust `ArchResult`.
export interface ArchResult {
  god_nodes: GodNodeRow[];
  subsystems: SubsystemRow[];
  surprising: SurprisingRow[];
}

/// V15 Feature 2: the system-shape overview (god nodes, subsystems, surprising
/// edges). Heuristic clustering — advisory, not authoritative. `root` defaults
/// to the launch directory.
export function graphArchitecture(root?: string): Promise<ArchResult> {
  return invoke<ArchResult>('graph_architecture', { root: root ?? null });
}

// ── V15 Feature 4: Graph View snapshot ───────────────────────────────────

/// One node in the Graph View snapshot. Mirror of Rust `VizNodeRow`.
/// The snapshot is FILE-level: every node is a file (`kind === 'file'`,
/// `label === file`). `degree` = node size; `subsystem` = node color.
export interface VizNodeRow {
  id: string;
  label: string;
  file: string;
  kind: string;
  degree: number;
  subsystem: string;
}

/// One edge in the Graph View snapshot. Mirror of Rust `VizEdgeRow`.
/// `kind` = edge color (call/import — calls are rolled up to file→file);
/// `confidence` = dash pattern.
export interface VizEdgeRow {
  src: string;
  dst: string;
  kind: string;
  confidence: string;
  /// `false` = over the per-node drawn quota: shown in the connections
  /// panel / selection highlight, but not as an ambient line.
  drawn: boolean;
}

/// A bounded {nodes, edges} subgraph for the Graph view (Tool Activity tab).
/// Mirror of Rust `VizGraphResult`.
export interface VizGraphResult {
  nodes: VizNodeRow[];
  edges: VizEdgeRow[];
}

/// V15 Feature 4: fetch the bounded FILE-level subgraph for the Graph View
/// tab (top-degree files + rolled-up edges among them, capped at
/// `graph_viz_max_nodes`). `root` defaults to the launch directory.
export function graphVizSnapshot(root?: string): Promise<VizGraphResult> {
  return invoke<VizGraphResult>('graph_viz_snapshot', { root: root ?? null });
}

/// Per-file Graph View presence. Mirror of Rust `VizFileStatusRow`.
export interface VizFileStatus {
  path: string;
  /// The file exists in the graph index at all.
  indexed: boolean;
  /// Rolled-up file-level call/import degree (0 = nothing to jump to).
  degree: number;
}

/// Workbench ⌖ support: per-file Graph View presence for a batch of
/// repo-relative paths — drives the jump button's enabled state.
export function graphVizFileStatus(paths: string[], root?: string): Promise<VizFileStatus[]> {
  return invoke<VizFileStatus[]>('graph_viz_file_status', { root: root ?? null, paths });
}

/// Workbench ⌖ support: the 1-hop FILE ego of `path` regardless of the
/// snapshot's top-N-by-degree cut — merged into the rendered graph
/// temporarily when a jump targets a file the snapshot dropped. Empty when
/// the file isn't indexed; a lone node when it has no connections.
export function graphVizEgo(path: string, root?: string): Promise<VizGraphResult> {
  return invoke<VizGraphResult>('graph_viz_ego', { root: root ?? null, path });
}

/// Clear one session's memory (`session` = its id) or the whole project's
/// (`session` omitted).
export function graphMemoryClear(session?: string, root?: string): Promise<void> {
  return invoke<void>('graph_memory_clear', { root: root ?? null, session: session ?? null });
}

/// Pin/unpin a note (pinned notes survive session eviction, show project-wide).
export function graphNoteSetPinned(noteId: string, pinned: boolean, root?: string): Promise<void> {
  return invoke<void>('graph_note_set_pinned', { root: root ?? null, noteId, pinned });
}

/// V32 Phase C2: resolve one quarantined note — `promote` clears the taint (the
/// note becomes ordinary memory, keeping its pinned state), `discard` deletes it.
export function graphNoteReview(
  noteId: string,
  action: 'promote' | 'discard',
  root?: string,
): Promise<void> {
  return invoke<void>('graph_note_review', { root: root ?? null, noteId, action });
}

/// V10 (Context): the result of a context retrieval. Mirror of Rust
/// `graph::context::RetrieveResult`.
export interface RetrieveResult {
  context_md: string;
  files_used: string[];
  chars: number;
  tokens_est: number;
}

/// Preview what context injection WOULD prepend for `prompt` (bypasses the
/// injection toggle, so it works while injection is off). `root` defaults to the
/// launch directory.
export function graphContextPreview(prompt: string, root?: string): Promise<RetrieveResult> {
  return invoke<RetrieveResult>('graph_context_preview', { root: root ?? null, prompt });
}

/// V12 Phase E (Memory): a durable project fact. Mirror of Rust
/// `graph::memory::ProjectFact`.
export interface ProjectFact {
  fact_id: string;
  text: string;
  /// The session that produced this fact, or `"manual"` for a UI-added fact.
  source_session: string;
  ts_ms: number;
  pinned: boolean;
  archived: boolean;
}

/// The project's live (non-archived) durable facts, pinned first then newest.
/// `root` defaults to the launch directory.
export function graphFacts(root?: string): Promise<ProjectFact[]> {
  return invoke<ProjectFact[]>('graph_facts', { root: root ?? null });
}

/// Pin / unpin / archive / delete one project fact. `root` defaults to the
/// launch directory.
export function graphFactUpdate(
  id: string,
  action: 'pin' | 'unpin' | 'archive' | 'delete',
  root?: string,
): Promise<void> {
  return invoke<void>('graph_fact_update', { root: root ?? null, id, action });
}

/// Manually add a project fact from the Facts UI's "add fact" input.
/// `root` defaults to the launch directory.
export function graphFactAdd(text: string, pin?: boolean, root?: string): Promise<void> {
  return invoke<void>('graph_fact_add', { root: root ?? null, text, pin: pin ?? null });
}

// ── V14 Phase D/D2: Usage section (token X-ray) + budget-tuning advisor ───

/// One tool's ranked contribution to a session (the Usage section's "top
/// consumers" table). Mirror of Rust `graph::ToolUsage`. `est_tokens` is a
/// `chars / 4` estimate — always render it labeled `est.`; `calls` is exact.
export interface ToolUsage {
  tool: string;
  est_tokens: number;
  calls: number;
}

/// One turn's token breakdown. Mirror of Rust `graph::memory::TurnUsage`
/// (V14 Phase C/D). `tool_chars` is the estimated tool-result characters
/// that arrived just before this turn — divide by 4 for the "est. tool"
/// stacked-bar segment.
export interface TurnUsage {
  msg_id: string;
  model: string | null;
  in_tok: number;
  out_tok: number;
  cache_read: number;
  cache_make: number;
  tool_chars: number;
  ts_ms: number;
  /// V24 Phase A: whether this turn was the main session or a sub-agent.
  /// Pre-V24 rows read 'session'. Mirror of Rust `graph::memory::UsageOrigin`.
  origin: 'session' | 'agent';
}

/// Summed token totals across a session's turns. Mirror of Rust
/// `graph::memory::UsageTotals`.
export interface UsageTotals {
  in_tok: number;
  out_tok: number;
  cache_read: number;
  cache_make: number;
}

/// The current (most-recently-active) session's usage readout. Mirror of
/// Rust `graph::memory::SessionUsage`.
export interface SessionUsage {
  session_id: string;
  turns: TurnUsage[];
  totals: UsageTotals;
  top_tools: ToolUsage[];
}

/// One project session's totals row (Sessions table). Mirror of Rust
/// `graph::memory::SessionUsageRow`.
export interface SessionUsageRow {
  session_id: string;
  agent: string;
  totals: UsageTotals;
  tool_chars: number;
  cache_hit_ratio: number;
  /// True when this session recorded no real Turn tokens (all four token
  /// totals are zero) — the table's "est" badge. V24 Phase E: token-less
  /// sessions (a harness that reported no tokens) keep it; any session with
  /// real tokens loses it.
  est_only: boolean;
  /// Session start / last-activity timestamps (epoch ms).
  started_ms: number;
  last_ms: number;
  /// Distinct model ids seen across the session's turns, descending by
  /// tokens attributed to each; empty when no turn carried a model.
  models: string[];
}

/// V24 Phase B: total tokens (all four categories summed) per usage origin —
/// how much of a model's session spend was the main session vs. sub-agents.
/// Mirror of Rust `graph::memory::OriginSplit`.
export interface OriginSplit {
  session_tok: number;
  agent_tok: number;
}

/// V24 Phase B: one model's contribution to a session — summed totals plus the
/// session/agent origin split, ordered by tokens desc in
/// `SessionUsageDetail.per_model`. Mirror of Rust `graph::memory::ModelUsage`.
export interface ModelUsage {
  model: string;
  totals: UsageTotals;
  origins: OriginSplit;
}

/// V24 Phase B: full drill-in detail for one session (any session, not just the
/// current one) — the `graph_session_usage` payload. Mirror of Rust
/// `graph::memory::SessionUsageDetail`. An unknown session id returns an empty
/// detail (all-zero `row`, empty arrays).
export interface SessionUsageDetail {
  row: SessionUsageRow;
  turns: TurnUsage[];
  top_tools: ToolUsage[];
  per_model: ModelUsage[];
}

/// The Effectiveness panel's measured counters — all exact chars, no
/// fabricated savings figure. Mirror of Rust `graph::memory::Effectiveness`.
export interface Effectiveness {
  injected_chars: number;
  deduped_chars: number;
  advisor_displaced_chars: number;
  /// V16 Feature 4: WHOLE-FILE chars of reminded files re-read via the
  /// shell anyway (est.) — what the bypasses re-spent. Display/audit only;
  /// NOT the netting subtrahend (different unit from
  /// `advisor_displaced_chars`, which sums reminder text).
  bypassed_chars: number;
  /// V16 Feature 4: reminder-TEXT chars of bypassed reminders — the
  /// like-for-like amount to subtract from `advisor_displaced_chars`
  /// (both sum reminder text).
  bypassed_advice_chars: number;
  /// V16 Feature 9: displaced chars re-counted on every later retrieve turn
  /// ("content kept out of context is saved again each turn").
  compounded_chars: number;
}

/// The advertised tool-surface size for both consumers (MCP + offload worker),
/// measured post-`lean_tools`-filter. Mirror of Rust `graph::mcp::SurfaceStats`.
export interface SurfaceStats {
  mcp_tools: number;
  mcp_chars: number;
  offload_tools: number;
  offload_chars: number;
}

/// The Usage section's full payload. Mirror of Rust `graph::UsageSnapshot`.
export interface UsageSnapshot {
  current: SessionUsage | null;
  sessions: SessionUsageRow[];
  effectiveness: Effectiveness;
  offload_local_tasks: number;
  /// V17 Phase E: measured tool-surface size (est. tokens ≈ chars / 4).
  surface: SurfaceStats;
  /// V24 Phase B: session ids live right now (open tabs + recency) — the
  /// Sessions list's active markers. Deduped; marks EVERY live session, not
  /// just the single most-recent `current`.
  active_session_ids: string[];
  /// Non-null ⇒ the store could not be read this tick, so this snapshot is
  /// NOT authoritative (`sessions` may be empty purely because the read
  /// failed). Consumers must keep their last-good snapshot instead of
  /// applying this one. `null` with an empty `sessions` is a genuine empty
  /// store and MUST render as 0 — empty is not absent.
  store_error: string | null;
}

/// The Usage section's token X-ray for `root` (defaults to the launch
/// directory).
export function graphUsage(root?: string): Promise<UsageSnapshot> {
  return invoke<UsageSnapshot>('graph_usage', { root: root ?? null });
}

/// V34: the session the tab keyed `tab` is currently working in, or `null` when
/// the app cannot PROVE one — an unpinned tab sharing a project with another
/// agent tab, a tab that hasn't started, or a non-agent tab. Callers must treat
/// `null` as "no answer" and fall back, never as "no session".
export function graphTabSession(tab: string): Promise<string | null> {
  return invoke<string | null>('graph_tab_session', { tab });
}

/// V24 Phase B: full drill-in detail for one session under `root` (defaults to
/// the launch directory) — totals row, per-turn series, top-tools, and
/// per-model totals with the S/A origin split. An unknown `sessionId` resolves
/// to an empty detail.
export function graphSessionUsage(root: string | undefined, sessionId: string): Promise<SessionUsageDetail> {
  return invoke<SessionUsageDetail>('graph_session_usage', { root: root ?? null, sessionId });
}

/// One budget-tuning proposal. Mirror of Rust `advisor::Proposal`.
/// `signature` is opaque to the UI — pass it straight through to
/// `advisorDismiss` unchanged, and never parse it.
///
/// `rule_id` is NOT unique within a payload since V35 Phase E: every
/// harness-capability drift notice speaks as `drift.capability.v1` and is told
/// apart by `signature` (and named by `capability`). Anything that identifies a
/// proposal — the `{#each}` key, the busy marker, the local drop after an
/// action — must use the (rule_id, signature) pair.
export interface AdvisorProposal {
  setting: string;
  current: string;
  proposed: string;
  rationale: string;
  rule_id: string;
  signature: string;
  /// V16: warn-only drift canaries render no Apply button (`setting` is
  /// empty).
  warn_only: boolean;
  /// V16: bespoke card action — currently only `"mark_verified"` (the
  /// harness version tripwire → `harnessMarkVerified`).
  action: string | null;
  /// V35 Phase E: for a `drift.capability.v1` notice, the harness capability id
  /// it is about (`harness::contract::Capability::id`) — the same join key the
  /// Settings window's gate lookup uses. `null` for every other rule.
  capability: string | null;
  /// V40 Phase C: the harness this notice is ABOUT, for the drift rules that
  /// evaluate per registered harness. `null` for every rule that is not about
  /// one.
  ///
  /// Passed straight back to `harnessMarkVerified` — before this, the card's
  /// "Mark verified" wrote the default harness's row whatever notice it sat
  /// under, and the only reason that was never wrong is that only one harness
  /// could raise the notice at all.
  harness: string | null;
}

/// Mirror of Rust `ipc::commands::AdvisorSnapshot`. `collecting` distinguishes
/// "not enough data yet" from "checked, all healthy" (an empty `proposals`
/// with `collecting: false`).
export interface AdvisorSnapshot {
  proposals: AdvisorProposal[];
  collecting: boolean;
}

/// The budget-tuning advisor's current proposals for `root` (defaults to the
/// launch directory).
export function graphUsageAdvice(root?: string): Promise<AdvisorSnapshot> {
  return invoke<AdvisorSnapshot>('graph_usage_advice', { root: root ?? null });
}

/// Dismiss one proposal (`ruleId` + its `signature`, both echoed verbatim
/// from the `AdvisorProposal` the user clicked Dismiss on).
export function advisorDismiss(ruleId: string, signature: string): Promise<void> {
  return invoke<void>('advisor_dismiss', { ruleId, signature });
}

/// Record that a proposal was APPLIED, starting its cooldown: the rule stays
/// quiet for a few sessions so fresh post-change data can accumulate before
/// the advisor re-evaluates (the rates are cumulative — an immediate
/// re-proposal would be judging the OLD value's data). Called right after
/// the `applySettings` that wrote the proposed value. `root` defaults to the
/// launch directory, matching `graphUsageAdvice`.
export function advisorMarkApplied(ruleId: string, root?: string): Promise<void> {
  return invoke<void>('advisor_mark_applied', { ruleId, root: root ?? null });
}

/// V16 Feature 1: stamp the currently-seen version of `harness` as verified
/// (the Advisor card's "Mark verified" — the user just re-ran the
/// MAINTENANCE.md contract checks).
///
/// **The harness is REQUIRED** (V40 Phase F, locked decision 23). Phase C added
/// the argument with a wire-compat default; the caller passes the id the button
/// actually sits under now, so a card raised for one harness can never stamp
/// another's row. The backend still accepts the absent form for older callers;
/// there are none left in this window.
export function harnessMarkVerified(harness: string): Promise<void> {
  return invoke<void>('harness_mark_verified', { harness });
}

/// One rule's row in the Advisor's rule reference. Mirror of Rust
/// `advisor::RuleReference`.
export interface RuleReference {
  id: string;
  thresholds: string;
}

/// Mirror of Rust `ipc::commands::AdvisorRules`.
export interface AdvisorRules {
  rules: RuleReference[];
  footer: string;
}

/// **The advisor's rule reference, from the backend** (V40 Phase F, locked
/// decision 23). The panel used to restate every threshold — and one harness's
/// mechanisms — in a hard-coded tooltip.
export function advisorRules(): Promise<AdvisorRules> {
  return invoke<AdvisorRules>('advisor_rules');
}

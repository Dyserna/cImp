# IMPL-PLAN V11 — Token Efficiency (Context Engine II)

Companion to `docs/MILESTONE-V11-token-efficiency.md`. File-by-file build plan:
structs, schema, tools, routes, UI wiring, per-phase checklists. The milestone's
open decisions are **assumed at their proposed defaults** (Claude-first hook
posture with OpenCode degradation; read-advisor default off; 16 KiB
`max_body_bytes`) — revisit the plan sections marked ⚠ if a decision flips.

Phases: **A** (`graph_snippet`) → **B** (repo map) → **C** (injection dedup) →
**D0** (PreCompact spike) → **D** (compaction survival) → **E** (read advisor) →
**F** (LLM digests) → **G** (code embeddings) → **H** (docs/tests/release).

Grounding anchors (verified against current `develop`, post-V10):
- IR: `graph/model.rs:371` (`Symbol` with `start_line`/`end_line`/`visibility`).
- Store: `graph/schema.rs` (`RELATIONS`, `GRAPH_SCHEMA_VERSION` reset-migration
  from V10 §0), `graph/index.rs` (`SymbolHit:25`, `semantic_doc_search:903`,
  `embedding_coverage:951`, pending-chunk scan `:858`, doc_vec epochs `:696+`).
- Tools: `graph/mcp.rs` (`tool_specs`, `dispatch_recorded`, `run_tool`,
  `fmt_symbols`), forward-to-warm-path at `offload/mcp.rs:710-729`.
- Context/memory: `graph/context.rs` (`retrieve`), `graph/memory.rs`
  (event classification `:70`), `graph/service.rs` (`/graph_run` backing `:282`,
  memory recording `:385`, injection gate note `:474`).
- Loopback: `offload/loopback.rs:345-347` (`/graph_run`, `/context/retrieve`,
  `/memory/event` route match), `handle_context_retrieve:591`.
- Hook shims: `context_hook.rs` (`--context-hook`, raw-HTTP POST at `:74`),
  hook command builder `statusline/mod.rs:50`, settings-overlay hooks object
  `tabs/config.rs:199-218`, OpenCode plugin writer `tabs/config.rs:395-460`.
- Offload: `offload/supervisor.rs:511` (`run_task` — local-only, slot-gated,
  agent loop), `offload/openai.rs` (completion client), embedder
  `graph/embed.rs:44` (`Embedder::new(endpoint, model)`).
- Settings: `settings/schema.rs:873` (`GraphSettings`), V10 context fields at
  `:926+`.
- Frontend: `lib/CodeIntelligenceView.svelte` (5-section router),
  `lib/graph.ts`, `lib/ToolsReference.svelte` (`GRAPH_TOOLS`).

---

## 0. Cross-cutting: one schema bump for the whole milestone

New relations land in phases C (`injected`), F (`digest`), G (`code_chunk`,
`code_vec`). Bump `GRAPH_SCHEMA_VERSION` **once** (V10 == 2 → 3) in the first
phase that ships a relation (C), and create *all* V11 relations in that bump —
later phases then add code, not schema, and users rebuild once, not three
times. The V10 §0 reset-migration mechanism is reused unchanged.

Coordination note: if V12 (its `is_test` column / `commit_touch` /
`project_fact`) is being developed in parallel, fold its relations into the
same bump.

---

## Phase A — `graph_snippet`

**A1. Index lookup** (`graph/index.rs`):
- `SymbolHit` (`:25`) gains `pub end_line: u32` — select it everywhere the hit
  is built (`find_symbol`, `outline`, …); it's already in the `symbol` relation.
- New `pub fn symbol_at(&self, file: &str, line: u32) -> AppResult<Option<SymbolHit>>`
  — smallest enclosing span: `*symbol{file, start_line <= line, end_line >= line}`,
  order by `end_line - start_line` asc, limit 1. (If V10 Phase C already added
  an enclosing-symbol helper for memory events, reuse and just widen its return
  type.)
- New `pub fn callers_count(&self, name_or_id: &str) -> AppResult<u64>` (cheap
  aggregate for the snippet header).

**A2. Slice + format** (`graph/mcp.rs`):
- Tool spec `graph_snippet { symbol?, file?, line?, context_lines? }` in
  `tool_specs` (both consumers; same gating as every graph tool).
- `run_tool` arm:
  1. Resolve target: `symbol` → `find_symbol` (0 hits → "not found"; >1 hits →
     return the disambiguation list via `fmt_symbols`, *no body*); `file+line`
     → `symbol_at`.
  2. Read the file from disk (project-root-joined, canonicalize + confine to
     root — same path discipline as `read_file` in `offload/tools/read_file.rs`).
  3. Slice `start_line..=end_line` ± `context_lines` (default 0), cap at
     `graph.max_body_bytes`; truncated ⇒ append `… [truncated at N bytes]`.
  4. Header line: `<file>:<start>-<end> · <kind> · <visibility> · <callers> callers`.
  5. Staleness: compare file mtime to the index's last-build stamp
     (`GraphStatus` already carries it); newer ⇒ prepend
     `note: file changed after last index; span may have drifted`.
  6. Whole-file span (start==1 && end>=file_lines−1) ⇒ return outline + a
     `use Read with offset/limit` hint instead of the body.

**A3. Settings** (`settings/schema.rs`, `GraphSettings`):
`pub max_body_bytes: u32` (default `16_384`), placed beside
`max_snippet_bytes`; defaults fn + frontend type + a numeric field in the
Settings "Code Intelligence" section.

**A4. Guidance** (`tabs/config.rs`): extend `GRAPH_GUIDANCE` + the OpenCode
instructions writer: *"For files > ~300 lines prefer `graph_outline` →
`graph_snippet` over reading the whole file."*

**A5. UI:** add `graph_snippet` to `GRAPH_TOOLS` in `ToolsReference.svelte`
with a one-line example. Activity ring records it automatically via
`dispatch_recorded`.

**A6. Tests** (`graph/mcp.rs` + `graph/index.rs`): name-resolve happy path;
ambiguous name returns list not body; file+line resolves smallest enclosing
symbol; byte cap truncates; whole-file span falls back to outline; path
escape (`../`) rejected.

---

## Phase B — `graph_repo_map`

**B1. Centrality** (`graph/index.rs`):
`pub fn file_centrality(&self, max: usize) -> AppResult<Vec<(String, u64)>>` —
count inbound `call` + `import` edges grouped by the *target symbol's file*
(join `edge` → `symbol` on dst), Datalog aggregate, order desc, limit.

**B2. Renderer** (`graph/context.rs`):
```rust
pub fn repo_map(graph: &GraphService, root: &Path, budget_chars: usize,
                session_boost: Option<&str>) -> String
```
- Rank files by centrality; when `session_boost` is a session id, multiply by
  the V10 working-set recency factor (reuse `mem_working_set`).
- Per file emit `- path — sig; sig; sig` using `outline`-style signatures,
  `visibility == public` first, greedy-pack to `budget_chars`.
- Header `## Project map (cImp)`.

**B3. Cache** (`graph/service.rs`): `repo_map_cache: Mutex<HashMap<PathBuf, String>>`
keyed by root; invalidate where the rebuild/watcher pass completes (the same
place `patch_status` announces a finished index pass). Renders are cheap; cache
is a nicety, not a correctness need.

**B4. Tool:** `graph_repo_map { budget_chars? }` in `tool_specs` → `run_tool`
arm → `repo_map(...)` with default `graph.repo_map_budget_chars`.

**B5. Session-start injection** (`graph/context.rs::retrieve` +
`graph/service.rs`):
- `sessions_greeted: Mutex<HashSet<String>>` (in-memory; a restart re-greeting
  once is acceptable and self-corrects).
- In `retrieve`: if `repo_map_on_session_start && context_injection` and the
  `session_id` is new to the set, prepend the map (its chars count against
  nothing — it has its own budget) and insert into the set.

**B6. Settings:** `repo_map_budget_chars: u32` (4000),
`repo_map_on_session_start: bool` (false). Frontend fields in the Context
subsection.

**B7. Tests:** centrality ordering (a file with 3 inbound edges outranks 1);
budget respected; greeting happens exactly once per session id; toggle off ⇒
never prepended.

---

## Phase C — Injection delta/dedup

**C1. Schema** (`graph/schema.rs`, the §0 bump):
```
:create injected {session_id: String, path: String => digest_hash: String,
                  turn: Int, ts_ms: Int}
```
Also add `turns: Int` default 0 to the `session` relation (retrieve-call
counter) — or store the counter in the in-memory session map if touching the
V10 relation is noisier; **decide at implementation, prefer the relation**
(survives restarts, and eviction cascades already exist for sessions).

**C2. Retrieve integration** (`graph/context.rs::retrieve`):
- Increment the session's `turn` on every call.
- After candidate scoring, before packing, for each candidate look up
  `injected`: hit **and** file unchanged (mtime ≤ `ts_ms` and
  `digest_hash == hash(current digest text)`) **and**
  `turn - injected.turn <= context_dedup_ttl_turns` ⇒
  - budget tight (candidates overflow the turn budget): drop it silently;
  - otherwise emit the one-liner `- path — injected turn N, unchanged`.
- Changed file ⇒ re-inject with `(updated)` suffix; refresh the row.
- After packing, upsert `injected` rows for everything emitted in full.
- Track suppressed chars; return them in `RetrieveResult` as
  `pub deduped_chars: usize` (new field).

**C3. Reset points:** `graph_memory_clear` IPC clears `injected` for the
scope it clears; Phase D's compaction route deletes the session's rows.

**C4. Settings:** `context_dedup_ttl_turns: u32` (default 10; `0` = dedup off).

**C5. UI** (`CodeIntelligenceView` Context section): last-injection panel rows
get a badge (`new` / `skipped` / `updated`) — extend the IPC payload the panel
reads (`graph_context_preview` and the last-injection event) with per-file
status; running counter gains "est. tokens avoided (dedup)" fed by
`deduped_chars / 4`, labeled *est.*

**C6. Tests:** same prompt twice ⇒ second retrieve suppresses; file touched
between ⇒ re-injected as updated; TTL expiry re-injects; `ttl=0` disables;
clear-session resets.

---

## Phase D0 — PreCompact spike (gates D)

Method mirrors the V10 OpenCode spike — empirical, against the pinned Claude
Code version cImp launches:
1. Overlay a `PreCompact` hook (`hooks.PreCompact[0].hooks[0] = {type:
   "command", command: "<marker-emitting script>"}`) via `--settings`.
2. Drive a session to `/compact` (manual trigger) with a transcript-capture
   session; also force an auto-compact if feasible (long dummy turns).
3. Record: the exact stdin JSON the hook receives (`trigger`,
   `custom_instructions`?, session id), **which stdout field reaches the
   compaction prompt** (documented candidates: plain stdout as
   additional-instructions vs `hookSpecificOutput`), and any size limits.
4. Write findings into the milestone doc (same style as the V10 D0 block).
   If **no** stdout field influences compaction, Phase D falls back to:
   `PreCompact` only *clears the dedup table + sets post_compaction* (still
   valuable — it makes Phases C/E correct across compaction), and the
   working-set block moves to the *next* `UserPromptSubmit` injection
   (`retrieve` prepends a one-time "post-compaction recap" block). Plan D is
   written so this fallback is a small diff, not a redesign.

---

## Phase D — Compaction survival

**D1. Shim** (`src-tauri/src/compact_hook.rs`, new — sibling of
`context_hook.rs`, registered in `main.rs` as `cimp --precompact-hook`):
- stdin JSON per D0; read loopback discovery (same helper `context_hook.rs`
  uses at `:74`); POST `/context/compaction`
  `{ session_id, cwd, trigger }` with ~300 ms timeout; print per the D0
  contract; on any failure print nothing, exit 0.

**D2. Route** (`offload/loopback.rs`): add
`("POST", "/context/compaction") => handle_context_compaction(...)` to the
match at `:345`. Handler:
- Gate: `graph.enabled && context_injection && compaction_context`.
- Build the block via a new `graph::context::compaction_block(graph, root,
  session_id) -> String`: top ~10 `mem_working_set` entries (one line each:
  path · last kind · top symbols), pinned `mem_note`s verbatim, unpinned notes
  as a bulleted digest. Hard cap ~2000 chars.
- Side effects (always, even when the gate suppresses the block): delete the
  session's `injected` rows (C3) and insert the session id into a
  `post_compaction: Mutex<HashSet<String>>` on `GraphService` (consumed by
  Phase E; also cleared per-file as re-reads happen).

**D3. Overlay** (`tabs/config.rs:199-218`): the hooks object gains a
`PreCompact` entry beside `UserPromptSubmit`, command built the same way as
`statusline/mod.rs:50`'s (`<exe> --precompact-hook`, timeout 5). Installed when
the D2 gate settings are on.

**D4. Settings:** `compaction_context: bool` (default **true**; effective only
when `context_injection` is on — the UI shows it nested under the injection
toggle).

**D5. Tests:** `compaction_block` renders working set + pinned notes within
cap; route clears `injected` + sets the post-compaction flag even when the
block is suppressed; shim prints nothing on connection refused (unit-test the
request builder like `context_hook.rs`'s existing tests, if any — else add
both).

---

## Phase E — Redundant-read advisor ⚠ (opt-in; depends on D)

**E1. Contract spike (small, rides D0's harness):** verify the `PreToolUse`
deny path: hook JSON in (`tool_name`, `tool_input.file_path`), and that a
deny's `permissionDecisionReason` (or the current field name — record it) is
surfaced **to the model**, not only to the user. If the reason does *not*
reach the model, the advisor cannot substitute content and the phase is
**cancelled** (recorded in the milestone) — a bare deny is worse than nothing.

**E2. Shim** (`src-tauri/src/read_hook.rs`, `cimp --read-hook`):
stdin JSON → POST `/context/should_read` `{ session_id, cwd, file_path }` →
- response `{ verdict: "pass" }` ⇒ print nothing (Read proceeds);
- `{ verdict: "remind", text }` ⇒ print the deny JSON with `text` as the
  reason (exact shape per E1). Timeout ~300 ms ⇒ pass. Exit 0 always.

**E3. Route + logic** (`offload/loopback.rs` +
`graph/context.rs::should_read`):
Pass (in order) when: `!read_advisor`; memory disabled/empty; file not in this
session's `mem_event` reads; file line-count < `read_advisor_min_lines`
(cheap: `std::fs` + bytecount, cached); file mtime > last read event's
`ts_ms`; session in `post_compaction` and file not re-read since (then clear
that file's entry); file already reminded once this session
(`reminded: Mutex<HashMap<(String,String), ()>>` in-memory).
Otherwise remind: `text` = outline digest (reuse the Phase A formatting) +
`"unchanged since you read it (turn N). Re-read with Read({file, offset,
limit}) if you need exact text."` Record an Activity event
(`kind: "remind"`, detail = est. displaced tokens = file_bytes/4).

**E3b. Substitute mode:** `read_advisor_mode: "advise" | "substitute"`
(default advise). In substitute mode the remind `text` additionally carries
the most relevant symbol body: when the hook JSON shows a targeted read
(`offset`/`limit` in `tool_input`) use `symbol_at` on that range; otherwise
pack the file's top-centrality public symbols (Phase B1's `file_centrality`
signal at symbol granularity) up to `max_body_bytes / 2`, via the Phase A
slicing helper. The escape-hatch line and the remind-once rule (E3) are
unchanged — substitution can never lock the agent out of exact text.

**E4. OpenCode** ⚠: extend the plugin writer (`tabs/config.rs:395-460`) with a
`tool.execute.before` handler **only if** a mini-spike shows a before-hook can
short-circuit the tool with substitute output in the pinned OpenCode version;
otherwise skip — Claude-only, documented.

**E5. Settings:** `read_advisor: bool` (false),
`read_advisor_min_lines: u32` (300), `read_advisor_mode: String`
(`"advise"`). Overlay installs the `PreToolUse` hook
only when `read_advisor` is on (keep the hook out of the settings JSON
otherwise — zero overhead for non-users).

**E6. Tests:** verdict matrix (unread/changed/small/post-compaction/second-ask
⇒ pass; read-unchanged-large ⇒ remind once); substitute mode includes a body
and respects the byte cap; Activity event recorded; route honors master
toggles.

---

## Phase F — LLM digests (local-only)

**F1. Internal runner** (`offload/supervisor.rs`, beside `run_task:511`):
```rust
pub async fn run_internal(&self, prompt: String, max_tokens: u32,
                          deadline: Duration) -> AppResult<String>
```
Same local-ready-server + slot acquisition as `run_task`, but a **single
plain completion** via the `openai.rs` client — no agent/tool loop, no MCP.
Never consults remote backends (`run_task` already only scans local
`running`; keep that property and assert it in a test). Tag requests
`source: "internal"` in `metrics.rs` so the dashboard shows them honestly.

**F2. Digest store + queue** (`graph/context.rs` + `graph/service.rs`):
- Relation (created in the §0 bump):
  `digest {file: String, content_hash: String => text: String, ts_ms: Int}`.
- `GraphService` gains a bounded `tokio::mpsc` digest queue + one worker task:
  dequeue file → hash content (reuse the builder's file-hash fn if exposed;
  else xxhash the bytes) → cache hit ⇒ skip → build prompt
  (`"Summarize for a code-assistant context block, ≤3 lines: <first 4 KiB>"`)
  → `run_internal(…, 128 tokens, 20 s)` → validate (non-empty, ≤400 chars) →
  upsert. Errors drop the item silently (fallback digest keeps working).
- Prune digests whose file left the index (same pattern as the doc_vec
  orphan sweep at `index.rs:760-782`).

**F3. Retrieve integration:** when `context_llm_digests` and a packed file has
no outline-based digest (the V10 fallback branch), check `digest` by
`(file, content_hash)` — hit ⇒ use it; miss ⇒ V10 fallback **and** enqueue.
Never wait on the queue.

**F4. Settings:** `context_llm_digests: bool` (false). UI: Context section
shows digest-cache coverage (`N cached`) + a health note when no local
backend is ready (mirror the semantic-search degraded wording).

**F5. Tests:** queue dedups by hash; validation rejects oversized output;
retrieve never blocks (cache-miss path returns the fallback synchronously);
`run_internal` errors don't poison the queue.

---

## Phase G — Code embeddings

Mirror the doc pipeline end to end; every function below has a `doc_`
counterpart to crib from (`index.rs:696-960`).

**G1. Chunk emission** (`graph/builder.rs`): during parse, for symbols with
`kind ∈ {fn, method, type/struct/class}` and span ≥ 3 lines, emit
`CodeChunk { id: symbol_id, file, text }` into `FileGraph` where
`text = signature + doc + body` truncated to ~1 KiB (the source is in memory
during parse — slice by the span). Respect `semantic_code_max_chunks` at
index level, priority = `visibility == public` first, then centrality
(post-pass trim in `index_file_graph` is fine for V1).

**G2. Store** (`graph/schema.rs`, §0 bump):
```
:create code_chunk {id: String => file: String, text: String}
:create code_vec   {chunk_id: String, epoch: String => vec: <F32; dims>}
```
plus the HNSW index on `code_vec` (copy the doc_vec creation, including the
dims-probe interplay noted at `index.rs:89`).

**G3. Index methods** (`graph/index.rs`): `pending_code_chunks(limit)`,
`store_code_vecs(rows, epoch)`, `orphan_code_vec_sweep()`,
`semantic_code_search(query_vec, k) -> Vec<(SymbolHit, f32)>` (join
`code_vec` k-NN → `code_chunk.id` → `symbol`), `code_embedding_coverage(epoch)`
— each a near-copy of the doc versions (`:858`, `:825`, `:760`, `:903`, `:951`).

**G4. Embed loop** (`graph/service.rs` / wherever the doc embed batch loop
lives): a second batch pass over `pending_code_chunks` behind
`semantic_code`, sharing the `Embedder` and epoch fingerprint; run at
index-idle after doc chunks (docs first — they're smaller and already
user-visible).

**G5. Tool:** `graph_semantic_code { query, k? }` → embed the query (embedder
must be up; else the standard degraded message) → `semantic_code_search` →
rows: `file:line · kind · signature · score`. **No bodies** — the guidance
addendum points at the `graph_semantic_code → graph_snippet` chain.

**G6. Settings:** `semantic_code: bool` (false),
`semantic_code_max_chunks: u32` (20 000). UI: Index section embedder card
gains a second coverage line ("code: N / M chunks").

**G7. Tests:** chunk emission respects kind/size filters and the cap;
round-trip search returns the seeded symbol first (reuse the doc-search test
fixture pattern); coverage counts; orphan sweep.

---

## Phase H — Docs, settings polish, tests, release

- README / `docs/FEATURES.md`: new tools (`graph_snippet`, `graph_repo_map`,
  `graph_semantic_code`), dedup/compaction/read-advisor/digests under the
  Context Engine section; `docs/MAINTENANCE.md`: schema v3 reset note, the
  PreCompact/PreToolUse hook contracts (D0/E1 findings), internal-offload
  digest jobs.
- `ToolsReference` `GRAPH_TOOLS` entries + guidance addenda finalized.
- Full `cargo test` + `npm run check`; CHANGELOG; version bump; release per
  `feedback_git_release_workflow` (develop → main, tag).

---

## Appendix — consolidated change surface

**New MCP tools** (3): `graph_snippet`, `graph_repo_map`,
`graph_semantic_code`.

**New loopback routes** (2): `POST /context/compaction`,
`POST /context/should_read`.

**New CLI subcommands** (2): `cimp --precompact-hook`, `cimp --read-hook`.

**New settings** (`GraphSettings`): `max_body_bytes`,
`repo_map_budget_chars`, `repo_map_on_session_start`,
`context_dedup_ttl_turns`, `compaction_context`, `read_advisor`,
`read_advisor_min_lines`, `read_advisor_mode`, `context_llm_digests`,
`semantic_code`, `semantic_code_max_chunks`.

**Schema (one bump, v3):** relations `injected`, `digest`, `code_chunk`,
`code_vec` (+ HNSW); `session.turns` counter; `SymbolHit.end_line`.

**New Rust files:** `compact_hook.rs`, `read_hook.rs`. New fns across
`graph/{index,context,service,builder,schema}.rs`,
`offload/supervisor.rs::run_internal`.

**Frontend:** Context-section badges + counters + digest/coverage readouts;
Settings fields; `ToolsReference` entries. No new tabs, no tab-schema change.

**Spikes:** D0 (PreCompact contract — gates D and shapes its fallback),
E1 (PreToolUse deny-reason visibility — gates E), E4 mini-spike (OpenCode
before-hook — gates OpenCode parity for E only).

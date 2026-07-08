# IMPL-PLAN V12 — Agentic Inner Loop

Companion to `docs/MILESTONE-V12-agentic-inner-loop.md`. File-by-file build
plan. Open decisions **assumed at proposed defaults** (`run_check` exposed to
cloud tabs only in V1, not the offload worker's native set; impact depth 3;
distillation default off) — sections marked ⚠ change if a decision flips.

Phases: **A** (`run_check`) → **B** (`graph_impact`) → **C** (test mapping) →
**D** (git-aware context) → **E** (distillation) → **F** (proactive
automation) → **G** (docs/tests/release).

Grounding anchors (verified against current `develop`, post-V10):
- Tools plumbing: `graph/mcp.rs` (`tool_specs`, `dispatch_recorded`,
  `run_tool`); MCP child forwards `graph_*` to the warm path via
  `offload/mcp.rs:710-729` (`POST /graph_run`), backed by
  `graph/service.rs:282`.
- Graph queries: `graph/index.rs` (`transitive` recursive-Datalog pattern,
  `SymbolHit:25`), `graph/model.rs:371` (`Symbol`), emit choke point
  `graph/builder.rs::emit_symbol`, generic engine `graph/tags.rs`.
- Ranking: `graph/context.rs::retrieve` (score compose step), digests emitted
  there; `fmt_symbols` in `graph/mcp.rs`.
- Memory: `graph/memory.rs`, session relations + eviction in
  `graph/service.rs:385+`; notes/`mem_note` per V10.
- Internal offload runner: **V11 Phase F** `offload/supervisor.rs::run_internal`
  (this plan's Phase E depends on it; `run_task:511` exists today as its
  template).
- Settings: `settings/schema.rs:873` (`GraphSettings`); per-project overlay =
  `.cimp/config.json` (settings/persistence overlay mechanism, V0.23 rebrand
  memory).
- Subprocess spawning: the Windows console-suppression helper used by every
  spawned subprocess (see the PTY/supervisor spawn sites) — reuse for `git`
  and checker processes.
- Frontend: `lib/CodeIntelligenceView.svelte` (Analyses + Memory sections),
  `lib/graph.ts`, `ipc/commands.rs` graph IPC block.

Schema coordination: Phases C/D/E add schema (`symbol.is_test`,
`commit_touch`, `project_fact`, `session.distilled`). If V11 hasn't shipped its
v3 bump yet, fold these into it; otherwise this milestone bumps
`GRAPH_SCHEMA_VERSION` v3 → v4 (same reset-migration mechanism, V10 §0).

---

## Phase A — `run_check` (structured diagnostics)

**A1. Module** (`src-tauri/src/checks/mod.rs`, new):
```rust
pub struct CheckDef { pub name: String, pub cmd: String, pub parser: ParserKind,
                      pub timeout_secs: u64 }               // serde, from settings
pub enum ParserKind { CargoJson, Tsc, EslintJson, Pytest, GenericGcc }
pub struct Diag { pub severity: Severity, pub code: Option<String>,
                  pub message: String, pub file: String, pub line: u32,
                  pub col: Option<u32> }
pub struct CheckReport { pub name: String, pub exit_code: Option<i32>,
                         pub duration_ms: u64, pub timed_out: bool,
                         pub groups: Vec<DiagGroup> }        // deduped
pub struct DiagGroup { pub key: String, pub severity: Severity,
                       pub message: String, pub count: usize,
                       pub sites: Vec<(String, u32)> }       // first N locations
pub async fn run(root: &Path, def: &CheckDef, changed_only: bool) -> AppResult<CheckReport>;
```
- Spawn `def.cmd` via the shell with cwd = root, console suppressed, both
  pipes captured, `tokio::time::timeout(def.timeout_secs.max(10))`; on timeout
  kill and parse partial output, `timed_out = true`.
- Dedup: group by `(severity, code, normalized message)` where normalization
  strips path/identifier specifics (replace `'…'`/`` `…` `` quoted spans with
  `‹›`); cap `sites` at 5, groups at `max_rows_per_query`.
- `changed_only`: filter groups to files in `git diff --name-only HEAD` ∪
  `git status --porcelain` untracked (helper shared with Phase B; lives in
  `checks/gitls.rs` for now, promoted if V13 lands its git module first).

**A2. Parsers** (`src-tauri/src/checks/parsers.rs`):
- `CargoJson`: serde over `--message-format=json` lines
  (`reason == "compiler-message"` → `message.{level,code.code,message,spans[0]}`).
- `Tsc`: regex `^(.+)\((\d+),(\d+)\): (error|warning) (TS\d+): (.*)$`.
- `EslintJson`: serde over `--format json` output.
- `Pytest`: the short-summary section (`FAILED file::test - msg`) + the
  tail counts line.
- `GenericGcc`: regex `^(.+?):(\d+)(?::(\d+))?:\s*(error|warning|note)?:?\s*(.*)$`.
Each ~30 lines + fixture-file unit tests (real captured outputs under
`src-tauri/tests/fixtures/checks/`).

**A3. Settings** (`settings/schema.rs`): new top-level
`pub checks: Vec<CheckDef>` (default empty) — lives at the root, not in
`GraphSettings` (it's project tooling, not graph config). It naturally rides
the `.cimp/config.json` overlay, which is where users will actually set it;
document that. Settings-UI: a simple table editor (name / command / parser
dropdown / timeout) under a new "Checks" settings section.

**A4. Tool exposure** ⚠ (`graph/mcp.rs`):
- Spec `run_check { name?, changed_only? }` in `tool_specs` — listed with the
  graph tools (they're the shared cloud-agent tool surface), but gated on
  `!settings.checks.is_empty()` in addition to `graph.enabled`... **no** — the
  checks feature must not require the graph. Gate on `checks` non-empty only;
  the tool spec text says "not configured" guidance when absent.
- Dispatch: the MCP child can run the command itself (it inherits the app's
  machine) but must **not** trust model-supplied commands: the model's `name`
  only *selects* among the user-configured `CheckDef`s. The child reads the
  defs from the overlay file at the project root (re-read per call — live
  edit friendly). No new loopback route needed.
- Record in the activity ring (`dispatch_recorded` does this for free) with
  detail = `name · errors=N · ms`.

**A5. IPC + UI:** `run_check` also gets a small human surface: Activity rows
render check runs; no dedicated section in V1 (the agent is the consumer).

**A6. Tests:** parser fixtures per A2; dedup groups repeated diagnostics;
timeout path returns partial + flag; unknown `name` errors listing configured
names; empty config returns the "not configured" message.

---

## Phase B — `graph_impact`

**B1. Reverse transitive** (`graph/index.rs`):
```rust
pub fn dependents_transitive(&self, roots: &[String], depth: u32, max: usize)
    -> AppResult<Vec<DependentHit>>   // { symbol: SymbolHit, via: String, depth: u32, approx: bool }
```
Recursive Datalog over inbound `call`/`references` edges (invert the existing
`transitive` rule — swap src/dst in the recursion), depth-capped, de-duped by
symbol id, `approx = true` for name-only (unresolved) edges — same honesty
flag `graph_references` uses.

**B2. Diff → symbols** (`src-tauri/src/graph/impact.rs`, new):
```rust
pub fn changed_symbols(root: &Path, index: &GraphIndex) -> AppResult<Vec<SymbolHit>>;
```
- Spawn `git diff --unified=0 HEAD` (console-suppressed); parse hunk headers
  (`@@ -a,b +c,d @@`) → per file the changed new-side line ranges; untracked
  files from `git status --porcelain` count whole-file.
- Map each (file, range) through `symbol_at`-style span overlap (one Datalog
  query per file: symbols whose `[start_line, end_line]` intersects any
  range). Files not in the index (docs, configs) are reported in a separate
  `unindexed: Vec<String>` list, not dropped silently.
- Not a git repo ⇒ typed error → the tool renders "requires git" guidance.

**B3. Tool** (`graph/mcp.rs`): `graph_impact { symbols?, depth?, include_tests? }`
- No `symbols` ⇒ B2 path. Output: per changed symbol a flattened dependent
  list (`file:line · name · depth`, `~` marker on approx), then a file-level
  rollup (`N files depend on your change`), then `unindexed` if any.
  `include_tests` (Phase C) appends the affected-test block.
- Bounded by `max_rows_per_query`; `depth` default 3 clamp 1..=6.

**B4. IPC + UI:** `ipc/commands.rs::graph_impact(root?)` (diff mode only) →
typed rows; `lib/graph.ts` fn + types; Analyses section third button
**"Impact of working-tree changes"** rendering the same table style as dead
exports, with the approx-edge caveat caption.

**B5. Tests:** hunk-header parsing (add/del/context-only hunks); span-overlap
mapping; reverse-transitive depth cap + approx flag; non-git root error;
fixture graph: change `a()` → dependents `{b, c}` at depths 1, 2.

---

## Phase C — Test↔symbol mapping

**C1. IR** (`graph/model.rs`): `pub is_test: bool` on `Symbol` (after
`visibility`). `emit_symbol` (`graph/builder.rs`) gains an `is_test: bool`
param — the same choke-point maneuver as V10's visibility bit; all callers
updated.

**C2. Per-walker detection** (`graph/builder.rs`):
- `parse_rust`: preceding `attribute_item` matching `test`/`tokio::test`/
  `rstest` (helper `rust_is_test(node, src)`); also any fn inside a
  `#[cfg(test)] mod` (track a boolean while descending into `mod_item`s).
- `parse_js_ts`: call-expression defs whose callee is `test`/`it`/`describe`
  get synthesized as test symbols where the walker already handles those; plus
  a **file-level** rule: any function defined in `*.test.*`/`*.spec.*`/
  `__tests__/` is `is_test`.
- `parse_python`: name starts `test_` in a file matching `test_*.py`/
  `*_test.py`/`tests/` path.
- `graph/tags.rs`: support an optional `@definition.test` capture per
  language's `tags.scm`; fallback = path heuristics (`_test.go`, `tests/`,
  `spec/`) applied in `parse_with_tags`. Languages with neither simply never
  set the bit (accurate-not-wrong, as with visibility).

**C3. Store** (`graph/schema.rs`): `symbol` relation gains `is_test: Bool`
(the coordinated schema bump, header note). `index_file_graph` writes it;
`SymbolHit` gains `pub is_test: bool`; `fmt_symbols` may append `[test]`.

**C4. Queries + tools** (`graph/index.rs`, `graph/mcp.rs`):
- `tests_for` = `dependents_transitive(roots, depth, max)` filtered
  `is_test == true` — **reuses Phase B outright**; no new recursion.
- Tools: `graph_tests_for { symbol | file, depth? }` (file mode unions the
  file's symbols first) and the `include_tests` flag on `graph_impact`
  (B3) which appends `affected tests: file:line · name` rows.
- Labeled candidates (dynamic dispatch/fixtures caveat) in the tool output
  footer.

**C5. Tests:** each walker's detection (attribute, cfg(test) mod, describe/it,
`test_` prefix, path fallbacks); `tests_for` finds a test two hops up; a
non-test caller is excluded; `include_tests` composition.

---

## Phase D — Git-aware context

**D1. Collector** (`src-tauri/src/graph/gitmeta.rs`, new):
```rust
pub struct FileChurn { pub file: String, pub last_ts: i64,
                       pub last_subject: String, pub touches_90d: u32 }
pub fn collect(root: &Path) -> AppResult<Vec<FileChurn>>;   // full 90-day pass
pub fn collect_for(root: &Path, files: &[String]) -> AppResult<Vec<FileChurn>>; // incremental
```
`collect`: one spawn of `git log --since=90.days --name-only --format=%x01%ct%x09%s`
parsed in a single pass (record separator `\x01`); paths normalized to
index-relative form. `collect_for`: `git log -1 --format=%ct%x09%s -- <file>`
per changed file (watcher batches are small). Probe
`git rev-parse --is-inside-work-tree` once; not a repo ⇒ feature disabled
(status note, everything else unaffected).

**D2. Store** (`graph/schema.rs`, the coordinated bump):
```
:create commit_touch {file: String => last_ts: Int, last_subject: String, touches_90d: Int}
```
Populate: full `collect` at the end of a rebuild pass; `collect_for` on each
watcher batch (both in `graph/service.rs` beside the existing post-pass
steps). Writes under the existing write lock.

**D3. Ranking + trailers** (`graph/context.rs`, `graph/mcp.rs`):
- `retrieve` score adds `churn_boost`: `+3` if `last_ts` within 7 days, `+1`
  within 30 (integer weights beside the existing term/centrality/session
  terms).
- Digest lines and `fmt_symbols` rows gain the optional trailer
  `last change: "<subject ≤60 chars>" (<rel-age>)` when a `commit_touch` row
  exists.
- Tool `graph_recent_changes { days?, path_prefix? }` → churn-ranked rows
  (`file · touches · last subject · age`), bounded.

**D4. Tests:** log parsing (multi-file commits, renames tolerated as adds);
incremental update overwrites; ranking boost ordering; trailer formatting;
non-git degrade.

---

## Phase E — Memory distillation ⚠ (default off; depends on V11-F `run_internal`)

**E1. Store** (`graph/schema.rs`, coordinated bump):
```
:create project_fact {fact_id: String => text: String, source_session: String,
                      ts_ms: Int, pinned: Bool, archived: Bool}
```
plus `distilled: Bool` (default false) on the `session` relation. Cap: ≤100
live (`archived == false`) facts; inserting past the cap archives the oldest
unpinned.

**E2. Distiller** (`graph/memory.rs` + `graph/service.rs`):
- Trigger points: (a) the session-eviction path (the >20-sessions prune) runs
  the distiller *before* deleting; (b) a low-frequency sweep (piggyback the
  existing poll/watcher tick) distills sessions with
  `last_ms < now − 24 h && !distilled`.
- `distill_session(root, session_id)`: gate on `memory_distillation` + a
  ready local backend (`supervisor` exposes readiness) — not ready ⇒ leave
  `distilled = false` (evict undistilled if forced; V10 semantics preserved).
  Prompt = working set (top 10) + all notes + this fixed instruction:
  *"Extract at most 3 non-obvious, durable facts a future coding session on
  this project would need. Skip anything derivable from the code itself.
  One line each, ≤200 chars, plain text, no numbering."*
  → `run_internal(prompt, 256 tokens, 30 s)` → split lines, validate (1–3
  lines, each ≤200 chars, non-empty) → insert facts, set `distilled`.
  Validation failure ⇒ log at debug, mark distilled anyway (never retry-loop
  a bad model output).

**E3. Recall + retrieve integration:**
- `context_recall` output gains a trailing `## Project facts` section (pinned
  first, then newest, ≤15 lines).
- `graph/context.rs::retrieve`: facts are a candidate *signal* — a fact whose
  text contains a candidate file's stem adds a small boost (`+2`); facts
  themselves are injected only via the recap/compaction blocks, not per-turn
  (keeps turn budgets for code).

**E4. IPC + UI** (Memory section): `graph_facts(root?)`,
`graph_fact_update { id, action: pin|unpin|delete|edit, text? }`,
`graph_fact_add { text, pin? }`. Facts list above Notes: pinned-first rows
with inline edit/delete; an "add fact" input. Facts show their source-session
tooltip.

**E5. Settings:** `memory_distillation: bool` (default false), surfaced under
the Memory subsection with the "requires a local offload backend" health note.

**E6. Tests:** eviction triggers distillation before delete; validation
rejects >3/oversized lines; cap archives oldest unpinned; pinned facts never
auto-archived; recall renders facts; no-backend leaves session undistilled
without error.

---

## Phase F — Proactive automation

**F0. Contract spike:** extend the V11 D0 capture harness to `PostToolUse`
(matcher `Edit|Write|MultiEdit`): record the stdin JSON shape and verify
which stdout field reaches the model as additional context. One harness run
covers `PreCompact` (V11) + `PostToolUse`; findings recorded in both
milestone docs. If nothing reaches the model, 6a/6b re-scope to
parked-block-only delivery (F2's drain path) — degraded but not cancelled.

**F1. Shim** (`src-tauri/src/postedit_hook.rs`, `cimp --postedit-hook`,
registered in `main.rs` beside `--context-hook`): stdin JSON (`tool_name`,
`tool_input.file_path`, session id, cwd) → POST `/context/post_edit` →
print the returned block per the F0 contract (empty ⇒ print nothing).
~300 ms client timeout, exit 0 always.

**F2. Route + logic** (`offload/loopback.rs` route + new
`src-tauri/src/checks/auto.rs`):
- **Debounce** per session (`auto_check_debounce_s`): edits inside the window
  coalesce; calls inside the window return empty fast; the first call after
  the window closes gets the result. A check slower than the hook budget
  parks its report in a per-session **pending block** drained by the next
  `/context/retrieve` or post-edit call — a turn is never blocked.
- **Run + diff:** `checks::run(root, def, changed_only=true)` per configured
  check; diff `DiagGroup` keys against the session's previous report
  (in-memory baseline per session); render only new/worsened groups
  (Phase A format), cap ~1.5k chars.
- **Auto-impact:** map the edited file through `impact::changed_symbols`'s
  span-overlap; any symbol with inbound count (V11-A1 `callers_count`) ≥
  `auto_impact_min_dependents` ⇒ append the two-line blast-radius note.
- Record an Activity event per injection (`kind: "auto_check"`, detail =
  group count) — the graduation evidence for milestone Decision 4.

**F3. Hook install** (`tabs/config.rs`): `PostToolUse` entry in the settings
overlay when `auto_check` is on. OpenCode: the plugin's existing
`tool.execute.after` handler additionally POSTs edit-class tools to
`/context/post_edit`; whether its return can carry context is spiked —
if not, OpenCode relies on the parked-block drain (already built).

**F4. Analyses trigger** (`graph/service.rs`): at the end of each completed
index pass (same spot as the V11-B3 repo-map cache invalidation), when
`analyses_auto`: run `dead_exports` + `import_cycles` bounded, store
`(dead_count, cycle_count)` in the `meta` relation, emit a `graph-analyses`
event when they changed. `CodeIntelligenceView` badges the Analyses section
control + the individual buttons ("+3 since last pass"). The V11-B2 repo-map
renderer appends one line when counts grew (reads the meta row).

**F5. Fact promotion** (`tabs/config.rs`): when `promote_pinned_facts`, the
guidance builder appends a marked `## cImp project facts` block of pinned
`project_fact` rows (cap 1500 chars, newest-pinned first) to the Claude
`--append-system-prompt` payload and the OpenCode instructions file at
launch. Launch-time only; the Facts UI notes "applies next launch".

**F6. Settings:** `auto_check: bool` (false), `auto_check_debounce_s: u32`
(5), `auto_impact_min_dependents: u32` (10), `analyses_auto: bool` (true),
`promote_pinned_facts: bool` (false). UI: first three under the Checks
settings section, `analyses_auto` under Code Intelligence, promotion under
the Memory subsection.

**F7. Tests:** debounce coalescing (three rapid edits → one check run);
new-vs-baseline diffing (identical second run → empty); impact threshold;
parked-block drain via retrieve; analyses event fires only on count change;
promotion block formatting + cap + pinned-only selection.

---

## Phase G — Docs, settings polish, tests, release

- README / `docs/FEATURES.md`: `run_check` (+ config example), impact/tests
  tools, `graph_recent_changes`, project facts. `docs/MAINTENANCE.md`: the
  schema bump, parser-fixture upkeep note, distiller prompt location.
- Guidance addenda (`tabs/config.rs`): the intended chain — *"before editing
  shared code run `graph_impact`; after edits run `run_check
  {changed_only:true}`; use `graph_tests_for` to pick tests."*
- `ToolsReference` `GRAPH_TOOLS` entries; full `cargo test` + `npm run
  check`; CHANGELOG; version bump; release per the standard workflow.

---

## Appendix — consolidated change surface

**New MCP tools** (4): `run_check`, `graph_impact`, `graph_tests_for`,
`graph_recent_changes`.

**New settings:** root-level `checks: Vec<CheckDef>`, `auto_check`,
`auto_check_debounce_s`, `auto_impact_min_dependents`; `GraphSettings`:
`memory_distillation`, `analyses_auto`, `promote_pinned_facts`. (Impact
depth is a tool arg, not a setting.)

**Schema (coordinated bump):** `symbol.is_test`; relations `commit_touch`,
`project_fact`; `session.distilled`.

**New Rust files:** `checks/mod.rs`, `checks/parsers.rs`, `checks/auto.rs`,
`graph/impact.rs`, `graph/gitmeta.rs`, `postedit_hook.rs`.

**New loopback route:** `POST /context/post_edit`. **New CLI subcommand:**
`cimp --postedit-hook`.

**New IPC:** `graph_impact`, `graph_facts`, `graph_fact_update`,
`graph_fact_add`.

**Frontend:** Analyses third button (impact) + auto-run badges; Memory Facts
list (+ promotion note); Checks settings table (+ auto-check fields);
`ToolsReference` entries.

**External process use (all spawned `git`/checker, console-suppressed,
user-configured commands only):** `git diff/status/log/rev-parse`, the
configured check commands. No new C dependencies.

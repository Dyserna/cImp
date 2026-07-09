# IMPL-PLAN V10 — Context Engine

Companion to `docs/MILESTONE-V10-context-engine.md`. This is the file-by-file
build plan: exact structs, schema, tools, routes, UI wiring, and per-phase
checklists. All decisions are locked (see the milestone's "Decisions — RESOLVED"
and the verified D0 spike). Numbering: **V10**, a new "context" pillar that
subsumes and renames the V9 Code Graph tab.

Phases: **A** (rename/shell) → **B1** (visibility bit) → **B2** (analyses) →
**C** (session memory) → **D** (context injection) → **E** (docs/settings/tests).

Grounding anchors (verified against current `develop`):
- IR: `graph/model.rs` (`Symbol`, `SymbolKind`, `Lang`, `FileGraph`, `emit_symbol` choke point at `graph/builder.rs:678`).
- Store: `graph/schema.rs` (`RELATIONS`), `graph/index.rs` (`GraphIndex`, `SymbolHit`).
- Tools: `graph/mcp.rs` (`tool_specs`, `dispatch_recorded`, `run_tool`).
- Service: `graph/service.rs` (`GraphService`, `GraphStatus`, `patch_status`).
- Loopback: `offload/loopback.rs` (`handle_conn` route match at `:343`, `/graph_run` precedent at `:521`).
- Claude launch flags: `tabs/config.rs::build_pre_args` (`--settings` overlay at `:193`, `--mcp-config` at `:213`).
- OpenCode launch config: `tabs/config.rs::build_opencode_config` + `write_opencode_instructions`.
- OOB tap: `oob/claude.rs:137-152` (drain loop), `oob/mod.rs` (`OobContext`), `pty/manager.rs:212` (context construction).
- Remote-worker gate: `offload/service.rs::worker_graph_allowed` (`:1313`).
- Frontend: `lib/GraphMonitorView.svelte`, `lib/graph.ts`, `lib/tabs/types.ts`, `lib/Pane.svelte:159`, `SettingsApp.svelte` (`'graph'` section).
- IPC: `ipc/commands.rs:927+` (`graph_status`/`graph_rebuild`/…).

---

## 0. Cross-cutting: a graph schema version + reset-migration

Adding columns (visibility) and relations (memory) to an existing `graph.db`
needs a migration. CozoDB has no cheap `ALTER`, and every graph row is derived
from source, so the migration is **"detect stale schema → `reset()` → full
rebuild."** Add once, reuse for all V10 schema changes:

1. `graph/schema.rs`: add `pub const GRAPH_SCHEMA_VERSION: i64 = 2;` (V9 == 1).
2. `graph/index.rs`: in `open`/`ensure_schema`, read a `meta` row
   `schema_version` (the `meta` relation already exists for embeddings — reuse
   it via `ensure_meta_relation`). If missing or `< GRAPH_SCHEMA_VERSION`, call
   `reset()` (drops all relations) then recreate at the new version and write
   `schema_version`. `GraphService::spawn_rebuild` on next launch repopulates.
3. On the very first V10 launch the store rebuilds once; cheap and invisible
   (already happens on watcher/startup). Document in MAINTENANCE.md.

This single mechanism covers B1 (visibility column) and C (memory relations).

---

## Phase A — Tab shell + rename to "Code Intelligence"

No backend behavior change; pure presentation + a section router. Keep the
internal id `graph-monitor` and `isGraphMonitorTab` stable (no tab/schema
migration — `lib/tabs/types.ts:27`, `isShellTab` exclusion at `:48`).

**A1. Rename the display label** (the tab-bar name, backend source):
- Find where the reserved `graph-monitor` tab's `name` is set — `state/manager.rs`
  / `tabs/registry.rs` (both reference `GraphMonitor`). Change the label string
  `"Code Graph"` → `"Code Intelligence"`. Leave the id.

**A2. Rename the component + add section nav** (`lib/GraphMonitorView.svelte`):
- Rename file → `lib/CodeIntelligenceView.svelte` (update the import in
  `lib/Pane.svelte:26` and the branch at `:163`; the branch guard
  `isGraphMonitorTab` is unchanged).
- Add `let section = $state<'index'|'activity'|'memory'|'context'|'analyses'>('index')`
  and a segmented control at the top of the root `<div>`.
- Move existing markup into the **Index** section (status card, language census
  buttons, embedder/semantic block) and **Activity** section (the "Recent calls"
  card at `:310`). Change `<h2>Code Graph</h2>` (`:213`) → `<h2>Code Intelligence</h2>`.
- Memory / Context / Analyses sections start as empty placeholders filled by
  Phases C/D/B2. Keep the existing 2 s `poll` (`onMount`) and
  `listenManaged(() => onGraphStatus(upsert))` (`:159`) — each new section adds
  its own fetch to `refresh()`.

**A3. Settings section label** (`SettingsApp.svelte`):
- Tab entry `{ id: 'graph', label: 'Code graph' }` (`:573`) → `label: 'Code Intelligence'`.
  Keep the `activeSection` union member `'graph'` and all `snapshot.graph.*`
  bindings (the settings *key* stays `graph` for back-compat).

**A4. Docs:** README/FEATURES "Code Graph" → "Code Intelligence" (defer to Phase E).

Tests: none (UI). Manual: tab renders all five section tabs; Index/Activity
unchanged from today.

---

## Phase B1 — Symbol visibility bit

Prerequisite for accurate dead-export detection. Adds a `visibility` field end to
end so `dead_exports` can restrict to genuinely public symbols.

**B1.1 IR** (`graph/model.rs`):
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility { Public, Private, Crate, Unknown }
impl Visibility {
    pub fn tag(self) -> &'static str { /* "public"|"private"|"crate"|"unknown" */ }
    pub fn from_tag(s: &str) -> Self { /* default Unknown */ }
}
```
Add `pub visibility: Visibility` to `Symbol` (after `doc`).

**B1.2 Emit choke point** (`graph/builder.rs::emit_symbol`, `:678`): add a
`vis: Visibility` param; store it on the `Symbol`. Every caller passes it:
- **`parse_rust`** (`:217`): inspect the def node for a `visibility_modifier`
  child — `pub` → `Public`, `pub(crate)`/`pub(super)`/`pub(in …)` → `Crate`,
  none → `Private`. Helper `rust_visibility(node) -> Visibility`.
- **`parse_js_ts`** (`:606`) + `emit_js_var_functions` (`:746`): `Public` if the
  def (or its `variable_declaration`) is under an `export_statement`/
  `export_declaration`; else `Private`. Helper `js_is_exported(node) -> bool`.
- **`parse_python`** (`:828`): name convention — leading `_` (and not dunder
  `__x__`) → `Private`; module-level `__all__` membership is out of scope for
  MVP → everything else `Public`.
- **Generic `tags.scm` engine** (`graph/tags.rs::parse_with_tags`): default
  `Unknown`. Opt-in precision via a `@definition.public` capture convention —
  if a language's `tags.scm` marks a def capture as public, emit `Public`. Ship
  the convention for go/java/csharp first (upstream tags already distinguish
  exported); the rest stay `Unknown`. `struct_search`-only langs unaffected.

**B1.3 Store** (`graph/schema.rs`): extend the `symbol` relation:
```
:create symbol {id: String => name, kind, file, start_line, end_line,
                signature: String, doc: String?, visibility: String}
```
Bump `GRAPH_SCHEMA_VERSION` (§0). `graph/index.rs::index_file_graph` writes
`visibility` (default `"unknown"` if a caller can't tell); `find_symbol`/
`outline`/`callers`/`callees` select it; add `pub visibility: String` to
`SymbolHit` (`index.rs:21`). `fmt_symbols` (`mcp.rs:406`) may append `[pub]`
when `visibility=="public"` for the model's benefit (optional).

**B1.4 Tests:** `builder.rs` — `pub fn`/`fn`/`pub(crate) fn` classify;
`export function` vs bare; `_helper` vs `helper`. `index.rs` round-trip carries
visibility. Guard test: every existing walker still compiles + green.

---

## Phase B2 — Packaged analyses (dead exports, dependency cycles)

**B2.1 Dead exports** (`graph/index.rs::dead_exports`):
```rust
pub fn dead_exports(&self, max: usize) -> AppResult<Vec<SymbolHit>>
```
Datalog: public symbols whose name has **zero** use-sites and zero inbound call
edges, minus an entrypoint allowlist.
```
?[id,name,kind,file,sl,sig] :=
    *symbol{id, name, kind, file, start_line: sl, signature: sig, visibility: "public"},
    not *ref{name},                       // no use-site anywhere with this name
    not *edge{kind: "call", dst: name},   // no call edge targets it by name
    not *edge{kind: "call", dst: id},     // …or by id
    !is_entrypoint[name]                    // main / test_* / new / default / etc.
:limit <max>
```
`is_entrypoint` is a small stored/inline set (`main`, names starting `test_`,
`#[test]` fns already lack visibility=public in Rust, framework hooks). Result
is **candidate** unused exports — the tool + UI label the false-positive caveat
(dynamic dispatch, external API, macro/reflection).

**B2.2 Dependency cycles** (`graph/index.rs::import_cycles`): import edges store
`edge{kind:"import", src: <file>, dst: <module-string>}` — dst is a raw module
path, not a file. Cycle detection needs file→file edges, so add a **best-effort
resolver** (not stored; computed per call):
```rust
fn resolve_import(lang: Lang, from_file: &str, module: &str, known_files: &HashSet<String>) -> Option<String>
```
Per-language, top languages first: TS/JS relative (`./x` → `x.ts|.js|/index.ts`),
Python (`a.b` → `a/b.py`), Rust (`crate::a::b` → `src/a/b.rs|a/mod.rs`), Go
(package dir). Unresolved imports are **dropped** (documented). Build the
file-import relation in memory, find SCCs of size ≥ 2 via recursive Datalog
(same pattern as `transitive`, `index.rs:411`), return each cycle as an ordered
file loop. Languages without a resolver simply never report cycles (honest).

**B2.3 Tools** (`graph/mcp.rs::tool_specs`, both consumers):
- `graph_dead_exports {}` → candidate unused public symbols (file:line, name, kind).
- `graph_cycles {}` → import cycles.
Route them in `dispatch_recorded` (`mcp.rs:179`) → `run_tool` new arms → the
`index.rs` methods. Both recorded in the activity ring. Bounded by
`max_rows_per_query`. Add to `GRAPH_TOOLS` in the UI (`CodeIntelligenceView`).

**B2.4 IPC + UI** (Analyses section):
- `ipc/commands.rs`: `graph_dead_exports(root?)` + `graph_cycles(root?)` →
  `GraphService` thin wrappers (`run_graph_tool`-style, resolve root, open warm
  index). Return typed rows for the table.
- `lib/graph.ts`: `graphDeadExports()`, `graphCycles()` + row types.
- `CodeIntelligenceView` Analyses section: two buttons + result tables; caption
  states the dead-export caveat. On-demand only (no background scheduling).

**B2.5 Tests:** `index.rs` — a public unused fn is flagged, a referenced one
isn't, a private one isn't; a 2-file import cycle is found, an acyclic graph
yields none; unresolved imports don't crash.

---

## Phase C — Session / action memory

Per-project rolling record of what each session read/edited/queried/decided, so
(a) the agent can recall its working set and (b) Phase D can rank session-hot
files first. Local, per-project, in `graph.db`; opt-in with the graph.

**C.1 Schema** (`graph/schema.rs`, §0 version bump already covers it):
```
:create session   {session_id: String => root: String, agent: String, started_ms: Int, last_ms: Int}
:create mem_event  {session_id: String, seq: Int => kind: String, path: String,
                    symbol: String?, line: Int?, ts_ms: Int, detail: String?}
:create mem_note   {note_id: String => session_id: String, text: String, ts_ms: Int, pinned: Bool}
```
`kind ∈ read | edit | query | note`. `seq` is a per-session monotonic counter
(read max+1 under the write lock). Ring-bound on insert: keep newest ~500
`mem_event` per session, newest ~20 sessions per root (evict oldest, cascade
their events/unpinned notes). Pinned notes survive session eviction.

**C.2 Event sources (per-agent, both converge on the store):**

- **Claude — transcript tap (zero new overhead).** In `oob/claude.rs`:
  - Capture the session id: `newest_jsonl` (`:295`) already picks the file;
    return its stem (`<id>`) alongside the path and thread it into the drain
    loop. File rotation (`:59-79`) = new session → new `session_id` (start a
    `session` row).
  - At the drain tap (`:142-150`, beside the existing `update_agents`), add
    `record_tool_events(&obj, &ctx)`: walk `message.content[]` for `tool_use`
    blocks (the `update_agents` walk at `:238` is the template). Map tool name →
    kind + path from `input`:
    `Read`/`NotebookRead` → read (`input.file_path`); `Edit`/`Write`/`MultiEdit`/
    `NotebookEdit` → edit (`input.file_path`); `Grep`/`Glob` → query
    (`input.pattern`/`input.path`); `Bash` → query (`input.command`, truncated
    into `detail`). Ignore `Task`, `TodoWrite`, our `mcp__cimp-offload__*`
    (those are captured by the activity ring already).
  - Emit into the store: add `mem: Option<Arc<GraphService>>` to `OobContext`
    (`oob/mod.rs:52`), populated in `pty/manager.rs:212` from managed state
    (clone the `Arc<GraphService>`). `ctx.record_read/edit/query(...)` calls
    `graph.record_mem_event(...)` in-process (no loopback). No-op when
    `graph.enabled == false` or `mem` is `None`.

- **OpenCode — the injection plugin's tool hooks.** The Phase-D plugin
  (`cimp-inject.js`) also registers `tool.execute.after`
  (`(input:{tool,sessionID,callID,args}, output) => …`) and POSTs
  `{session_id, agent:"opencode", tool, args}` to a new loopback route
  `POST /memory/event`. Map `read`/`edit`/`grep`/`bash` tool ids → kind + path
  the same way. (OpenCode's SSE OOB stream has no tool events — this is the only
  route; the plugin already exists for injection, so it's free.)

- **Graph queries** already land in `graph::activity` (source claude/offload).
  Keep that ring for the Activity section; optionally also emit a `query`
  `mem_event` from `dispatch_recorded` when a `session_id` is known (Claude MCP
  child usually doesn't have one → skip; the tap already covers Claude's side).

**C.3 GraphService methods** (`graph/service.rs`):
```rust
pub fn record_mem_event(&self, root: &Path, session_id: &str, agent: &str,
                        kind: &str, path: &str, symbol: Option<&str>,
                        line: Option<u32>, detail: Option<&str>);   // upserts session.last_ms, appends event, prunes
pub fn mem_working_set(&self, root: &Path, session_id: Option<&str>) -> Vec<WorkingSetEntry>;
pub fn mem_add_note(&self, root: &Path, session_id: &str, text: &str, pin: bool) -> String;
pub fn mem_notes(&self, root: &Path, session_id: Option<&str>) -> Vec<MemNote>;
pub fn current_session(&self, root: &Path) -> Option<String>;      // session with max(last_ms)
pub fn memory_snapshot(&self, root: &Path) -> MemorySnapshot;      // sessions + current working set + notes (IPC)
```
`WorkingSetEntry { path, touches, last_kind, last_ms, top_symbols: Vec<String> }`,
scored `recency × frequency × kind_weight` (edit 3 / query 2 / read 1), newest
session default. All writes under the existing `write_lock`; reads open the warm
index via `index_for`.

**C.4 Tools** (`graph/mcp.rs::tool_specs`): scope to the resolved root's
**current session** (no session id crosses the MCP boundary):
- `context_recall {}` → ranked working set (files + top symbols + recent edits),
  token-bounded. "What has this session been working on."
- `context_note { text, pin? }` → records a decision/fact (`mem_note`).
- `context_notes {}` → this session's notes + pinned notes for the project.
Dispatch in `dispatch_recorded`; run against `current_session(root)`.
Guidance addendum (`tabs/config.rs`, sibling of `GRAPH_GUIDANCE`): nudge the
agent to `context_note` non-obvious decisions and `context_recall` at the start
of a follow-up task. Gated on `graph.enabled` (+ a new `memory` sub-toggle if we
want it independently switchable — default on with the graph).

**C.5 Remote-worker gate:** memory tools follow the graph gate exactly —
`worker_graph_allowed` (`service.rs:1313`) already governs whether the offload
worker sees graph tools; the new tools are added to the same gated set (they
expose project activity, so a remote worker needs `allow_remote_worker_access`).

**C.6 IPC + UI** (Memory section):
- `ipc/commands.rs`: `graph_memory(root?) -> MemorySnapshot`.
- `lib/graph.ts`: `graphMemory()` + `MemorySnapshot`/`WorkingSetEntry`/`MemNote`
  types; optional `onGraphMemory` if we emit a `graph-memory` event (else the
  2 s poll covers it).
- Memory section: current-session working-set table (file · touches · last-kind ·
  when), Notes list (pinned first), a "recent sessions" collapsed list. Actions:
  pin/unpin note, Clear this session, Clear project memory → new IPCs
  `graph_memory_clear(root?, session?)`.

**C.7 Tests:** `service.rs`/`index.rs` — event append + ring prune; working-set
ranking (edit outranks read; recent outranks old); note pin survives session
eviction; `current_session` picks max `last_ms`. `oob/claude.rs` — a
`Read`/`Edit` tool_use JSONL line yields the right `(kind, path)`; `Task`/MCP
calls are ignored.

---

## Phase D — Context injection (Claude + OpenCode, verified parity)

Opt-in (`context_injection`, default off). Ranks files relevant to the user's
prompt and injects a budget-bounded digest before the agent runs. Both agents
share one retrieval core; only the injection shim differs.

**D.1 Retrieval core** (`graph/context.rs`, new module):
```rust
pub struct RetrieveResult { pub context_md: String, pub files_used: Vec<String>, pub chars: usize, pub tokens_est: usize }
pub async fn retrieve(graph: &GraphService, root: &Path, prompt: &str, session_id: Option<&str>) -> RetrieveResult;
```
Algorithm:
1. `extract_terms(prompt)` — identifiers `[A-Za-z_][A-Za-z0-9_]{2,}`, quoted
   strings, path-like tokens; drop stopwords + tokens < 3 chars.
2. Candidates: per term → `find_symbol` + `references` files; `search_docs`
   (+ `semantic_docs` when the embedder is up) → source paths; plus the session
   **working set** (Phase C) when `context_include_session`.
3. Score(file) = `Σ term_hits·3 + doc_hits·2 + inbound_edges·1 + session_recency·4`;
   dedup by file.
4. Budget-pack (mirrors GrapeRoot): sort desc; for each file until
   `context_turn_budget_chars`, emit a **digest** = `outline` (top signatures) +
   the single best-matching snippet, capped at `context_per_file_chars`. Digest,
   not whole file — a 2 k-line file costs ~300 chars.
5. Skip entirely when top score < `context_min_score` (meta/"hi" prompts inject
   nothing). Render markdown (`## Relevant context (cImp)` + bullet per file +
   a session-working-set footnote). Everything reuses warm-index reads; never
   blocks a rebuild.

**D.2 Loopback route** (`offload/loopback.rs`): add `("POST", "/context/retrieve")`
to the `handle_conn` match (`:343`), handler mirrors `handle_graph_run` (`:521`)
— resolve `Arc<GraphService>` from managed state, body
`{ cwd, prompt, session_id? }`, return `RetrieveResult` JSON. Gate: return empty
context when `graph.context_injection == false`. Fast path only (no streaming).

**D.3 Claude shim — `UserPromptSubmit` hook** (`tabs/config.rs::build_pre_args`):
extend the `--settings` overlay (today only `statusLine`, `:193`) to also carry a
hook when `graph.enabled && graph.context_injection`:
```jsonc
"hooks": { "UserPromptSubmit": [ { "hooks": [
  { "type": "command", "command": "<exe> --context-hook", "timeout": 5 }
] } ] }
```
New `--context-hook` subcommand (`main.rs`, sibling of `--statusline`/
`--offload-mcp`): read the hook JSON from stdin (`{session_id, prompt, cwd}`),
read the loopback discovery file (`offload/loopback.rs::read_discovery` — port +
token next to the exe), POST to `/context/retrieve` with a ~300 ms client
timeout, print `additionalContext` (Claude's documented
`{hookSpecificOutput:{hookEventName:"UserPromptSubmit", additionalContext}}`).
On any error/empty → print nothing, exit 0 (never block the turn). Merge with
the existing `statusLine` overlay object rather than pushing a second
`--settings`.

**D.4 OpenCode shim — the plugin** (verified in the D0 spike). At OpenCode
launch, write a dependency-free plugin (sibling of `write_opencode_instructions`
in `tabs/config.rs`) to `<project>/.opencode/plugin/cimp-inject.js`, **baking in
the current loopback port + token** (the token rotates per launch, so regenerate
each launch — idempotent overwrite, like the instructions file). The plugin:
```js
export default async (input) => ({
  "chat.message": async (inp, out) => {
    if (!CIMP_INJECT_ENABLED) return;
    const p = out.parts.find(x => x.type === "text"); if (!p) return;
    try {
      const r = await fetch(`${CIMP_LOOPBACK}/context/retrieve`, {
        method: "POST",
        headers: { authorization: `Bearer ${CIMP_TOKEN}`, "content-type": "application/json" },
        body: JSON.stringify({ cwd: input.directory, prompt: p.text, session_id: inp.sessionID }),
        signal: AbortSignal.timeout(300),
      });
      const j = await r.json();
      if (j.ok && j.text) p.text += "\n\n" + j.text;   // in-place append (schema-safe; verified)
    } catch {}
  },
  "tool.execute.after": async (inp) => {   // Phase C memory source for OpenCode
    try { await fetch(`${CIMP_LOOPBACK}/memory/event`, { method:"POST",
      headers:{authorization:`Bearer ${CIMP_TOKEN}`,"content-type":"application/json"},
      body: JSON.stringify({ session_id: inp.sessionID, agent:"opencode", tool: inp.tool, args: inp.args }),
      signal: AbortSignal.timeout(300) }); } catch {}
  },
});
```
Delivery notes (from the spike): **never launch OpenCode with `--pure`** (it
disables plugins); keep the plugin dependency-free (node builtins + global
`fetch`, so no launch-time `bun install`); add `.opencode/` to the project's
`.git/info/exclude` at launch so the generated plugin + OpenCode's own
`.opencode/.gitignore` don't dirty `git status`. Remove the plugin file when
`context_injection` and memory are both off (like the instructions file cleanup).

**D.5 `/memory/event` route** (`offload/loopback.rs`): add
`("POST", "/memory/event")` → resolve `GraphService`, body
`{ session_id, agent, tool, args }`, map tool→kind+path, call
`record_mem_event`. This is the OpenCode memory ingress (Claude uses the tap).

**D.6 Settings** (`GraphSettings`, `settings/schema.rs:873` + defaults `:939`):
```rust
pub context_injection: bool,          // default false
pub context_per_file_chars: u32,      // default 800
pub context_turn_budget_chars: u32,   // default 6000
pub context_include_session: bool,    // default true
pub context_min_score: u32,           // default 3  (integer score threshold)
```
Frontend `GraphSettings` type + `SettingsApp.svelte` "Code Intelligence" section:
a "Context injection" subsection (toggle + the four budgets), gated behind
`graph.enabled`.

**D.7 UI** (Context section): on/off + budget controls; a **last-injection panel**
(prompt truncated, files chosen, chars/budget, running est-tokens-injected per
session); a **"Preview for a prompt…"** box that calls `/context/retrieve`
(via a new `graph_context_preview(prompt)` IPC) and shows what *would* be
injected — the tuning surface. Be honest: show injected tokens, not a fabricated
"saved %".

**D.8 Tests:** `graph/context.rs` — term extraction; ranking prefers
session-hot + symbol-match files; budget cap respected; sub-threshold prompt →
empty. Loopback — `/context/retrieve` returns JSON; injection-off → empty.
Hands-on (already proven in the spike, re-run on the pinned build): the marker
reaches the model via `chat.message`; the Claude hook prints `additionalContext`.

---

## Phase E — Docs, settings polish, tests, release

- README / `docs/FEATURES.md` / `docs/MAINTENANCE.md`: rename Code Graph →
  Code Intelligence; document memory, injection (opt-in + budgets + the
  `.opencode/` git-exclude + never-`--pure` notes), analyses, the schema-reset
  migration, and the loopback routes (`/context/retrieve`, `/memory/event`) as
  new local surfaces (same trust model as `/graph_run`).
- `ToolsReference` (`GRAPH_TOOLS`): add the 5 new tools with examples.
- Guidance addenda text (memory recall/note nudge) beside `GRAPH_GUIDANCE`.
- Full `cargo test` + `npm run check`; CHANGELOG entry; version bump; release
  merge per `feedback_git_release_workflow` (develop → main, tag).

---

## Appendix — consolidated change surface

**New MCP tools** (5): `graph_dead_exports`, `graph_cycles`, `context_recall`,
`context_note`, `context_notes` — all in `graph/mcp.rs::tool_specs`, both
consumers, gated by `graph.enabled` (+ remote-worker gate).

**New IPC commands**: `graph_dead_exports`, `graph_cycles`, `graph_memory`,
`graph_memory_clear`, `graph_context_preview` (+ existing graph IPCs unchanged).

**New loopback routes**: `POST /context/retrieve`, `POST /memory/event`.

**New settings** (`GraphSettings`): `context_injection`,
`context_per_file_chars`, `context_turn_budget_chars`, `context_include_session`,
`context_min_score`.

**New CLI subcommand**: `cimp --context-hook` (Claude UserPromptSubmit shim).

**Schema**: `symbol.visibility` column; `session`/`mem_event`/`mem_note`
relations; `GRAPH_SCHEMA_VERSION` reset-migration.

**New Rust modules/files**: `graph/context.rs` (retrieval/ranking). New methods
on `GraphIndex` (`dead_exports`, `import_cycles`), `GraphService` (memory +
context), and `Symbol.visibility`/`Visibility` in `graph/model.rs`.

**Frontend**: `GraphMonitorView.svelte` → `CodeIntelligenceView.svelte` (5-section
router); `graph.ts` new fns/types; `Pane.svelte` import; `SettingsApp.svelte`
label + Context subsection.

**Backend touch points**: `oob/mod.rs` (`OobContext.mem`), `oob/claude.rs`
(tap + session-id capture), `pty/manager.rs:212` (wire `GraphService` into
`OobContext`), `tabs/config.rs` (Claude hook overlay + OpenCode plugin writer +
`.git/info/exclude`), `main.rs` (`--context-hook`).

**Per-file model note for the visibility bit:** `emit_symbol` (`builder.rs:678`)
is the single choke point for the bespoke walkers; the generic `tags.rs` engine
routes through it too, so the field lands in one place and each caller supplies
`Visibility`.

# V10 — Context Engine (session memory · auto-injection · code analyses)

**Status:** SPEC (written 2026-07-08). Not yet coded.
**Builds on:** V9-01 code knowledge graph (`docs/MILESTONE-V9-01-code-knowledge-graph.md`), V9-02 multi-language graph. Reuses the V8 offload service + loopback and the V20 out-of-band transcript tail.

## Why

GrapeRoot ("Dual-Graph") ships three things our code graph doesn't, and they're all
graph-adjacent, so they belong in one place:

1. **Session/action memory** — a second graph of what the agent *read, edited,
   queried, and decided* this session, so context **compounds** across turns
   ("after the first question, the AI goes straight to the right files").
2. **Automatic context pre-loading** — rank the files relevant to the user's
   prompt and inject a budget-bounded digest **before** the agent starts
   exploring, turning explore-tokens into reason-tokens (their headline
   30–70% cost claim).
3. **Packaged analyses** — turnkey *dead-export detection* and *dependency-cycle
   finder* over the import graph (their Pro tier).

Our graph is already structurally deeper (Datalog call graph, real embeddings,
struct search, ~28 langs, dual cloud+worker consumers). V10 closes the three
gaps **on top of** that graph and folds the info + status for all of it into the
one reserved tab — which outgrows the name "Code Graph" and gets renamed.

Everything stays **local + per-project** (`.cimp/`, git-ignored), matching the
existing graph posture; opt-in, default off.

---

## Tab rename + consolidation

The reserved, app-rendered `TabId::GraphMonitor` tab (`GraphMonitorView.svelte`)
becomes the home for all of V10. Keep the **internal** id `GraphMonitor` stable
(no schema/tab migration — see `src/lib/tabs/types.ts:isGraphMonitorTab`,
`state/manager.rs`, `settings/persistence.rs`); change only the **display label +
icon** and add section navigation inside the view.

**Name — DECIDED: "Code Intelligence".** Umbrella term covering graph + memory +
context + analyses. Applies to the tab label, the tab icon, and user-facing docs.

The Settings section "Code graph" is renamed to match (cosmetic; the settings
*key* `graph` stays for back-compat, or a one-time rename with a serde alias).

**New view layout** — a left rail (or top segmented control) with five sections,
each fed by the existing `graph-status` event + new IPCs:

1. **Index** — today's Overview: build state, per-language census buttons,
   node/edge counts, embedder health, Rebuild/Refresh (unchanged).
2. **Activity** — today's "Recent calls" ring, extended to also show session
   working-set events (reads/edits), filterable by source (claude/offload/user).
3. **Memory** *(new, Feature 1)* — the current session's working set + the notes
   store.
4. **Context** *(new, Feature 2)* — auto-injection on/off, budgets, and a live
   "last injected context / tokens saved" panel.
5. **Analyses** *(new, Feature 3)* — Dead exports / Dependency cycles reports.

---

## Feature 1 — Session / action memory graph

### Goal
Persist, per project, a rolling record of what each agent session touched and
decided, so (a) the agent can recall its own working set, and (b) Feature 2 can
rank "files this session already cares about" first.

### Where the events come from (no new interception)
We already tail the agent's transcript JSONL out-of-band for TTS (V20,
`src-tauri/src/oob/`). That stream contains every `tool_use` the agent emits:
`Read`, `Edit`/`Write`, `Bash`, `Grep`, and our `graph_*` MCP calls. **Tap the
same tail** to derive memory events — no extra process, no PTY scraping:

- `Read` / `Grep` on a path → `read` event, anchored to the file (and symbol if
  we can map the line via the graph).
- `Edit` / `Write` → `edit` event.
- `graph_*` calls already land in `graph::activity` (the ring) with
  `source = claude|offload`; unify that ring into the same event store rather
  than keeping two.
- **Decisions/facts** are explicit, written by the agent via a new tool
  (below) — we don't try to infer them.

### Data model (CozoDB, in the existing `graph.db`)
New relations alongside the graph (one writer = the app; consumers read-only):

```
session      { session_id => started_ms, agent, cwd, last_ms }
mem_event    { session_id, seq => kind, path, symbol?, line?, ts_ms, detail }
                kind ∈ read | edit | query | note
mem_note     { session_id, note_id => text, ts_ms, pinned }   # decisions/facts
```

`session_id` is the transcript/session identifier we already track for TTS.
Ring-bounded per session (cap ~500 events) and per project (cap ~20 sessions,
oldest evicted), so the store can't grow unbounded. A "working set" for a session
is `mem_event` grouped by `path`, scored by `recency × frequency × kind-weight`
(edit > query > read).

### Tools (added to `graph::mcp::tool_specs`, both consumers)
- `context_recall` — "what has this session been working on?" → ranked working
  set (files + top symbols + recent edits), token-bounded. This is what lets the
  agent "go straight to the right files."
- `context_note { text, pin? }` — record a decision/fact for this session
  (writes `mem_note`).
- `context_notes` — list this session's notes (+ pinned notes carried across
  sessions in the same project).

Guidance addendum (`GRAPH_GUIDANCE` sibling in `tabs/config.rs`) nudges the agent
to call `context_note` when it makes a non-obvious decision, and `context_recall`
at the start of a follow-up task.

### UI — Memory section
- **This session:** ranked working-set table (file · touches · last-kind · when),
  and a Notes list (pinned first). Live via the event bus.
- **Recent sessions:** collapsed rows; click to inspect a past session's set.
- Small actions: Pin/unpin a note, Clear this session, Clear project memory.

### Edge cases
- **No transcript / OpenCode differences:** memory degrades to graph-tool events
  only (still useful). Feature announced health-accurately like semantic search.
- **Privacy:** same as the graph — per-project `.cimp/`, git-ignored, never
  leaves the machine. The remote offload worker gets memory tools **only** under
  the existing `graph.allow_remote_worker_access` gate (defense-in-depth in the
  host router, same as graph tools).

---

## Feature 2 — Automatic context pre-loading (budget-bounded injection)

### Goal
Before the agent processes a user prompt, prepend a compact, ranked "relevant
context" block so it spends tokens reasoning rather than re-discovering the same
files. Opt-in; default off (auto-injecting into every turn costs tokens and can
mislead — it must be a deliberate choice).

### How injection actually happens (fits our launch model)
GrapeRoot injects via a Claude Code plugin **hook**. We already own the entire
launch: we inject a `--settings` overlay and an `--append-system-prompt`, and we
run a loopback HTTP server (V8-03) that the MCP child and warm graph path call.
So:

- Add a **`UserPromptSubmit` hook** to the injected Claude settings overlay
  (never seed `~/.claude` — same rule as the statusline/`--settings` work). The
  hook is a tiny `cimp` subcommand (or a curl-free shell shim) that POSTs the
  user's prompt to the app loopback and emits the returned markdown as
  `additionalContext`.
- New loopback route **`POST /context/retrieve`** `{ prompt, cwd, session_id }`
  → `{ context_md, files_used, chars, tokens_est }`, served by a new
  `graph::context` module against the warm `GraphIndex` + session memory.
- OpenCode: **parity required before ship (DECIDED).** Injection lands for both
  Claude and OpenCode together. **D0 spike RESOLVED (2026-07-08) — gate cleared:**
  OpenCode's plugin SDK exposes **`experimental.chat.messages.transform`**:
  `(input: {}, output: { messages: { info: Message; parts: Part[] }[] }) => Promise<void>`.
  The handler is async (can `fetch` our loopback) and can rewrite the **entire
  messages array**, including the last user message's `parts` — so it reads the
  user's text, calls `/context/retrieve`, and prepends a context part. This is a
  direct parity with Claude's `UserPromptSubmit` hook. (Runners-up: `chat.message`
  sees the message but is observational; `chat.params` can only tune
  temperature/topP etc., not inject text; `experimental.chat.system.transform`
  injects into the *system* prompt but does **not** receive the user message —
  see OpenCode issues #17637/#27401 — so it can't do query-conditioned
  injection.)
  Both agents share the *same* `POST /context/retrieve` loopback + ranking; only
  the per-agent shim differs (Claude: settings-overlay hook → cimp subcommand;
  OpenCode: the plugin above).

  **Hands-on verification DONE (2026-07-08) — both risks retired.** Built a
  capture harness: OpenCode **1.17.11** (matching `@opencode-ai/plugin@1.17.11`,
  whose real `.d.ts` was read to confirm the hook shapes) run headless via
  `opencode run` with `OPENCODE_CONFIG_CONTENT` (cImp's exact mechanism) pointed
  at a custom openai-compatible `provider` whose baseURL is a **fake local LLM
  that logs the outbound request body**. A plugin in `<project>/.opencode/plugin/`
  injected markers; the captured request to the model contained:
  `"hello there"\n\n[[CIMP_CTX_INJECTED_via_chat_message]]\n\n[[CIMP_CTX_INJECTED_via_transform]]`.
  Findings:
  - **Both `chat.message` (non-experimental) and `experimental.chat.messages.transform`
    reach the model.** Prefer **`chat.message`** — it's not experimental and its
    `output.parts` is the incoming user message. Mutate **in place**
    (`part.text += …` on the existing text part). Do **not** `push` a bare
    `{type,text}` part: it fails schema validation (`invalid user part before
    save` — a Part needs `id`/`sessionID`/`messageID`); in-place text edit
    sidesteps that entirely.
  - **Hooks are `async`** and `PluginInput` exposes `serverUrl` + a Bun `$` shell
    + the opencode `client`, so the handler can `fetch` our `/context/retrieve`
    loopback. (Fetch not separately E2E'd; async execution confirmed.)
  - **Plugin delivery: `<project>/.opencode/plugin/` (singular) works** for a
    dependency-free `.js` ESM module — confirmed loaded + fired. This is the
    delivery path for cImp. **Caveat (the one real cost):** OpenCode also writes
    `.opencode/.gitignore` and, if the plugin/provider pulls npm deps, a
    `.opencode/node_modules/` into the project — so it **touches the user's
    repo**. Mitigation: keep the injection plugin **dependency-free** (node
    built-ins + global `fetch` only, so no launch-time `bun install`) and add
    `.opencode/` to `.git/info/exclude` at launch. Still-open nicety (not on the
    critical path): whether the config `plugin` array accepts a `file:`/absolute
    specifier so the plugin can live next-to-exe with **zero** repo touch —
    worth a follow-up test.
  - **Gotchas for the cImp launcher:** (1) **never pass `--pure`** — it disables
    all external plugins. (2) A custom `npm` provider triggers a one-time
    `bun install` (slow, needs network once); irrelevant if we keep using the
    user's real provider and only add our zero-dep plugin. (3) The
    experimental-hook-mutation-discard bug (issue #17100) affects
    `system.transform`, **not** the hooks we use — but since we chose
    `chat.message`, we're clear of it regardless.

### Retrieval + ranking (`graph::context::retrieve`)
Cheap, no new index — compose what we have:
1. Extract salient terms from the prompt (identifiers, quoted strings, file-ish
   tokens).
2. Candidate files from: `find_symbol`/`references` on terms → their files;
   `search_docs` + `semantic_docs` (if embedder up) hits; **session working set**
   (Feature 1) with a recency boost.
3. Score = term-match + graph-centrality (inbound edges) + session-recency;
   dedup by file.
4. **Budget-pack** (mirrors GrapeRoot's knobs): per-file char cap +
   per-turn char cap. For each chosen file emit a *digest*, not the whole file —
   prefer `outline` (signatures) + the top-matching snippet, so a 2k-line file
   costs ~300 chars, not 2000. Fall back to first-N-chars when no outline.
5. Return markdown:
   ```
   ## Relevant context (cImp)
   - `src/foo.rs` — fn a(), fn b(); match near L42: …
   - `docs/design.md` [Auth] — …
   (session working set: src/foo.rs edited 2 turns ago)
   ```

### Settings (`GraphSettings`, new fields)
- `context_injection: bool` (default false)
- `context_per_file_chars: u32` (default 800)
- `context_turn_budget_chars: u32` (default 6000)
- `context_include_session: bool` (default true)
- `context_min_score` threshold to suppress low-signal turns (inject nothing when
  nothing is clearly relevant — avoids noise on "hi" / meta prompts).

### UI — Context section
- On/off toggle + the four budget sliders (live-editable, like the semantic
  fields today).
- **Last injection panel:** the prompt (truncated), the files chosen, chars used
  / budget, and a running **"est. tokens injected"** counter per session so the
  user can see the tradeoff (we inject to save exploration, but injection isn't
  free).
- A "Preview for a prompt…" box: type a prompt, see what *would* be injected,
  without running the agent — the debugging surface for tuning budgets.

### Edge cases
- **Injection ≠ savings guaranteed:** be honest in the UI — show injected tokens,
  not a fabricated "saved X%". Let the user judge.
- **Stale index:** retrieval uses the warm index; if a rebuild is mid-flight,
  serve from what's there (never block the prompt). Hook has a tight timeout
  (~300 ms) and emits nothing on miss — a slow retrieval must never stall the
  user's turn.
- **Loop risk:** the hook must not itself trigger graph tool logging that feeds
  back into memory as agent activity; tag loopback-origin reads so they're
  excluded from `mem_event`.

---

## Feature 3 — Packaged analyses (dead exports, dependency cycles)

### Goal
Two project-wide reports that are cheap given the graph, useful to both the agent
(as tools) and the human (as tab buttons).

### Backend (`graph::index` — new Datalog queries)
- **Dead exports** `dead_exports()` → symbols that are public/exported yet have
  **zero** references and zero callers project-wide, minus a small entrypoint
  allowlist (`main`, `#[test]`, `pub` re-exports, framework hooks). Report as
  **candidates** (dynamic dispatch, external API, macro use → false positives are
  still possible; label clearly).
  **Visibility bit first (DECIDED):** add a real visibility/export flag to the
  extraction layer *before* shipping this, rather than approximating with
  "zero inbound edges." Concretely: a `visibility` field on `model::Symbol`
  (`public | private | crate | unknown`), populated by (a) the bespoke walkers
  (`parse_rust` reads `pub`/`pub(crate)`; `parse_js_ts` reads `export`;
  `parse_python` uses name-underscore convention) and (b) the generic `tags.scm`
  engine via a `@definition.public` capture convention per language, defaulting
  to `unknown` where a grammar can't tell. Persisted in the `symbol` relation
  (schema bump). Dead-export = `visibility = public AND zero inbound edges`, so
  private helpers and unknowable symbols are excluded — far fewer false
  positives. This is the one part of Phase B with real cross-language surface;
  languages whose tags don't yet mark visibility simply never report dead
  exports (accurate, not wrong).
- **Dependency cycles** `import_cycles()` → strongly-connected components in the
  file-level `imports` edge relation via recursive Datalog (same technique as
  `transitive`). Report each cycle as an ordered file loop.
- (Stretch, cheap follow-ons the same machinery gives: **orphan files** = no
  inbound imports and no exported symbol used; **hotspots** = highest inbound
  edge count.)

### Tools
- `graph_dead_exports {}` → list of candidate unused exports (file:line, name).
- `graph_cycles {}` → list of import cycles.
Both bounded by `max_rows_per_query`, both recorded in the activity ring.

### UI — Analyses section
- Two buttons ("Find dead exports", "Find dependency cycles"), each runs the
  query on the warm index and renders a results table (click a row → the file
  path, copyable). Results are advisory; a caption states the false-positive
  caveat for dead exports.
- Cheap enough to run on demand; no background scheduling in V1.

---

## Phasing

| Phase | Scope | Notes |
|---|---|---|
| **A. Tab shell + rename → "Code Intelligence"** | Section nav in `GraphMonitorView`, label/icon rename, keep `GraphMonitor` id | Pure UI; unblocks everything |
| **B1. Visibility bit** | `Symbol.visibility` field, populate in bespoke walkers + `tags.scm` convention, schema bump, re-index | Cross-language; the gate for accurate dead exports |
| **B2. Analyses** | `dead_exports` (uses B1) + `import_cycles` queries, 2 tools, Analyses section | Ships the first user-visible win |
| **C. Session memory** | mem relations, transcript-tap event derivation, unify activity ring, 3 tools, Memory section | Reuses OOB tail |
| **D0. OpenCode injection spike** | ✔✔ VERIFIED HANDS-ON (OpenCode 1.17.11) — `chat.message` in-place text mutation reaches the model; plugin loads from `<project>/.opencode/plugin/`. Both parity risks retired | **Gate cleared, empirically.** Chosen hook: `chat.message` (non-experimental); keep plugin dep-free; never pass `--pure` |
| **D. Context injection (Claude + OpenCode)** | `graph::context` + loopback route + per-agent injection shim + budgets + Context section | Highest risk (hook timing + parity); opt-in |
| **E. Docs/settings/tests** | README/FEATURES/MAINTENANCE, settings UI, guidance addenda, unit+integration tests | Per repo convention |

Suggested order **A → B1 → B2 → C → D0 → D → E**: rename first, add the
visibility bit, land analyses, then the memory layer Feature 2 depends on, then
spike + build injection for both agents.

## Decisions — RESOLVED
1. **Tab name** — **Code Intelligence.** ✔
2. **Injection scope** — **Block on OpenCode parity.** ✔ Injection ships for
   Claude + OpenCode together; Phase D opens with the D0 spike that gates it.
3. **Dead-export precision** — **Add the visibility bit first** (Phase B1) before
   shipping dead-export detection. ✔
4. **Numbering** *(open, cosmetic)* — filed as V10 (new "context" pillar) vs
   V9-03 (extends the graph pillar). V10 chosen to signal the broadened identity;
   trivially renamable.

## Cost note
Implementation fan-out (multi-file edits, parser work) should use Sonnet/Haiku
for the mechanical passes and reserve Opus for the injection-ranking design and
review — per the standing agent-cost guidance.

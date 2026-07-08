# V11 — Token Efficiency (Context Engine II)

**Status:** SPEC (written 2026-07-08). Not yet coded.
**Builds on:** V10 context engine (`docs/MILESTONE-V10-context-engine.md` — session
memory, `/context/retrieve`, the Claude `UserPromptSubmit` hook shim in
`context_hook.rs`, the OpenCode `.opencode/plugin`), V9-01/-02 graph, V8-03
loopback + offload host.

## Why

V10 made context *available* (memory, injection, analyses). V11 makes context
*cheap*. The three biggest token sinks in a real agentic session are:

1. **Whole-file `Read`s** to see one function — a 2k-line file costs ~20k tokens
   when the agent needed 40 lines.
2. **Re-exploration after compaction** — the summary loses the working set and
   the agent re-reads what it already knew.
3. **Redundant injection** — V10 re-ranks and can re-inject the same digests
   every turn.

Every feature below attacks one of those, and every one composes from shipped
machinery: symbol spans already carry `start_line`/`end_line`
(`graph::model::Symbol`), the loopback already brokers hook↔app traffic, and the
offload pool gives us a free local model for compression. All opt-in knobs
default consistent with V10's posture (tools on when graph is on; *behavioral*
hooks opt-in, default off).

---

## Feature 1 — `graph_snippet` (symbol-body fetch)

### Goal
Let the agent fetch **just a definition body** instead of `Read`ing the file.
Highest-ROI item in the milestone: small backend, large behavioral shift.

### Tool (added to `graph::mcp::tool_specs`, both consumers)
`graph_snippet { symbol?, file?, line?, context_lines? }`
- By name: resolve via the same lookup as `graph_find_symbol`; on multiple hits
  return the disambiguation list (same shape as `find_symbol`) instead of a body.
- By `file` + `line`: return the enclosing symbol's span (the graph already maps
  line → symbol for memory events).
- Returns: a header line (`file:start-end · kind · visibility · N callers`) +
  the source slice `start_line..=end_line` (± `context_lines`, default 0),
  capped by `max_snippet_bytes` semantics (a new `max_body_bytes`, default
  16 KiB, since bodies are larger than result-row snippets).
- Reads the file from disk at call time (spans can be a few lines stale between
  watcher debounces — include the graph's indexed-at note when the file mtime is
  newer than the last index pass, so the agent knows the span may have drifted).

### Guidance
Extend `GRAPH_GUIDANCE` (`tabs/config.rs`): *"For files over ~300 lines, prefer
`graph_outline` → `graph_snippet` over reading the whole file."* Same addendum
in the OpenCode instructions file.

### Edge cases
- Span drift after edits mid-debounce: serve the slice anyway + a `stale: true`
  flag; never block on a rebuild.
- Symbol spans that are the whole file (top-level scripts): fall back to
  outline + a "use Read with offset/limit" hint rather than dumping the file.

---

## Feature 2 — `graph_repo_map` (session-start orientation map)

### Goal
Aider's headline token trick, done with a real graph: a **once-per-session**,
budget-bounded skeleton of the project's most central symbols, so the agent
starts oriented and burns fewer explore-turns. Distinct from V10 per-prompt
injection: this is *orientation*, that is *relevance*.

### Backend (`graph::context`, new fn `repo_map(budget_chars)`)
- Rank files by **inbound edge count** (import + call edges — the same
  centrality signal `/context/retrieve` scoring uses), boost by session
  working-set recency when memory has data.
- Emit per file: path + the top exported signatures (from `Symbol.signature`,
  `visibility = public` first), greedily packed to `budget_chars`.
- Cache the rendered map on the warm index; invalidate on watcher re-index.

### Exposure (two consumers, one renderer)
- **Tool:** `graph_repo_map { budget_chars? }` — agent-pullable any time.
- **Injection (opt-in):** when `context_injection` is on and a *new session* is
  detected (first `/context/retrieve` call for a `session_id`), prepend the map
  once to that first turn's context block. No new hook needed — the V10 shim
  already posts every prompt with `session_id`.

### Settings (`GraphSettings`)
- `repo_map_budget_chars: u32` (default 4000)
- `repo_map_on_session_start: bool` (default false — it rides the existing
  `context_injection` master toggle AND this flag)

---

## Feature 3 — Injection delta/dedup (stop re-sending unchanged digests)

### Goal
V10 injects per-turn with no memory of what it already injected. Track per
session what was sent and inject only **deltas**.

### Design (`graph::context`)
- New in-memory (per-session) table: `injected { session_id, path => digest_hash, ts }`,
  persisted alongside `mem_event` so it survives app restarts within a session.
- On `/context/retrieve`: a candidate file that was already injected **and** is
  unchanged (watcher mtime ≤ injected ts, digest hash equal) is demoted to a
  one-line reminder (`- src/foo.rs — injected turn 3, unchanged`) or dropped
  entirely when budget is tight. Changed files re-inject with a `(updated)` tag.
- Compaction interaction: after a compaction the agent may have *lost* the
  earlier injection. The PreCompact hook (Feature 4) clears the session's
  `injected` table so the next turn re-injects fresh. Until Feature 4 lands,
  a `context_dedup_ttl_turns` cap (default 10) bounds how long a dedup
  suppression lives.

### UI
Context section's "last injection" panel gains a per-file badge:
`new / unchanged (skipped) / updated`. The running est-tokens counter now also
shows **tokens avoided by dedup** — measured, not fabricated (chars of the
digests that were suppressed).

---

## Feature 4 — Compaction survival (`PreCompact` memory injection)

### Goal
Compaction is where sessions bleed tokens: the summary loses the working set
and the agent re-explores. Feed the compactor the session's ranked working set +
pinned notes so they survive verbatim.

### Design
- **Claude:** add a `PreCompact` hook to the injected settings overlay (same
  overlay that carries `UserPromptSubmit` — `tabs/config.rs` /
  `statusline/mod.rs` command builder). The shim (`cimp --precompact-hook`, a
  sibling of `context_hook.rs`) POSTs to a new loopback route
  **`POST /context/compaction`** `{ session_id, cwd, trigger }` → the app
  returns a compact block: ranked working set (top ~10 files, one line each),
  pinned `mem_note`s verbatim, unpinned notes summarized. Shim emits it as the
  hook's additional-context/custom-instructions payload.
- **D0-style spike (gate):** verify the exact `PreCompact` hook output contract
  (which JSON field reaches the compaction prompt, and size limits) against the
  shipped Claude Code version — same hands-on method as the V10 OpenCode spike
  (capture harness, assert the marker lands in the compaction request).
- **OpenCode:** no known compaction hook — **degrade gracefully** (Claude-only
  at ship; the plugin's `tool.execute.after` memory feed still keeps recall
  working, and `context_recall` remains the manual fallback). Recorded as an
  accepted asymmetry (see Decisions) — unlike V10 injection, this feature
  *improves* an agent-internal event Claude alone exposes, so parity-blocking
  would mean shipping nothing.
- On success, also clear the session's `injected` dedup table (Feature 3).

### Settings
`compaction_context: bool` (default **true** when `context_injection` is on —
it costs a few hundred chars *once per compaction* and pays for itself
immediately; still master-gated by the injection toggle).

---

## Feature 5 — Redundant-read advisor (`PreToolUse` / `tool.execute.before`)

### Goal
The agent re-`Read`s files it already read this session, unchanged. Session
memory knows both facts; intercept the read and answer with a cheap reminder
instead of 2k lines. **The most behavior-altering feature here — strictly
opt-in, default off.**

### Design
- **Claude:** `PreToolUse` hook (matcher: `Read`) in the settings overlay →
  `cimp --pretooluse-hook` shim → **`POST /context/should_read`**
  `{ session_id, cwd, file_path }` → verdict:
  - `pass` (default): emit nothing, the Read proceeds. Always `pass` when the
    file changed since the last read, was never read, is small (< ~300 lines),
    or memory has no data.
  - `remind`: deny the Read with a reason containing the **outline digest** +
    `"unchanged since you read it (turn N). Re-read with Read({file, offset,
    limit}) if you need exact text."` Never a bare refusal — the agent must
    always get usable content, because its context may have been compacted away.
- **Compaction interaction (the real hazard):** after compaction the agent
  genuinely lost the file content. The `/context/compaction` route (Feature 4)
  marks the session `post_compaction`; `should_read` then passes everything
  until each file is re-read once. Feature 5 therefore **depends on Feature 4**
  and ships after it.
- **One remind per file per session** — if the agent asks again after a remind,
  pass. (An agent that insists knows better than our heuristic.)
- **OpenCode:** the plugin already hooks `tool.execute.after`; a
  `tool.execute.before` handler can call the same route. Spike whether a
  "before" hook can veto/replace the tool result in the shipped OpenCode; if
  not, Claude-only, same degradation posture as Feature 4.

### Settings
`read_advisor: bool` (default false), `read_advisor_min_lines: u32` (default 300).

### UI
Activity section logs `remind` events (file, tokens of the Read it displaced,
estimated from file size). Honest accounting: estimated, labeled as such.

---

## Feature 6 — Offload digest compression (local model writes the digests)

### Goal
`/context/retrieve` falls back to outline + first-N-chars for files without a
useful outline (docs, configs, long scripts). Route those through the **local**
backend to produce a 3-line semantic digest — cloud tokens saved, local GPU
does the work, nothing leaves the machine.

### Design (`graph::context` → `offload` internal call)
- New internal path: `offload::run_internal(prompt, max_tokens, deadline)` that
  bypasses the MCP tool surface and the router's cloud tiers — **local backends
  only, never remote/cloud** (digests contain project source; the
  `allow_remote_worker_access` gate does not apply because we simply never
  route these off-box).
- Async + cached: digests are computed **out-of-band** (on index pass or first
  miss) and cached in `graph.db` (`digest { file, content_hash => text, ts }`),
  because the injection hook has a ~300 ms budget and must never wait on an
  LLM. A retrieve that misses the cache uses the V10 fallback and *enqueues*
  the digest for next time.
- Only files that actually rank into injections get digested (demand-driven,
  bounded queue) — not the whole repo.

### Settings
`context_llm_digests: bool` (default false; requires an offload backend
configured and ready — announced health-accurately like semantic search).

---

## Feature 7 — Code embeddings (semantic search over code, not just docs)

### Goal
"Where is the retry-backoff logic?" currently costs a Grep fan-out with large
payloads. Embed **symbol-level code chunks** so one `graph_semantic_code` call
answers it. Also feeds `/context/retrieve` ranking as a fourth candidate source.

### Design
- Reuse the whole `graph::embed` pipeline (Qwen3-Embedding endpoint, HNSW,
  dims auto-probe): new chunk kind `code_chunk` = `signature + doc + body`
  truncated to the embedder's window, one per symbol (functions/methods/types;
  skip trivial spans < 3 lines).
- Same health gating as doc embeddings: embedder down ⇒ feature reported
  degraded, everything else unaffected.
- Tool: `graph_semantic_code { query, k? }` → ranked symbols (id, file:line,
  signature, score) — rows, not bodies; the agent chains `graph_snippet`
  (Feature 1) to pull the one body it wants. That chain (search → snippet) is
  the token-efficient replacement for grep → Read.
- Volume note: symbols ≫ doc chunks (this repo: tens of thousands). Embed
  **public + top-centrality first**, cap via `semantic_code_max_chunks`
  (default 20 000), and batch at index-idle so a rebuild isn't blocked on the
  embedder.

### Settings
`semantic_code: bool` (default false — the embedding pass is the one genuinely
expensive step in this milestone), `semantic_code_max_chunks: u32`.

---

## Phasing

| Phase | Scope | Notes |
|---|---|---|
| **A. `graph_snippet`** | Tool + guidance addenda | No schema change; ships alone |
| **B. Repo map** | `repo_map()` + tool + session-start injection + settings | Caching on the warm index |
| **C. Injection dedup** | `injected` table + retrieve changes + Context-panel badges | Small; pairs with D |
| **D0. PreCompact spike** | Verify hook output contract w/ capture harness | Gates D; V10-spike method |
| **D. Compaction survival** | `/context/compaction` route + `--precompact-hook` shim + overlay | Clears dedup table |
| **E. Read advisor** | `/context/should_read` + `PreToolUse` shim + OpenCode before-hook spike | Depends on D; opt-in |
| **F. LLM digests** | `run_internal` local-only path + digest cache + queue | Independent of D/E |
| **G. Code embeddings** | `code_chunk` extraction + `graph_semantic_code` + gating | Heaviest; last |
| **H. Docs/tests** | README/FEATURES/MAINTENANCE, settings UI, unit+integration | Per repo convention |

Suggested order **A → B → C → D0 → D → E → F → G → H**. A/B/C are a coherent
first release (pure additive, no hooks beyond what V10 ships); D/E are the
hook-behavior half; F/G are independent tails.

## Decisions — OPEN

1. **Feature 4/5 parity posture** — proposed: ship Claude-first with graceful
   OpenCode degradation (unlike V10 injection, these hook agent-internal events
   OpenCode may not expose). Confirm before Phase D.
2. **Read-advisor default** — proposed off. Could default on once field data
   (Activity logs) shows the reminder is never harmful.
3. **`max_body_bytes` for snippets** — 16 KiB proposed; tune against real
   symbol-size distribution once Phase A lands.

## Cost note

Phases A–C are mechanical (Sonnet/Haiku fan-out fine). Reserve Opus for the D0
spike analysis, the read-advisor verdict heuristics, and review — per the
standing agent-cost guidance.

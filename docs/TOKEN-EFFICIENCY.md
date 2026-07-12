# Token Efficiency in cImp — Features & the Effectiveness Card

*Written 2026-07-12. Grounded in `src/lib/CodeIntelligenceView.svelte`,
`src-tauri/src/graph/service.rs`, and `docs/FEATURES.md`. For the original
design rationale see `docs/MILESTONE-V11-token-efficiency.md`.*

cImp attacks cloud-token spend from four directions, layered across
milestones. This doc lists each feature, how it saves tokens, and then
explains how to read the **Effectiveness** card in Code Intelligence →
Overview → Usage.

---

## 1. Task offload (V8) — don't send the work to the cloud at all

The `offload_task` / `offload_batch` MCP tools let the cloud model hand a
self-contained subtask (broad searches, log/file summarization, web research)
to a **local llama-server**. The heavy input never enters the cloud context;
only the synthesized result comes back. The local worker has its own native
tools (`read_file`, `code_search`, `run_command`) plus user-configured MCP
servers, so it can do multi-step work independently.

## 2. Code graph tools (V9 / V12 / V15) — indexed answers instead of file dumps

`graph_find_symbol`, `graph_callers` / `graph_callees`, `graph_references`,
`graph_outline`, `graph_impact`, `graph_tests_for`, `graph_path`,
`graph_architecture` answer code-structure questions from the CozoDB index
with precise, token-bounded rows — replacing grep fan-outs and whole-file
Reads. `run_check` returns deduplicated, grouped diagnostics (≤5 sample sites
per group) instead of a raw compiler/linter dump.

## 3. Context engine (V10) — inject relevance so the agent doesn't explore

Session memory tracks what files each agent session touched; on every prompt,
`/context/retrieve` (via the Claude `UserPromptSubmit` hook / OpenCode
plugin) injects a budget-bounded set of ranked file digests. The bet: a few
hundred injected chars displace multi-thousand-token exploration turns.

## 4. Token Efficiency milestone (V11) — the dedicated push

All shipped:

- **`graph_snippet`** — fetch one definition's body (16 KiB cap,
  `max_body_bytes`) instead of Reading a 2k-line file. Flagged `stale` when
  the on-disk hash has drifted from the index.
- **`graph_repo_map`** — a once-per-session, char-budgeted
  (`repo_map_budget_chars`) skeleton of the most call-central files and their
  top exported signatures, so the agent starts oriented instead of burning
  explore turns. Agent-pullable any time; opt-in first-prompt injection
  (`repo_map_on_session_start`).
- **Injection dedup** — a file already injected in full and unchanged is
  demoted to a one-line "unchanged" reminder on later turns instead of being
  re-sent; re-injects with an `(updated)` tag on change or after
  `context_dedup_ttl_turns` (default 10).
- **Compaction survival** — a Claude `PreCompact` hook
  (`cimp --precompact-hook` → `POST /context/compaction`) feeds the compactor
  the session's ranked working set + pinned notes and clears the dedup state,
  so the agent doesn't re-explore after compaction (the biggest single token
  bleed in long sessions). `compaction_context`, default on.
- **Redundant-read advisor** (opt-in, off by default) — a Claude `PreToolUse`
  hook on `Read` (`cimp --read-hook` → `POST /context/should_read`) denies a
  re-read of an unchanged already-read file, replying with its outline
  (`advise` mode) or outline + relevant symbol body (`substitute` mode)
  instead of the full content — never a bare refusal. One reminder per file
  per session; always passes right after a compaction.
- **Local-model context digests** — files with no useful outline
  (docs/configs/long scripts) get a ≤3-line semantic digest written by the
  **local** offload backend (never routed off-box) and cached, so injection
  stays compact without cloud spend. `context_llm_digests`.
- **Code embeddings + `graph_semantic_code`** — symbol-level semantic code
  search returning signature rows (never bodies) to chain into
  `graph_snippet`; the search → snippet chain is the token-efficient
  replacement for grep → Read.

## 5. Measurement layer (V14)

The **Usage** section in Code Intelligence: per-turn stacked token bars, a
top-consumers table, per-session totals + cache-hit ratio, the Effectiveness
panel below, and a **budget-tuning Advisor** that proposes measured,
propose-and-confirm changes to the V10/V11 knobs (e.g. raise
`context_min_score` when injected files go unused; raise
`read_advisor_min_lines` when reminders are usually followed by a full
re-read anyway).

---

## The Effectiveness card

Location: **Code Intelligence → Overview → Usage**. Design rule, stated in a
comment in the component itself: **"measured counters, never fabricated
savings."** Every number is a real character count taken at the point the
event happened; token figures are simply `chars ÷ 4` and are explicitly
badged `est.`. All counters are in-memory, so they read as **"since app
restart,"** not a permanent ledger.

| Value | What it measures | Scope | Savings or spend? |
|---|---|---|---|
| **chars injected** | Cumulative chars of full digests actually injected into prompts by the context engine, summed from each retrieve's measured output | This project's sessions, since restart | **Spend** — what injection costs |
| **chars suppressed by dedup** | Chars of digests that *would* have been re-injected but were demoted/dropped because the file was unchanged since its last injection | This project's sessions, since restart | **Direct measured saving** |
| **chars displaced by read-advisor** | Sizes of the file Reads the advisor denied with a reminder (Activity events `source: read_advisor`, `tool: remind`) | Process-wide (Activity store has no per-project key), since process start | **Saving, estimated** — hence the `est.` label |
| **tasks served locally** | Count of offload jobs the local llama-server handled instead of the cloud model (filled in from the OffloadService) | Offload host | **Proxy count** — whole subtasks diverted; volume on the Offload Server tab |

### How to interpret it

- **Dedup + advisor-displaced are the genuine savings numbers** — real
  content that was measurably not sent to the cloud.
- **Injected is the spend those savings ride on.** It only pays off if
  injected files displace exploration; the Advisor's injection-follow-rate
  rule checks exactly that (were injected files later read/edited in the
  session?) and proposes raising `context_min_score` when they go unused.
- **Tasks served locally** is a count, not a token figure — each one
  represents an entire subtask's tokens kept off the cloud bill; the actual
  local token volume lives on the Offload Server tab.

A healthy readout is dedup/advisor chars growing relative to injected chars
over a session. When they don't (injections unused, reminders always followed
by a full re-read anyway), the Advisor card above the panel exists precisely
to propose tightening the knobs.

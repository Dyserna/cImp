# V12 — Agentic Inner Loop (impact · tests · diagnostics · durable memory · proactive automation)

**Status:** SPEC (written 2026-07-08). Not yet coded.
**Builds on:** V9-01/-02 graph (`graph_transitive`, extraction walkers), V10
session memory (`graph::memory`, `mem_note`), V11 `graph_snippet` (Feature 1
there is the natural companion to `graph_impact` here), V8 offload host native
tools (`read_file` / `code_search` / `run_command`).

## Why

V11 makes each turn cheaper. V12 makes the agent's **edit → check → fix loop**
tighter and better-aimed:

- Agents answer "what else could this break?" badly and expensively — the graph
  can answer it exactly.
- Agents run the whole test suite (slow, huge output) when the graph knows
  which tests exercise the changed symbols.
- Raw `cargo check` / `tsc` dumps are 10–50k tokens of repetition; a
  deduplicated structured report is ~1k.
- Session memory evicts at ~20 sessions; the durable lessons inside it die
  with it. A distillation pass promotes them to project facts.
- Commit history is the densest free documentation a repo has; the ranker
  ignores it today.
- And the meta-gap: every tool above is **agent-pull** — it only saves tokens
  if the model chooses to call it, and guidance nudges decay after
  compaction. Feature 6 converts the highest-value pulls into harness-driven
  hooks, so the loop tightens even when the model never asks.

All local, all per-project, same `.cimp/` posture as V9/V10.

---

## Feature 1 — `run_check` (structured diagnostics)

### Goal
One tool that runs the project's checker(s) and returns **deduplicated,
structured diagnostics** instead of a raw dump. This is the single biggest
inner-loop token cut after whole-file reads.

### Design
- **Configured per project** in the `.cimp/config.json` overlay:
  ```
  checks: [ { name: "cargo",  cmd: "cargo check --message-format=json", parser: "cargo-json" },
            { name: "tsc",    cmd: "npx tsc --noEmit --pretty false",   parser: "tsc" } ]
  ```
  Shipped parsers: `cargo-json` (serde over the message stream), `tsc`,
  `eslint-json`, `pytest` (summary-line + failures), and `generic-gcc`
  (file:line:col regex) as the fallback. Parser = a small trait in a new
  `src-tauri/src/checks/` module.
- **Tool:** `run_check { name?, changed_only? }` (added to
  `graph::mcp::tool_specs` and the offload host's native tool set, same
  security posture as `run_command` — the command comes from the *user's*
  config, never from the model; a model-supplied `name` only selects among
  configured entries).
- **Output:** grouped by unique diagnostic (code + message template), each with
  first occurrence `file:line`, total count, and up to N sample locations;
  bounded like every graph tool (`max_rows_per_query`). A 400-error `tsc` run
  becomes ~30 rows.
- `changed_only: true` filters to diagnostics in files touched since HEAD
  (git status) — pairs with Feature 2.
- **UI:** results also land in the Activity ring (name, duration, error count),
  so the human sees the loop the agent is running.

### Edge cases
- No `checks` configured: the tool reports "not configured" with a pointer to
  the setting — never guesses a build command.
- Long runs: hard timeout (default 120 s, per-check override), partial output
  parsed on timeout and labeled as such.

---

## Feature 2 — `graph_impact` (blast radius for a diff)

### Goal
Given the working-tree diff (or an explicit symbol list), return the transitive
dependents — *the* question agents ask badly ("what else could this break?").

### Design (`graph::index`, composing existing queries)
- Input modes: `graph_impact {}` (default: working-tree diff vs HEAD via
  `git diff --unified=0`, mapped through symbol spans to changed symbols) or
  `graph_impact { symbols: [...] }`.
- Compute: changed symbols → inbound `calls`/`references` edges →
  **transitive callers** (the existing recursive-Datalog machinery behind
  `graph_transitive`, reversed), capped by depth (default 3) and
  `max_rows_per_query`.
- Output: per changed symbol, the dependent tree flattened to
  `file:line · symbol · depth`, plus a file-level rollup ("7 files depend on
  your change"). Name-only (unresolved) references are included but flagged
  approximate — same honesty convention as `graph_references`.
- Diff→symbol mapping reuses the line→enclosing-symbol lookup that memory
  events use.

### UI
Analyses section gains a third button: **"Impact of working-tree changes"** —
same table treatment as dead exports / cycles.

---

## Feature 3 — Test↔symbol mapping (`graph_tests_for` / `graph_tested_by`)

### Goal
Close the loop opened by Feature 2: change → affected tests → run just those.

### Design
- **Extraction:** tag test definitions with an `is_test` bit on
  `graph::model::Symbol` (schema bump, like V10's `visibility`):
  bespoke walkers read `#[test]`/`#[tokio::test]` (Rust), `test()/it()/describe()`
  callee names + `*.test.ts`/`*.spec.ts` (JS/TS), `test_*` in `test_*.py` /
  pytest conventions (Python); the generic `tags.scm` engine gets an optional
  `@definition.test` capture, defaulting to path-convention heuristics
  (`tests/`, `*_test.go`). Languages that can't tell simply have no test bits —
  accurate, not wrong (same posture as visibility).
- **Query:** `tests_for(symbol)` = test symbols whose transitive **callee**
  closure (existing `transitive` machinery) reaches the symbol, depth-capped.
  Inverse `tested_by` is the same query from the other end.
- **Tools:** `graph_tests_for { symbol | file }`, and `graph_impact` (Feature 2)
  gains `include_tests: true` to append the affected-test list to its report —
  the intended agentic chain is *impact → tests_for → run_check(pytest/cargo
  test with a filter)*.
- **Honesty:** dynamic dispatch / fixtures make this approximate; results are
  labeled candidates, same caveat convention as dead exports.

---

## Feature 4 — Git-aware context

### Goal
Rank-boost recently-churned files and attach the last commit message touching a
matched symbol — commit messages are free, dense documentation.

### Design
- New lightweight relation in `graph.db`, populated at index pass and watcher
  ticks from `git log --since=90.days --name-only --format=...` (bounded, no
  full-history walk): `commit_touch { file => last_commit_ts, last_subject,
  touch_count_90d }`.
- **Ranking:** `/context/retrieve` scoring adds a churn term (files touched in
  the last week get a boost; the exact weight sits next to the existing
  term-match/centrality/recency weights in `graph::context`).
- **Context lines:** injected digests and `graph_find_symbol` rows gain an
  optional trailer: `last change: "fix: cap retry at 30s" (3d ago)`.
- **Tool:** `graph_recent_changes { days?, path_prefix? }` → churn-ranked files
  with subjects — the "what's been happening here" question every fresh session
  asks.
- Not a blame index (no per-line attribution — that's a much bigger table);
  file-level only, 90-day window, size-bounded.

### Edge cases
- Not a git repo / shallow clone: feature reports unavailable, everything else
  unaffected (standard degraded-health convention).

---

## Feature 5 — Memory distillation (durable project facts)

### Goal
Session memory (V10) caps at ~20 sessions and evicts. Before a session ages
out, distill its events + notes into 2–3 durable **project facts** so the
project *accumulates* knowledge instead of forgetting it. This is the
difference from GrapeRoot — and the local model does it for free.

### Design
- New relation: `project_fact { fact_id => text, source_session, ts_ms, pinned,
  archived }` — capped (~100 live facts), oldest-unpinned archived first.
- **Distiller:** when a session is about to be evicted (or goes idle > 24 h), an
  out-of-band job sends its working set + notes to the **local-only** offload
  path (`offload::run_internal` from V11 Feature 6 — never remote/cloud) with a
  tight extraction prompt ("2–3 non-obvious, durable facts a future session
  would need; skip anything derivable from the code"). Output validated
  (length caps, count caps) before insert. No local backend ready ⇒ the session
  evicts undistilled — memory keeps V10 semantics, feature reports degraded.
- **Recall:** `context_recall` output gains a "project facts" section (pinned
  first); facts also become a ranked candidate source for `/context/retrieve`
  (a fact mentioning `foo.rs` boosts that file).
- **UI:** Memory section gains a **Facts** list: pin / edit / delete / add
  manually (facts are user-legible state, not a black box).
- **Promotion (fully automatic recall):** `context_recall` is still
  agent-pull. Opt-in `promote_pinned_facts: bool` (default false): **pinned**
  facts — and only pinned, the human-curated tier — are appended to the
  launch-time guidance payload (Claude `--append-system-prompt`, OpenCode
  instructions file) inside a clearly marked `cImp project facts` block,
  capped ~1500 chars. Durable knowledge then arrives with zero tool calls.
  Launch-time only; facts pinned mid-session apply on the next launch, and
  the Facts UI says so.

### Edge cases
- Distillation quality is model-dependent: facts carry their source session id,
  are individually deletable, and the whole feature sits behind
  `memory_distillation: bool` (default false until the prompt is tuned on real
  sessions).

---

## Feature 6 — Proactive automation (the harness acts, unasked)

### Goal
Convert this milestone's agent-pull tools into automatic behavior at the
three moments the agent most needs them and least asks: after an edit, after
a *risky* edit, and after a rebuild. All reuse the V10/V11
shim → loopback pattern; the behavior hooks are opt-in, default off — same
posture as V11's read advisor.

### 6a — Auto-check after edits
A `PostToolUse` hook (matcher `Edit|Write|MultiEdit`) POSTs to a new loopback
route `POST /context/post_edit`. The app debounces (agent edits arrive in
bursts), runs the configured checks with `changed_only`, diffs the diagnostic
groups against the session's previous run, and returns **only new/worsened**
diagnostics as additional context (bounded, in Feature 1's structured
format). The agent learns it broke something in the same turn, in ~1k tokens,
instead of three turns later via a raw build dump. Nothing new ⇒ inject
nothing.

### 6b — Auto-impact on risky edits
Same hook, same route: when the edited span maps to a symbol whose inbound
edge count ≥ `auto_impact_min_dependents`, append a two-line blast-radius
note ("14 dependents across 6 files; 3 tests cover this — `graph_impact` /
`graph_tests_for` for the list"). The moments the agent most needs impact
analysis are exactly the moments it doesn't think to ask.

### 6c — Analyses on a trigger
Dead exports and cycles (V10) are human-pull buttons today. Run both after
each completed index pass (cheap on the warm index), store the counts, and
badge the Analyses section when they change ("+3 import cycles"); optionally
one appended line in the V11 repo map when counts grew. Removes the
"never clicked it" failure mode; the on-demand buttons stay.

### Settings
`auto_check: bool` (default false), `auto_check_debounce_s: u32` (default 5),
`auto_impact_min_dependents: u32` (default 10), `analyses_auto: bool`
(default true — read-only, cheap, feeds only a badge).

### Edge cases
- **Hook contract:** `PostToolUse` additional-context delivery is verified by
  the same capture-harness method as V11's D0 spike (one harness run covers
  `PreCompact` + `PostToolUse`).
- **OpenCode:** the plugin's `tool.execute.after` is already wired for memory
  events; whether its return can carry context back to the model is spiked,
  not assumed — if it can't, parked results drain via the retrieve path
  below.
- **Never block the turn:** the hook has a tight timeout; a slow check parks
  its report per session and the next `/context/retrieve` (or the next
  post-edit call) drains it.

---

## Phasing

| Phase | Scope | Notes |
|---|---|---|
| **A. `run_check`** | `checks/` module + parsers + tool + config overlay schema + Activity | Independent; ships first (biggest win) |
| **B. `graph_impact`** | Diff→symbol mapping + reverse-transitive query + tool + Analyses button | Pure graph work |
| **C. Test mapping** | `is_test` bit (schema bump + re-index) + queries + tools + impact integration | Cross-language surface like V10-B1 |
| **D. Git-aware context** | `commit_touch` relation + ranking term + trailer + `graph_recent_changes` | Small; independent |
| **E. Distillation** | `project_fact` relation + distiller job + recall/retrieve integration + Facts UI | Depends on V11-F (`run_internal`) |
| **F. Proactive automation** | `PostToolUse` spike + `/context/post_edit` + auto-check/auto-impact + analyses trigger + fact promotion | Depends on A (checks), B (impact), E (facts); hooks opt-in |
| **G. Docs/tests** | README/FEATURES/MAINTENANCE, settings UI, guidance addenda, unit+integration | Per repo convention |

Suggested order **A → B → C → D → E → F → G**. A alone is a worthwhile
release; F is where the milestone stops depending on the model's tool-calling
discipline.

## Decisions — OPEN

1. **`run_check` exposure** — proposed: both the cloud tabs (MCP) and the
   offload worker's native tool set. Confirm the worker actually needs it
   (running checks is usually the orchestrator's job).
2. **Impact default depth** — proposed 3. Tune on this repo's graph once B lands.
3. **Distillation default** — off until prompt quality is validated; revisit
   after ~2 weeks of real sessions.
4. **`auto_check` default** — proposed off (a behavior hook, like the V11
   read advisor). Graduation evidence: Activity logs showing auto-check
   injections were followed by a fix in the same turn at a high rate.

## Cost note

Parsers and extraction tagging are mechanical (Sonnet/Haiku). Reserve Opus for
the distillation prompt design and the impact-query review — per the standing
agent-cost guidance.

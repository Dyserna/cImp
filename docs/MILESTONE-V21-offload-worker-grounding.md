# V21 — Offload Worker Grounding & Abilities

**Status:** SPEC (written 2026-07-13, extended same day with Features 4–9).
Not yet coded.
**Builds on:** V8 local offload (the agent loop in `offload/agent.rs`, native
tools in `offload/tools/`, `ToolCtx::confine` at `tools/mod.rs:64`), V8-02
tier routing (`offload/router.rs`), V8-03 tool toggles (`OffloadToolToggles`
in `settings/schema.rs:1719`), V12 `run_check` (`checks/` module; already on
the MCP surface at `offload/mcp.rs:193`), the `offload_task`/`offload_batch`
MCP surface (`offload/mcp.rs`), the OpenAI-compat client
(`offload/openai.rs::ChatRequest:100`).

## Why

A live `offload_batch` smoke test (2026-07-13) produced a confident, wrong
answer: asked to count the top-level `.md` files in `docs/`, the worker
(Qwen3.6-35B-A3B Q5, Q8 KV) claimed 26 files and listed several that don't
exist at that path, while missing three that do. Root-cause analysis:

1. **The worker cannot enumerate a directory.** Its native surface is
   `read_file` (needs an exact path), `code_search` (greps content), and
   `run_command` — which is deny-by-default (`command_allowlist` defaults
   empty, `settings/schema.rs:1176`). "List/count files" tasks are
   unanswerable with the tools it has, so the model reconstructs a plausible
   inventory from search snippets instead.
2. **The system prompt licenses guessing.** `SYSTEM_PROMPT`
   (`offload/agent.rs:252`) says "make reasonable assumptions" with no
   epistemic guardrail scoping that to task *interpretation* — the model
   reads it as permission to assert unverified filesystem/code facts.
3. **`thinking: off` skips the care that catches this.** With
   `ThinkingMode::Off`, `think_on_turn` (`agent.rs:276`) never reasons — not
   on planning ("do my tools even cover this?") and not on the final turn,
   which is also where the scratch-narration leak ("Good, `docs/README.md`
   exists. Now let me…") came from. `Auto` thinks on exactly those two turns.

Not a quantization problem: Q5_K weights + Q8 KV produce noise-level errors,
not coherent invented file lists. This is a capability gap plus compliance
pressure, and it reproduces at any precision.

Features 1–3 close that specific failure. Features 4–9 (added after the
follow-up "what else would help" review) harden the same trust boundary from
the other side — mechanical verification the model can't talk its way past,
a quality signal the router can act on, and ability upgrades (proof-by-
`run_check`, git history, grammar-enforced output) that raise what the
worker can be *trusted* to do rather than just what it attempts.

No graph.db schema bump anywhere; settings changes are additive
`serde(default)` fields (no migration). Every feature is independently
shippable; suggested order: 1→2→3 (the original incident), then 4, 8, 9
(loop-level guards), then 6, 7 (new abilities), then 5 (needs 4's marker).

---

## Feature 1 — `list_dir` native tool

### Goal
Give the worker a first-class, read-only, confined way to enumerate
directories, eliminating the "reconstruct the file list from memory" failure
class at the source. Native tool, not a `run_command` allowlist entry —
cross-platform (`ls` vs `dir`), zero user setup, and confined by the same
root machinery as `read_file`.

### Design (`src-tauri/src/offload/tools/list_dir.rs`, new)
- **Params:** `path` (string, required — directory, resolved and confined via
  `ToolCtx::confine` exactly like `read_file`), `max_depth` (int, default 1,
  clamped to 1–3), `glob` (string, optional — filename filter, e.g. `*.md`,
  matched with the same glob crate `code_search` uses).
- **Output:** one entry per line, dirs suffixed `/`, files with byte size —
  `NAME<TAB>SIZE` — sorted dirs-first then alphabetical, so counts and
  filtering are trivial for the model. Header line states the resolved
  directory and total entry count *before* capping, so a truncated listing
  can never masquerade as a complete one.
- **Caps:** 500 entries and 32 KiB output (mirroring
  `run_command::MAX_OUTPUT_BYTES`), with the standard
  `[result truncated — …]` marker from `cap_result`. Always skip `.git`;
  everything else is listed (the worker legitimately inspects `target/`,
  `node_modules/` etc. when asked).
- **Registration:** `def()`/`execute()` following `read_file.rs`; wire into
  `enabled_defs` and `dispatch` (`tools/mod.rs:111,128`). Tool description
  tells the model this is *the* way to answer "what files exist / how many"
  questions.
- **Toggle:** `OffloadToolToggles.list_dir: bool`, default **true**
  (`serde(default)` struct — old settings files deserialize fine).

### Tests
Confinement (escape via `..` and absolute-outside-root rejected); glob
filter; depth clamp; entry cap sets the truncation marker while the header
count stays accurate; `.git` skipped; toggle off ⇒ not in `enabled_defs`;
dispatch routes.

---

## Feature 2 — Verified-facts system prompt

### Goal
Close the compliance-pressure hole: the worker must not assert filesystem or
code facts it did not observe through a tool call in the current run, and
must say "could not verify" instead of guessing.

### Design (`offload/agent.rs::SYSTEM_PROMPT`)
Extend the prompt with an epistemic rule and scope the assumptions clause:

> Only state filesystem or code facts (paths, file lists, counts, contents,
> versions) that you verified with a tool call in this run. Never
> reconstruct file lists, contents, or counts from memory or from search
> snippets. If your tools cannot answer part of the task, say so explicitly
> in your answer instead of guessing. Do not ask clarifying questions; make
> reasonable assumptions **about the task's intent** and state them — this
> licence covers interpretation, never facts.

Also add final-answer discipline (pairs with Feature 3's leak fix):

> Your final message must be only the synthesized answer — no running
> narration of tool steps.

Prompt-only; no settings. The out-of-budget nudge (`agent.rs:719`) gains the
same "state what you could not verify" clause, since budget exhaustion is the
other path that pressures the model into asserting unverified claims.

### Tests
Prompt constants are exercised indirectly; add a unit test pinning that
`SYSTEM_PROMPT` contains the "verified with a tool call" sentence (tripwire
against accidental rewording dropping the rule), mirroring how other
load-bearing strings are pinned.

---

## Feature 3 — Thinking guard for tool-using runs

### Goal
`thinking: off` should mean "don't burn reasoning tokens on cheap
transforms", not "skip verification care on agentic runs". Two prongs:
steer the orchestrator's choice, and make `Off` fail safe when tools
actually get used.

### Design
- **Steering (`offload/mcp.rs`):** tighten the `thinking` param description
  on both `offload_task` (`:249`) and `offload_batch` (`:536`): `'off'` is
  for pure transforms of *provided* context (summarize/extract/reformat from
  the `context` arg); any task that needs tool calls (file reads, searches,
  counting, web) should use `'auto'` (default) or `'on'`. The main-session
  guidance text that describes `offload_task` gets the same one-line rule.
- **Fail-safe (`offload/agent.rs`):** `think_on_turn(mode, is_planning,
  is_final)` gains a `used_tools: bool` (the loop already knows whether any
  tool call happened this run). New rule: `Off` thinks on the **final** turn
  iff `used_tools` — one bounded thinking pass to reconcile evidence and
  synthesize cleanly, exactly the turn where both the wrong-count and the
  scratch-leak failures occurred. Planning stays non-thinking under `Off`
  (the orchestrator asked for cheap; we only spend where the damage is).
  `Auto`/`On` unchanged. `gen_reserve` already budgets for thinking-heavy
  turns, so no context-window change.
- No new setting: the guard is strictly-better and bounded (at most one
  thinking turn added, only when the run already paid for tool round-trips).
  If field use shows a backend where this hurts, gate it then.

### Tests
`think_on_turn` truth table extended: `Off` + `used_tools` + final ⇒ true;
`Off` + no tools ⇒ all false; `Off` + `used_tools` + planning ⇒ false;
`Auto`/`On` rows unchanged.

---

## Feature 4 — Evidence citations + mechanical answer verifier

### Goal
Make grounding *checkable*: every factual claim in the final answer must
trace to a tool observation from this run, and the loop verifies that
deterministically — a guard the model cannot talk its way past, in the same
family as the leaked-tool-call guard and the out-of-budget nudge.

### Design (`offload/agent.rs`)
- **Citations (prompt-level):** the loop labels each tool result message
  with an observation id (`[T1]`, `[T2]`, …) as it appends it to the convo.
  `SYSTEM_PROMPT` gains: "cite the observation supporting each factual claim
  (`[T3]`), and cite nothing you did not observe." Citations are stripped
  from the answer before it is returned to the orchestrator (regex removal
  of `\[T\d+\]`), so downstream consumers see clean prose.
- **Observed-set collection:** the loop already sees every tool call and
  result; accumulate an *observed set* per run — file paths that were
  `read_file`d / `list_dir`ed (both the listed dir and each entry),
  `code_search` match paths, and paths/symbols returned by graph tools and
  `run_check`. Normalize through the same resolution `ToolCtx::confine`
  uses so spelling variants can't cause false alarms.
- **Verifier pass (post-final, deterministic):** scan the final answer for
  path-like tokens (contains `/` or `\` with a file extension, or matches a
  quoted `` `…` `` span that resolves under an allowed root). Each mention
  must be in the observed set. On violations: **one** corrective turn —
  "your answer mentions `X`, `Y` which you never observed this run; verify
  them with tools or remove/mark them unverified" — then re-check. Still
  dirty after the retry ⇒ return the answer with an appended
  `[worker note: the following mentions were not verified by any tool call:
  …]` so the orchestrator sees the taint instead of silently trusting it.
- **Bounds:** verifier is string-scanning only (no I/O beyond what the run
  did), the corrective turn counts against `max_steps`/deadline as normal,
  and the whole feature is inert when the final answer mentions no paths.
- **Accounting:** the corrective turn appears in `RunTrace` with a new
  `call_kind` of `"verify"` so the Offload Server tab's run log shows when
  the guard fired.

### Settings
None — constant-on guard, matching the loop's other guards.

### Tests
Observed-set accumulation across all four tool kinds; path-token extraction
(windows + posix separators, backtick spans, extension heuristic); clean
answer ⇒ no extra turn; unobserved mention ⇒ exactly one corrective turn;
still-dirty ⇒ taint note appended; citations stripped from the returned
answer; `[T…]` labels present on tool messages.

---

## Feature 5 — Confidence marker + tier escalation

### Goal
The worker states how grounded its answer is; the router acts on it.
`fast`-tier answers that come back partially verified get one automatic
retry on the `quality` backend instead of returning a shrug to the
orchestrator.

### Design
- **Marker (`agent.rs`):** `SYSTEM_PROMPT` requires the final message to end
  with one line: `verified: fully` or `verified: partially — <what could not
  be verified>`. Feature 4's verifier *overrides* the model's self-report:
  if the taint note fired, the marker is rewritten to `partially` with the
  unverified mentions. The marker line is parsed off the answer; the
  orchestrator-facing result carries it as a trailing footer (kept — the
  orchestrator *should* see it).
- **Escalation (`offload/mcp.rs::run_one` + `offload/router.rs`):** when the
  run resolved to the fast backend (router decision already known at call
  time), the marker parses as `partially`, and a distinct quality backend is
  configured and ready ⇒ re-run the task once on the quality backend and
  return the better of the two (quality result wins unless *it* is also
  `partially` and the fast one was `fully` — can't happen by construction,
  but ties go to quality). Escalated runs are labeled in the run log
  (`escalated_from: fast`) and in the returned footer so the cost is
  visible. No escalation for `offload_batch` subtasks beyond the same
  per-subtask rule; concurrency/slot rules unchanged.
- **Loop safety:** at most one escalation per task, never quality→quality,
  never when the backends are the same instance.

### Settings
`offload.escalate_partial: bool`, default **true** (inert unless a second
backend exists, so zero-config setups see no change).

### Tests
Marker parse (fully/partially/missing ⇒ treated as partially); verifier
override wins over self-report; escalation fires only for fast + partial +
distinct-ready-quality; single-escalation bound; footer and run-log
labeling; toggle off ⇒ no escalation.

---

## Feature 6 — `run_check` as a worker-native tool

### Goal
Let the worker *prove* claims about code instead of asserting them: "this
compiles", "these two tests fail", "the lint is clean" become tool
observations (which Feature 4 can then count as evidence) rather than
plausible text.

### Design (`offload/tools/mod.rs` + `checks/`)
- The V12 MCP surface already exposes `run_check` to Claude/OpenCode
  (`offload/mcp.rs:193`); the worker's native dispatch does not. Add a
  `"run_check"` route in `tools::dispatch` (`tools/mod.rs:128`) beside the
  `graph_` route, calling the same checks entry point the MCP handler uses —
  same `CheckDef` resolution, same parser/dedup machinery, same structured
  (bounded) report. No new execution surface: `run_check` only runs the
  project's *configured* check commands, which the user already vetted.
- Advertise its `ToolDef` in `enabled_defs` when checks are configured for
  the project root; description tells the model to use it to verify
  build/test/lint claims before stating them.
- **Known gate, worth stating:** exposure on BOTH surfaces (MCP and worker)
  requires a non-empty top-level `checks` setting (`graph/mcp.rs:337`) —
  a fresh project sees no `run_check` anywhere ("run_check MCP tool isn't
  exposed in this session"), by design. The dogfood repo itself has no
  `checks` configured as of 2026-07-13; the F6 live probe below requires
  configuring them first (e.g. `cargo` check + `cargo-test` parsers, both
  already implemented). The discoverability fix — a checks editor,
  exposure-status line, and language auto-detection — is specced separately
  as **V22** (`MILESTONE-V22-run-check-generalization.md`); F6 here only
  needs the gate itself. **(V22 implements that discoverability fix — checks
  editor, exposure-status line, and language auto-detection — see its Phase E,
  which renders this exact "run_check exposed: MCP ✓ / offload worker ✓" line.)**
- Long checks vs. slot deadlines: `run_check` inherits the checks module's
  own timeout; the tool result on timeout says so explicitly ("check timed
  out — report as unverified"), which composes with Feature 2's
  say-what-you-couldn't-verify rule.

### Settings
`OffloadToolToggles.run_check: bool`, default **true** (same
`serde(default)` struct as `list_dir`).

### Tests
Dispatch routes; not advertised when no checks configured or toggle off;
timeout result carries the unverified wording; report size stays within the
existing checks caps.

---

## Feature 7 — Curated read-only command preset

### Goal
`run_command` is inert on every fresh install (`command_allowlist` defaults
empty, deny-by-default — correct, but it means git history questions are
unanswerable). Ship a one-click, curated, read-only preset so users stop
hand-authoring the obvious safe set.

### Design
- **Preset constant** (`settings/schema.rs`, next to
  `default_command_policies():1674`): `git`, `cargo metadata`, `cargo tree`.
  `git` is already hardened by the default `CommandPolicy` (read probes
  allowed, exec vectors blocked — `run_command.rs` tests pin this). Add a
  `cargo` policy allowing only the `metadata` and `tree` subcommands (both
  resolve/read; neither runs build scripts) and denying everything else, so
  an allowlisted `cargo` can never reach `cargo run`/`build`.
- **UI:** Settings → Tools → Offload native tools gains an "Enable safe
  read-only commands" button — merges the preset into `command_allowlist`
  (no duplicates) and installs the `cargo` policy if absent. It's a
  merge-into-settings action, not a mode: users see exactly what got added
  and can prune it.
- **Explicitly not in the preset:** anything that writes, fetches the
  network by default, or executes project code (`npm`, `make`, bare
  `cargo`). The preset can grow later; each addition needs a policy story.

### Settings
No new fields — this populates the existing `command_allowlist` +
`command_policies`.

### Tests
Preset merge is idempotent; `cargo metadata`/`cargo tree` pass the policy,
`cargo run`/`cargo build`/glued forms are blocked; existing git policy tests
unchanged.

---

## Feature 8 — Identical-call short-circuit (loop breaker)

### Goal
Small models loop. A repeated tool call with identical arguments burns slot
time and steps for zero information; short-circuit it and tell the model to
change course.

### Design (`offload/agent.rs` loop)
- Per-run `HashMap<(tool name, canonical args JSON), first result>`. Canonical
  = `serde_json` value with object keys sorted, so key-order variants match.
- Second identical call ⇒ return the cached result *without executing*,
  prefixed: `[repeat call — identical to an earlier call this run; result
  unchanged. Try a different tool, different arguments, or answer with what
  you have.]` Third and later ⇒ short error only (no result body), keeping
  the pressure to move on while never wedging the loop.
- Exemption: `run_check` re-executes (its whole point is observing change —
  though within one worker run nothing should change, the cost asymmetry
  says don't cache it). Everything else (reads, searches, listings, graph
  queries) is deterministic within a run and caches safely.
- Cache is bounded by `max_steps` (≤16 entries) — no size management needed.
- Repeated-call events get a marker in the `RunTrace` call record so loops
  are visible in the Offload Server tab.

### Settings
None — constant-on.

### Tests
Key canonicalization (key order, whitespace); second call served from cache
with the nudge, executor not invoked (spy); third call gets the short form;
`run_check` exempt; distinct args miss.

---

## Feature 9 — Grammar-enforced structured output

### Goal
An optional `schema` on `offload_task`/`offload_batch` whose enforcement
happens at the *sampler* level (llama.cpp grammar), so the returned answer
is guaranteed-parseable JSON — composable into scripts and workflows, no
prose re-parsing. Structurally eliminates narration leaks: narration can't
fit the grammar.

### Design
- **MCP surface (`offload/mcp.rs`):** new optional `schema` param (JSON
  Schema object) on `offload_task` and per-subtask on `offload_batch`.
  Threaded through `OffloadTask`.
- **Request plumbing (`offload/openai.rs::ChatRequest:100`):** new
  `response_format: Option<serde_json::Value>` field
  (`skip_serializing_if = "Option::is_none"` like its siblings), serialized
  as llama-server's `{"type": "json_schema", "json_schema": {…}}`.
- **Only the final turn is constrained:** tool-call turns must stay free-form
  (tool calling has its own format). Set `response_format` on the
  final-synthesis request — both the natural final turn and the forced-final
  path (`agent.rs:548`) — never on planning/ingestion turns.
- **Thinking interaction (spike first):** verify how the pinned llama-server
  build combines `enable_thinking` with a JSON grammar on one request (the
  `<think>` block must land in `reasoning_content`, not get strangled by the
  grammar). If the combination misbehaves, suppress thinking on the
  constrained final turn (`chat_template_kwargs` already carries the
  per-request switch) and note it in the run log.
- **Validation & result:** parse the final text with `serde_json` as a
  belt-and-braces check (grammar should make failure impossible; a parse
  failure ⇒ return an explicit error string, never a half-JSON blob). The
  MCP result is the JSON text verbatim.
- **Interplay:** Feature 4's citation stripping skips schema runs (citations
  would violate the schema; the prompt instead tells the model to cite
  nothing and the verifier still runs on the JSON's string values). Feature
  5's marker moves into the footer only, never into the JSON.

### Settings
None — per-call API surface.

### Tests
`response_format` present only on final-turn requests; forced-final path
carries it; parse-failure returns the explicit error; batch threads
per-subtask schemas; schema runs skip citation stripping; footer marker
stays out of the JSON body.

---

## Decisions (proposed defaults)

1. **`list_dir` as a native tool, not a `run_command` allowlist recipe** —
   cross-platform, zero-setup, confined. Allowlisting `ls`/`dir` would leave
   fresh installs (empty allowlist) exactly as broken as today.
2. **Guards (Features 3, 4, 8) without settings toggles** — bounded,
   strictly-better, matching the loop's existing guards; revisit only on
   field evidence.
3. **Verifier is advisory-after-one-retry, not blocking** — a taint note
   beats an error: the orchestrator can still use the verified parts, and a
   hard failure would push users to disable the guard.
4. **Escalation defaults on but is structurally inert** without a second
   backend — zero-config setups are unaffected.
5. **`cargo` preset limited to `metadata`/`tree`** — the first subcommands
   that are read-only *and* don't execute project code; the preset grows
   only with a policy story per addition.
6. **Numbering:** V18 was never used; V19/V20 are shipped. This is V21.

## Out of scope

- Recursive tree dumps / repo-map output from `list_dir` (the graph's
  `graph_repo_map` already covers structure-at-a-glance; `list_dir` is for
  ground-truth enumeration).
- Sampling changes — `temperature: 0.2` (`agent.rs:586`) is appropriate for
  tool work. Known aside, *not* V21 work: Qwen3 upstream recommends higher
  temperatures in thinking mode to avoid repetition loops; if the worker is
  ever seen looping under `thinking: on`, a per-mode temperature split
  (~0.6 thinking / 0.2 non-thinking) is the fix.
- LLM-based hallucination scoring on worker output — Feature 4's
  deterministic verifier is the V21 answer; semantic claim-checking stays an
  orchestrator-side concern.
- **Speculative decoding** (draft model via llama-server `--model-draft`) —
  evaluated and deliberately skipped this milestone: it buys throughput, not
  correctness, and VRAM headroom on the current setup is unproven. Revisit
  as a standalone spike if slot deadlines become the binding constraint.

## Verification (live)

Re-run the failing smoke: `offload_batch` with the "count top-level `.md`
files in `docs/`" task at `thinking: off`. Expect: correct count via
`list_dir`, no invented filenames, clean final synthesis with no narration;
and a variant task the tools *cannot* answer (e.g. "list files on drive Q:")
must come back "could not verify", not a guess.

Additional live probes per feature:
- **F4:** a task whose answer tempts unobserved paths ("which files import
  X?" answered from graph hits) — confirm citations in the run log, a clean
  stripped answer, and that a deliberately-baited prompt ("also mention
  src/nonexistent.rs") triggers the corrective turn.
- **F5:** with both backends up, a fast-tier task designed to come back
  `partially` — confirm one quality-tier re-run, footer + run-log labels.
- **F6:** "does this project's test suite pass?" — answer must cite a
  `run_check` observation, not an assertion.
- **F7:** click the preset, then ask "summarize the last 5 commits" —
  answered via `git log`, while `cargo build` through `run_command` stays
  blocked.
- **F8:** run-log shows the repeat marker on a task engineered to loop
  (tiny context + a search that returns nothing).
- **F9:** `offload_task` with a small schema (`{count, files[]}`) on the
  docs/ question — result parses with `serde_json`, no prose, no marker
  contamination; check thinking-mode interaction on the pinned llama-server
  build first (the spike in F9's design).

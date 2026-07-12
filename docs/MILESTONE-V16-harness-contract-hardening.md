# V16 — Harness Contract Hardening (Read Advisor II)

**Status:** SPEC (written 2026-07-12). Not yet coded, except Feature 6's
basic controls (shipped 2026-07-12 ahead of the milestone): the Token
efficiency group (`read_advisor` + min-lines + mode, `context_llm_digests`)
and the injection-nested knobs (`context_dedup_ttl_turns`,
`repo_map_on_session_start`, `compaction_context`) are in
`SettingsApp.svelte`. Still owed by Feature 6: the E1-fail disabled state
(Feature 0), the trust-TTL input (Feature 5), and the
`context_llm_digests` offload-health awareness.
**Builds on:** V11 token efficiency (`read_hook.rs` / `compact_hook.rs`, the
`should_read` verdict path, remind-once state), V14 workflow visibility (the
Advisor card + versioned rules in `advisor.rs`, the Usage section, `usage_stat`
via the OOB transcript tap), V10 context engine (hook overlay in
`tabs/config.rs`, the generated `.opencode/plugin`), the persistent Activity
store (`crate::activity`).

## Why

The V10–V14 context features depend on **behavior contracts of two
user-installed, self-updating CLIs cImp does not pin** — Claude Code and
OpenCode. `docs/MAINTENANCE.md` ("Claude Code / OpenCode CLIs — hook & plugin
behavior contracts") now inventories those contracts and a manual periodic
check, but three structural problems remain:

1. **Two contracts were never verified at all.** The V11 `TODO(spike)` block
   still stands: whether a `PreCompact` hook's `additionalContext` reaches the
   compaction prompt (D0), and whether a `PreToolUse` deny's
   `permissionDecisionReason` reaches **the model** (E1). The read advisor's
   whole value rides on E1 — if the reason is dropped, every remind is a bare
   refusal, the exact failure mode the V11 spec said must cancel the feature.
2. **Drift is silent.** A harness auto-update that changes a hook payload or
   deny semantics produces no error anywhere — hooks fail open by design. The
   only current detection is a human noticing the Effectiveness counters
   flatline.
3. **The V11 behavioral mitigations have known gaps** (recorded in
   `docs/TOKEN-EFFICIENCY.md` and the maintenance entry): shell-workaround
   reads are invisible, the advisor trusts an agent's memory indefinitely
   within a session, and none of the V11 behavioral toggles ever got a
   Settings UI control — `read_advisor` can only be enabled by hand-editing
   `settings.json`.
4. **The Usage section's accounting misleads on two axes** (cache-read
   analysis, 2026-07-12). (a) The per-turn bars plot raw token counts, but
   the four token types differ ~50× in price (cache read ~0.1× base input,
   output ~5×) — cache reads at 90% of volume read as "90% of cost" when
   they're often nearer half. (b) The Effectiveness panel counts displaced
   chars **once**, but every char kept out of context is *re-saved as a
   cache read on every subsequent turn* (the API re-sends the whole
   conversation per turn) — the panel systematically undersells the V11
   features on exactly the metric users look at.

Every feature below reuses shipped machinery: the OOB transcript tap already
parses every Claude tool call, the Advisor card already renders versioned
propose-and-confirm rules with per-rule dismiss memory, and the Activity store
already persists per-source event streams. No new hooks are added (one is
*considered* and rejected — see Feature 4's design note).

**Posture.** Everything here is detection + verification + UI; nothing changes
agent behavior beyond the existing V11 features, so nothing needs a new
opt-in. The one behavior-adjacent knob (Feature 5's trust TTL) tightens an
existing opt-in feature and defaults off.

---

## Feature 0 — Contract verification baseline (close the V11 spikes)

### Goal
Hands-on verify the two `TODO(spike)` contracts against the currently
installed Claude Code, using the V10 capture-harness method. This is a
prerequisite, not just hygiene: Feature 1's version tripwire needs a
**last-verified version** to compare against, and E1's outcome decides whether
the read advisor keeps shipping at all.

### Method
- **D0 (`PreCompact`):** scripted session that forces a compaction (small
  `CLAUDE_CODE_MAX_OUTPUT_TOKENS`-style pressure or `/compact`), with the shim
  swapped for a capture stub emitting a marker string; assert the marker
  appears in the post-compaction summary/context.
- **E1 (`PreToolUse` deny reason):** capture stub denies a `Read` with a
  marker in `permissionDecisionReason`; assert the *model's next turn* shows it
  saw the marker (it references or acts on it), not just the user-facing UI.
- Record outcomes + the tested CLI version in `MAINTENANCE.md`'s contract
  table (replacing the `TODO(spike)` block) and as the initial
  `verified_harness_version` (Feature 1).
- **E1 fails ⇒** flip the read-advisor default posture from "opt-in" to
  "blocked": the settings toggle (Feature 6) renders disabled with an
  explanatory hint, and the hook is not installed regardless of the JSON
  value. Revisit on the next Claude Code bump.

---

## Feature 1 — Harness version tripwire

### Goal
Know when the harness changed, and say so where the user already looks.

### Design
- The OOB transcript tap already parses every Claude session's JSONL; entries
  carry a CLI version field (confirm exact field name during implementation —
  it's `version` on current builds). Capture it once per session.
- New tiny persisted state (settings-adjacent, not graph.db — it's global, not
  per-project): `harness_versions { claude: { last_seen, last_verified },
  opencode: { last_seen } }`. `last_verified` is set by Feature 0 and manually
  re-set from the Advisor card after a re-verification pass.
- OpenCode: version from `opencode --version` at tab spawn (the event stream
  carries no version; acceptable — spawn-time is enough for a tripwire).
- When `last_seen` moves past `last_verified`: raise a **drift notice**
  (Feature 2's rule class) — *"Claude Code updated 2.1.x → 2.2.0 — hook
  contracts unverified. Re-run the checks in MAINTENANCE.md → harness
  contracts."* Two actions: **Mark verified** (writes `last_verified`) and
  **Dismiss** (standard per-rule dismiss memory; re-fires on the *next*
  version change, not the same one).

---

## Feature 2 — Runtime drift canaries (a warn-only Advisor rule class)

### Goal
Detect the *symptoms* of a broken contract from data already collected, and
surface them on the Advisor card the user already reads. Versioned rules,
same gating discipline as V14 (minimum-sample gates, dismiss memory).

### Rules (initial set)
- **`drift.read_reason.v1`** — read advisor on, ≥15 remind events in the
  window, and ≥90% were followed by an immediate full `Read` of the same file
  (the V14 raise-min-lines rule fires at ≥50%; ~100% is a different disease —
  the deny *reason* isn't reaching the model, i.e. bare refusals). Proposal:
  **disable `read_advisor`** (Apply writes the setting) + re-verify per
  Feature 0. Takes precedence over the raise-min-lines rule when both match.
- **`drift.read_hook_silent.v1`** — read advisor on, ≥3 sessions in the
  window, session memory shows ≥10 re-reads of large unchanged files (the
  exact condition `should_read` reminds on), yet **zero** remind events
  reached the loopback. The hook isn't firing (overlay ignored, matcher
  renamed, shim broken). Warn-only (nothing safe to auto-apply).
- **`drift.injection_unseen.v1`** — context injection on, injected-chars
  counter growing, but the injection follow-rate (V14 D2) is ~0% over ≥5
  sessions **and** ≥30 injected files. Distinct from the existing
  raise-min-score rule by magnitude: near-zero follow means the block likely
  never reaches the model at all. Warn-only.
- **`drift.usage_fields_gone.v1`** — Claude sessions active but `usage_stat`
  rows stopped carrying token fields (transcript schema change). Warn-only.

### Non-goals
No network calls, no changelog scraping, no version-DB of known-good harness
releases. Symptoms + version changes only — both fully local signals.

---

## Feature 3 — Shim payload validation (`contract_drift` events)

### Goal
Catch payload-shape drift at the earliest observable point: the hook shims
themselves.

### Design
- The three shims (`context_hook.rs`, `compact_hook.rs`, `read_hook.rs`)
  currently deserialize leniently and fail open silently. Keep failing open —
  but when required fields are missing (`session_id`, `cwd`,
  `tool_input.file_path` for the read hook), POST a one-line
  `POST /activity/contract_drift { shim, missing }` before exiting 0.
  Loopback handler records an Activity event (`source: "harness"`,
  `tool: "contract_drift"`). Rate-limited app-side (one per shim per session)
  so a systematically-broken payload doesn't flood the store.
- `drift.payload.v1` Advisor rule: any `contract_drift` event in the window →
  warn with the shim name and missing fields.
- Cost note: this adds one extra loopback POST only on the *broken* path;
  the happy path is unchanged.

---

## Feature 4 — Advisor bypass detection (shell reads of reminded files)

### Goal
Make the read advisor's blind spot — the agent answering a denial with
`cat`/`Get-Content` via Bash — visible and measurable.

### Design — transcript tap, NOT a new hook
A `PostToolUse` hook on Bash was considered and rejected: it would add a shim
spawn to *every* shell command, the single most frequent tool after Read, to
observe a rare event. The OOB transcript tap already sees every Bash
`tool_use` block with its full command string — detection is free there.

- `GraphService` already keeps per-session remind-once state (which files were
  reminded). Expose a small query: `reminded_recently(session, path, n_turns)`.
- In the tap, on a Bash tool_use: extract path-like tokens from the command
  (simple heuristic — quoted strings and whitespace-split tokens containing a
  path separator; no shell parsing) and test each against the session's
  reminded set (full path *or* basename match, within `n_turns = 3` of the
  remind). On a hit, record an Activity event `source: "read_advisor"`,
  `tool: "bypass"`, `chars: file_size`.
- **`drift.read_bypass.v1`** Advisor rule: ≥10 reminds and ≥40% bypassed →
  propose disabling `read_advisor` (an agent routing around the advisor is
  strictly worse than no advisor: same tokens spent, plus the remind
  overhead, minus memory's read tracking).
- Honest-accounting note: bypass detection is heuristic (command-string
  matching); events are labeled `est.` wherever counted, and the Effectiveness
  panel's advisor-displaced figure **subtracts** bypassed reminds' chars — a
  displaced Read that came back via `cat` displaced nothing.
- OpenCode: same detection rides `tool.execute.after` (the plugin already
  reports tool executions) if/when Feature 7 lands; not required before.

---

## Feature 5 — Read-advisor trust TTL

### Goal
Bound how long the advisor trusts the agent's memory of a file, covering
context loss the advisor can't observe (context editing, tool-result
truncation — compaction is already handled).

### Design
- `should_read` gains one more pass condition: current session turn minus the
  turn of the last full read of the file > `read_advisor_ttl_turns` ⇒ pass.
  The turn counter already exists (`InjectState.turn`, the dedup TTL clock);
  read turns come from the same `mem_event` rows `should_read` already
  consults.
- `read_advisor_ttl_turns: u32`, default **0 = off** (the existing behavior;
  the V14 Advisor can learn to propose a value later if remind-then-re-read
  correlates with remind age — *not* in scope to automate now).

---

## Feature 6 — Settings UI for the V11 behavioral toggles

### Goal
Close the gap found 2026-07-12: none of `read_advisor`,
`read_advisor_min_lines`, `read_advisor_mode`, `compaction_context`,
`repo_map_on_session_start`, `context_dedup_ttl_turns`, `context_llm_digests`
have controls in `SettingsApp.svelte` — schema-only, hand-edit to enable.

### Design
- New "Token efficiency" group in the Code Intelligence settings section
  (below "Context injection"), same checkbox + hint pattern as
  `context_injection`, including the "re-launch the tab to pick it up" hint on
  every hook-backed toggle.
- Read advisor block: master checkbox, min-lines number input, mode select
  (`advise`/`substitute`), and (Feature 5) the TTL input — nested under the
  master toggle like the injection knobs are.
- `context_llm_digests` renders with the same health-awareness as semantic
  search (disabled + hint when no local offload backend is ready).
- If Feature 0's E1 spike fails, the read-advisor block renders disabled with
  the explanation (see Feature 0).

---

## Feature 7 — OpenCode read advisor (spike-gated)

### Goal
Extend the read advisor to OpenCode tabs — the V11 Feature-5 asymmetry, still
open.

### Design
- **Spike first (gates everything):** in the shipped OpenCode, can a
  `tool.execute.before` plugin handler (a) veto the read and (b) get
  substitute text back **to the model** (e.g. a thrown error whose message
  reaches the model, or an output-mutation API)? Same capture-harness method
  as Feature 0.
- Pass ⇒ the generated `.opencode/plugin` gains a `before` handler on the
  read tool calling the existing `/context/should_read` (session id and cwd
  already flow through the plugin); remind text rides whatever channel the
  spike proved. Same gates as Claude (graph on + `read_advisor` on; the
  setting stays harness-global per the global-only posture).
- Fail ⇒ record the outcome in MAINTENANCE.md's contract table and close the
  question — Claude-only becomes permanent-until-upstream-changes, not
  "pending spike".

---

## Feature 8 — Price-weighted Usage view

### Goal
Let the Usage section show what a session *costs*, not just what it counts —
so a cache-read-dominated bar chart stops reading as a cache-read-dominated
bill.

### Design
- A **tokens | est. cost** toggle atop the per-turn stacked bars and the
  per-session totals table (Usage section, `CodeIntelligenceView.svelte`).
  Cost mode multiplies each segment by its per-MTok price before stacking;
  same segments, same colors, different heights.
- Prices come from a small per-model price table in `settings.json`
  (`usage_prices: [{model_prefix, input, cache_read, cache_write, output}]`,
  $/MTok), shipped with defaults for the current Claude models (e.g. Opus:
  5 / 0.5 / 10 / 25 — cache-write at the **1h-TTL 2× multiplier**, which is
  what Claude Code sessions actually use; the API's 5m tier is 1.25× but
  doesn't apply here) and editable in the same settings group as Feature 6.
  Match by longest `model_prefix` against the session's model id (already in
  the transcript `usage_stat` rows); no match ⇒ that session renders
  token-mode only, with a hint. **No network calls, no price scraping** —
  prices drift, so they're user-editable config with honest defaults, and
  every cost figure is labeled `est.` (doubly so for subscription users,
  where $-cost is notional).
- Cache-write (`cache_creation_input_tokens`) becomes its own segment if the
  transcript rows carry it — currently it's not plotted separately; verify
  what the tap already stores before adding a column (additive, no schema
  bump, same as `usage_stat` itself).
- OpenCode sessions are `est_only` (no token fields) — cost mode shows the
  same est-badge posture the token view already uses.

---

## Feature 9 — Compounding context savings (cache-read-aware Effectiveness)

### Goal
Count what a displaced char is actually worth: a file kept out of context at
turn N is re-saved as a cache read on **every turn after N**. The current
Effectiveness counters record the one-shot saving only.

### Design
- Per-session accumulator on the existing dedup state (`InjectState`,
  `graph/service.rs`): a running `displaced_chars_total` (chars kept out of
  context so far this session — dedup-suppressed digests + advisor-displaced
  reads) and `compounded_chars` (the compounding readout). On **each
  retrieve turn** (the per-session turn counter already ticks there):
  `compounded_chars += displaced_chars_total`. Measured turn-by-turn as the
  session actually runs — no projection, no assumed session length; honest
  by the same rule as the existing counters.
- Advisor plumbing: `should_read` already receives `session_id` on the
  loopback route — on a `remind` verdict, add the displaced file's chars to
  that session's `displaced_chars_total` (session-scoped, unlike the
  process-wide Activity sum the panel shows today; the two coexist — the
  Activity events stay as the audit trail).
- Feature 4's bypass subtraction applies here too: a bypassed remind is
  removed from `displaced_chars_total` (it displaced nothing), so it stops
  compounding from that turn forward.
- UI: one new Effectiveness line — *"chars of cache-reads avoided
  (compounding) — est. ~N tok"* — with a tooltip explaining the mechanism
  in one sentence ("content kept out of context is saved again on every
  later turn"). With Feature 8's price table present, the same line can show
  an `est. $` figure at the cache-read rate; without it, tokens only.
- In-memory, since-restart, root-scoped via the same session-map filtering
  as `effectiveness_totals` — identical semantics to the existing counters.

---

## Phasing

| Phase | Scope | Notes |
|---|---|---|
| **0. Spikes** | D0 + E1 capture-harness verification; record baseline versions | Gates 2 (read rules) and the advisor's continued existence |
| **A. Tripwire** | Version capture (tap + spawn), `harness_versions` state, drift notice + Mark-verified | Small; ships with 0's baseline |
| **B. Canary rules** | `drift.*` rule class in `advisor.rs` (warn-only + the two disable-proposals), card rendering | Data sources all exist |
| **C. Shim validation** | `contract_drift` POST + Activity event + `drift.payload.v1` | Touches all three shims |
| **D. Bypass detection** | Tap-side matcher + `bypass` events + Effectiveness subtraction + `drift.read_bypass.v1` | No new hook |
| **E. Trust TTL** | `read_advisor_ttl_turns` in `should_read` | One condition + one setting |
| **F. Settings UI** | Token-efficiency group in `SettingsApp.svelte` | Independent; can ship first if wanted |
| **G. OpenCode spike (+ extension)** | `tool.execute.before` veto spike; plugin `before` handler if it passes | Independent tail |
| **H. Cost view** | Price table setting + tokens\|cost toggle + cache-write segment check | Independent; pairs with F's settings group |
| **I. Compounding savings** | `InjectState` accumulators + `should_read` session plumbing + Effectiveness line | Small backend; D's bypass subtraction hooks in here |
| **J. Docs/tests** | MAINTENANCE contract-table updates (spike outcomes), FEATURES, TOKEN-EFFICIENCY.md card table, unit tests per rule | Per repo convention |

Suggested order **0 → A → B → C → D → E → F → H → I → G → J**; F and H have
no dependencies and are the highest user-visible value per line — fine to
land alongside 0/A.

## Decisions — RESOLVED (2026-07-12)

1. **E1-fail posture** — DECIDED: hard-block the toggle (Feature 0), don't
   delete the feature; the next harness update may restore the contract.
2. **Where `harness_versions` lives** — DECIDED: global `settings.json`
   (it's per-install, not per-project).
3. **Default price-table values** — DECIDED: ship current published API
   $/MTok for the mainstream Claude models, editable, no auto-update.
   Cache-write defaults to the **1h-TTL 2× multiplier** (what Claude Code
   actually uses; e.g. Opus 4.8 write = $10/MTok, not the 5m-tier $6.25).
   No TTL knob — a user on a different pattern edits the write price directly.
4. **Cost view default mode** — DECIDED: default to tokens (the current
   view), cost as the toggle; per-user choice persisted via the existing
   view-state mechanism (`viewSection.ts`).

## Remaining implementation-time checks (not decisions)

- **Bypass thresholds** (≥40% in `drift.read_bypass.v1`, ≥90% in
  `drift.read_reason.v1`) — placeholders; tune once real rates are observed.
- **`version` field name in the Claude transcript** — assumed `version`;
  confirm against the current JSONL during Phase A (cheap: one session,
  grep the tap's input).
- **Cache-write column** — verify whether `usage_stat` rows already carry
  `cache_creation_input_tokens` before adding the segment (Feature 8).
- **Feature 0 (D0/E1) and Feature 7 (OpenCode `tool.execute.before`) spike
  outcomes** — empirical; recorded in MAINTENANCE.md when run.

## Cost note

Phases A–F are mechanical (Sonnet/Haiku fan-out fine; the advisor rules have
existing patterns to copy). Reserve Opus for Phase 0 / G spike analysis and
review — per the standing agent-cost guidance.

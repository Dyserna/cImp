# V24 — Session Usage Insights (S/A attribution, session drill-in, per-model cost)

**Status:** SPEC (2026-07-15). Not yet coded.
**Builds on:** the V16 usage bar chart + tokens|cost toggle
(`CodeIntelligenceView.svelte:1022-1124`, `usageMath.ts`), the V17 sub-agent
transcript tap (`oob/claude.rs` — `subagents/*.jsonl` drain, `SubAgentTails`),
the `usage_stat` relation + `UsageEvent` (`graph/memory.rs:184-200`,
`graph/index.rs:3031`), the `llm_pricing` template store
(`settings/schema.rs:430-491`, `matchPricing` in `usageMath.ts:104`), and the
V10 OpenCode `chat.message` plugin + loopback `/memory/event`
(`offload/loopback.rs:921-985`).

## Why

The Usage group in Code Intelligence answers "what did this project spend"
but hides three things the data can almost already answer:

1. **Which turns were the main session and which were sub-agents.** The tap
   *knows* (sub-agent lines come from `<sid>/subagents/agent-*.jsonl` or
   `isSidechain:true`) but drops that fact — `record_usage` merges everything
   into the parent session with no flag (`oob/claude.rs:824-831`). The bar
   chart can't show where agent fan-out spend went.
2. **Historical sessions in the same detail as the current one.** Turn series
   and per-tool rankings are already persisted per session in `usage_stat`,
   but the UI only queries them for the current session; clicking a session
   row opens a cost popup that prices the whole session at ONE rate set —
   mixed-model sessions (Fable main + Opus agents) are mispriced, and the
   per-turn/per-tool detail is unreachable.
3. **Real OpenCode numbers.** The pinned OpenCode `/event` SSE carries no
   token fields (`oob/opencode.rs:20-38`), and the loopback handler records
   only `ToolResult` chars — so OpenCode session rows show all-zero token
   totals. But OpenCode's own assistant messages DO carry token/cost data;
   our injected plugin can forward it.

Resume-by-id is real in both CLIs (verified 2026-07-15):
`claude --resume <uuid>` (id = transcript filename stem, exactly what we
store as `session_id`; must run from the project dir — which is cImp's
`launch_cwd`) and `opencode -s ses_<id>` / `opencode run "…" -s <id>`.
Surfacing a **copyable id** makes every session row actionable today; a
Resume-in-new-tab button is deliberately deferred (decided 2026-07-15).

**Decisions locked (2026-07-15):**
- S/A visual = grouping lane under the chart **plus** a subtle tint/outline
  on agent bars.
- Session card shows a **copyable id only** — no Resume button in v1.
- OpenCode fix = **plugin forwards usage** as real Turn events (spike-gated).
- "Active" session marking = **open tabs + recency** fallback.

Non-goals for v1: Resume-in-new-tab button, per-agent-id attribution beyond
the session/agent split (no `agent-<id>` breakdown), backfilling `origin`
for pre-V24 turns (forward-only, like the V17 sub-agent fix), persisting
the selected session across app restarts. (OpenCode sub-session roll-up
was spike-verified as cheap and IS in scope — see Phase F.)

---

## Phase A — `origin` attribution on usage turns (backend)

### Goal
Every recorded turn knows whether it came from the main session transcript
or a sub-agent transcript. Forward-only; old rows read as `session`.

### Design
- `UsageEvent::Turn` (`graph/memory.rs:184-200`) gains
  `origin: UsageOrigin` — closed enum `Session | Agent`, stored as a string
  column (`"session"` / `"agent"`) in `usage_stat`.
- **Migration:** `usage_stat` is a fixed Cozo relation → recreate-and-copy
  with `origin` defaulted to `"session"` for existing rows, bump the graph
  store schema version by 1 (V15 3→4 precedent in `graph/index.rs`). Old
  stores open cleanly; no data loss.
- **Tagging at the tap** (`oob/claude.rs`):
  - Parent-transcript lines → `Session`, EXCEPT lines with
    `isSidechain:true` (Claude 1.x inline form) → `Agent`.
  - Lines drained by `SubAgentTails::drain` (~917-970) → `Agent`
    (thread the origin through the shared `record_usage` helper at line 831).
- **Surfacing:** `TurnUsage` (`graph/memory.rs:217-226` + TS mirror
  `graph.ts:504-513`) gains `origin: 'session' | 'agent'`;
  `usage_turn_series` (`index.rs:3253`) selects the new column.
- OpenCode loopback turns (Phase F) pass `Session` unless the spike shows a
  reliable parent/child signal.

### Tests
Rust: migration round-trip (old-version store opens, old rows read
`session`); upsert keyed by `msg_id` preserves origin; tap tagging unit
tests for all three line forms (parent, sidechain, subagent file). Tripwire:
pin the `"session"`/`"agent"` wire strings alongside the existing
usage-field tripwires.

---

## Phase B — Session drill-in plumbing + live-session registry (backend)

### Goal
Any session (not just the current one) can be fetched at full detail, and
the snapshot says which sessions are live right now.

### Design
- **New command `graph_session_usage`** (`ipc/commands.rs`, next to
  `graph_usage` at 1924): `(root, session_id) →`
  ```rust
  struct SessionUsageDetail {
      row: SessionUsageRow,            // existing shape
      turns: Vec<TurnUsage>,           // usage_turn_series for that session
      top_tools: Vec<ToolUsage>,       // usage_tool_ranking for that session
      per_model: Vec<ModelUsage>,      // NEW — see below
  }
  struct ModelUsage { model: String, totals: UsageTotals, origins: OriginSplit }
  struct OriginSplit { session_tok: u64, agent_tok: u64 }  // total tokens per origin
  ```
  `usage_turn_series` / `usage_tool_ranking` already take a session id —
  parameterize the callers, no new persistence.
- **`per_model` query:** `usage_session_models` (`index.rs:3194-3218`)
  already sums per model internally and throws the sums away — new sibling
  `usage_session_model_totals` returns `(model, UsageTotals, origin split)`
  ordered by total tokens desc. Also used to fix `SessionUsageRow` pricing
  honesty (Phase D).
- **Live-session registry** in `GraphService`: `HashMap<tab_id, (agent,
  session_id, last_seen_ms)>`.
  - Claude tap: upsert on every drain tick with its current `session_id`
    (it always knows it — `claude.rs:126-133`), remove on cancel/rotation.
  - OpenCode: the loopback `/memory/event` handler upserts keyed by the
    reporting session id with a timestamp; entries expire by TTL (no tab
    binding exists on that path).
  - `usage_snapshot` (`service.rs:1111`) gains
    `active_session_ids: Vec<String>` = registry entries fresh within TTL
    **∪** any session with `last_ms` within the recency window (default
    5 min) — the decided "open tabs + recency" semantics. TTL/window are
    constants, not settings.
- `graph_usage` snapshot also keeps its existing single `current` — the
  live "This session" default view is unchanged (most-recently-active).

### Tests
Rust: `graph_session_usage` returns turns+tools+per_model for a seeded
session; per-model totals sum to `SessionUsageRow.totals`; origin split
matches seeded origins; registry TTL expiry; `active_session_ids` = tabs ∪
recency and dedups.

---

## Phase C — "This session" card: S/A chart + session selection (frontend)

### Goal
The card shows whose spend each bar is, can display any clicked session,
and its title identifies the session well enough to resume it by hand.

### Design (`CodeIntelligenceView.svelte:1022-1124`)
- **S/A lane (primary visual):** a thin lane row under `.ubars` — one cell
  per shown turn, contiguous same-origin runs merged into segments labeled
  `S` / `A` (label hidden when a segment is narrower than ~2 chars; tooltip
  carries it). Session segments use a muted track color; agent segments use
  the theme accent.
- **Bar tint (secondary):** agent bars get a subtle treatment on the
  `.ubar` container — accent-colored 1px outline + slight desaturation of
  the segment colors (CSS filter on the col), so agent turns scan even when
  the lane is cramped. Segment stacking (5 categories, `CHART_SEGS`) is
  unchanged.
- Legend row gains an `S/A` key. Colors derive from the theme accent, not
  new `graph.usage_color_*` settings entries.
- **Session selection:** clicking a Sessions row no longer opens the cost
  popup — it sets `selectedSession` and fetches `graph_session_usage`,
  which the card renders instead of `usage.current`. Turn-cap logic
  (`shownTurns`) reused as-is.
- **Title:** currently the static "This session" summary. Becomes:
  - live mode: `This session · claude · 2026-07-15 14:32` (from
    `usage.current` meta) — falls back to today's shape if no session yet.
  - selected mode: `Session · <agent> · <date> <time> · <id-prefix…>` plus a
    **copy-id button** (full `session_id` → clipboard via the tauri
    clipboard plugin — WebView2 denies `navigator.clipboard`, see
    project_webview_clipboard_wheel) and a **Live** pill that clears the
    selection and returns to the current session.
  - A short hint line in selected mode: `resume: claude --resume <id>` /
    `opencode -s <id>` (text only — the deferred Resume button's future
    home).
- Selected session row in the Sessions list gets a selected highlight
  (distinct from the active marker, Phase E).
- Tokens|cost toggle keeps working in both modes (turnCost already prices
  per turn model).

### Tests
Vitest: lane segmentation (merge runs, S/A labels, single-origin session →
one segment); agent-bar class applied by origin; selection swaps card data
and title; Live pill restores current; copy button calls the clipboard
plugin. Follow the dataviz skill when finalizing lane/tint colors.

---

## Phase D — Cost card (collapsible, per-model, what-if pricing)

### Goal
Replace the cost popup with a persistent, collapsible **Cost** card under
"This session" that prices each model in the session separately and lets
the user what-if any model's rates.

### Design
- New `<details>` card directly below "This session" (same styling as its
  siblings at 1022/1181), default collapsed, its open/closed state in
  `viewSection.ts` UI-state like the neighbors.
- **Data:** `per_model` from Phase B — live mode uses the current session's
  detail (extend `usage_snapshot.current` or lazy-fetch via
  `graph_session_usage` on card open); selected mode uses the already
  fetched `SessionUsageDetail`. One row per model, ordered by tokens desc —
  a Fable-main + Opus-agents session shows exactly 2 entries, each with its
  S/A token share (from `OriginSplit`) as a secondary line.
- **Per-entry pricing select** (reuses the popup's select, per row now):
  1. default = auto-match via `matchPricing(model, rows)` (longest
     `model_prefix` wins);
  2. no match → **Custom…** (the popup's four rate inputs, per row);
  3. **Free** — a fixed all-zero rates option, always listed. So the user
     can compare actual cost vs "what if this ran on X" per model.
- Each row renders the popup's 3-line breakdown (tokens / $-per-MTok / cost
  across input, cache write, cache read, output — `sessionCost` in
  `usageMath.ts:82` applied per model) + per-row subtotal; card footer =
  grand total across models.
- **Delete the popup** (`openCostPopup`/`closeCostPopup` at 402-422, markup
  1774-1855, `.cost-*` styles) — its pricing-select/Custom logic moves into
  the row component. Keep the fresh `llmPricingGet()` on card open so
  Settings edits keep applying.
- `SessionUsageRow` mixed-model honesty: the Sessions-list auto cost badge
  now sums per-model auto-matched costs (models without a match fall back
  to the current single-rate behavior + the existing `mixed` flag).

### Tests
Vitest: 2-model session renders 2 rows with correct per-model math and
grand total; auto-match → custom → free precedence; free row prices $0;
S/A share line; popup code fully gone. Rust: none beyond Phase B.

---

## Phase E — Sessions list: active + selected marking

### Goal
Every live session is visibly marked; the selected one is distinct.

### Design (`CodeIntelligenceView.svelte:1181-1243`)
- `usage.active_session_ids` (Phase B) → rows get an **active marker**: a
  theme-accent left border + pulsing dot before the agent label, title
  tooltip "active now". ALL active sessions are marked — a Claude tab and
  an OpenCode tab on the same project both show (fixes the
  single-`current` collapse noted in `service.rs:1125`).
- Selected marker (Phase C) = filled row background; can coexist with the
  active border.
- The `est` badge becomes data-driven: shown when the session has zero
  Turn-token totals (not `agent != "claude"`) — pre-V24 OpenCode sessions
  keep it, plugin-reporting ones (Phase F) lose it. Change is in
  `usage_all_sessions` (`index.rs:3290-3302`).
- Keep the `⎇ N` commits button exactly as is.

### Tests
Vitest: multiple active rows marked; selected+active coexist; est badge
gates on totals. Rust: `est_only` derivation from totals.

---

## Phase F — OpenCode real token usage (spike + plugin forwarding)

### Goal
New OpenCode sessions record real per-turn token totals (and model ids),
ending the all-zero rows.

### Spike — DONE 2026-07-15 (OpenCode v1.18.1, live run, PASS)
Method: a dump-everything plugin in `.opencode/plugin/` + headless
`opencode run` against the local llama-server (findings from real event
payloads, not docs). Results:

- **`chat.message` does NOT carry tokens** (fires on the user prompt) —
  the forwarding point is the **`event` hook** filtering
  `message.updated` with `properties.info.role === "assistant"`.
- The assistant `message.updated` info carries everything:
  `tokens: {total, input, output, reasoning, cache: {read, write}}`,
  `cost`, `modelID`, `providerID`, `id` (msg id), `sessionID`, `finish`
  (`"stop"` / `"tool-calls"`), `time.completed`. Cache-read was live
  (7621 cache tokens observed on turn 2).
- **Emission pattern:** the assistant message is emitted first with ZERO
  tokens (creation), then re-emitted (twice, duplicated) with final tokens
  once `finish`/`time.completed` is set → forward only when
  `info.time.completed` is present; upsert-by-msg_id makes the duplicate
  final harmless.
- **`session.updated` is NOT usable:** its session-level `tokens`/`cost`
  stayed 0 across all emits even after completed turns.
- **`parentID` IS reliable:** child (task-tool) sessions emit
  `session.created` with `info.parentID = <parent ses_*>`, and the plugin
  receives the child session's `message.updated` events with full tokens.
  Verified live with a `task` fan-out (`General Agent`).
- `cost` is provider-computed (0 for local llama); we ignore it and price
  from our templates (consistent with Claude sessions), though it's free
  to forward for future use.
- `tool.execute.after` still fires fine in 1.18.1 (existing plugin hook
  contract unbroken).
- Ops note (for MAINTENANCE recipes): headless `opencode run` under a
  non-TTY shell **hangs before creating a session** unless stdin is closed
  (`cmd /c "opencode run … < NUL"`). Irrelevant to cImp tabs (PTY), but it
  will bite any scripted verify.

### Design (spike-confirmed)
- Plugin `event` hook: on `message.updated` where `role === "assistant"`
  and `time.completed` is set, POST `/memory/event` body kind `"usage"`:
  `{ session_id, parent_session_id?, msg_id, model, in_tok, out_tok,
  cache_read, cache_make }` — mapped from `tokens.*`; `reasoning` folds
  into `out_tok` (priced as output everywhere that matters); `model` =
  `providerID + "/" + modelID` shape decided at implementation (must stay
  `matchPricing`-able).
- The plugin keeps an in-memory `Map<childSessionID, parentSessionID>`
  populated from `session.created` events (children are always created
  while the plugin is running) and stamps `parent_session_id` on usage
  POSTs from child sessions.
- Loopback `handle_memory_event` (`loopback.rs:921-985`) grows a match arm
  → when `parent_session_id` is present, record against the PARENT session
  with `origin: Agent` (mirrors the Claude contract: sub-agent spend is
  the parent's spend); else `origin: Session`. Upsert by `msg_id` handles
  the duplicate final emit.
- `oob/opencode.rs` header comment (lines 20-38) updated: token path now
  exists, via the plugin, not the SSE.
- Model ids land in `usage_stat.model` → per-model Cost card rows and
  `matchPricing` work for OpenCode too (pricing table may need OpenCode
  provider rows with `model_prefix` — seed additions to
  `default_llm_pricing()` only if the spike shows stable model id shapes).

### Tests
Rust: loopback usage-event arm records a Turn (unit, seeded body);
upsert-by-msg_id idempotence; bad/missing fields ignored without panic.
Live-verify: real OpenCode tab → session row shows nonzero tokens, no
`est` badge, Cost card prices it.

---

## Phase G — Live verification + release

- MAINTENANCE.md recipes (hand-run, outcomes recorded):
  1. Claude tab with a sub-agent fan-out → chart shows A-lane segments +
     tinted bars; agent tokens visible in the S/A split of the Cost card.
  2. Click an old session → card + Cost card swap to it; copy-id →
     `claude --resume <id>` from the project dir actually resumes it.
  3. Two live tabs (Claude + OpenCode) → both rows marked active.
  4. OpenCode session (post-Phase F) → real tokens, priced per model.
- Version bump + CHANGELOG on release per feedback_git_release_workflow
  (develop → main merge, tag).

## Residuals / future
- Resume-in-new-tab button (Claude: `--resume <uuid>` from `launch_cwd`;
  OpenCode: `-s ses_*`) — plumbing exists, deferred by decision.
- Per-agent-id (`agent-<id>`) attribution and lane tooltips naming the
  agent.
- Backfill of `origin` for historical turns (would need re-reading old
  transcripts — likely never worth it).

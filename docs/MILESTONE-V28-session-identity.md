# V28 — Per-Session MCP Identity (session-scoped memory tools)

**Status:** SPEC (2026-08-05). Closes GitHub issue #13 (NC-1/D-9 from the
2026-08-04 maintenance run) and the V10 residual "same-agent tabs share
memory scope".
**Builds on:** V10 context engine (`session`/`mem_event` relations,
`graph/index.rs::mem_current_session_for:3876`, the `context_recall`/
`context_notes`/`context_note` tools at `graph/mcp.rs:758-826`), the V24
live-session registry (`graph/service.rs::live_sessions:170`,
`mark_live_session`, TTL 90 s), NC-2's hook→tab resolver discipline
(`offload/loopback.rs::resolve_permission_tab:1444` — session-first,
fail-closed on ambiguity), and the single-proxy MCP design (one
`cimp --offload-mcp` stdio child **per tab**, spawned from
`tabs/config.rs::build_pre_args` / `build_opencode_config`).

## Why

Memory **writes** are already correctly keyed by the real harness session id
(Claude: transcript tap → `record_mem_event`; OpenCode: plugin →
`POST /memory/event` with `inp.sessionID`). The bug is the **read side**: MCP
tool calls carry no session identity at all — `/graph_run` bodies have only
`{cwd, name, args, consumer}` — so `context_recall`/`context_notes`/
`context_note` resolve "the session" via `mem_current_session_for(agent)`:
*the most-recently-active session for that agent name*. Two Claude tabs (or
two OpenCode tabs) on the same project are indistinguishable; tab A silently
reads and writes tab B's working set whenever B was active more recently.

## Design — locked decisions

1. **Identity rides the spawn args, not hooks.** The `cimp --offload-mcp`
   child is per-tab and its argv is fully composed by cImp, so the tab
   identity is baked at spawn: `--tab <tab-id>` appended in BOTH harness
   configs (Claude `--mcp-config` server entry, OpenCode
   `OPENCODE_CONFIG_CONTENT.mcp.cimp-offload`). `TabId` is already in scope
   in `build_launch_spec`; thread it into `build_pre_args` /
   `build_opencode_config` / `compose_ai_env` as needed.
   *Rejected:* the issue's original PreToolUse-hook correlation (match next
   MCP call by tool name + args) — racy across tabs calling the same tool
   with the same args, costs a shim spawn per MCP call, and has no OpenCode
   symmetry (`tool.execute.before` is not implemented in the plugin). The
   maintenance report's "definitive negative" (MCP servers get no per-session
   env) ruled out *session*-level identity at spawn; *tab*-level identity at
   spawn plus a live tab→session lookup at call time sidesteps it.
2. **Tag on the wire: `/graph_run` only.** `GraphRunBody` gains optional
   `tab: Option<String>`; `offload/mcp.rs::proxy_graph:669` forwards it.
   `/mcp/call` (ddg/context7 proxying) stays untouched — external servers
   hold no cImp memory scope; revisit only with NC-4 channel work.
3. **Resolve tab→session server-side at call time.** Generalize
   `live_claude_sessions()` to `live_session_for_tab(tab, agent) ->
   Option<session_id>` (same map, TTL-filtered, exact tab-key match, no
   guessing). `handle_graph_run` resolves and passes an explicit session to
   `run_graph_tool` → `dispatch_recorded` → `run_tool`; the mem tools use it
   when present.
4. **Fail-open everywhere, to exactly today's behavior.** Missing `--tab`
   (old running child from before the upgrade), unknown tab key, TTL-stale
   registry entry → fall back to `mem_current_session_for(agent)`. A tool
   call must NEVER error for lack of identity. No settings, no schema bump,
   no `spawn_inject_sig` entry (a tab's id is not Settings-derived and never
   changes while it runs).
5. **OpenCode tab binding (the one open question — Phase C spike).** The
   registry's OpenCode entries are currently keyed by *session id* (the
   loopback `/memory/event` path has no tab binding), so tab→session
   resolution works only for Claude today. Extend the per-tab OpenCode oob
   tap (`src-tauri/src/oob/` `/event` SSE side) to
   `mark_live_session(tab_id, "opencode", session_id)` mirroring
   `oob/claude.rs:208`, keeping the existing session-keyed writes (the Usage
   "live now" badge reads them). If the per-tab SSE tap turns out not to see
   the session id, OpenCode degrades to fail-open (= today's behavior) and
   the gap is documented in ARCHITECTURE.md — Claude-side isolation still
   lands.

## Cross-module invariants

- The write path is already session-correct — this milestone changes the
  READ side only; `record_mem_event` call sites are untouched.
- Sub-agent tool calls arrive through the same per-tab child and therefore
  resolve to the tab's *current main* session — intended: memory scope is
  per-conversation-tab, not per-subagent.
- The `offload` consumer (worker-native context tools) keeps its current
  (agent-`None`) scope — workers have no tab.
- Registry discipline matches NC-2: TTL-filtered, exact-match, never guess;
  ambiguity/absence = fallback, not attribution.
- Loopback trust model unchanged: `tab` is bearer-token-authed like every
  other body field; same-user forgery is out of scope.

## Failure modes considered

- **Idle-wake race:** a tab idle >90 s loses its registry entry; the first
  MCP call of the next turn could beat the ~200 ms drain tick that re-marks
  it. Window is tiny (the user's prompt lands in the transcript before the
  assistant's tool call) and fail-open covers it.
- **`/clear` rotates the session id:** registry updates on the next drain
  tick; a call in the ≤200 ms window attaches to the old session — harmless
  (that session was the live one moments before).
- **Two same-agent tabs, same project:** the target case — now isolated.
- **Stale child (pre-upgrade spawn):** no `--tab` → fail-open until the tab
  is restarted; no restart hint owed (see decision 4).

## Phases

- **A — plumbing (Rust):** `live_session_for_tab`, `GraphRunBody.tab`,
  explicit-session threading through `run_graph_tool`/`dispatch_recorded`/
  `run_tool`, mem tools honor it. Tests: explicit-session recall isolation
  (note in session A invisible to session B), fallback when `tab` absent /
  unknown / stale.
- **B — spawn threading:** `--tab` in both harness configs
  (`tabs/config.rs`); tests: per-tab argv carries the right id; the CD-4
  single-`--settings` contract test untouched.
- **C — OpenCode oob spike + wiring:** confirm the per-tab SSE tap sees
  session ids; mark tab-keyed live sessions; test. If infeasible: document
  the degradation instead.
- **D — docs + live verify:** ARCHITECTURE.md identity paragraph; recipes
  below.

## Live verification (by hand, after A–C)

1. Two Claude tabs, same project: `context_note` in tab A →
   `context_recall` in tab B must NOT return it; B's own notes round-trip.
2. Same with two OpenCode tabs (Phase C landed) — else confirm documented
   degradation.
3. `/clear` in a tab, then `context_note` → lands in the NEW session
   (check `session` relation timestamps).
4. Offload worker `context_*` tools behave exactly as before.
5. Kill the app, relaunch, immediate `context_recall` in a still-running
   tab (stale child, no `--tab` reaches nothing): falls back to
   most-recent-session behavior, no tool error.

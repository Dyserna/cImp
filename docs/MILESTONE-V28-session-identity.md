# V28 — Per-Session MCP Identity (session-scoped memory tools)

**Status:** IMPLEMENTED (Phases A–D, 2026-08-05) — live verification below still
pending. Closes GitHub issue #13 (NC-1/D-9 from the
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

   **Spike VERDICT (2026-08-05): POSITIVE — wired, no degradation.** Evidence:
   the V20 spike-0a capture kept at `docs/spikes/v20/ev.ndjson` shows
   `properties.sessionID` on **every** session-scoped SSE event —
   `session.created`, `session.status`, `session.idle`, `message.updated`,
   `message.part.updated` and `message.part.delta` all carry it. (`oob/opencode.rs`'s
   module doc claimed the consumer "has neither a `cwd`/session"; that was about
   the *token* fields the SSE lacks — the session id is there. The doc is
   corrected.) The tap already owns the `TabId` via `OobContext.tab`, so it now
   calls `ctx.mark_live_session(sid, "opencode")` exactly like the Claude tap,
   with two additions the spike surfaced:
   - **Sub-agent sessions ride the same stream.** `session.created` announces a
     child with `properties.info.parentID` (V24 Phase F spike, confirmed live).
     Children are recorded and skipped, so a tab binds to its current **main**
     session — otherwise decision-6's invariant would hold for Claude but not for
     OpenCode. A child whose `created` event was missed (tap attached mid-run, or
     a reconnect reset the tracker) binds: fail-open, never an error.
   - **A 5 s mark throttle**, because token-level `message.part.delta` events
     arrive by the dozen per turn (69 in the captured turn) and each carries the
     session id. Far inside the registry's 90 s TTL, and a turn always opens with
     low-frequency events before the assistant can issue an MCP call.

   An OpenCode tab's TAB-keyed entry is also RAII-cleared on tab exit (mirroring
   `claude::LiveSessionGuard`); the loopback's separate session-keyed entries are
   untouched, since tab lookups are exact-match and a tab id never equals a
   `ses_*` id.

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

- **A — plumbing (Rust): DONE.** `GraphService::live_session_for_tab` (+ the
  pure `lookup_live_session_for_tab`), `GraphRunBody.tab`, explicit-session
  threading through `run_graph_tool` → `dispatch_recorded` → `run_tool`, mem
  tools honor it. `main.rs` parses `--tab`; `offload/mcp.rs` stores it and
  `proxy_graph` forwards it on `/graph_run` only.
- **B — spawn threading: DONE.** `--tab <id>` on the `cimp-offload` entry in
  both harness configs; `tab` threaded through `build_pre_args` /
  `build_opencode_config` / `compose_ai_env` from `build_launch_spec`'s `TabId`.
  The `cimp-code-audit` child is deliberately left identity-free (it never hits
  `/graph_run`). The CD-4 single-`--settings` contract test is untouched in
  meaning — it inspects the `--settings` overlay, not `--mcp-config`.
- **C — OpenCode oob spike + wiring: DONE, spike POSITIVE** (see decision 5).
- **D — docs + live verify:** ARCHITECTURE.md § "Memory-tool session scoping"
  rewritten; MAINTENANCE.md § Context Engine memory scoping flipped from
  "residual limitation / watch upstream" to "closed, watch this seam instead".
  Live recipes below still to run.

### Scope taken beyond the letter of the spec

`graph_repo_map`'s session boost (`repo_map_session_boost`) resolved its session
through the *same* `mem_current_session_for(agent)` call, so a second same-agent
tab's project map was ranked by the other tab's working set — the identical
read-side defect. It now takes the same explicit session (same fail-open
fallback). Three lines; noted here rather than left as a silent inconsistency.

## Live verification (by hand, after A–C)

1. Two Claude tabs, same project: `context_note` in tab A →
   `context_recall` in tab B must NOT return it; B's own notes round-trip.
2. Same with two OpenCode tabs — Phase C landed, so full isolation is expected
   (no documented degradation to confirm). Also check an OpenCode `task`
   fan-out: `context_recall` from inside a sub-agent must return the TAB's main
   working set, not the sub-session's.
3. `/clear` in a tab, then `context_note` → lands in the NEW session
   (check `session` relation timestamps).
4. Offload worker `context_*` tools behave exactly as before.
5. Kill the app, relaunch, immediate `context_recall` in a still-running
   tab (stale child, no `--tab` reaches nothing): falls back to
   most-recent-session behavior, no tool error.

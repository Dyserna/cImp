# V28 — Per-Session MCP Identity (session-scoped memory tools)

**Status:** IMPLEMENTED (Phases A–D, 2026-08-05; H1 review fix 2026-08-05 —
decision 4a: same-root multi-tab Claude degrades to unscoped) — live
verification below still pending.
**Superseded in part (V34, 2026-08-09):** decision 4a's degradation no longer
applies to a tab cImp could pin. See **[4b](#4b-v34-pinned-session-ids-retire-the-degradation)**
below — the tap now follows a `--session-id` cImp generated, so same-root
co-tenancy stops making a tab's binding unprovable. 4a's rules still govern any
tab that could not be pinned. Closes GitHub issue #13 (NC-1/D-9 from the
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

   **4a. Ambiguous binding degrades to unscoped (H1 fix, 2026-08-05 review).**
   The Claude registry entry is only as good as the tap that writes it, and the
   tap binds by tailing the newest `*.jsonl` under `~/.claude/projects/<slug>` —
   a *project*-derived root with no per-process discriminator. So when **two or
   more Claude tabs run on one project** (the built-in `claude` + `claude-local`
   both have `cwd: None` ⇒ identical root), both taps follow whichever session
   wrote last and neither tab's registry entry is proof of anything. The
   registry therefore detects that condition and withholds the answer:
   `live_session_for_tab` returns `None` (⇒ unscoped, decision 4's fallback) and
   `live_claude_sessions` drops the pair (⇒ the NC-2 permission resolver refuses
   rather than attributing a badge/TTS edge to the wrong tab). One predicate,
   `graph::service::tab_binding_is_ambiguous`, serves both consumers.

   Inputs are the tabs that are **actually running**, not the configured ones: a
   Claude tap declares `(tab_id → transcript root)` via
   `GraphService::mark_live_tab_root` on every 200 ms poll tick and its RAII
   guard clears the claim on tab exit, so a configured-but-closed `claude-local`
   never degrades the running `claude`, and closing one of two open tabs restores
   the survivor's scoping immediately. OpenCode never registers a root (it binds
   per-tab off its own SSE stream, decision 5) and is never degraded. The spawn
   dir both seams key off is one function, `tabs::config::ai_working_dir`, so the
   ambiguity key and the hook's cwd fallback cannot drift apart.

   Wrong-scope is worse than unscoped: an ambiguous tab silently writing into
   another tab's memory is exactly the defect V28 exists to remove, whereas
   unscoped is the documented pre-V28 behavior and still answers every tool call.
   #### 4b. V34: pinned session ids retire the degradation

   **(2026-08-09.)** 4a treats same-root co-tenancy as fatal to a tab's identity
   because the Claude tap binds by tailing the newest `*.jsonl` under a
   *project*-derived root — "no per-process discriminator". That was true of the
   binding, not of the harness: `claude --session-id <uuid>` sets the session id
   for the conversation, and cImp composes the child's argv itself.

   So cImp now **generates a UUID per Claude tab at spawn** and passes it
   (`tabs/config.rs::resolve_oob_source`, alongside the `OobSpec` that carries
   it — one function, so the flag and the file the tap follows cannot drift).
   The tap then follows `<root>/<uuid>.jsonl` *and nothing else*
   (`oob/claude.rs::pin_step`), and `tab_binding_is_ambiguous` returns `false`
   for a pinned tab however many co-tenants share its root: ambiguity was only
   ever a property of the newest-wins race, and a pinned tap never enters it.

   Consequences: `context_*` memory scoping and the NC-2 permission resolver
   work with two Claude tabs on one project, and the Code Intelligence Overview
   can follow the *focused* tab (`graph_tab_session`) instead of the
   most-recently-active session.

   **Deliberately asymmetric.** Pinning tab A does not rescue an unpinned tab B
   beside it: B is still the one guessing, and the newest file it grabs may well
   be A's. B keeps 4a's rules exactly.

   **Residuals — 4a still governs these:**
   * *A tab that selects its own session.* `--resume` / `--continue` /
     `--session-id` / `--fork-session` / `--from-pr` in the tab's args suppress
     the pin (`args_select_session`): the user's selector names a conversation
     that already exists, and a competing one would be rejected or fight it.
   * *`/clear` (and any in-session rollover).* The pin names the session the tab
     STARTED. After a clear, Claude writes a new session id, our pinned file
     stops growing, and the tab degrades to newest-wins — i.e. back to 4a — for
     the rest of its life. Re-pinning would need a rotation signal we do not
     have; not attempted.
   * *A harness that ignores `--session-id`.* The tap waits `PIN_GRACE_MS`
     (120 s) for its transcript, then drops the pin and reverts to newest-wins,
     re-marking the tab unpinned so ambiguity re-applies. Expiring is safe even
     when it fires wrongly, because the fallback *is* the pre-V34 behaviour.
   * *OpenCode.* Unchanged — it binds per-tab off its own SSE stream (decision
     5) and was never degraded.

   **Contract dependency (new tripwire surface).** This rests on Claude Code
   honouring `--session-id` and naming the transcript `<session-id>.jsonl`. The
   grace-window fallback keeps a drift non-fatal, but it is silent apart from a
   `warn!`; if per-tab scoping ever looks wrong, check for that log first.

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
  ambiguity/absence = fallback, not attribution. Enforced at ONE seam (the
  registry) by ONE predicate, so graph/memory scoping and permission
  attribution can never disagree about which tabs are ambiguous.
- A tab's spawn dir has one definition (`tabs::config::ai_working_dir`), read by
  both the out-of-band tap (⇒ the transcript root the ambiguity predicate groups
  by) and `claude_tab_dirs` (⇒ the permission hook's cwd fallback). Pinned by
  `claude_oob_root_and_permission_cwd_resolve_to_the_same_dir`.
- "Running" means a live tap (PTY-scoped), never "configured": a closed tab must
  never suppress a running tab's scoping.
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
- **Two same-agent tabs, same project (the target case) — outcome differs by
  agent, and by root:**
  - *OpenCode:* isolated. The tap reads `properties.sessionID` off its own
    per-tab SSE stream, so two tabs on one project are genuinely distinguishable
    (decision 5).
  - *Claude, tabs on DIFFERENT project dirs:* isolated. Different `cwd` ⇒
    different `~/.claude/projects/<slug>` root ⇒ each tap tails its own file.
  - *Claude, tabs on the SAME project dir:* **not** isolated and not
    isolatable — the transcript tap has no per-process discriminator (both tabs
    tail the newest file under one root). This degrades to **unscoped**
    (decision 4a): the memory tools fall back to
    `mem_current_session_for(agent)` — the documented pre-V28 behavior, tool
    calls unaffected — and the permission hook refuses to attribute rather than
    guess. Isolating this case would need a per-process discriminator Claude
    Code does not expose (the transcript filename is the session id, and the
    session id is not knowable at spawn); revisit if upstream ever puts the
    session id in the child's environment.
- **Launch-order window (H1):** tab A's tap can rotate onto tab B's *fresh*
  transcript and mark it live before B's own tap confirms, so the registry could
  hold `A → ses_B` **uniquely** and send a permission edge to the wrong tab.
  Narrowed to a sub-millisecond gap by the same predicate: B's tap declares its
  root as its first instruction (before any file read), so from that instant on
  both tabs are registered co-tenants and the window is covered.
  *Honest bound, not an invariant (H1-R3):* `PtyManager::start` spawns the child
  and then the tap, ~15 straight-line statements and one scheduling hop apart,
  so B's transcript **can** in principle exist before B's tap registers. In
  practice Claude Code takes seconds to boot and write its first transcript line
  against a sub-ms gap, so the window has no realistic content — but nothing
  *enforces* the ordering. It is held by a load-bearing comment at the spawn site
  (`pty/manager.rs`, between the two spawns): no `.await` may be introduced
  between the child spawn and `crate::oob::spawn`.
- **Residual: closing one of two same-root tabs.** The survivor's scoping is
  restored at once (RAII clear), but its tap may still be pointed at the closed
  tab's transcript — the newest file until the survivor writes again — so for
  ≤ one poll tick (200 ms) into its next turn it can report that session. The
  turn's own user prompt lands in the survivor's transcript first, which rotates
  the tap before the assistant can issue a tool call; and fail-open means the
  worst case is a note filed against a session that was live moments earlier
  (the same shape as the `/clear` rotation window above). Not closed by design.
- **Residual (H1-R3): the predicate only sees cImp's own taps — external
  `claude` processes are a structural blind spot.** The ambiguity registry is
  written exclusively by out-of-band taps, i.e. by Claude processes cImp itself
  launched in an AI tab. A `claude` run started anywhere else against the same
  project — a Shell tab, an external terminal, an editor plugin, or a `claude`
  invoked by an agent — writes into the very same `~/.claude/projects/<slug>`
  directory and registers nothing. The AI tab's tap can then rotate onto that
  foreign transcript (it tails the newest file under the root) and, being the
  only registered tab, bind to it **confidently**: the memory tools scope to
  another process's session and the permission hook attributes to this tab.
  Same hole for an AI tab whose configured command file-stem isn't `claude`
  (no `ClaudeTranscript` oob spec ⇒ no tap ⇒ no claim). Severity is **higher**
  than the two-tab case it superficially resembles: that one degrades to
  unscoped (safe), this one is confident-and-wrong, and it is reachable with a
  single AI tab open. Not fixable at this seam — distinguishing "my child" from
  "some other `claude`" needs a per-process discriminator on the transcript
  (process-level attribution) that Claude Code does not expose; the same
  upstream dependency as decision 4a. **Mitigation:** if per-tab memory scoping
  or permission attribution matters to you, don't run external `claude` sessions
  against a project that has an open cImp AI tab.
- **Conservative over-refusal (accepted).** A running Claude tab that can never
  actually conflate still counts as a co-tenant: notably one launched with
  `CLAUDE_CODE_CHILD_SESSION=1` (writes no transcript at all), which V30's
  `env_remove` strips by default but a per-tab `env` entry can re-introduce.
  Cost is unscoped memory tools on the sibling — the pre-V28 behavior — which is
  the correct direction for a trust predicate.
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

1. **Claude scoping — three cases (decision 4a).** Run them in order; each is a
   separate contract.
   a. *Single tab per root (the common case, scoping ON).* One Claude tab open.
      `context_note` a distinctive fact, then `/clear` (new session) and
      `context_recall`: the note must NOT come back as the current session's own
      working set. Cross-check `RUST_LOG=cimp=debug` shows the `/graph_run` body
      carrying `tab`, and the note lands under the session id the tab's
      transcript filename names.
   b. *Two tabs, SAME project dir (**V34: now ISOLATED, not degraded**).* Open
      `claude` and `claude-local` with no `cwd` override. **This recipe inverted
      at V34 — it previously asserted the 4a fallback, so a stale copy of it
      passes on a broken build.**
      - `context_note` a distinctive fact in A, `context_recall` in B: B must
        **NOT** see A's note, and each tab's notes must round-trip to itself.
      - Confirm the mechanism, not just the outcome: `ls ~/.claude/projects/
        <slug>/` shows **two** transcripts whose stems are the UUIDs cImp put on
        the two children's command lines (`RUST_LOG=cimp=debug` logs each
        `--session-id`). If both tabs are writing ONE file, the pin did not take.
      - Permission prompts must now attribute to the right tab via the hook
        (no TUI-regex fallback). A badge on the WRONG tab is still the
        regression to watch for.
      - Code Intelligence → Overview: the "This session" card must follow
        whichever tab has focus (header reads "Focused tab · … follows focus"),
        and switching tabs must switch the card. Clicking a session row pins it
        ("Session ·", no follow chip) until **Live** is pressed.
   b2. *Two tabs, SAME dir, one un-pinnable (4a still governs it).* Give
      `claude-local` a `--continue` in its args, so only `claude` is pinned.
      Expected: `claude` stays scoped (its own notes only); `claude-local`
      degrades to unscoped exactly as in the pre-V34 recipe — succeeds, never
      errors, may see the other's note. This is the asymmetry in decision 4b;
      if BOTH degrade, the pinned tab is wrongly being treated as ambiguous.
   b3. *Pin fallback (contract drift).* Hardest to stage — needs a `claude` that
      ignores `--session-id`. If it ever happens in the field the symptom is a
      tab with no TTS/usage/memory for two minutes, then a
      `"pinned transcript never appeared"` warning and a silent return to
      pre-V34 behaviour. Grep that string first when per-tab scoping misbehaves.
   b4. *`/clear` rollover (known residual).* In a pinned tab, `/clear`, then
      `context_note` + `context_recall`. The tab reverts to newest-wins for the
      rest of its life, so with a co-tenant open it degrades to (b2)'s
      behaviour. Documented, not fixed — see decision 4b's residuals.
   c. *Two tabs, DIFFERENT project dirs (isolated).* Give one tab a `cwd`
      override (a worktree). `context_note` in A → `context_recall` in B must NOT
      return it; B's own notes round-trip. Then close one tab and re-run (b)'s
      recipe on the survivor: scoping must come back immediately (the RAII guard
      drops the closed tab's root claim), not after the 90 s TTL.
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

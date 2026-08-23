# v0.53.0-rc.9 live-verify baselines (2026-08-23)

Captured from the installed rc.9 for the next RC's 'unchanged vs previous RC' items (V40 live-verify 6, 8, 30, 41).

## tools/list — consumer claude (tab claude; lean_tools=true via project overlay, no enabled command plugins, role None)
```
context_note
context_notes
context_recall
graph_callees
graph_callers
graph_find_symbol
graph_impact
graph_imports
graph_outline
graph_recent_changes
graph_references
graph_repo_map
graph_search_docs
graph_semantic_docs
graph_snippet
graph_tests_for
graph_transitive
offload_batch
offload_task
run_check
testprov__hostile_tool
testprov__scan_garbage
testprov__scan_sarif
testprov__scan_slow```

## tools/list — consumer opencode
```
context_note
context_notes
context_recall
graph_callees
graph_callers
graph_find_symbol
graph_impact
graph_imports
graph_outline
graph_recent_changes
graph_references
graph_repo_map
graph_search_docs
graph_semantic_docs
graph_snippet
graph_tests_for
graph_transitive
offload_batch
offload_task
run_check```

## cimp --harness-canary
```
cimp 0.53.0-rc.9 — harness live probe (L2). Non-zero exit iff something FAILED.

  PASS        opencode.tool_registry            [C] 14 live tool ids, all classified (9 gated by OPENCODE_NATIVE_TABLE, 5 reviewed and deliberately ungated). Declared but NOT served upstream (a note, not drift — a tool that does not exist cannot be exploited): execute, lsp, patch, plan_exit.
  PASS        opencode.route.noauth             [B] the documented server-password env pair enforces Basic auth on all 2 probed route(s), and cImp's own credential is accepted (GET /experimental/tool/ids → 200 authenticated, 401 not, GET /session/:id → 500 authenticated, 401 not). The Tier-D unauthenticated-loopback exposure is closed for every tab cImp launches.
  PASS        claude.flag.session_id            [B] all 5 declared flag(s) still declared by `claude --help`: --session-id, --resume, --continue, --fork-session, --from-pr.
  PASS        claude.flag.settings_overlay      [B] all 1 declared flag(s) still declared by `claude --help`: --settings. NOTE: the deeper half of this row — whether the installed CLI still HONORS the `hooks` / `statusLine` / `permissions` keys inside the overlay — needs a scripted turn and is NOT covered here (issue #64 stays open).
  PASS        claude.transcript.usage           [C] 36/36 usage Turn(s) substantive out of 36 assistant line(s); cache counters non-zero on 36. (The cache pair is REPORTED, not asserted: prompt caching can legitimately be off for an account, so failing on it would be a false alarm.)
  PASS        claude.transcript.tool_result     [C] 18/18 tool_result block(s) read with an id and >0 chars, against 19 `tool_use` block(s); `is_error` true on 1 (reported, not asserted — a session with no failed tool call is normal).
  PASS        claude.transcript.identity        [C] 188/189 line(s) carry a matching top-level `sessionId`; CLI build string(s) seen: 2.1.241; `isSidechain` on 0, `isMeta` on 0 (both reported, not asserted — a session with no sub-agent and no synthetic line is normal).
  PASS        claude.transcript.assistant_text  [C] 7/36 assistant line(s) yielded speakable prose (7 text block(s) total)
  PASS        claude.transcript.stop_reason     [C] 36/36 assistant line(s) declare a stop reason; 0 read as the END of a turn and 36 as mid-turn (a window of nothing but tool-calling turns is normal, so only the field itself is asserted)
  UNKNOWN     delegation.worker                 [D] no probe can settle it: the property is that a REAL turn typed into a REAL TUI comes          back readable, which needs a live worker tab and a live model call — the scripted-turn          class, doubled. Covered meanwhile by the fail-closed gate itself (preflight refuses a          tab with no completion signal rather than typing into it), by the recorded          input-profile spike the gate reads, and by V39 live-verify recipes 1/2/10
  UNKNOWN     claude.hook.user_prompt_submit    [B] needs a scripted turn (L2 residual): proving the stdout envelope reaches the model requires installing a temporary hook via --settings and running one real prompt
  UNKNOWN     claude.hook.precompact            [B] needs a scripted turn AND spike D0: whether the additionalContext reaches the compaction prompt is a Behavior dep no payload reveals
  UNKNOWN     claude.hook.pretooluse_deny       [B] needs a scripted turn AND spike E1: whether the deny reason reaches the model is a Behavior dep no payload reveals
  UNKNOWN     claude.hook.posttooluse           [B] needs a scripted turn (L2 residual): the payload only exists while a real Edit/Write is being made
  UNKNOWN     claude.hook.notification          [B] needs a scripted turn (L2 residual), and the open question — which of the flat and nested payload shapes this build sends — only answers itself when a real permission prompt fires
  UNKNOWN     claude.hook.stop                  [B] needs a scripted turn (L2 residual): `last_assistant_message` exists only when a real turn finishes. The open question is a Behavior dep besides — whether its rendering of a multi-block message matches the transcript reader's join
  UNKNOWN     claude.hook.tool_result           [B] needs a scripted turn (L2 residual): the payload exists only while a real tool call returns, and the property worth proving is that the all-tools matcher fires for tools the sibling entry does not name
  UNKNOWN     claude.hook.subagent              [B] needs a scripted turn (L2 residual) AND a session that happens to launch a sub-agent — the same 'an absence proves nothing' problem `claude.transcript.subagents` has, one layer up
  UNKNOWN     claude.transcript.subagents       [C] needs a scripted turn (L2 residual): a transcript tail can only show the subagents/ layout if the tailed session happened to launch a sub-agent, so an absence proves nothing and a presence is luck
  UNKNOWN     claude.statusline.stdin           [C] needs a scripted turn (L2 residual): the payload exists only when the CLI invokes the statusLine command, so probing it means running a turn with an overlay installed
  UNKNOWN     claude.hook.taint_beacon          [B] needs a scripted turn (L2 residual): the hook only fires when a real turn reaches for WebFetch/WebSearch, and the property worth proving is that the beacon LANDED before the tool ran — an ordering, not a payload shape. Unchanged by the 2026-08-17 http migration, which moved the row to Tier B: what it bought is app-observable DELIVERY, which is a production signal rather than something this probe can drive
  UNKNOWN     claude.hook.checkpoint_beacon     [B] needs a scripted turn (L2 residual), and the load-bearing half is an ORDERING no fixture can express: that the tool call does not begin until the hook's response arrives. Since 2026-08-17 that ordering is upstream's DOCUMENTED deny contract rather than an observed behaviour, so what a probe would add is confirmation, not coverage
  UNKNOWN     claude.input.profile              [D] no probe can settle it: whether a bracketed paste plus a submit yields exactly ONE turn          is a `Dep::Behavior` visible only as a real turn in a real TUI. Manual input-profile          spike, outcome in `harness_versions.input_profile_status` — the same class as D0/E1, and          `Mark verified` survives for exactly these
  UNKNOWN     perm.tui_scrape                   [D] no probe can settle it: a scrape of rendered TUI chrome. Re-characterized in minutes with RUST_LOG=perm_capture=debug; the real fix is the D→C→B migration of decision 2
  UNKNOWN     opencode.sse.events               [C] needs a scripted turn (L2 residual): GET /event on an idle server streams nothing, so the event kinds only arrive if a real agent turn is driven
  UNKNOWN     opencode.route.push               [B] needs a scripted turn (L2 residual): the dangerous half is `noReply` losing its meaning, which is only observable as an agent turn that should not have started
  UNKNOWN     opencode.plugin.load_all          [D] no probe can settle it, and it is inside the TCB: nothing outside a harness can verify that a control inside it ran. A plugin that loads but skips the `throw` looks fully functional. Manual OpenCode-veto spike; Phase I's `chp` handshake at least makes a STALE plugin a mismatch instead of a mystery
  UNKNOWN     opencode.input.profile            [D] no probe can settle it — same behaviour, same spike, same recorded outcome as          `claude.input.profile`

  9 pass, 0 fail, 19 unknown, 0 transition
```

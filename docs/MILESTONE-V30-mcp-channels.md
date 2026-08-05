# V30 — MCP channels (session push)

**Status:** Phase 0 spike IN PROGRESS (2026-08-05). Investigation done — full
contract report + go/no-go in the
[#28 closing comment](https://github.com/Dyserna/cImp/issues/28#issuecomment-5191836292)
(qualified GO). Umbrella issue #15, GH milestone 4 (NC-4/CD-1 from the
2026-08-04 maintenance run).
**Builds on:** the single-proxy stdio child (`cimp --offload-mcp`, one per
tab), its existing out-of-band notification spine
(`offload/mcp.rs::events_relay` → `emit_list_changed`, the one unsolicited
`notifications/tools/list_changed` writer), and V28 per-tab identity
(`--tab` + `graph/service.rs` live registry).

## Contract summary (verified 2026-08-05, docs + 2.1.222 binary)

- Server declares `capabilities.experimental["claude/channel"]: {}` at
  `initialize`; optional top-level `instructions` is injected into Claude's
  system prompt.
- Push = JSON-RPC notification `notifications/claude/channel`
  `{content: string, meta?: Record<string,string>}`, any time, unsolicited.
  Meta keys must match `^[a-zA-Z_][a-zA-Z0-9_]*$` (others silently dropped).
- Surfaces model-visible as `<channel source="…" k="v">content</channel>`,
  queued at the next turn boundary; **starts a turn when idle**. TUI shows a
  condensed `← server: …` line.
- stdio-only; session targeting implicit (the session owns the child).
- Registration for a bare `mcpServers` entry:
  `--dangerously-load-development-channels server:cimp-offload` — interactive
  warning dialog **every** startup (consent is in-memory). `--channels` proper
  is allowlisted `plugin:@marketplace` only. Both flags hidden from `--help`.
- Fire-and-forget: misconfig/policy → **silent drop**, no server-side error.
- Research preview; contract may change.

## Invariants (cross-module — do not violate)

1. **Legacy-era handshake is load-bearing.** The client skips channel
   registration when the connection negotiated the modern (2026-07-28) MCP
   protocol era ("no unsolicited notification path"). The child's
   `PROTOCOL_VERSION` stays `2025-06-18` on the harness connection; CD-6
   modernization applies to `mcp_host.rs` (host→external servers) only.
2. **Every push has a pull twin.** Pushes are best-effort notify-only; any
   result delivered by push must also be retrievable via a tool call.
   (Silent-drop failure mode + "every quality signal needs a consumer".)
3. **Pushes are instance-scoped.** Tab ids repeat across app instances
   (`claude`, `opencode`, …); a tab-addressed push must be bound to this
   instance (pid/root), never matched on tab id alone.
4. **If the channel flag becomes Settings-gated, it gets a
   `spawn_inject_sig` entry** (`tabs/config.rs:262` rule) + restart hint.

## Phase 0 — spike (gates everything)

Spike harness (env-gated, `CIMP_CHANNEL_SPIKE=<trigger-file>`, zero effect
when unset; child-only, marked for removal):

- `initialize` gains `experimental.{claude/channel}` + verification-oriented
  `instructions`.
- T+20 s auto-push (`meta.kind=spike_auto`), then a trigger-file poll (2 s):
  on mtime change, pushes the file's content (`meta.kind=spike_file`).
- `spike_slow {seconds=150}` — server-side sleep, tests >2 min
  auto-backgrounding without llama.
- `spike_slow_progress {seconds=150, interval=15}` — same, emitting
  `notifications/progress` per interval iff the client sent a
  `progressToken`; tests the stall-timer reset.

### Recipe

Setup (PowerShell):

```powershell
$cfg = "$env:TEMP\v30-spike.mcp.json"
$trig = "$env:TEMP\v30-push.txt"
@"
{"mcpServers":{"cimp-offload":{
  "command":"P:\\Documents\\AI-private\\cc-avatar\\cctts\\src-tauri\\target\\debug\\cimp.exe",
  "args":["--offload-mcp","--tab","spike"],
  "env":{"CIMP_CHANNEL_SPIKE":"$($trig -replace '\\','\\\\')"}}}}
"@ | Set-Content $cfg
claude --mcp-config $cfg --strict-mcp-config --dangerously-load-development-channels server:cimp-offload
```

Record per test (kill criteria in **bold**):

- [ ] **T1 dialog:** exact consent-dialog text/options/keystrokes; whether it
      precedes the TUI; the startup notice line; whether anything persists a
      choice. → feeds the dialog-policy decision (manual ack vs opt-in PTY
      auto-ack).
- [ ] **T2 idle push (kill: #45563):** stay idle ≥20 s → auto-push must
      arrive, **start a turn**, and Claude must repeat content + meta
      verbatim (its `instructions` say to). Note the `←` TUI line.
- [ ] **T3 mid-turn push:** ask Claude to `sleep 40` in Bash; meanwhile from
      another terminal `Set-Content $env:TEMP\v30-push.txt "midturn-<nonce>"`
      → delivery batched at the next turn boundary, not lost.
- [ ] **T4 auto-backgrounding (CD-1):** "call spike_slow with seconds=150"
      (this standalone session has no `…AUTO_BACKGROUND_MS=0`, default
      120 s applies) → expect task-id handoff at ~2 min + result via task
      notification. Record the notification shape and whether the result text
      reaches the model end-to-end.
- [ ] **T5 progress keepalive:** "call spike_slow_progress with seconds=180"
      → if a progressToken was sent (result text says), the call should stay
      foregrounded past 2 min. If no token: finding — stall-reset lever is
      unavailable to MCP servers, note it on #15.
- [x] **T6 `-p` probe** — RUN 2026-08-05 (Sonnet, stream-json, spike_slow(30)
      kept the session alive to T+30): tool ran fine over MCP, but **no
      channel message was delivered and nothing warned** — in `-p` the
      dev-channels consent cannot be granted, registration silently fails.
      Channels are interactive-TUI-only; the silent-drop failure mode is
      real and observable. (Bonus finding: bare Bash `sleep` is blocked by
      the 2.1.222 harness in `-p`.)

### Decisions owed at spike close

- Dialog policy: per-tab manual ack vs explicit opt-in PTY auto-ack vs shelve.
- Per use case, the completion path: channel push / native auto-backgrounding
  / progress keepalive — for `offload_task`, `offload_batch`, audit runs,
  graph indexing.
- Whether `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0` (`tabs/config.rs:1232`)
  stays, becomes conditional, or goes.

## Phases A–D (sketch — full text in the #28 comment)

- **A — child capability plumbing:** parse+store client `initialize` params
  (today discarded, `offload/mcp.rs:132`); declare the capability +
  `instructions` when enabled; `emit_list_changed` → generalized
  `emit_notification` (done in the spike); revisit the client-notification
  drop in `mcp_stdio.rs:78` if `notifications/initialized` matters.
- **B — identity + payload bus:** `/events?tab=&consumer=` registration; an
  app-side live-children registry (RAII on SSE close, instance-scoped); a
  tab-addressed payload bus separate from the existing `broadcast<()>`
  capability pulse; real SSE parsing in the child.
- **C — producers:** origin-tab on `RunBody`/`/audit/run`; completion
  publishers (audit, graph index, batch stragglers); a pull tool per push
  (invariant 2).
- **D — OpenCode backend:** same bus, different transport —
  `POST /session/:id/prompt_async` + `noReply:true` (session id from the live
  registry; OpenCode has **no** MCP inbound path — SDK v2 was reverted in
  1.18.9; v2-branch elicitation is instance-global) + `/tui/show-toast` for
  human-only notices. Watch: v2 `PromptInput` currently lacks `noReply`.
- **Settings:** one gate (e.g. `offload.session_push`), default **off**.
- **Out of scope:** MCP elicitation; the `claude/channel/permission` relay
  (NC-2 candidate, noted in #28).

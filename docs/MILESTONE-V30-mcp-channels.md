# V30 — MCP channels (session push)

**Status:** Phase 0 spike COMPLETE (2026-08-05) — all six tests run live,
GO confirmed, decisions below. Investigation report + go/no-go in the
[#28 closing comment](https://github.com/Dyserna/cImp/issues/28#issuecomment-5191836292);
spike results also on #15. Umbrella issue #15, GH milestone 4 (NC-4/CD-1
from the 2026-08-04 maintenance run). **Phase A IMPLEMENTED 2026-08-05**
(settings gate `offload.session_push` default-off, schema v28→v29, spawn flag
+ `spawn_inject_sig` entry, child capability declaration + client-init
storage, UI toggle; live-verify: enable the toggle with offload/graph on,
restart a Claude tab, expect the channels banner + `/status` "Listening" +
the child stderr line "declared the claude/channel capability" in the MCP
log). Next: Phases B–D.
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

Results (ALL RUN 2026-08-05, Claude Code 2.1.222, Windows 11, Max account,
session 70576d64; findings also posted to #15):

- [x] **T1 dialog: THERE IS NO CONSENT DIALOG in 2.1.222.** Registration is
      silent (MCP log `Channel notifications registered` ~4 s after spawn,
      before any interaction). The only UX: a persistent banner ("Channels
      (experimental) messages from server:cimp-offload inject directly in
      this session · restart without --dangerously-load-development-channels
      to stop") plus a **cosmetic bogus warning** "server:cimp-offload · no
      MCP server configured with that name" (dev-flag validation runs before
      `--mcp-config` files load; function unaffected). `/status` shows
      "Channels: Listening for messages from server:cimp-offload". → Dialog
      policy is moot today; add a drift tripwire for when the documented
      dialog materializes.
- [x] **T2 idle push: PASS, #45563 does not reproduce.** Push delivered at
      T+24 s, **started a turn from idle** with zero user input; landed as an
      `isMeta` user message `<channel source="cimp-offload" kind="spike_auto"
      seq="0">…</channel>`; Claude echoed content + meta per the injected
      `instructions`.
- [x] **T3 mid-turn push: PASS.** Trigger-file push during an in-flight
      `spike_slow` queued and delivered at the next turn boundary as
      `kind="spike_file" seq="1"`; nothing lost.
- [x] **T4 auto-backgrounding (CD-1): PASS end-to-end.** At exactly 120 s the
      call moved to background ("moved to the background as task k653fxpb4 …
      does not survive exiting this session"); Claude kept working; at 150 s
      the **complete tool-result text arrived** in a `<task-notification>`
      user message. Backgrounding loses nothing (for text results).
- [x] **T5 progress keepalive: FAILS — docs claim is wrong.** Claude Code
      DOES send a `progressToken`; the child emitted 11
      `notifications/progress` (every 15 s); the call was **backgrounded at
      120 s anyway**. MCP progress notifications do NOT reset the
      auto-background stall timer in 2.1.222. The keepalive lever is dead;
      the real choices are `AUTO_BACKGROUND_MS=0` (blocking) vs native
      backgrounding (verified safe).

**Spike gotcha for future harness tests:** a claude spawned from within a
Claude Code session inherits `CLAUDE_CODE_CHILD_SESSION=1` and runs with
**no transcript, no history, no session records** (turns still execute).
Strip the harness env vars when spawning test sessions. (cImp's GUI-spawned
tabs are unaffected.)
- [x] **T6 `-p` probe** — RUN 2026-08-05 (Sonnet, stream-json, spike_slow(30)
      kept the session alive to T+30): tool ran fine over MCP, but **no
      channel message was delivered and nothing warned** — in `-p` the
      dev-channels consent cannot be granted, registration silently fails.
      Channels are interactive-TUI-only; the silent-drop failure mode is
      real and observable. (Bonus finding: bare Bash `sleep` is blocked by
      the 2.1.222 harness in `-p`.)

### Decisions (made at spike close, 2026-08-05)

1. **Dialog policy: none needed.** No dialog exists in 2.1.222; registration
   is silent + banner-only. Add a contract-drift tripwire (harness_versions)
   for a future consent dialog — research preview, it may yet appear.
2. **Completion paths per use case:**
   - `offload_task` / `offload_batch` (per-call results): **native
     auto-backgrounding** — remove `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0`
     from the Claude spawn env in Phase C (T4 proved the full result text
     arrives via task-notification; the child's synchronous NDJSON pipeline
     is unaffected because backgrounding is purely client-side). Progress
     keepalive is not an option (T5).
   - Audit runs, graph-index completion, batch stragglers / cross-call
     notices: **channel push** (Phases B/C) — the only mechanism for results
     not tied to an open call.
3. **Adoption confirmed GO**, settings-gated default-off (`offload.
   session_push`): zero registration friction today, but the banner + bogus
   warning line are user-visible in every tab, the contract is research
   preview, and pushes remain silent-drop (invariant 2 stands).

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

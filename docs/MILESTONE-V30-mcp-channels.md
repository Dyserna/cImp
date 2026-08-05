# V30 — MCP channels (session push)

**Status:** Phase 0 spike COMPLETE (2026-08-05) — all six tests run live,
GO confirmed, decisions below. Investigation report + go/no-go in the
[#28 closing comment](https://github.com/Dyserna/cImp/issues/28#issuecomment-5191836292);
spike results also on #15. Umbrella issue #15, GH milestone 4 (NC-4/CD-1
from the 2026-08-04 maintenance run). **Phase A IMPLEMENTED 2026-08-05**
(settings gate `offload.session_push` default-off, schema v28→v29, spawn flags
+ `spawn_inject_sig` entry, child capability declaration + client-init
storage, UI toggle). **Both halves of the gate are argv-baked** (review M5):
`tabs/config.rs::build_pre_args` reads `offload.session_push` ONCE per tab spawn
and emits Claude's `--dangerously-load-development-channels` *and* the
`cimp-offload` child's own `--channel-push` from that one read, so a child
crash-restart cannot re-decide from a settings file the running Claude process
never saw. The `spawn_inject_sig` `"channels"` entry carries the EFFECTIVE value
(`session_push && advertises_offload_to_claude`) so a toggle that cannot change
argv raises no restart hint. Live-verify: enable the toggle with offload/graph on,
restart a Claude tab, expect the channels banner + `/status` "Listening" +
the child stderr line "declared the claude/channel capability" in the MCP
log). **Phase B IMPLEMENTED 2026-08-05** (`PushRegistry` + `PushNotice` +
`/events` `event: push` frames). **Phase C IMPLEMENTED 2026-08-05**: the
`CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0` kill switch is REMOVED from the Claude
spawn env per spike decision 2 (`tabs/config.rs::compose_ai_env` now carries a
do-not-re-add comment), and two real producers ride the bus —
`graph/service.rs::announce_index_complete` (**user-initiated** full rebuilds
only — `RebuildOrigin::User`, so startup/watcher-recovery/schema-migration/
`DirWalk::TooBig` rebuilds stay silent — wall clock ≥ 30 s, `kind=graph_index`)
and `audit/runner.rs::announce_scan_complete` (GUI-initiated, **not cancelled**,
≥ 30 s, `kind=audit`). Both take an `Option<Arc<PushRegistry>>` at construction,
both re-read `offload.session_push` **live at fire time** (so toggling the
feature off stops app-side pushes with no tab restart; the child-side capability
declaration stays latched until the tab restarts — the documented residual), and
both are best-effort/log-and-continue. Each gate is one pure predicate
(`index_push_worthy` / `scan_push_worthy`) so every arm is unit-pinned.
Live-verify: enable `offload.session_push`, restart a Claude tab, then (a) force
a full graph rebuild of a big project and (b) click Scan in the Code audit tab —
each should surface a `← server: …` line and start a turn in the idle tab; an
MCP-initiated `security_audit` must NOT produce a second push.
**Phase D IMPLEMENTED 2026-08-05** — OpenCode tabs join the same bus over
OpenCode's own HTTP API instead of MCP (it has none inbound): the per-tab
`oob/opencode.rs` event tap registers itself in-process on the `PushRegistry`
(`OobContext::pushes`, threaded from `pty/manager.rs`; RAII dereg beside its
`LiveSessionGuard`), gains a `select!` arm on the notice queue, and forwards each
push as `POST /session/<main session>/message` with
`{"noReply":true,"parts":[{"type":"text","text":"<channel …>…</channel>"}]}` —
the SAME envelope Claude renders, built locally. The `offload.session_push` gate
is read LIVE (not spawn-baked, so **no** tab restart and no `spawn_inject_sig`
entry on this side): the tap SUBSCRIBES to the bus only while the setting is on
— registering/deregistering off the settings broadcast, which is also what keeps
`PushRegistry::deliver`'s count honest — and re-checks it once more at delivery.
Delivery is best-effort (5 s timeout, log-and-continue, no retries) and runs on a
small dedicated task fed from the same bounded queue, so a wedged OpenCode server
cannot stall TTS/avatar processing or delay the tab's cancel (review LOW).
The target is always the tab's MAIN session, and — **review M7/M8** — the target
and the sub-agent exclusion set live on the TAB, not on one SSE connection: a
fresh `/event` stream replays nothing, so a per-connection tracker would forget
where to push (notice dropped as `NoSession`, defeating "notify the idle tab")
and which sessions are children (a sub-agent's deltas would become the target).
Second defence for the reconnect-mid-sub-agent case: before its first delivery to
a session the tap never saw `session.created` for, it probes
`GET /session/{sessionID}` and refuses (permanently, for that id) any session with
a `parentID`. Empty-content notices are refused before the POST and a literal
`</channel>` inside content is escaped, mirroring the Claude side's parse
boundary. Live-verify: with an OpenCode tab open and `offload.session_push` ON
(no restart), force a full graph rebuild / run a GUI audit scan — the notice must
appear in the OpenCode transcript as context **without starting a turn**, and
toggling the setting off must stop the fanout immediately. **Also verify the push
does NOT start a turn on the INSTALLED OpenCode version** — the installed binary
is now **1.18.13** (re-checked 2026-08-05: `opencode --version`), whose OpenAPI
`/doc` carries `noReply: boolean` on `session.prompt`'s body, so the 1.18.1
concern below is resolved on this machine; a downgrade would reinstate it.
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

**Spike harness status: REMOVED (2026-08-05, review M4).** Everything described
in this section is gone from the tree — the `CIMP_CHANNEL_SPIKE` env gate, the
child's `spike_slow` / `spike_slow_progress` tools, `SPIKE_INSTRUCTIONS`, the
T+20 s auto-push and trigger-file poller, and the app-side `POST /push_test`
route. It is documented here as the historical record of how the results below
were obtained; the recipe no longer runs against a current build. The rig had
outlived its purpose and had become a hazard: its `initialize` arm was checked
*before* the `offload.session_push` gate, so a stray env var injected the
spike's system-prompt `instructions` and two fake tools into every Claude
session with the feature off, and `/push_test` was an arbitrary-text-injection
route into live sessions. What survived is the contract-pinning unit tests,
rewritten against the production frame builders.

Spike harness, as it was (env-gated, `CIMP_CHANNEL_SPIKE=<trigger-file>`, zero
effect when unset; child-only):

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
Strip the harness env vars when spawning test sessions. **Correction (review
M9):** cImp's GUI-spawned tabs are NOT inherently unaffected — a cImp launched
from inside a Claude Code session passes the marker straight through to every
tab, and the resulting transcript-less Claude blinds the oob tap silently (no
TTS, no usage, no live-session entry, no V28 scoping, no log). Every AI tab's
spawn now strips `CLAUDE_CODE_CHILD_SESSION`, `CLAUDECODE` and
`CLAUDE_CODE_ENTRYPOINT` (`tabs/config.rs::HARNESS_ENV_VARS` →
`PtyLaunchSpec::env_remove`, applied before the per-tab `env` additions so an
explicit user value still wins). Shell tabs are untouched by design.
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
  (invariant 2). **As built (revised 2026-08-05, review M1):** the origin-tab
  plumbing was NOT built, and is now a conscious no. Its purpose was addressing
  a completion notice back at the agent tab that started the work, but the
  producers that shipped are both *GUI*-initiated — there is no origin tab —
  and the per-call notices that would have used it (offload-task stragglers)
  were dropped at spike close in favour of native auto-backgrounding
  (decision 2). The one remaining candidate is barred by the audit runner's
  echo guard (`Initiator::Agent` never pushes: its report is already returning
  through the open call, and a second copy would loop). Adding a `tab` field to
  `RunBody`/`AuditRunBody` today would be a signal with no consumer, so both
  producers `push_broadcast`, and `PushRegistry::push_to_tab` is retained
  `cfg(test)` for the first agent-initiated producer that needs it.
  Blast radius is instead bounded at the source: an automatic graph rebuild
  never pushes (`RebuildOrigin::User` only), a cancelled audit never pushes,
  both producers have a 30 s duration floor, and both re-read
  `offload.session_push` live at fire time.
- **D — OpenCode backend:** same bus, different transport —
  `POST /session/:id/message` + `noReply:true` (OpenCode has **no** MCP inbound
  path — SDK v2 was reverted in 1.18.9; v2-branch elicitation is
  instance-global). **As built:** the receiver is the existing per-tab
  `oob/opencode.rs` SSE tap, not a new task — it already holds the connection,
  the port and the tab's current MAIN session id, so it registers on the push
  registry in-process; a small sibling task owns the subscription and the
  delivery. The session id comes from the tap's own observation of the SSE
  stream (the same `last_mark` decision that drives V28's live-session registry,
  which excludes sub-agent sessions), NOT from the graph live registry — but it
  is stored per TAB (`TabSessions`), not per connection, so it survives a stream
  drop (review M7/M8). `/tui/show-toast` for human-only notices was not built (no
  producer needs it). Endpoint path is `/session/:sessionID/message` in 1.18.x,
  not `prompt_async`. Verified live against the installed 1.18.13 `/doc`:
  `noReply` is on the message body; `session.get` (`GET /session/{sessionID}`)
  returns an optional `parentID`, which is what the reconnect-safety probe uses;
  `session.list` and `session.children` also exist if a future phase needs them.
  Watch: v2 `PromptInput` lacks `noReply` — irrelevant while v2 stays reverted,
  but it is the field this phase depends on.
- **Settings:** one gate (e.g. `offload.session_push`), default **off**.
- **Out of scope:** MCP elicitation; the `claude/channel/permission` relay
  (NC-2 candidate, noted in #28).

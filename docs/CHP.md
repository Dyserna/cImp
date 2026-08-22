# CHP — the cImp Harness Protocol

**Protocol version:** `chp = 2`

**Status:** declared V35 Phase I (2026-08-16), issue #66; extended additively by
V35 Phase J (2026-08-17), issue #67 — § 4.5 — and again by V35 Phase L
(2026-08-17), issue #69 — § 4.6, which realizes three of the six reserved
read-path events and makes `serves` load-bearing. Extended additively once more
on **2026-08-17 (Claude Code 2.1.233)**: the two beacon hooks moved off
`type: "command"` and `PostToolUseFailure` was wired — three new routes in § 4.5,
no new events. This document is the wire contract;
`src-tauri/src/harness/chp.rs` is the code half, and `harness::chp::CHP_VERSION`
is checked equal to the version above by
`harness::chp::tests::the_doc_states_this_version`. The two move in one commit
or neither.

**`chp` stays at 1 through all of it.** Nothing on the wire changed shape: the
routes in § 4.2 carry the same bodies from the same senders, and § 4.5's
Claude-native ingress is *additive* — new routes, new meaning, per compatibility
rule 4. What changed is who sends them.

Design: [DESIGN-harness-plugin-architecture.md](DESIGN-harness-plugin-architecture.md)
(§ 2 the four layers, § 3 D1/D3/D4/D5, § 7 step 1) and
[MILESTONE-V35-harness-resilience.md](MILESTONE-V35-harness-resilience.md)
(locked decisions 9 and 10).

---

## 1. What CHP is, and what it is not

CHP is **the wire both harnesses have already been speaking**, given a name and
a version. `context_hook.rs` and the generated OpenCode plugin have posted the
same harness-neutral body to the same loopback routes since V10; `agent` has
been the discriminator all along. Phase I documents that reality, stamps a
version on it, and adds one route (`/session/hello`). It changes **no
behavior**.

CHP is **not** a public extension point. cImp does not load harness plugins it
did not ship (milestone locked decision 10): a new harness is a PR adding
`harness/<id>/`, released as part of cImp. The version and the handshake exist
so that *cImp's own* generated artifacts, which outlive the binary that wrote
them, can be recognized when they are out of step — not so that a third party
can implement this document.

### The seam it names

```
  L3  Session bus      capability registry, degradation decisions
  ══  L2  CHP  ═════════════════ THE STABLE SEAM ══════════════════
                       versioned HTTP+JSON on loopback, bearer auth,
                       `agent` discriminator, capability negotiation
  L1  Harness plugin   GENERATED, harness-native — the only per-harness artifact
  L0  Harness          Claude Code · OpenCode · next one
```

---

## 2. Transport

| Property | Value |
|---|---|
| Transport | HTTP/1.1 over loopback TCP, `127.0.0.1:<port>` |
| Endpoint discovery | `.cimp-offload.json` / `.cimp-discovery/<pid>.json` in the exe directory, or baked into the generated artifact at spawn |
| Auth | `Authorization: Bearer <launch token>` on **every** request |
| Content type | `application/json` |
| Encoding | UTF-8 |
| Max body | `offload::loopback::MAX_BODY` |

**The bearer token is not a trust boundary.** It is readable by any process
running as the user (from the discovery file, and from the generated plugin
inside the project tree). "Authenticated" here means *a local process*, never
*cImp's own child*. Every handler is written to that standard; see
`offload/loopback.rs`'s per-route honesty clauses.

**Every route answers `200` on the happy path and is fail-open for the
caller.** A CHP client must treat any error — a refused connect, a 401, a
timeout, a non-2xx, a malformed reply — as "unreported", never as "refused".
The one exception is the `throw` in the OpenCode plugin's `tool.execute.before`,
which is a *security control* and is deliberately the only escaping error in a
generated artifact (§ 7).

---

## 3. The message envelope

Every CHP message is a JSON object. Three fields are the envelope; the rest are
the route's own.

| Field | Type | Meaning |
|---|---|---|
| `chp` | integer | The protocol version the sender speaks. **Additive and tolerated-absent** — absent means *pre-CHP* (treated as `chp = 0`) and is never rejected. |
| `agent` / `consumer` | string | The harness discriminator: `claude` or `opencode`. **Two spellings exist on the live wire** — see below. |
| `tab` | string | The cImp tab id the sender was spawned for. Baked into the artifact at spawn; a hook payload never carries one. |

### 3.1 `agent` vs `consumer` — documented, not redesigned

The discriminator is spelled **`agent`** on `/context/*`, `/memory/event` and
`/workbench/tool_checkpoint`, and **`consumer`** on `/latch/beacon` and
`/latch/state`. Both normalize through `graph::source_for_consumer`, so one tab
keys the same state from every route.

This is a real inconsistency in the accidental protocol and Phase I records it
rather than fixing it: renaming a field is a wire change, and every artifact on
disk that speaks the old name outlives the binary that would rename it. The
envelope reader (`chp::Envelope::agent_token`) accepts either. A future phase
may converge them — as an additive `agent` alias with `consumer` still accepted,
never as a swap.

### 3.2 `tab` is the identity, and it is baked

No harness hands its extension a cImp tab id. Claude Code's hook payload carries
`session_id` and `cwd`; OpenCode's plugin input carries `directory` and a
session id. So `--tab <id>` is baked into every shim's argv and `CIMP_TAB_ID` is
baked into every generated plugin, at spawn. **This is what makes the artifact
spawn-baked**, and therefore what makes `chp` necessary (§ 6).

A message with no `tab` is accepted and simply attributed to nothing — routes
that need a scope resolve none and fail open.

---

## 4. Routes

### 4.1 `POST /session/hello` — capability negotiation

New in Phase I. Sent **once at artifact load**, which for a generated plugin is
per tab launch — exactly the spawn-baked moment worth stamping.

```json
POST /session/hello
{ "chp": 1,
  "agent": "opencode",
  "tab": "opencode-1",
  "harness_version": "1.18.13",
  "serves": ["hello", "prompt", "memory.event", "tool.gate"],
  "cannot": [{ "id": "taint.beacon", "why": "native web visibility is off or deny" }] }
```

| Field | Required | Notes |
|---|---|---|
| `chp` | no | Absent ⇒ pre-CHP. |
| `agent` | no | Defaults to `claude` when absent, as every other route does. |
| `tab` | **yes in practice** | A hello with no tab, or one naming a tab the user has not configured, is refused `400` and recorded nowhere. |
| `harness_version` | no | The harness's own version. **Omitted by OpenCode today** — OpenCode exposes no version to a plugin at module scope, and inventing one would be cImp attesting to itself. See § 6.2. |
| `serves` | no | CHP event ids (§ 5) this artifact will actually push, with its per-tab flags applied. |
| `cannot` | no | `[{id, why}]` for the rest. A capability absent from `serves` is **unavailable, not broken**. |

Response: `200 {"ok": true, "chp": <CHP_VERSION>}`. The ack carries the
*server's* version so a future client can adapt; today's plugin discards it.

**Nothing gated on `serves`/`cannot` in Phase I; § 4.6 is where that changed.**
Since Phase L a hello's `serves` decides, per capability and per tab, whether the
fallback reader's tap runs — so a hello is no longer only a description, it is
the arbitration input. `cannot` is still purely descriptive, and deliberately:
"this artifact will not push X" and "therefore the reader must" are the same
statement, and one of them is enough to act on.

**Claude Code sends a hello since Phase J.** Its L1 was five stateless shim
binaries plus a spawn-baked `--settings` overlay, with nowhere to put a version
or a declaration — which is why Phase I recorded "no hello yet" rather than
faking one. Phase J's `SessionStart` hook is the hello: it POSTs to
`/claude/hook/session_start` (§ 4.5) and the handler synthesizes exactly the
record above, with `agent: "claude"`, `tab` and `chp` from headers, and
`serves`/`cannot` from the `X-CIMP-Hello` header the generator baked in.
`harness_version` stays **absent** for OpenCode's reason (§ 6.2): no documented
hook-input field carries the CLI version, and cImp will not attest to itself.

### 4.2 The push routes that exist today

Documented as they **actually are** on `develop`, field for field. The
"harness-neutral core" the design describes (`{cwd, prompt, session_id, agent,
tab}`) is real but it is the shape of `/context/retrieve` specifically; the
other routes carry their own bodies.

Since Phase J the **Claude** sender of five of these is cImp's own handler for
the corresponding `/claude/hook/*` route (§ 4.5), not a shim binary. The body is
unchanged: the handler builds exactly what the deleted shim POSTed, and a
pre-upgrade tab still POSTs it over the wire from the old binary. Both are
listed, because both are live.

| Event | Route | Sent by | Body (beyond the envelope) |
|---|---|---|---|
| `prompt` | `POST /context/retrieve` | `/claude/hook/user_prompt_submit`'s handler — or, from a pre-Phase-J tab, `cimp --context-hook` (claude); plugin `chat.message` (opencode) | `cwd`, `prompt`, `session_id` |
| `context.compaction` | `POST /context/compaction` | `/claude/hook/pre_compact`'s handler (was `cimp --precompact-hook`) | `cwd`, `session_id`, `trigger` |
| `context.should_read` | `POST /context/should_read` | `/claude/hook/pre_tool_use`'s handler (was `cimp --read-hook`) | `cwd`, `session_id`, `file_path`, `offset`, `limit` |
| `context.post_edit` | `POST /context/post_edit` | `/claude/hook/post_tool_use`'s handler (was `cimp --postedit-hook`) (claude), plugin `tool.execute.after` (opencode) | `cwd`, `session_id`, `file_path`, `tool_name` |
| `memory.event` | `POST /memory/event` | plugin `tool.execute.after` and `event` (opencode only) | tool form: `cwd`, `session_id`, `tool`, `args`, `parent_session_id?` · usage form: `kind: "usage"`, `msg_id`, `model`, `in_tok`, `out_tok`, `cache_read`, `cache_make` |
| `permission.event` | `POST /permission/event` | `/claude/hook/notification`'s handler (was `cimp --notify-hook`) | `cwd`, `session_id`, `transcript_path`, `event`, `notification_type`, `message`, `tool_name` — **carries neither `agent` nor `tab`** (see § 4.4) |
| `taint.beacon` | `POST /latch/beacon` | `/claude/hook/pre_tool_use_taint`'s handler — or, from a pre-2026-08-17 tab, `cimp --taint-beacon` (claude); plugin `tool.execute.before` (opencode) | `tool`, `cwd`, `session_id` |
| `tool.gate` | `POST /latch/state` | plugin `tool.execute.before` (opencode only) | — (identity only, deliberately: the answer must not depend on what the caller claims about the tool) |
| `checkpoint.pre_mutation` | `POST /workbench/tool_checkpoint` | `/claude/hook/pre_tool_use_checkpoint`'s handler (was `cimp --checkpoint-beacon`), plugin `tool.execute.before` (opencode) | `tool`, `cwd`, `session_id` |
| `contract.drift` | `POST /activity/contract_drift` | every converted hook raises this report **in process** (same tokens, same ledger, same row); a pre-upgrade tab's own shim binary still POSTs it over the wire | `shim`, `missing`, `session_id` — **carries no `tab`**, so its reports are attributed by shim name, not by tab |

### 4.4 Where the live wire departs from the design's summary

The design doc summarizes the push path as *"both harnesses already send an
identical, harness-neutral body"* — `{cwd, prompt, session_id, agent, tab}`.
That is exactly true of `/context/retrieve`, and it is the reason CHP is worth
naming. It is **not** true of the whole surface, and Phase I documents the
reality rather than reshaping it:

- **Two discriminator spellings** (`agent` and `consumer`) — § 3.1.
- **`/permission/event` carries neither `agent` nor `tab`.** `notify_hook.rs`
  posts the raw Claude notification payload plus `cwd`/`session_id` and nothing
  else, so its events are attributed by session, not by tab. It is a
  Claude-only route today, so the discriminator is implied.
- **`/activity/contract_drift` carries no `tab` either** — a fact
  `loopback::contract_drift_row` already records honestly (`Unattributed`), and
  the reason its reports are attributed to a capability by *shim name*.
- **`/latch/state` carries no payload beyond identity**, on purpose: the answer
  must not depend on anything the caller claims about the tool it is about to
  run.
- **`/memory/event` carries two different bodies** on one route, discriminated
  by `kind`.

The consequence for staleness detection (§ 6.1): a route with no `tab` cannot be
attributed to a peer, so `contract.drift` and `permission.event` contribute no
`chp` observation. Neither is a loss — every tab that posts those also posts
`/context/retrieve`.

### 4.5 `POST /claude/hook/*` — the Claude-native ingress (Phase J)

**Additive extension, `chp` unchanged.** Twelve routes that take Claude Code's
own hook-input JSON verbatim rather than a CHP body. They are not a second body
shape on an existing route (compatibility rule 4 forbids that) and they are not
in the § 5 vocabulary: they are a *transport* for events that already have ids.

**Since V40 Phase C these routes are registered by the harness, not by core.**
The table below is `harness::claude::hook::ROUTES_TABLE`, returned from
`HarnessPlugin::routes()`; the loopback's router matches its own CHP-neutral
arms first and appends every registered plugin's routes after them, so a plugin
can neither shadow `/session/hello` nor add a route core does not enumerate
(`loopback::tests::every_loopback_route_declares_what_it_does_about_the_latch`
covers both halves of the surface). The handler answers a neutral `HookReply` —
a status and a body — which core writes without reading, so the *"Answers"*
column below is this harness's envelope and stays inside its directory.
`POST /permission/event` (§ 4.3) is in that table too: it carries neither
`agent` nor `tab` because the only thing that has ever posted to it is this
harness's pre-Phase-J `--notify-hook` shim, which is the same fact § 4.4 already
recorded.

| Claude event | Route | CHP event it feeds | Answers |
|---|---|---|---|
| `UserPromptSubmit` | `POST /claude/hook/user_prompt_submit` | `prompt` | `hookSpecificOutput.additionalContext`, or `{}` |
| `PreCompact` | `POST /claude/hook/pre_compact` | `context.compaction` | `hookSpecificOutput.additionalContext`, or `{}` |
| `PreToolUse` (`Read`, `Bash`) | `POST /claude/hook/pre_tool_use` | `context.should_read` | `permissionDecision: "deny"` + reason, or `{}` |
| `PostToolUse` (`Edit\|Write\|MultiEdit`) | `POST /claude/hook/post_tool_use` | `context.post_edit` | `hookSpecificOutput.additionalContext`, or `{}` |
| `Notification`, `PermissionDenied` | `POST /claude/hook/notification` | `permission.event` | always `{}` (observe-only) |
| `SessionStart` | `POST /claude/hook/session_start` | `hello` | always `{}` |
| `Stop` | `POST /claude/hook/stop` | `assistant_text` (§ 4.6) | always `{}` (observe-only) |
| `PostToolUse` (matcher `""`) | `POST /claude/hook/post_tool_use_result` | `session.tool_result` (§ 4.6) | always `{}` (observe-only) |
| `PostToolUseFailure` (matcher `""`) | `POST /claude/hook/post_tool_use_failure` | **none of its own** — it is the errored half of `session.tool_result` (see below) | always `{}` (observe-only) |
| `SubagentStart`, `SubagentStop` | `POST /claude/hook/subagent` | `session.subagent` (§ 4.6) | always `{}` (observe-only) |
| `PreToolUse` (`WebFetch\|WebSearch`) | `POST /claude/hook/pre_tool_use_taint` | `taint.beacon` | always `{}` (report-only) |
| `PreToolUse` (`Edit\|Write\|MultiEdit\|Bash`) | `POST /claude/hook/pre_tool_use_checkpoint` | `checkpoint.pre_mutation` | always `{}` (report-only) — **answered only after the snapshot is taken** |

**Minimum Claude Code: 2.1.63**, when `type: "http"` hooks shipped; the contract
is verified unchanged through **2.1.233 (2026-08-17)**. Phase J is a hard switch —
the overlay generates no command-hook fallback — so an older CLI gets hook entries
it does not understand and every one of these capabilities is simply absent.

**Two of the twelve are newer than that floor**, and each degrades differently:

- `PostToolUseFailure` (the event itself is newer than 2.1.63) — a CLI without it
  ignores the entry, so *failed* tool results go uncounted while successes keep
  flowing. Nothing reports it: an absent hook event cannot.
- the two beacon routes are not new upstream, only new here — they were
  `type: "command"` shims (`cimp --taint-beacon`, `cimp --checkpoint-beacon`)
  until 2026-08-17. **The migration is a tier change, not a transport preference:**
  each rested on an *undocumented* behaviour (a hook that writes nothing and exits
  0 is non-blocking including on timeout; the tool does not begin until the hook
  process exits), and the http contract states both facts in writing. In
  particular a `PreToolUse` http hook **blocks the tool call until the response** —
  the documented mechanism that makes `permissionDecision: "deny"` expressible —
  which is what turns "the checkpoint precedes the call" into a guarantee cImp can
  enforce by awaiting the snapshot before it answers. Multiple `PreToolUse` entries
  run in parallel and all must resolve before the tool starts, so the beacons do
  not serialize against the read advisor. A tab open across the upgrade keeps its
  old command hooks and gets neither beacon until it is restarted; the flags
  survive in `main.rs` as stdin-draining tombstones, because a `cimp` that no
  longer recognized one would fall through and launch a second GUI per web call.

**The failure half maps to no CHP event on purpose.** Two ids that can never be
declared independently are one id: the failure entry is emitted from the same
per-tab boolean as the success entry, feeds the same core, the same consumer, the
same drift token and the same `served` predicate, so there is no per-tab decision
a second event could report (the same reasoning that keeps `session.usage` off
*both* sides of Claude's hello). What a second mapping would cost is concrete —
the quiet detector **resets** a served capability's counter on each push of it, so
a rare failure push would silently rearm the detector watching the common success
entry. Staleness observation is unaffected either way: a hook route's envelope
rides headers and is read before the event join.

**Identity rides headers, because a hook's body is the harness's.** cImp gets no
field in the payload, so the envelope of § 3 is carried alongside it:

| Header | Meaning |
|---|---|
| `Authorization: Bearer $CIMP_HOOK_TOKEN` | The launch token, substituted by the harness from its own environment. The variable **must** be named in the entry's `allowedEnvVars` or it substitutes to the empty string; cImp sets it on the Claude child at spawn (`tabs::config::compose_ai_env`) rather than baking a literal into the `--settings` argv value. |
| `X-CIMP-Tab` | § 3.2's baked tab id. Caller-asserted and validated against the user's configured Claude tabs before anything is recorded. |
| `X-CIMP-Agent` | Always `claude`. |
| `X-CIMP-Chp` | § 3's `chp`, substituted from `CHP_VERSION` at generation. |
| `X-CIMP-Hello` | `SessionStart` only: `{"serves":[…],"cannot":[{id,why}…]}`, the § 4.1 declaration, computed from the booleans that decided what this tab's overlay actually wired. |

**Fail-open, in HTTP terms.** The shims' contract was "print nothing, exit 0".
The equivalent here is the harness's own: a timeout, a refused connection and any
non-2xx are **non-blocking**; a 2xx JSON body with no directive is a no-op.
Blocking is expressible *only* as 2xx plus a decision field, which is why the
read advisor is structurally unable to refuse a read by failing. Every handler
answers `200 {}` when it has nothing to say, and every emitted entry carries an
explicit `timeout: 1` — the deleted shims' 600 ms budget, rounded up, pinned by a
test rather than left to the harness's 600 s / 30 s defaults.

**`terminalSequence` is never emitted.** It writes escape sequences into the PTY
cImp renders; it is not a CHP capability, and a test asserts no handler produces
one.

Not CHP, listed so the boundary is explicit: `/run`, `/graph_run`, `/audit/run`,
`/mcp/list`, `/mcp/call` (MCP JSON-RPC — another protocol's body),
`/activity/discovery_skipped` (a cImp MCP *child* reporting on itself, not a
harness), `/describe`, `/events`, `/health`, `/status`.

### 4.6 The read path, pushed (Phase L)

Design D2 retires the Tier-C read path — `harness/claude/read.rs` tailing
transcript JSONL, `harness/opencode/read.rs` consuming SSE,
`harness/claude/statusline.rs` parsing stdin — by having the harness push the
same facts. **Phase L realized three of the six** routes Phase I reserved. `chp`
stays at **1**: these are new routes with new meaning (compatibility rule 4),
not a reshaping of anything already on the wire.

| Event | Route | Status | Replaces |
|---|---|---|---|
| `assistant_text` | `POST /session/assistant_text` | **live** | `harness/{claude,opencode}/read.rs` assistant prose → TTS |
| `session.tool_result` | `POST /session/tool_result` | **live** | `harness/claude/read.rs` tool_result sizing |
| `session.subagent` | `POST /session/subagent` | **live** | `harness/claude/read.rs` sub-agent lifecycle (`update_agents`) |
| `session_end` | `POST /session/end` | reserved | session lifecycle inferred from the tap |
| `session.usage` | `POST /session/usage` | **reserved, and stuck** | `harness/claude/read.rs::parse_usage_line` |
| `session.context` | `POST /session/context` | **reserved, and stuck** | `harness/claude/statusline.rs` context window / quota |

Posting to a reserved route gets a `404`.

**Two of them cannot be realized, and that is upstream's constraint rather than
a missing phase.** No Claude Code hook input carries token counts — the common
payload set is `session_id`, `transcript_path`, `cwd`, `permission_mode`,
`hook_event_name`, and `PostCompact` exposes no compaction metrics — and none
carries a context window or a `rate_limits` block. The only documented
token-usage surface is the OpenTelemetry `claude_code.token.usage` metric, which
is an exporter integration and not a hook. So `claude.transcript.usage` and
`claude.statusline.stdin` stay Tier C, permanently-until-upstream-changes, and
these two rows stay named-but-reserved rather than being deleted: an absence with
a stated reason is a fact, a deletion is a blank.

#### Bodies

```json
POST /session/assistant_text
{ "chp": 1, "agent": "claude", "tab": "claude-1",
  "text": "The complete final assistant message, as prose." }

POST /session/tool_result
{ "chp": 1, "agent": "claude", "tab": "claude-1",
  "cwd": "C:/proj", "session_id": "…", "tool": "Read", "chars": 4211 }

POST /session/subagent
{ "chp": 1, "agent": "claude", "tab": "claude-1",
  "agent_id": "agent_01…", "active": true }
```

`text` is **prose, never markup or control**: the sender does not segment, and
everything below `to_speakable` — escape stripping, markdown reduction, sentence
segmentation — is cImp's (`tts/prose.rs`, design § 5.2). `chars` and not the
result content: the consumer is usage accounting, whose estimated-token proxy has
always been a character count, and shipping the content would put an unbounded
model-influenced blob on the wire for a `u32`'s worth of information. `active`
and not an event name: an id that started and has not stopped is an agent
running, which is the whole fact the avatar needs.

#### Arbitration: per capability, per tab, push wins when served

**This is where `serves` stopped being descriptive.** § 4.1 said gating "becomes
load-bearing in Phase L, when the read path moves onto CHP and a capability's
absence has to turn a feature off rather than merely describe it". The rule, in
one sentence:

> A capability is **served** for a tab when that tab has sent a hello AND that
> hello lists the capability. The fallback reader's tap for that capability on
> that tab is then suppressed, and the push core acts. Otherwise the reader
> serves it and the push core refuses.

Both sides ask one predicate (`harness::chp::served`), so exactly one path
produces each datum. Three properties, each closing a specific failure:

- **Per capability.** A tab that pushes assistant text still has its transcript
  tailed for usage and identity. Suppressing a whole reader because one of its
  taps migrated is how a migration loses data.
- **Per tab.** Two Claude tabs can run two different spawn-baked overlays; one
  may push and one (launched before the upgrade) may not.
- **Requires a hello, not a setting.** Settings say what cImp *asked* for; the
  hello says what the artifact on disk actually does, and those differ for
  exactly as long as a stale tab stays open (§ 6.1). A pre-upgrade tab sends no
  hello, is served by its reader, and behaves precisely as it did before.

**The mid-session switchover.** `SessionStart` fires on `resume` and `clear`, so
a hello can land after the reader has already spoken part of a turn that is about
to arrive as one complete `Stop` payload. Speaking the push whole would *replay*;
dropping it would *lose* the remainder. So the reader records the speakable prose
it last emitted for a tab and the first push after the switchover strips it as a
prefix — no replay, no dropped message boundary (`tts::prose`, one `String` per
tab, consumed on read).

**A served capability that goes quiet is reported, not silently un-served.** If a
harness update kills a hook, falling back to the reader would restore the data
and hide the breakage — the exact silent-drift class this protocol exists to
delete. So the reader stays suppressed while the hello's claim stands, and the
silence raises a `contract.drift` report under that capability's own token.
"Demonstrably active" is defined by a **witness**: another push whose arrival
proves this one should also have fired (`prompt` witnesses `assistant_text`;
`context.post_edit` witnesses `session.tool_result`). Three witness pushes with
the served capability silent ⇒ one report. `session.subagent` has no witness
and declares so: a session may legitimately launch no sub-agents forever.

#### Who produces these today

**Claude, through § 4.5's native ingress**, because a hook's body is the
harness's and cannot carry a CHP envelope:

| Claude event | Route | CHP event it feeds |
|---|---|---|
| `Stop` | `POST /claude/hook/stop` | `assistant_text` |
| `PostToolUse` (matcher `""`) | `POST /claude/hook/post_tool_use_result` | `session.tool_result` |
| `PostToolUseFailure` (matcher `""`) | `POST /claude/hook/post_tool_use_failure` | `session.tool_result`, its errored half |
| `SubagentStart`, `SubagentStop` | `POST /claude/hook/subagent` | `session.subagent` |

The tool-result entry is a **second `PostToolUse` matcher group pointing at a
second route**, never a widening of the auto-check entry: Claude evaluates every
matching group, so one shared route would run the project's checks twice and
count one result twice.

**And it needs a third entry, because `PostToolUse` fires only on success**
(2026-08-17). Without `PostToolUseFailure`, a failed tool result reached cImp only
through the transcript tail — which arbitration switches OFF on exactly the tabs
that serve `session.tool_result`, so a serving tab lost every failure's size, and
a failing `Bash` returns as much text as a succeeding one. The failure entry sizes
its `error` field through the same `tool_result_chars` the success entry sizes
`tool_result` with, because that is the function the transcript reader sizes a
failed result's content with too — so the two paths report the same number for the
same failure. It is gated on the same boolean as the success entry, and that
coupling is load-bearing rather than tidy: the reader's tap is suppressed per
CAPABILITY, so wiring one entry without the other would either lose failures or
double-count them.

**"Errored" here means exactly what it means in the transcript reader.** That
reader keeps two readers over one `tool_result` block: one sizes every result
including failures and never looks at `is_error`, the other exists solely to keep
a failed result out of the session→commit provenance tap. The push path mirrors
the first by construction and the second **structurally** — it carries a character
count and never the result text, and provenance is mined only by the transcript
reader, which is not arbitrated. There is nothing on this path for a failed result
to leak into.

**OpenCode produces none of them, declares so, and keeps its SSE reader.** This
is a Phase L outcome under design D6 ("a fallback contained and declared beats a
lossy migration"), not an unfinished migration — the plugin API *can* reach both,
and the `cannot` reasons in its hello say why neither is wired:

- *assistant text* — `experimental.text.complete` delivers one completed text
  **part**, while the SSE reader speaks one **message** with its parts joined, so
  pushing per part would change the unit the sentence segmenter is fed (locked
  decision 2). The alternative, widening the plugin's existing `event` handler,
  reads `properties.part.text` / `properties.delta` — the *same Tier-C shapes the
  reader already reads*, over a different transport, for no tier gain.
- *tool results* — `tool.execute.after`'s second parameter carries
  `{title, output, metadata}` and cImp's handler takes only the first, so the
  result text is one parameter away; but OpenCode usage is estimate-only by
  design and there is no consumer, so wiring it would *add* a capability rather
  than migrate one.

The neutral routes above therefore have no external producer today. They are
implemented rather than deferred because they are the seam: when
`experimental.text.complete` graduates, or when a third harness arrives, the
change is a plugin change and nothing above L2 moves — which is design D6's whole
promise.

---

## 5. The event vocabulary — core and optional, never a union

Design D4. Two harnesses' event sets cannot be unioned without the intersection
rotting into the shape of whichever was implemented first. So: a small **core**
every harness must serve to be usable at all, and an **optional** set each
capability declares a dependency on.

**Core:** `hello`, `prompt`, `assistant_text`, `session_end`.
Three of the four are live since Phase L; `session_end` remains reserved. A
harness that serves none of the read path is still usable on the strength of its
declared fallback reader in `harness/<id>/read.rs` — that is what "unavailable,
not broken" means here, and OpenCode is the standing example (§ 4.6).

**Optional, live:** `context.compaction`, `context.should_read`,
`context.post_edit`, `memory.event`, `permission.event`, `taint.beacon`,
`tool.gate`, `checkpoint.pre_mutation`, `contract.drift`,
`session.tool_result`, `session.subagent`.

**Optional, reserved:** `session.usage`, `session.context` — and both are
reserved *permanently until upstream changes*, for the reason § 4.6 gives: no
hook payload carries token counts or a context window.

**Optional, live since V40 Phase D (`chp = 2`):** `harness.output_started`
(`POST /session/output_started`), `harness.output_stopped`
(`POST /session/output_stopped`) and `subagents.active`
(`POST /session/subagents_active`) — the activity edges. A harness that knows
its own turn boundaries POSTs them instead of leaving cImp to infer them from
the terminal, and each lands on the same core signal an in-process reader emits.
All three are gated on the tab's own hello: pushing an edge a tab never declared
does nothing, which is what keeps a pushing tab and cImp's TUI-marker heuristic
from driving one avatar at the same time (§ 4.6's arbitration rule, applied to
activity).

**Optional, reserved (V40 Phase C, `chp = 2`):** `permission.detected`
(`POST /permission/detected`), `permission.resolved`
(`POST /permission/resolved`), `turn.usage` (`POST /session/turn_usage`) and
`drift` (`POST /activity/drift`).

Each names something cImp already knows, in a vocabulary that is currently one
harness's:

| Event | What it replaces |
|---|---|
| `harness.output_started` / `harness.output_stopped` | `StateSignal::ClaudeOutputStarted` / `…Stopped` — the core activity signal vocabulary, named after one product. The TUI-marker heuristic that produces it today stays as a declared `ActivitySource` (V40 locked decision 18). | **Renamed `HarnessOutput*` and served over the wire since Phase D.**
| `subagents.active` | `AgentsActiveChanged`, documented as "the count of in-flight `Task` sub-agents … emitted by the transcript tail" — a Claude mechanism stated as a core signal. | **Renamed `SubagentsActiveChanged` and served since Phase D** — distinct from `session.subagent`, which reports one sub-agent's lifecycle and lets cImp derive the edge.
| `permission.detected` / `permission.resolved` | The neutral half of prompt detection. Which notification type or TUI footer means a prompt is on screen is the harness's own grammar (`harness/<id>/prompts.rs`, `harness/claude/hook.rs`); what reaches core is an edge and a tab. `permission.event` above is unchanged and stays — it is the legacy `--notify-hook` transport, a Claude payload posted verbatim. |
| `turn.usage` | One turn's reading, carrying the neutral `QuotaWindow` / `TokenKinds` / `TurnOrigin` types. Distinct from `session.usage`, which is the whole-session roll-up. |
| `drift` | Contract drift a *reader* reports — today an Activity row core writes on the reader's behalf. |

**The `live` column is what tells the two apart.** The vocabulary is what L3
gates against, so it is one list rather than a live half plus a design document.
V40 Phase D wired the three activity producers; `is_push_route` answers `true`
for their routes and `false` for the remaining four, so a POST to one of those is
still a 404.

### Why `chp` went to 2

Vocabulary additions only. Every existing event, route and body keeps its shape,
so an artifact written by a `chp = 1` build still speaks the whole of what it
knew — nothing is refused, and § 6.1's staleness report is the entire
consequence: a tab open across this upgrade reads as `old_plugin` until it is
restarted, which is true (it cannot serve an event its generator had never heard
of) and is precisely the mismatch the field exists to make legible.

The table lives in code as `harness::chp::EVENTS` and is checked against this
document by `harness::chp::tests::the_doc_documents_every_event`, so an id
cannot exist in one and not the other.

---

## 6. Versioning, and the failure it closes

### 6.1 Stale-artifact detection

The generated plugin and the `--settings` overlay are **written to disk at tab
launch**. An upgraded binary with an old tab still running therefore has an old
artifact talking to new loopback code. V32 hit this four times (F-13, M-16,
F-23, F-32's child half) and each time the mitigation was the same sentence:
*"needs a FRESH TAB or it reads as a failure."*

With `chp` on the wire, cImp detects it. Every routed POST and every hello is
observed per `(agent, tab)`, and three states are reported in Settings →
*Harness health*, under the harness's own header:

| State | Reported as | Meaning |
|---|---|---|
| `chp` **lower** than this build's, from a harness cImp generates a plugin for | `old_plugin` | The tab is running a plugin an older build wrote. **Restart the tab.** |
| `chp` **higher** than this build understands | `new_plugin` | An older binary running beside a plugin a newer build wrote — the same trap from the other side. This binary is the stale one. |
| a hello's `harness_version` differs from the CLI version cImp currently observes | `harness_version` | The tab is running against a build whose contracts cImp has not verified. |

Nothing is refused in any of the three. The whole value is that the mismatch is
*legible* — the design's accepted permanent cost (§ 8 of the design doc) is that
every `chp` value that ever shipped must keep being understood, precisely
because a stale artifact is a normal state rather than an error.

**Since Phase J, an absent `chp` from Claude IS staleness.** Phase I answered
`false` from `harness::chp::expects_chp` for Claude because it shipped no
CHP-speaking artifact — five shim binaries with nowhere to put a version. Phase J
made the `--settings` overlay exactly such an artifact (`X-CIMP-Chp`, § 4.5), so
that flipped to `true` and every Claude tab is now version-checked by the same
code.

The immediate consequence is intended, not a false positive: **every Claude tab
open across the Phase J upgrade reports `old_plugin` until it is restarted.** It
really is running an overlay full of `cimp --context-hook` command hooks that
this build no longer generates, and *restart the tab* really is the fix. Those
tabs keep working, inert — the retired dispatch flags survive in `main.rs` as
tombstones that drain stdin and exit 0, so an old overlay costs a fast no-op per
hook rather than launching a second cImp — but they get no injection, no read
advisor, no auto-check, and permission detection falls back to the TUI regex.
**Since 2026-08-17 that list is seven flags and includes the two beacons**, so
such a tab also has no native-web taint sensor (the proxied half of the latch
still catches anything routed through cImp) and no per-call rewind points (the
prompt-level checkpoints remain). This is the V32 "needs a FRESH TAB" trap being
*named at the moment it applies*, which is what § 6.1 exists for.

### 6.2 Why OpenCode's hello omits `harness_version`

OpenCode does not expose its own version to a plugin at module-evaluation scope,
and cImp will not bake in a number and let the plugin report it back as though
the harness had said it — that would be cImp attesting to itself. cImp learns
OpenCode's version from `GET /global/health` during a probe run instead (V35
Phase H).

**Claude's hello omits it for the same reason, and Phase J did not change that.**
The hook-input contract has no CLI version field — the common set is
`session_id`, `transcript_path`, `cwd`, `permission_mode`, `hook_event_name` —
so `handle_claude_session_start` reads a top-level `version` opportunistically
(absent in every shape documented today) and leaves `harness_version` empty when
it finds none. cImp learns Claude's version from the transcript's own top-level
`version` instead (`oob::claude::cli_version_of` → `harness_versions.claude_last_seen`).
**The `harness_version` arm of § 6.1 therefore still has no producer on either
harness.** It is implemented and unit-tested; what Phase J supplied is the `chp`
arm, which is the one the deploy trap actually needs.

---

## 7. The trust note

Three CHP events are **security controls that execute inside the harness**, not
data pipes: `tool.gate` (the V32 Phase H native-tool refusal — the `throw` in
`tool.execute.before`), `checkpoint.pre_mutation` (V33 Phase F) and
`taint.beacon` (V32). cImp only *computes* the verdict; enforcement is in the
generated artifact, because only it sits in the harness's own tool path.

An artifact that omits the `throw` — through malice, a misunderstanding, or an
upstream API change its author did not track — **silently disables native-tool
containment while appearing completely functional**, and no cImp-side test can
catch it: the control does not run in cImp's process, and nothing outside a
harness can verify that a control inside it ran. That finding is why cImp loads
no plugin it did not ship (locked decision 10), and why the capability registry
carries a TCB column marking those three rows.

`serves` is therefore **not** a trust claim. An artifact declaring
`serves: ["tool.gate"]` has said nothing cImp relies on; the gate's authority
comes from cImp computing the verdict, and the artifact's only power is to
refuse *more* than it was told to.

**Two of those three read differently on Claude, and always did.** `taint.beacon`
and `checkpoint.pre_mutation` have a second enforcement site — Claude's own
`PreToolUse` path — where cImp is not inside the harness at all: the harness calls
cImp, and cImp does the work. Since 2026-08-17 that site is an http hook plus its
handler rather than a shim binary, which makes the difference sharper: on OpenCode
these controls run in code cImp cannot verify ran, while on Claude the delivery is
observable (the route is reached or it is not) and the checkpoint's ordering is
enforced app-side by answering only after the snapshot is taken. `tool.gate` stays
OpenCode-only, and that is a fact rather than a gap — Claude's beacons are
report-only by V32 locked decision 14 and are structurally incapable of denying,
because a hook that emits no decision field cannot.

---

## 8. Compatibility rules

1. **`chp` is additive and tolerated-absent, forever.** A message without it is
   pre-CHP, never invalid.
2. **Unknown fields are ignored** on every route. No body type sets
   `deny_unknown_fields`, deliberately: a newer artifact sending a field this
   build does not know must degrade to "that field was not read", never to a
   `400`.
3. **A field is never renamed in place.** Add the new name, accept both, and
   retire the old one no sooner than the oldest artifact that can still exist —
   which, since artifacts are written at tab launch and tabs outlive upgrades,
   is bounded only by how long a tab stays open.
4. **A route is never repurposed.** New meaning, new route.
5. **Bumping `CHP_VERSION`** means: the constant in `harness/chp.rs`, the header
   of this document, and a row in § 6.1's table if the bump changes what a
   mismatch means. The first two are enforced by a test.

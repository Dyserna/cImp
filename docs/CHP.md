# CHP — the cImp Harness Protocol

**Protocol version:** `chp = 1`

**Status:** declared V35 Phase I (2026-08-16), issue #66. This document is the
wire contract; `src-tauri/src/harness/chp.rs` is the code half, and
`harness::chp::CHP_VERSION` is checked equal to the version above by
`harness::chp::tests::the_doc_states_this_version`. The two move in one commit
or neither.

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

**Nothing gates on `serves`/`cannot` in Phase I.** They are recorded per tab and
displayed in Settings → *Harness health*. Gating becomes load-bearing in
Phase L, when the read path moves onto CHP and a capability's absence has to
turn a feature off rather than merely describe it.

**Claude Code sends no hello yet.** Its L1 today is five stateless shim binaries
plus a spawn-baked `--settings` overlay; there is no persistent plugin to
negotiate with. *Hello arrives with Phase J (`type: "http"` hooks); until then
Claude capabilities are declared by the registry (`harness/contract.rs`), not
negotiated.* This is milestone locked decision 5 for the phase, and it is why an
absent `chp` from a Claude tab is **not** reported as staleness (§ 6.1).

### 4.2 The push routes that exist today

Documented as they **actually are** on `develop`, field for field. The
"harness-neutral core" the design describes (`{cwd, prompt, session_id, agent,
tab}`) is real but it is the shape of `/context/retrieve` specifically; the
other routes carry their own bodies.

| Event | Route | Sent by | Body (beyond the envelope) |
|---|---|---|---|
| `prompt` | `POST /context/retrieve` | `context_hook.rs` (claude), plugin `chat.message` (opencode) | `cwd`, `prompt`, `session_id` |
| `context.compaction` | `POST /context/compaction` | `compact_hook.rs` | `cwd`, `session_id`, `trigger` |
| `context.should_read` | `POST /context/should_read` | `read_hook.rs` | `cwd`, `session_id`, `file_path`, `offset`, `limit` |
| `context.post_edit` | `POST /context/post_edit` | `postedit_hook.rs` (claude), plugin `tool.execute.after` (opencode) | `cwd`, `session_id`, `file_path`, `tool_name` |
| `memory.event` | `POST /memory/event` | plugin `tool.execute.after` and `event` (opencode only) | tool form: `cwd`, `session_id`, `tool`, `args`, `parent_session_id?` · usage form: `kind: "usage"`, `msg_id`, `model`, `in_tok`, `out_tok`, `cache_read`, `cache_make` |
| `permission.event` | `POST /permission/event` | `notify_hook.rs` | `cwd`, `session_id`, `transcript_path`, `event`, `notification_type`, `message`, `tool_name` — **carries neither `agent` nor `tab`** (see § 4.4) |
| `taint.beacon` | `POST /latch/beacon` | `taint_beacon.rs` (claude), plugin `tool.execute.before` (opencode) | `tool`, `cwd`, `session_id` |
| `tool.gate` | `POST /latch/state` | plugin `tool.execute.before` (opencode only) | — (identity only, deliberately: the answer must not depend on what the caller claims about the tool) |
| `checkpoint.pre_mutation` | `POST /workbench/tool_checkpoint` | `checkpoint_beacon.rs` (claude), plugin `tool.execute.before` (opencode) | `tool`, `cwd`, `session_id` |
| `contract.drift` | `POST /activity/contract_drift` | all five Claude shims + both beacons | `shim`, `missing`, `session_id` — **carries no `tab`**, so its reports are attributed by shim name, not by tab |

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

Not CHP, listed so the boundary is explicit: `/run`, `/graph_run`, `/audit/run`,
`/mcp/list`, `/mcp/call` (MCP JSON-RPC — another protocol's body),
`/activity/discovery_skipped` (a cImp MCP *child* reporting on itself, not a
harness), `/describe`, `/events`, `/health`, `/status`.

### 4.3 Reserved (Phase L)

Design D2 retires the Tier-C read path — `oob/claude.rs` tailing transcript
JSONL, `oob/opencode.rs` consuming SSE, `statusline/mod.rs` parsing stdin — by
having the plugin push the same facts. Those routes are named now so the
vocabulary is one table:

| Event | Route | Replaces |
|---|---|---|
| `assistant_text` | `POST /session/assistant_text` | `oob/*` assistant prose → TTS |
| `session_end` | `POST /session/end` | session lifecycle inferred from the tap |
| `session.usage` | `POST /session/usage` | `oob/claude.rs::parse_usage_line` |
| `session.tool_result` | `POST /session/tool_result` | `oob/claude.rs` tool_result extraction |
| `session.subagent` | `POST /session/subagent` | `oob/claude.rs::SubagentFile` discovery |
| `session.context` | `POST /session/context` | `statusline/mod.rs` context window / quota |

None of these is served today. Posting to one gets a `404`.

---

## 5. The event vocabulary — core and optional, never a union

Design D4. Two harnesses' event sets cannot be unioned without the intersection
rotting into the shape of whichever was implemented first. So: a small **core**
every harness must serve to be usable at all, and an **optional** set each
capability declares a dependency on.

**Core:** `hello`, `prompt`, `assistant_text`, `session_end`.
Two of the four are reserved (§ 4.3) — a harness is usable today on the strength
of the OOB fallback readers, which Phase L retires into `harness/<id>/read.rs`.

**Optional, live:** `context.compaction`, `context.should_read`,
`context.post_edit`, `memory.event`, `permission.event`, `taint.beacon`,
`tool.gate`, `checkpoint.pre_mutation`, `contract.drift`.

**Optional, reserved:** `session.usage`, `session.tool_result`,
`session.subagent`, `session.context`.

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

**An absent `chp` from Claude is not staleness.** Claude ships no CHP-speaking
artifact until Phase J, so `harness::chp::expects_chp` answers `false` for it and
its tabs are never reported. The day Phase J lands, that flips to `true` and
every Claude tab starts being version-checked by the same code.

### 6.2 Why OpenCode's hello omits `harness_version`

OpenCode does not expose its own version to a plugin at module-evaluation scope,
and cImp will not bake in a number and let the plugin report it back as though
the harness had said it — that would be cImp attesting to itself, which is the
same objection that keeps Claude from sending a synthesized hello at all
(locked decision 5). cImp learns OpenCode's version from `GET /global/health`
during a probe run instead (V35 Phase H). The `harness_version` arm of § 6.1 is
implemented and unit-tested; its producer arrives with Phase J's Claude hello.

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

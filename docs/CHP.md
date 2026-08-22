# CHP — the cImp Harness Protocol

**Protocol version:** `chp = 2`

**Status:** declared V35 Phase I (2026-08-16), issue #66; extended additively by
V35 Phase J (2026-08-17), issue #67 — § 4.5 — and again by V35 Phase L
(2026-08-17), issue #69 — § 4.6, which realizes three of the six reserved
read-path events and makes `serves` load-bearing. Extended additively once more
on **2026-08-17**: two beacon capabilities moved from a command transport to a
harness's own HTTP ingress, and a tool-failure entry was wired — new routes in
§ 4.5, no new events. **V40 Phase C took `chp` to 2** and V40 Phase D wired the
first three of its producers (§ 5). This document is the wire contract;
`src-tauri/src/harness/chp.rs` is the code half, and `harness::chp::CHP_VERSION`
is checked equal to the version above by
`harness::chp::tests::the_doc_states_this_version`. The two move in one commit
or neither.

**`chp` went 1 → 2 for vocabulary additions only.** Nothing already on the wire
changed shape: the routes in § 4.2 carry the same bodies, and every harness's
own ingress (§ 4.5) is *additive* — new routes, new meaning, per compatibility
rule 4. The one consequence is the intended one: **a tab opened before the
upgrade reports `old_plugin` until it is restarted** (§ 6.1). That is true — its
generator had never heard of the new events — and it is precisely the mismatch
the field exists to make legible.

Design: [DESIGN-harness-plugin-architecture.md](DESIGN-harness-plugin-architecture.md)
(§ 2 the four layers, § 3 D1/D3/D4/D5, § 7 step 1) and
[MILESTONE-V35-harness-resilience.md](MILESTONE-V35-harness-resilience.md)
(locked decisions 9 and 10).

---

## 1. What CHP is, and what it is not

CHP is **the wire the shipped harnesses have already been speaking**, given a
name and a version. Both harnesses' generated artifacts have posted the same
harness-neutral body to the same loopback routes since V10; `agent` has been the
discriminator all along. Phase I documents that reality, stamps a version on it,
and adds one route (`/session/hello`). It changes **no behavior**.

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
| `agent` / `consumer` | string | The harness discriminator: **a registered harness id** — the `id` of a `HarnessDescriptor` in `harness/registry.rs`, resolved through `HarnessId::from_id` / `from_consumer`. Never a closed enum: a harness is added by adding a descriptor row, and this field's value space widens with it. **Two spellings exist on the live wire** — see below. |
| `tab` | string | The cImp tab id the sender was spawned for. Baked into the artifact at spawn; a harness's own native payload never carries one (§ 4.5). |

**An unregistered id is refused, not defaulted.** `HarnessId::from_id` and
`from_consumer` answer `None` for a token no descriptor declares, and every
consumer of that answer treats `None` as a refusal rather than as a fall-back to
some named product: `graph::mcp::source_for_consumer` answers
`UNKNOWN_SOURCE` (which filters to no sessions), the routes that *require*
identity refuse, and `cimp --consumer <unknown>` fails the proxy start naming
the registered list. The one exception is an **absent** discriminator, which is
a different question with a different answer — see § 3.1.

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

**An absent discriminator resolves to `harness::DEFAULT_HARNESS`**, and that is
a compatibility statement with an expiry date rather than a default harness:
*a body with no identity on this wire came from a build old enough that only one
product could have sent it.* It is one named constant with that rationale
attached (it replaced thirteen `unwrap_or` literals), and **new code does not get
to use it** — a route that can carry an identity must require one. Two routes
invert it, and the asymmetry is load-bearing rather than tidy: a plugin claims
them through `HarnessPlugin::legacy_wire_default_routes()` because only one
harness has ever posted to them at all, so reading them as the default harness
would attribute one product's tool gate to another's tab.
`ingress::tests::every_inverted_wire_default_names_a_route_that_exists` fails
the build if such a claim names a route nothing serves, or resolves to a harness
other than the claimant.

### 3.2 `tab` is the identity, and it is baked

No harness hands its extension a cImp tab id — a harness's own payload names
its session and its working directory, and nothing that identifies a cImp tab.
So the tab id is **baked into the artifact at spawn**, by whatever mechanism
that harness's plugin uses to carry values into it (argv, an environment
variable, a substituted template slot). **This is what makes the artifact
spawn-baked**, and therefore what makes `chp` necessary (§ 6). Where a harness's
ingress carries the whole envelope outside the body instead, it says so itself —
`HarnessPlugin::identity_of_request` (§ 4.5).

A message with no `tab` is accepted and simply attributed to nothing — routes
that need a scope resolve none and fail open.

---

## 4. Routes

### 4.1 `POST /session/hello` — capability negotiation

New in Phase I. Sent **once at artifact load**, which for a generated plugin is
per tab launch — exactly the spawn-baked moment worth stamping.

```json
POST /session/hello
{ "chp": 2,
  "agent": "opencode",
  "tab": "opencode-1",
  "harness_version": "1.18.13",
  "serves": ["hello", "prompt", "memory.event", "tool.gate"],
  "cannot": [{ "id": "taint.beacon", "why": "native web visibility is off or deny" }] }
```

| Field | Required | Notes |
|---|---|---|
| `chp` | no | Absent ⇒ pre-CHP. |
| `agent` | no | A registered harness id (§ 3). Absent ⇒ `harness::DEFAULT_HARNESS`, as on every other route — the compatibility statement of § 3.1, not a guess. |
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

**A harness whose extension mechanism cannot POST a CHP body still sends a
hello** — through its own ingress (§ 4.5), whose handler synthesizes exactly the
record above from the envelope the request carries out-of-band plus a
declaration its generator baked in. Both shipped harnesses send one, and
`every_registry_entry_is_fully_wired` refuses to let a registered harness ship
without declaring a `chp::EV_HELLO` at all: without a hello, Phase I's
stale-artifact detection cannot cover its tabs and a capability's absence is
indistinguishable from nobody having written it down. `harness_version` is
absent from both of today's hellos, for the reason § 6.2 gives.

### 4.2 The push routes that exist today

Documented as they **actually are** on `develop`, field for field. The
"harness-neutral core" the design describes (`{cwd, prompt, session_id, agent,
tab}`) is real but it is the shape of `/context/retrieve` specifically; the
other routes carry their own bodies.

**Two producer shapes reach these routes, and the table does not distinguish
them** because the body is identical either way:

* a harness's **generated artifact** POSTs the CHP body directly (the case for
  any harness whose extension mechanism can make an HTTP request); or
* the harness POSTs its own native payload to a route **its plugin registered**
  (§ 4.5), and that plugin's handler builds the same body and reaches the same
  core.

Which of the two a given harness uses per event is that harness's declaration
(`HarnessPlugin::routes()` and `chp_event_for_route()`), published in its plugin
README's *CHP — event → route table*: for Claude Code,
[`src-tauri/src/harness/claude/README.md`](../src-tauri/src/harness/claude/README.md#chp--event--route-table).
Both shapes are live at once — a tab open across an upgrade may still be POSTing
from the artifact an older build wrote — which is exactly why the two must land
on one core per capability.

| Event | Route | Body (beyond the envelope) |
|---|---|---|
| `prompt` | `POST /context/retrieve` | `cwd`, `prompt`, `session_id` |
| `context.compaction` | `POST /context/compaction` | `cwd`, `session_id`, `trigger` |
| `context.should_read` | `POST /context/should_read` | `cwd`, `session_id`, `file_path`, `offset`, `limit` |
| `context.post_edit` | `POST /context/post_edit` | `cwd`, `session_id`, `file_path`, `tool_name` |
| `memory.event` | `POST /memory/event` | tool form: `cwd`, `session_id`, `tool`, `args`, `parent_session_id?` · usage form: `kind: "usage"`, `msg_id`, `model`, `in_tok`, `out_tok`, `cache_read`, `cache_make`. **One of the two inverted wire defaults** (§ 3.1). |
| `permission.event` | `POST /permission/event` | `cwd`, `session_id`, `transcript_path`, `event`, `notification_type`, `message`, `tool_name` — **carries neither `agent` nor `tab`** (see § 4.4). A legacy transport, kept because a pre-upgrade tab still posts to it. |
| `taint.beacon` | `POST /latch/beacon` | `tool`, `cwd`, `session_id` |
| `tool.gate` | `POST /latch/state` | — (identity only, deliberately: the answer must not depend on what the caller claims about the tool). **The other inverted wire default.** |
| `checkpoint.pre_mutation` | `POST /workbench/tool_checkpoint` | `tool`, `cwd`, `session_id` |
| `contract.drift` | `POST /activity/contract_drift` | `shim`, `missing`, `session_id` — **carries no `tab`**, so its reports are attributed by the reporting artifact's name, not by tab. A converted ingress route raises the same report **in process**, under the same token and into the same ledger. |

The ledger those `contract.drift` rows are keyed by is **declared, not a core
constant**: `HarnessPlugin::drift_vocabulary()` supplies one `&'static str`
bucket per capability, and `harness::ingress::drift_tokens()` is the union over
the registry — so the key space stays bounded by construction (a caller-supplied
string can never become a key) while the names stay the harness's, because they
name its hooks.

### 4.4 Where the live wire departs from the design's summary

The design doc summarizes the push path as *"both harnesses already send an
identical, harness-neutral body"* — `{cwd, prompt, session_id, agent, tab}`.
That is exactly true of `/context/retrieve`, and it is the reason CHP is worth
naming. It is **not** true of the whole surface, and Phase I documents the
reality rather than reshaping it:

- **Two discriminator spellings** (`agent` and `consumer`) — § 3.1.
- **`/permission/event` carries neither `agent` nor `tab`.** It is the legacy
  notification transport: the harness's raw notification payload plus
  `cwd`/`session_id` and nothing else, so its events are attributed by session,
  not by tab. Exactly one harness has ever posted to it, so the discriminator is
  implied — and *that* is a declaration, not an inference: the route's owner
  claims it through `legacy_wire_default_routes()` where the asymmetry matters
  (§ 3.1).
- **`/activity/contract_drift` carries no `tab` either** — a fact
  `loopback::contract_drift_row` already records honestly (`Unattributed`), and
  the reason its reports are attributed to a capability by the **reporting
  artifact's declared token** rather than by tab. Those tokens are the
  harness's, from `HarnessPlugin::drift_vocabulary()`; the ledger and its bound
  are core's.
- **`/latch/state` carries no payload beyond identity**, on purpose: the answer
  must not depend on anything the caller claims about the tool it is about to
  run.
- **`/memory/event` carries two different bodies** on one route, discriminated
  by `kind`.

The consequence for staleness detection (§ 6.1): a route with no `tab` cannot be
attributed to a peer, so `contract.drift` and `permission.event` contribute no
`chp` observation. Neither is a loss — every tab that posts those also posts
`/context/retrieve`.

### 4.5 Harness-native ingress — routes a plugin registers

**Additive extension.** A harness whose extension mechanism cannot POST a CHP
body — because what it emits is *its own* hook-input JSON — reaches cImp on
routes **its own plugin registers**. Those routes are not a second body shape on
an existing route (compatibility rule 4 forbids that) and they are not in the
§ 5 vocabulary: they are a *transport* for events that already have ids.

**Core registers none of them** (V40 Phase C, locked decisions 15 and 22).
`HarnessPlugin::routes()` returns a `&'static [Route]` of `(method, path,
handler)`; `offload/loopback.rs` matches every CHP-neutral arm **first** and
falls through to `harness::ingress::route(method, path)` afterwards, so a plugin
can neither shadow `/session/hello`, `/mcp/*` or the audit and push routes, nor
add a route core does not reach. The handler answers a `HookReply` — a status
and a body — which **core serializes without reading**, because "this harness
answers hook-output JSON and that one answers `{"ok":true}`" is not something
core may know. Four tests hold it: `no_two_plugins_claim_one_route` (a wire
boundary may not depend on registry order), `no_plugin_route_shadows_a_core_route`,
`every_declared_timeout_outlasts_the_budget` and
`every_inverted_wire_default_names_a_route_that_exists`.

Two further declarations travel with the routes:

* **`chp_event_for_route(route)`** — which § 5 event one of this harness's own
  routes feeds. It is the join the quiet detector needs in order to speak about
  *capabilities* rather than about transports: a harness whose payload cannot
  carry a CHP envelope still reaches the same capability core, and this is what
  says which.
* **`identity_of_request(route, req)`** — see § 6.2. `None` (the default) means
  *read the CHP envelope*, which is what core does for every ordinary caller.

**The per-harness table — which of its events maps to which route, what each
route answers, the CLI version floor it needs and how each capability degrades
below it — lives with the plugin, not here.** For Claude Code:
[`src-tauri/src/harness/claude/README.md` § *CHP — event → route table*](../src-tauri/src/harness/claude/README.md#chp--event--route-table).
`POST /permission/event` (§ 4.2) is one of these registered routes too, kept
because a pre-upgrade tab still posts to it; it carries neither `agent` nor
`tab`, which is the same fact § 4.4 records.

**Fail-open, in HTTP terms, on every one of them.** A timeout, a refused
connection and any non-2xx are **non-blocking**; a 2xx JSON body with no
directive is a no-op. Blocking is expressible *only* as 2xx plus a decision
field, which is why the read advisor is structurally unable to refuse a read by
failing. Every handler answers a 2xx with an empty directive when it has nothing
to say.

**The reply budget is derived, not hand-set.** A harness declares how long its
out-of-process caller waits before abandoning cImp's reply and starting the tool
anyway (`HarnessPlugin::hook_reply_timeout()`; `None` = "never waits", and does
not participate). Core takes `min(every declared timeout) −
ingress::HOOK_REPLY_MARGIN` as the time it may spend before answering
(`ingress::hook_reply_budget()` — 1800 ms with the two shipped plugins, pinned
by a test). The ordering is the whole mechanism: the harness starts the tool the
instant its own timer fires, so cImp's answer has to land first or the app is
staging into a call it believes it gated. Each emitted entry pins its own
timeout at generation rather than inheriting the harness's defaults.

**No handler emits a terminal control directive.** Writing escape sequences into
the PTY cImp renders is not a CHP capability, and a test asserts no handler
produces one.

Not CHP, listed so the boundary is explicit: `/run`, `/graph_run`, `/audit/run`,
`/mcp/list`, `/mcp/call` (MCP JSON-RPC — another protocol's body),
`/activity/discovery_skipped` (a cImp MCP *child* reporting on itself, not a
harness), `/describe`, `/events`, `/health`, `/status`.

### 4.6 The read path, pushed (Phase L)

Design D2 retires the Tier-C read path — each harness's fallback reader in
`harness/<id>/read.rs`, plus the status-line stdin reader — by having the
harness push the same facts. **Phase L realized three of the six** routes
Phase I reserved, and left `chp` alone: these are new routes with new meaning
(compatibility rule 4), not a reshaping of anything already on the wire. (The
later bump to `chp = 2` is § 5's vocabulary addition, and likewise reshapes
nothing.)

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
{ "chp": 2, "agent": "claude", "tab": "claude-1",
  "text": "The complete final assistant message, as prose." }

POST /session/tool_result
{ "chp": 2, "agent": "claude", "tab": "claude-1",
  "cwd": "C:/proj", "session_id": "…", "tool": "Read", "chars": 4211 }

POST /session/subagent
{ "chp": 2, "agent": "claude", "tab": "claude-1",
  "agent_id": "agent_01…", "active": true }
```

(`agent` is a registered harness id — § 3 — and these bodies are the same shape
whichever harness sends them.)

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

**One harness produces all three, through § 4.5's own-ingress routes**, because
its hook body is the harness's own and cannot carry a CHP envelope. Which of its
events feeds `assistant_text`, `session.tool_result` and `session.subagent` — and
the two structural facts that come with that mapping — is published in that
plugin's README:
[`src-tauri/src/harness/claude/README.md` § *CHP — event → route table*](../src-tauri/src/harness/claude/README.md#chp--event--route-table).

Two properties of the mapping are protocol-level rather than per-harness, and
they generalize to any harness that wires these:

* **A success entry and a failure entry are one capability, not two.** Where a
  harness reports successful and failed tool results through different events,
  both must be wired from the **same** per-tab flag and both must feed
  `session.tool_result`. Arbitration suppresses the fallback reader per
  *capability*, so wiring one without the other either loses every failure's size
  or double-counts the successes. The failure half deliberately maps to **no CHP
  event of its own**: two ids that can never be declared independently are one
  id, and a second id would let a rare failure push reset the quiet detector that
  is watching the common one.
* **The push carries a size, never the result text.** `chars` and not content —
  the consumer is usage accounting, whose estimated-token proxy has always been a
  character count, and shipping the content would put an unbounded
  model-influenced blob on the wire for a `u32`'s worth of information. It also
  means a failed result has nothing on this path to leak into: session→commit
  provenance is mined only by the fallback reader, which is not arbitrated.

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

| Event | What it replaces | Status |
|---|---|---|
| `harness.output_started` / `harness.output_stopped` | the core activity-signal vocabulary, which was named after one product (`StateSignal::ClaudeOutput*`). The TUI-marker heuristic that infers the edges for a harness that does not report them stays, as a declared `ActivitySource` (V40 locked decision 18). | **live** — renamed `StateSignal::HarnessOutput*` and served over the wire since Phase D |
| `subagents.active` | a core signal documented as "the count of in-flight sub-agents … emitted by the transcript tail" — one harness's mechanism stated as a core fact. | **live** — renamed `SubagentsActiveChanged` and served since Phase D. Distinct from `session.subagent`, which reports one sub-agent's lifecycle and lets cImp derive the edge. |
| `permission.detected` / `permission.resolved` | the neutral half of prompt detection. Which notification type or TUI footer means a prompt is on screen is the harness's own grammar (`harness/<id>/prompts.rs` and that harness's own ingress); what reaches core is a `PermissionEdge` and a tab. `permission.event` above is unchanged and stays — it is the legacy notification transport, one harness's payload posted verbatim. | reserved |
| `turn.usage` | one turn's reading, carrying the neutral `QuotaWindow` / `TokenKinds` / `TurnOrigin` types. Distinct from `session.usage`, which is the whole-session roll-up. | reserved |
| `drift` | contract drift a *reader* reports — today an Activity row core writes on the reader's behalf, keyed by the reader's declared `drift_report_tools()` name. | reserved |

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

**Whether an absent `chp` from a given harness counts as staleness is
declared** — `HarnessDescriptor.expects_chp`, read by `harness::chp::
expects_chp` (V40 Phase A). It answers *does cImp generate a CHP-speaking
artifact for this harness?*, and it used to be a hard-coded disjunction over two
names, which meant stale-artifact detection was silently OFF for any harness not
spelled in it: a new harness would have shipped with the mechanism disabled and
no diff saying so. Both shipped harnesses declare `true`.

**Every bump has the same immediate consequence, and it is intended rather than
a false positive: a tab open across the upgrade reports `old_plugin` until it is
restarted.** It really is running an artifact this build no longer generates,
and *restart the tab* really is the fix. Such tabs keep working, inert — cImp's
retired dispatch flags survive in `main.rs` as tombstones that drain stdin and
exit 0, so an old artifact costs a fast no-op rather than launching a second
cImp — but each capability that artifact never wired is simply absent for that
tab, and the capabilities that have a fallback reader fall back to it. That is
the V32 "needs a FRESH TAB" trap being *named at the moment it applies*, which
is what § 6.1 exists for. **`chp = 2` is the standing example**: a tab generated
by a `chp = 1` build cannot serve an event its generator had never heard of, so
it reads as `old_plugin` and is correct to.

### 6.2 Identity outside the envelope, and the empty `harness_version`

**A harness may declare its identity out-of-envelope.** Where its extension
mechanism emits *its own* payload, there is no field in the body for the § 3
envelope to ride in — so the `(chp, agent, tab)` triple arrives some other way,
and the harness says so itself:
`HarnessPlugin::identity_of_request(route, req)` answers the triple for the
routes that harness owns and `None` for everything else. `None` is the default
and it means *read the CHP envelope*, which is what core does for every ordinary
caller; core holds no special case (it used to hold one — an
`is_hook_route(route)` test inside `note_chp` — and that is exactly what V40
Phase C's locked decision 22 deleted). The values are caller-asserted and
validated before anything is recorded, exactly like the body fields they
replace.

Where each harness puts them — the header names, the token substitution, the
baked hello declaration — is in that plugin's README:
[`src-tauri/src/harness/claude/README.md` § *CHP — hook-body identity*](../src-tauri/src/harness/claude/README.md#chp--hook-body-identity).

**`harness_version` is empty in both of today's hellos, and that is a refusal
rather than a gap.** Neither harness exposes its own version to the artifact
cImp generates: OpenCode does not expose one to a plugin at module-evaluation
scope, and no documented hook-input field carries a CLI version. cImp will not
bake a number in and let the artifact report it back as though the harness had
said it — that would be cImp attesting to itself. It learns each version by
observation instead, per harness: OpenCode's from `GET /global/health` during a
probe run (V35 Phase H), Claude's from its transcript's own top-level `version`
(`harness::claude::read::cli_version_of`), each stored in that harness's own
settings row (`Settings.harness[<id>].last_seen` — the per-harness map V40
Phase B replaced `harness_versions.<id>_last_seen` with).
**The `harness_version` arm of § 6.1 therefore still has no producer on either
harness.** It is implemented and unit-tested; what has a producer is the `chp`
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

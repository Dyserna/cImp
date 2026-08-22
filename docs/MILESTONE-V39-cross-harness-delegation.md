# V39 — Cross-harness delegation (tab drives tab)

**Status:** APPROVED DESIGN (2026-08-21) — implementation not started.
GitHub: umbrella #90, milestone 13; phases A #91 · B #87 · C #88 · D #89.
**Sequencing:** builds on V30 (push bus + per-tab addressing), V32 (result
screening), V35 (hook push / CHP), V37 (live `tools/list` propagation), V38
(`offload_task` live description). Nothing here needs a schema migration step
(new fields all default correctly); no new spawn-baked setting is introduced.

## Motivation

cImp already runs several harnesses side by side (Claude Code, OpenCode) and
can already offload work to local/LAN model servers. What it cannot do is let
one harness use **another harness** as a worker: OpenCode cannot hand a task
to the Claude Code tab and get the answer back, and Claude Code cannot use an
OpenCode tab the same way. Both harnesses are strictly better at some things
(model, tooling, subscription quota, a project-specific config) and the user
is currently the relay.

This milestone adds a **delegation engine** that drives an existing,
visible harness tab exactly as a user would — types the request, waits for
the turn to finish, reads the answer off the tab's own session — and exposes
it to the requesting harness in two forms: an explicit, user-steered tool, and
an invisible facade behind the ordinary `offload_task` backend list. It also
adds the **read-only tab mode** that makes a driven tab safe to leave open.

## Locked decisions

1. **The worker is a normal, visible PTY tab. Always.** No headless spawn,
   no `-p`/print mode, no hidden session, no second process. cImp types the
   request into the worker tab's PTY as keyboard input and reads the reply
   from the same tab's session. Every request and every response is on
   screen, in the tab's scrollback, and in the harness's own transcript —
   the same audit trail a user-driven turn leaves. *Why:* auditability and
   zero new trust surface — the worker keeps its sandbox, permission
   prompts, injection protection and MCP surface exactly as configured.
2. **Input path = the existing user path.** Delegation writes go through
   `TabRegistry::write` (`tabs/registry.rs:431`) via the same pre-write
   pipeline `pty_write` runs (`ipc/commands.rs:166`: TTS-marker
   registration, `note_typed_input`, `UserSubmit` state signal). It does
   **not** use the V30 push bus (`push_to_tab`, `service.rs:626`): a channel
   notice / `noReply` message is *not* a user turn — Claude treats it as a
   notification and OpenCode does not reply to it. *How* a turn is submitted
   (paste encoding, submit key, settle delay) is harness-specific and lives
   in the plugin layer (decision 16) — the engine never knows.
2a. **The worker tab shows who asked — the worker model does not.** The
   attribution `[delegated by OpenCode · tab "api-work" · via cImp]` is
   rendered **client-side only**, in two places: (1) a banner strip on the
   worker tab for the whole flight (driver harness + tab name + elapsed +
   "Take over"), and (2) a local-echo line written into the xterm widget
   inline, just before the request appears — written by the frontend into the
   terminal display, never to the PTY, never into the backend scrollback ring,
   so no harness ever sees it. The glyph title repeats it. The typed request
   is the task **verbatim** — no header, no marker, nothing the worker model
   could read as provenance. The Events lane row (decision 14) is the durable
   record of who asked. *Why:* the user must be able to tell, by looking at
   the tab, that a turn was not theirs and who started it — but the worker
   harness must receive exactly what a user would have typed. (Local echo
   does not survive a scrollback re-seed on tab rebind; the banner and the
   Events row do.)
3. **Two driver modes, one engine:**
   - **Explicit (user-initiated): one proxy tool per worker harness,**
     `delegate_task_<harness-id>` — `delegate_task_claude`,
     `delegate_task_opencode`, and one more for every future harness — on the
     single `cimp-offload` server. Arguments: `task`, `context?`,
     `timeout_s?`. No tab argument: **at most one tab per harness holds the
     Manual role** (decision 8), so tool = harness = tab. The tool **set**
     is generated from the registry's harness ids (`harness/contract.rs`
     `Harness`, the CHP `agent` discriminator) — a tool exists iff that
     harness's Manual tab exists, is not the consumer's own tab, and passes
     the worker gate — so the engine
     names no harness literal, and the tool list itself tells the model
     which harnesses are available right now. There is deliberately **no
     generic `delegate_task`**: one way to do it. *Why per-harness:* the
     user delegates *to a harness* ("send this to Claude Code"), and a model
     selects a tool by name far more reliably than it fills an enum argument.
     Each description names that harness's Manual tab (live, via
     `GET /describe`, like `offload_task`) and is for **user-directed**
     delegation only. The pinned description opens with this contract (a
     test asserts the sentence is present for every generated tool):
     > *Hand a task to an open <Harness name> tab and return its answer.
     > Call this ONLY when the user explicitly asked for a task to be
     > delegated to <Harness name> (e.g. "send this to
     > Claude Code", "use the OpenCode tab for this"). Never call it on your
     > own initiative — for work you decide to offload yourself, use
     > `offload_task`, which you may call automatically whenever you judge
     > it useful.*
     The tools are thereby distinguished from `offload_task` by **who
     decides**: the user (`delegate_task_*`) vs the harness (`offload_task`,
     including facade backends). The requesting harness sees a harness; the
     tab behind it is named in the description.
   - **Facade (automatic):** a worker tab registered as a **third offload
     backend kind**, `OffloadBackendKind::HarnessTab { tab }`. It appears in
     `offload_task`'s backend prose under a **user-chosen backend name**
     (`lan-worker-2 (remote, quality, all tools)`) — never as "Claude tab".
     The requesting harness cannot tell it from an HTTP server; the router
     (`offload/router.rs::select`) picks it by the existing cascade
     (ready → tool scope → context fit → free slot / tier). *Why two modes:*
     the user asked for both; they share the engine and differ only in who
     chooses the worker (model-by-name vs router).
4. **Read-only tab mode, enforced server-side.** A per-tab `ReadOnly` state
   with two sources:
   - `User` — set via the communication-icon popover's Access radio
     (decision 7); sticky; persisted in `AiToolTabConfig.read_only: bool`
     (default false). (Named `User`, not `Manual`, so it cannot be confused
     with the Manual *role* of decision 8.)
   - `Auto` — set by the engine when a delegation starts on the tab, cleared
     when it ends; controlled by the global setting
     `delegation.auto_read_only` (default **on**), exposed as a checkbox in
     Settings.
   Enforcement is in the backend write path (`pty_write` refuses with
   `AppError::ReadOnly { tab, reason }` — *read-only (user)* vs *driven by <tab>*), the
   xterm widget is only a courtesy gate (input swallowed + toast naming the
   reason). **Exempt from the lock** (both sides, shared fixtures): terminal
   protocol replies xterm sends on the program's behalf (cursor-position
   reports, device attributes, focus in/out — refusing them wedges a TUI
   waiting for one, exactly while a delegation drives it) and **mouse wheel**
   reports (scrolling is reading). Mouse clicks/drags, bracketed paste and
   every keystroke are refused. Both sources coexist in one entry
   (`{user, driven_by}`), `Driven` wins; clearing one never lifts the other. Engine writes bypass the lock by construction (they do not enter
   through `pty_write`). A user read-only tab **can** still be a worker —
   read-only governs the user's keyboard, not the engine.
5. **Permission and question prompts unlock the keyboard.** When a driven
   tab raises `awaiting_permission` / `awaiting_question` (`state/manager.rs`
   `TabState`), the auto lock relaxes for that prompt only and the driver's
   wait is extended while the prompt stands; a notification ("worker <tab>
   is waiting for your permission") fires through the existing notifications
   path. *Why:* answering a prompt the worker addressed to the user is not
   "using the tab by mistake" — it is the only way the delegation completes.
   The lock re-engages on `PermissionPromptResolved`. **The deadline does
   not stall:** a prompt *rising edge* buys one bounded grant
   (`PROMPT_GRACE` = 5 min, B1 decision) — a standing prompt polled 50× moves
   the deadline once (test-pinned). Notification = the existing
   `AwaitingPermissionChanged` announcement (no delegation-specific wording;
   it would double-announce).
6. **Take-over is always available.** The popover (decision 7) and, as a
   mirror, the tab context menu offer "Take over (cancel delegation)" on a
   driven tab: the engine stops waiting, the driver
   receives `cancelled: user took over`, the lock clears. cImp never sends
   Escape or any other key into the worker on cancel/timeout — the worker
   finishes what it is doing, visibly.
7. **The tab communication icon — the one control surface.** Every AI tab
   carries a communication glyph next to the shield. **Click opens a
   popover** (the rc.4 shield-popover precedent) with two controls:
   - **Role** (radio): *None* · *Manual* · *Remote offload* — decision 8.
   - **Access** (radio): *Read/write* · *Read-only* — the manual lock of
     decision 4 (the auto lock while driven is shown here as a disabled
     "Read-only (driven by <tab>)" state, with a **Take over** button).
   Glyph state is derived in testable TS (`latch.ts::protectionTint`
   precedent) from `(role, access, in-flight)`:
   - *off* — role None, read/write (dim glyph);
   - *manual* / *remote* — role set, idle (outline glyph; title names the
     role, and for Remote the backend name);
   - *driven* — a delegation is in flight (filled glyph, accent colour; title
     = the attribution line of 2a);
   - a *lock* overlay on any of the above when access is Read-only.
   *Driven* always wins while in flight. Status-bar gets one chip,
   `delegation`, counting in-flight delegations (`sandboxChip.ts`
   precedent). The tab context menu mirrors only **Take over** (so it is
   reachable without the popover); role and access are set in the popover.
8. **Tab role: None | Manual | Remote offload — exclusive, persisted.**
   `AiToolTabConfig.delegation_role: DelegationRole` (default `None`) is the
   **single source of truth** for both modes:
   - **Manual** — the tab is the target of `delegate_task_<harness>`.
     **At most one Manual tab per harness.** Choosing Manual on a second
     tab of the same harness **moves** the role (the previous tab drops to
     None, with a toast on it and an Events row) — a radio across tabs, not
     a refusal; the popover names the tab that currently holds it.
   - **Remote offload** — the tab is a facade backend. **Any number** per
     harness. The `HarnessTab` backend entry is **synthesized from the tab
     role** (the `effective_backends()` precedent that synthesizes the local
     backend) — there is no separate "add backend" step and no second place
     that can disagree. Per-backend knobs (`backend_name`, defaulting to the
     tab name; `tier`; `declared_context`) live on the tab config next to
     the role and are editable in the same popover.
   - A tab is never both: the roles are one enum, not two flags.
   A harness whose Manual tab is absent, is the consumer's own tab, or fails
   the worker gate gets no `delegate_task_*` tool (no dead tool). A Remote
   offload tab that is closed is a not-ready backend, not a deleted one —
   reopening the tab restores it.
9. **One delegation per worker at a time; no nesting by default.** A worker
   is single-slot (`slots = 1`, in-flight tracked like `RemoteBackend`);
   a second request gets the router's "no free slot" / the explicit tool's
   `busy` refusal — never queued silently. A tab that is currently driving
   cannot be driven, a tab being driven cannot drive (acyclic by
   construction; checked at start, refused with the chain named).
   `delegation.max_depth` (default 1) allows opening this later without a
   redesign.
10. **Reply = the worker's final assistant message of the turn that the
    request started.** Primary source: Claude `Stop` hook
    `last_assistant_message` (`harness/claude/hook.rs::ROUTE_STOP`),
    OpenCode session-idle event from the SSE reader
    (`harness/opencode/read.rs`). Fallback: the transcript/event reader's
    `assistant_texts`. Correlation is by turn, not by marker: the first
    completion whose turn began after the submit timestamp (message ids from
    the reader; a test pins that an earlier in-flight turn is not mistaken
    for the reply). **As built:** correlation is a `submit_ms` timestamp
    comparison (unix ms both sides); the completion signal fires ONLY on the
    turn-over edge with the turn's last assistant text (review HIGH-1 — the
    readers' per-message TTS taps are not a delegation source). The attribution (2a) is client-side display only;
    nothing typed into the worker is used as a correlation marker — the task
    text is typed verbatim.
11. **Reply goes through V32 screening — and the call rides the V32 taint
    latch.** The worker's text is wrapped by `detection::wrap_external_result`
    (`offload/detection/mod.rs:695`) under the name `delegation__<worker tab
    id>` (a bare tab name could collide with a trusted tool name and dodge
    the screen) before it is returned to the driver; and a *latched*
    (injection-flagged) driver tab is refused at `/delegate` exactly as it is
    at `offload_task` (V32 C-1c) — a user-directed hand-off does not launder
    model-authored task text —
    the same boundary every external tool result crosses. A harness's output
    is model-generated text entering another model's context; it is not
    trusted because it came from a sibling tab.
12. **Pre-flight, or refuse.** A delegation starts only if the worker tab
    is: open and its process alive; idle (no `ClaudeOutputStarted` burst in
    progress, no pending permission/question prompt); has an empty input
    line (no partial user-typed text per `note_typed_input`); and has a
    completion signal available (`chp::served(agent, tab, EV_ASSISTANT_TEXT)`
    or `harness::reader::has_live_reader(tab)` — OpenCode declares `cannot`
    for `assistant_text` by design, so its reader IS the normal path). Any
    failing condition → immediate structured refusal
    naming the condition; the engine never types into a tab it cannot read
    back from. *Why:* an unreadable worker would silently swallow the task.
13. **Empty is not absent.** A completed turn whose extracted text is
    non-substantive (whitespace, or only tool-call scaffolding) is returned
    as `error: worker produced no text` — never as an empty success.
14. **Every outcome is an Events-tab row.** A new `ActivityKind::Delegation`
    (`"delegation"`) in the unified tool-activity store (`activity.rs`), so
    rows appear in the **Events tab** (`EventsView.svelte` derives its Kind
    filter from the feed — the kind shows up by itself) and persist across
    restarts. One row per transition, the `offload_server` convention:
    `tool` = the transition (`start` / `done` / `refused` / `timeout` /
    `takeover` / `worker_exited` / `role_moved`; `cancelled` reserved), `target` = the worker tab
    name (+ reason on refusals), `source` = the driver harness, attribution
    = the driver tab, `ok` = outcome, `ms` = flight time, `request`/`response`
    = the verbatim task and the screened reply (`request` also on `start`,
    `response` on `done` only). Needs its
    own retention lane (`kind_cap` → `DELEGATION_CAP`) — a kind without a
    lane silently falls into the graph lane — a `rowMeta` branch in
    `EventsView.svelte` (a transition row has no payload; do not print
    "0 chars"), and the `activity.ts` kind union.
    **Facade runs produce two rows by design, not one:** the driver side
    already mints an `offload` row for every completed `offload_task` run
    regardless of backend kind (`offload/service.rs:1561`, `source` = the
    backend name, so it reads `lan-worker-2` — the facade holds on the
    Events tab too); the worker side adds the `delegation` rows. Same split
    as `offload` vs `offload_server`: the task vs what carried it.
15. **No new spawn-baked setting.** the `delegate_task_*` set rides the child proxy's
    live `tools/list` (+ V37 `list_changed` pulse); the facade rides
    `offload_task`'s live description. Changing a tab's role takes effect on the next turn without restarting either tab.
    `spawn_inject_sig` is untouched — a test pins that.
16. **Delegation goes through the harness plugin layer; the engine is
    harness-agnostic.** Per `docs/HARNESS-PLUGIN-LAYER.md`: the engine is an
    L4 capability (`delegation/`) that speaks cImp domain types and
    `contract::gate(id)` only — the `no_harness_literals_outside_harness`
    layering test forbids it naming Claude or OpenCode. Its two harness-facing
    needs sit where the ladder already puts them:
    - **Read half (reply + turn end) = CHP `EV_ASSISTANT_TEXT`**, the L2 event
      TTS already consumes, arbitrated by `chp::served(agent, tab, ev)` and
      falling back to the L1 reader when a harness cannot push. Delegation is
      a **second consumer** of `assistant_text_core`
      (`offload/loopback.rs:7336`) — no new event, no new route.
    - **Push half (submitting a turn) = one L1 `InputProfile` per harness**
      (`harness/<id>/input.rs`: paste encoding — bracketed or not — submit
      byte sequence, settle delay, max paste size), reached from L3 through a
      harness-neutral lookup keyed by the tab's harness id. It is registered
      as a capability row (Seam **D**, `Dep::Behavior("multi-line paste +
      submit yields exactly one turn")`, `Degradation::FailClosed`) with an
      L2 probe against the installed CLI, because a TUI that splits a paste
      into two turns would silently corrupt the task.
    - The first **`Harness::Any` row** the registry has ever constructed:
      `delegation.worker` — contract "serves `EV_ASSISTANT_TEXT` for the
      session's final message and accepts the input profile"; gate id
      `CAP_DELEGATION_WORKER`, mirrored to the frontend like the other
      `GATED` ids. A tab whose harness is not gate-clean is **not a valid
      worker** — not listed as a target, not routable as a facade backend,
      refused at preflight with the gate's reason.
    *Why:* the user wants future harnesses to plug in. With this split a new
    harness becomes a worker by (a) following developer guide A steps 1–6
    (it then serves `EV_ASSISTANT_TEXT` or has a fallback reader) and (b)
    adding one input profile + its probe. No engine change, ever. The
    read-only lock, glyph, Events lane and facade backend are all above the
    seam and inherit it.

## Architecture

### Harness plugin layer seam

```
  L4  delegation/engine.rs  ── gate(CAP_DELEGATION_WORKER) · InputProfile lookup
                               · subscribes assistant_text_core
  L3  harness/contract.rs   ── row `delegation.worker` (Harness::Any, Seam D)
                               row `<id>.input.profile` per harness (Seam D, probed)
  L2  harness/chp.rs        ── EV_ASSISTANT_TEXT (unchanged)
  L1  harness/claude/input.rs · harness/opencode/input.rs   (the only new L1 files)
      harness/claude/hook.rs  (Stop → assistant_text, unchanged)
      harness/opencode/templates/plugin.js (session.idle → assistant_text, unchanged)
```

- `every_harness_dir_declares_its_capabilities` extends naturally: a harness
  dir without `input.rs` simply has no `*.input.profile` row and fails the
  worker gate — fail closed, with the reason "harness has no input profile".
- `MAINTENANCE.md`'s drift table gains the new rows (`matrix_matches_
  maintenance_doc` enforces it).

### Engine (`src-tauri/src/delegation/`)

- `Delegation { id, driver: Option<TabId>, worker: TabId, mode: Explicit|Facade, started, deadline, state }`
  in a registry keyed by worker (single slot). State machine:
  `preflight → typed → waiting(prompt?) → done | refused | timeout | cancelled | worker_exited`.
- `drive(req) -> Result<Reply, DelegationError>`: preflight (decision 12, incl.
  `gate(CAP_DELEGATION_WORKER)`) → engage auto lock → task (verbatim)
  encoded by the worker harness's `InputProfile` → subscribe to the tab's
  completion edge (hook push or reader) and to `pty-exit`, prompt signals,
  take-over → extract + screen → release lock → Events row.
- Driver identity: the explicit tool carries `--tab` from the child proxy
  argv (unforgeable, `activity.rs:415`); the facade carries the routing
  request's tab. A request with no resolvable driver tab (headless
  consumer) is refused — the cycle check needs it.

### Explicit tool

- `offload/mcp.rs` `tools/list`: one `delegate_task_<id>` per harness id
  whose Manual tab exists, is not the consumer's own tab, and passes the
  worker gate; each description names that tab, rendered live from
  `GET /describe`. Dispatch matches the `delegate_task_` prefix and resolves
  the harness id from the suffix via the registry (no literal match arms).
- Target resolution is a lookup, not a search: the harness id from the tool
  suffix → the one tab whose `delegation_role == Manual` for that harness.
  If it vanished between `tools/list` and the call (role moved, tab closed),
  the call is refused naming the condition.
- Result shape mirrors `offload_task` (text + meta: worker, duration,
  screening verdict) so harness-side guidance needs no special casing.

### Facade backend

- `OffloadBackendKind::HarnessTab { tab: String }` alongside `Local`/`Remote`
  (`settings/schema.rs:3592`) — **never written by the user**: entries are
  synthesized in `effective_backends()` from every AI tab whose
  `delegation_role == RemoteOffload`, carrying the tab's `backend_name` /
  `tier` / `declared_context`. The existing backend editor lists them
  read-only with a "configured on the tab" note. `Backend` impl
  (`offload/mod.rs:78`):
  `is_ready` = preflight conditions minus idleness (idle is "free slot"),
  `n_ctx` = declared_context or a generous default, `slots = 1`,
  `tool_scope = All`. New match arms at the sites the survey listed
  (`service.rs:1882/2257/2682`, `supervisor.rs:445`, `mcp.rs:2129`,
  `outbound.rs:886`) — the `Remote => 1` slot hardcode becomes a match.
- `agent.rs` gets a bypass: for a `HarnessTab` backend the worker loop is
  replaced by one `drive()` call (instructions + context become the typed
  request; the worker's own tools do the work). The `schema`/`profile`
  options are REQUESTS on a facade, not boundaries: the profile is stated as
  an instruction (cImp does not own the worker's tool surface; V32
  containment for a facade run = the worker tab's own sandbox/permissions),
  the schema is rendered into the task, and the reply is checked to parse as
  JSON when a schema was requested (named error otherwise) — no grammar.
- Readiness: `is_ready` is evaluated live from tab state (open + readable);
  there is no health checker for offload backends — V37's checker and its
  `mcp_health` rows cover MCP servers only, and today a *remote* offload
  backend going down mints **no** row at all (`offload_server` lifecycle rows
  come from the local supervisor alone — pre-existing gap, tracked
  separately). A `HarnessTab` that is not ready is simply not routed to, and
  an explicit call against it is a `refused` delegation row naming why.

### Read-only + UI

- `ReadOnlyTabs` — a shared `Arc<RwLock<HashMap<TabId, ReadOnlyEntry>>>`
  handle in `AppState` (the `InputLengths` precedent: `TabState` lives inside
  the state-manager actor with no query path, so the write path cannot read
  it synchronously); `ReadOnlySource { User | Driven { by } }` in
  `state/manager.rs`. `pty_write` checks it before any side effect; the
  runtime map is re-synced from the persisted `read_only` flags on every
  settings broadcast (`sync_users`), so a Settings-window or overlay change
  cannot leave a second source of truth; `Driven` rows are untouched by that
  sync. IPC `tab_set_read_only`. Shipped in Phase A (`656e32a..b086f73`): no
  schema bump (additive fields under container-level `serde(default)`);
  `delegation` rides the project overlay like `offload`/`graph`.
- `Tab.svelte`: the communication glyph per decision 7; click →
  `DelegationPopover.svelte` (role radio, access radio, Remote-offload knobs,
  Take over); state derived in `src/lib/delegation.ts` (pure, tested:
  `(role, access, inFlight) → glyph state`, and the one-Manual-per-harness
  move rule). IPC: `tab_set_delegation_role`, `tab_set_read_only`,
  `delegation_take_over`. `TabContextMenu.svelte` gains only "Take over".
- Settings → Tools: `delegation.auto_read_only`, `delegation.default_timeout_s`
  (default 600). `delegation.max_depth` is deliberately NOT exposed while
  its only legal value is the default. No backend creation UI — roles are
  set on tabs.

## Failure modes (adversarial)

- **Worker exits mid-task** (`pty-exit`): driver gets `worker_exited`, lock
  clears, row minted. The tab stays open with its exit banner.
- **Worker stalls on a permission prompt nobody answers**: decision 5
  unlocks + notifies; the deadline still runs; on timeout the driver gets
  `timeout (worker awaiting permission)` and the prompt is left standing —
  never auto-answered.
- **Driver tab closes while waiting**: the worker continues visibly; a
  normal `done` row is minted (the reply survives in the row's `response`)
  but nothing receives it — the child's HTTP connection died with the tab;
  lock clears on completion as usual.
- **Worker replies with injection**: screened (decision 11); a blocking
  verdict returns the V32 refusal envelope, not the text.
- **Model invokes a `delegate_task_*` tool unprompted**: the description restricts it
  to user-directed use, and the tab glyph + Events row make every use
  visible. Accepted residual — same class as any tool the model may
  over-call; the per-tab role opt-in bounds the blast radius.
- **Two drivers race for one worker** (or A→B ∥ B→A): the cycle/depth check
  and the slot claim run under ONE registry lock (`claim_checked`, review
  M-8); the loser is refused `busy` / with the chain named. `claim` is the
  LAST preflight step, which is what makes "no refusal after the claim" true. Facade: the router sees in-flight=1 and routes
  elsewhere or returns `NoBackendReady`.
- **User types during the paste window** (before the lock engaged): the lock
  engages *before* the write, so the window is closed by ordering; a test
  pins it.
- **Completion signal regresses** (hook not installed, reader dead): caught
  by preflight — refused, not hung. A mid-flight loss surfaces as timeout.
- **App restart mid-delegation**: nothing persists; on start no tab is
  `Driven`; user read-only and tab roles are restored from settings.
- **Worker is itself exposed to its driver as a facade and loops**: the
  acyclic check (decision 9) refuses at start and names the chain.
- **Role changed mid-flight** (Manual moved to another tab, or set to
  None/Remote while driven): the in-flight delegation completes under the
  role it started with (the V37 "toggle during in-flight call" rule); the
  new role applies from the next `tools/list` / route. Closing the driven
  tab is the `worker_exited` path.

## Out of scope

- Driving a harness without a visible tab (explicitly rejected — decision 1).
- Streaming partial worker output to the driver (turn-granular only).
- Multi-slot workers / queueing (decision 9 refuses instead).
- Driving Shell or Preview tabs.
- Cross-machine delegation (worker tab in another cImp instance).

## Phases

- **A — Read-only mode + icon**: `ReadOnlySource`, server-side refusal in
  `pty_write`, the communication icon + popover with the Access radio only
  (the Role radio arrives in B), persisted `read_only`,
  `delegation.auto_read_only` setting, glyph *off* / *lock* states, chip.
  *(Independently shippable; useful on its own.)*
- **B — Engine + explicit tool**: `delegation/` module, preflight, write +
  completion correlation for both harnesses, screening, Events lane,
  the generated `delegate_task_*` set in the proxy, the Role radio (None /
  Manual) + one-Manual-per-harness move rule, `manual`/`driven` glyph
  states, banner + local echo, take-over.
  Plus the plugin-layer half: `InputProfile` for Claude and OpenCode, the
  `delegation.worker` (Any) and `*.input.profile` registry rows, their L2
  probes, the `CAP_DELEGATION_WORKER` gate + frontend mirror, MAINTENANCE.md
  drift-table rows.
- **C — Facade backend**: `HarnessTab` kind synthesized from tab roles,
  `Backend` impl, router/agent bypass, live readiness, the Remote offload
  role + knobs in the popover, read-only listing in the backend editor,
  `remote` glyph state.
- **D — Live-verify** (fresh tabs not required — decision 15 — but one
  fresh-tab pass confirms that claim).

## Live-verify

1. Click the Claude tab's communication icon → Role: Manual; in an OpenCode tab ask "send this to
   Claude Code: summarise src/lib/latch.ts". Claude tab shows the
   attribution banner + local-echo line naming the OpenCode tab, the typed
   request (verbatim, no header — confirm in the Claude transcript JSONL) and
   its answer; OpenCode receives the answer; Events row
   `done`; glyph went *driven* → *manual*; keyboard refused during flight
   with the reason toast.
2. Same in reverse (Claude → OpenCode).
3. Facade: on a second Claude tab set Role: Remote offload, backend name
   "lan-worker-2"; the offload backend list shows it read-only; from
   OpenCode call `offload_task` (quality tier). The Remote-offload Claude tab
   performs it;
   OpenCode's result names `lan-worker-2`, never the tab.
4. Permission prompt during a delegation: notification arrives, keyboard
   accepted for the prompt, lock re-engages, delegation completes.
5. Take over mid-flight: driver gets `cancelled`, worker keeps running.
6. Cycle: OpenCode tab A is Manual, Claude tab B is Manual; from A delegate
   to B a task that says "delegate this back to OpenCode" → B's call is
   refused with the chain named (A is driving). Also: a tab never sees a
   `delegate_task_*` for its own tab.
7. Timeout: `default_timeout_s=30` against a long task → `timeout`, worker
   finishes visibly, no keys sent.
8. User read-only and tab roles survive app restart; `Driven` does not.
9. Set the Manual Claude tab back to None → `delegate_task_claude` gone next
   turn without a restart;
   spawn_inject_sig unchanged (test + the save-time restart hint must NOT
   fire).
10. Gate: break a harness's input profile probe (or point a tab at a harness
    dir without `input.rs`) → that harness's `delegate_task_*` tool is absent, its Remote-offload tabs
    are not routed to, and preflight names the gate reason.
11. One-Manual-per-harness: with Claude tab A Manual, set Claude tab B to
    Manual → A drops to None with a toast + Events row; the popover on A
    names B; `delegate_task_claude` now drives B. Setting B to Remote offload
    while Manual → it is Remote only (never both).

## Implementation record (B1 — backend, 2026-08-21/22)

Commits `6f73787..80b640f` + follow-ups. Facts that refine the design above:
- **Prompt-state mirror** = `state::TabActivity` (the `ReadOnlyTabs` shape:
  shared map in `AppState`, one writer — a `note_signal` fold at the top of
  the state-manager loop, ahead of every `continue`). Fields
  `awaiting_permission`, `awaiting_question`, `output_running`, `exited`.
- **Wait loop** = one 200 ms poll over completion / prompt edge / take-over /
  process liveness / deadline — not four subscriptions.
- **Input profiles** (`harness/<id>/input.rs`): bracketed paste for both
  (mode 2004 verified passed through in `terminals.ts`); `settle_ms` Claude
  150 / OpenCode 80 and `max_paste_bytes` 64 KiB are floors, NOT
  measurements; "one paste = one turn" is UNVERIFIED → rows sit in
  `probe::DECLARED_UNPROBED` with waivers, and the gate reads a new spike
  field `harness_versions.input_profile_status` via `spike_status_blocks`
  (opt-in until proven broken; `"fail"` removes every `delegate_task_*` and
  refuses preflight). Live-verify 1 & 2 are the spike.
- **Tool set is generated child-side** from live settings per `tools/list`
  (no `/describe` analogue needed — no app-only health to fetch). Role
  changes reach the V37 pulse through a new `delegation_sig` component of
  `graph::SurfaceFingerprint` (Manual set + gate verdict).
- **Task composition** = `task` or `task + "\n\n" + context`; the blank line
  is the only non-caller byte.
- **IPC/event surface**: `tab_set_delegation_role(tab, role) → {tab, role,
  displaced}`, `delegation_take_over(tab) → bool`, `delegation_status(tab)`,
  `delegation_statuses()`, Tauri event `delegation-changed` carrying a full
  `in_flight` snapshot (`InFlightView { driver, driver_name, driver_agent,
  mode, started_ms, awaiting_prompt }`); `HarnessVersions.input_profile_status`;
  `CAP_DELEGATION_WORKER = "delegation.worker"`. Health panel gained a
  `Cross-harness` panel for the first `Harness::Any` row.
- **Latch gating (`d14cc89`)**: `/delegate` is a *fixed-tool route* — the
  child resolves the harness id and forwards `{harness}`; the app route never
  sees the model-typed name. Canonical unrouted row `delegate_task`
  (`ToolClass::LocalCapability`, same class as `offload_task`), admitted via
  `delegate_admit` (the `hook_admit` shape) *before* the worker is resolved,
  the slot claimed, the lock engaged or a byte typed. **New
  `LatchRoute::Delegation`** — the first cImp-named *elective* call in the
  V32 model (cImp-named like `Hook`, but may MOVE the latch like `Native`);
  reusing `Hook` would have let a tab delegate unboundedly without latching,
  reusing `Native` would have made the gate a silent no-op. `ROUTE_CONTAINMENT`
  now `GatesFixedTool { refused_under_external: true }`, computed from
  `toolclass`. Refusal text is byte-identical to `offload_task`'s.
- **Take-over = one `takeover` row** minted by the engine on its way out
  (`d252b21`); `DelegationError` `Display` carries no transition prefix so the
  driver's tool result and the row reason are one string.

## Implementation record (B2 — UI, 2026-08-22)

Commits `67a9a42 41d55af 831f998 bd62b5f e5159bf` (vitest 780, svelte-check
0/0, no Rust). Facts:
- Phase A's TS `DelegationRole` literal was `'remote'`; serde writes
  `remote_offload` — fixed at the seam (glyph base state stays `'remote'`).
- Two write paths on purpose: role → `tab_set_delegation_role` (backend owns
  the one-Manual-per-harness move); Remote knobs → ordinary
  `AiToolTabConfig` save via the store's `applySettings`.
- `HARNESS_LABELS` in `src/lib/delegation.ts` is the ONE harness display-name
  mapping in TS (none existed); `tabHarness` mirrors Rust `tab_consumer`.
- Banner = overlay at the top of `.pane-content` (a real row would refit
  xterm and resize the PTY mid-turn); renders only for the pane's active
  driven tab. **Live-verify: it covers the terminal's top row.**
- Elapsed clock = one `readable` whose interval exists only while subscribed.
- Local echo fires once per flight on the flight's first edge, keyed on the
  opening-paint baseline (`e5159bf`), via `getTerminal(tab).writeln`.
- Events `rowStatus` gained `driving` / `takeover` / `moved` so a `start`
  row no longer reads "Call succeeded".
- `delegation.max_depth` deliberately NOT exposed (its only legal value is
  the default while nesting is refused). `delegation_status(tab)` has no
  frontend consumer (`delegation_statuses` + the event cover it).

## Implementation record (C — facade, 2026-08-22)

Commits `a2db16d 8bfabb7 b803efc 85fbd68 4fc007e` (cargo 2685/0/6, vitest
782). New `offload/harness_tab.rs`. Facts:
- `Settings::effective_offload_backends()` appends facades in both branches;
  on the raw list by design: `supervisor::local_backends` (process funnel),
  `primary_local_command`, `outbound::Policy` (no endpoint), the backend
  editor (a round-trip save never writes `harness_tab` — test-pinned). Name
  collision: configured wins, facade dropped, one `warn!` per name.
- Bypass lives in `service::run_on` (right after the slot is taken, before
  any tool surface), not `agent.rs` — `agent::run` has no `AppHandle`.
  `agent::facade_format_note` owns the appended text.
- **`schema`/`profile` on a facade are REQUESTS, not boundaries** (refines
  decision 3): cImp does not own another harness's tool surface, so the
  profile is stated as an instruction and the schema is rendered into the
  task (worker has no grammar); the facade path does not re-validate. What
  holds is the worker tab's own sandbox / permissions / MCP surface.
- `n_ctx` default 200 000 (a routing number); `slots = 1`; `in_flight` also
  counts the worker's OWN user mid-turn (`busy_reason`) so a busy worker
  spills instead of failing; a headless consumer is never routed at a facade
  (`routable`); `PoolEntry.is_remote = false`; opaque `harness-tab://<id>`
  base_url for the escalation guard; `Refused → OffloadNotReady`
  (re-routable), other errors → `Offload`.
- Driver-side `offload` row stays `Attribution::Headless` (F-29; also keeps
  the facade indistinguishable); the driver is on the worker-side rows.
- A closed RemoteOffload tab stays in the pool as `ready: false`; the 12 s
  health watch refreshes it and fires the `list_changed` pulse.
Review: docs/reviews/code-review-V39-2026-08-22.md. Fix commits listed there once landed.

## Review-fix record (2026-08-22)

All findings in `docs/reviews/code-review-V39-2026-08-22.md` closed in
`4fc007e..e32cade` (cargo 2708/0/6, vitest 788) + two follow-ups (OpenCode
main-session-only idle; Claude `stop_reason` canary + row). Facts that refine
the design:
- **Turn boundary, as built.** OpenCode: `close_turn` after `flush_all`, last
  assistant message of the turn (not a concatenation), MAIN session only.
  Claude fallback: `message.stop_reason` present and ≠ `tool_use` (unknown
  value ends the turn), filed at the end of a drain pass, sidechain excluded,
  a user prompt restarts. This is a NEW contract dependency
  (`claude.transcript.stop_reason`, canaried).
- **Task hygiene.** `compose` normalises `\r\n`→`\n`; any ESC / C0 / C1
  control other than `\n`,`\t` (incl. bare `\r`) is refused by name and
  offset before the claim — never sanitised. `write_through_pipeline` takes
  an explicit `Submit` so only the submit write emits `UserSubmit`.
- **Restart.** `pty_restart` re-seeds `TabActivity` (AI tabs never emit
  `ShellRestarted`); `LIVE_READERS` is tab → spawn epoch.
- **Facade opacity.** Every `DelegationError` maps to a backend-shaped
  message naming only the facade (`facade_error`, variant-driven; the full
  reason stays in the log + Events row). Default facade name =
  `worker-<4 hex FNV-1a of tab id>`, identical in Rust and TS.
- **Locks.** `ReadOnlyEntry.prompt_relaxed` opens the keyboard for BOTH
  sources while a prompt stands; cleared on the falling edge and on
  `set_driven(None)`. Cycle/depth/claim = `claim_checked` under one lock;
  precedence: busy/mid-turn reasons before the chain.
- **Substantiveness.** Fenced blocks count unless they are one of two
  scaffold SHAPES (empty block, empty JSON payload) — tool-protocol words
  cannot appear in L4; the `NoText` row keeps the raw text.
- **Facade schema.** Parse-only (one surrounding fence stripped); the repo
  has no JSON-Schema validator and real backends also only parse.
- **Driver gone.** `run_facade` selects on the cancellation token; on cancel
  it marks `driver_gone` and AWAITS the engine's teardown (the future holds
  the slot and the lock). New transition `driver_gone` (renders failed).
- Knobs have their own IPC `tab_set_delegation_backend`; `withTabBackend`
  deleted. Facade collisions are shown as "not in the pool" in Settings.
- Pre-existing, out of scope: TTS speaks sidechain lines (#103).

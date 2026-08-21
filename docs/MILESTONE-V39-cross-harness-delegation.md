# V39 — Cross-harness delegation (tab drives tab)

**Status:** DESIGN (2026-08-21) — awaiting user approval; no implementation
before approval. GitHub: to be filed on approval.
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
   - **Explicit (user-initiated):** a new proxy tool `delegate_task`
     (`target`, `task`, `context?`, `timeout_s?`) on the single
     `cimp-offload` server. Its description names the currently available
     worker tabs (live, via `GET /describe`, like `offload_task`) and states
     it is for **user-directed** delegation only. The pinned description
     opens with this contract (a test asserts the sentence is present):
     > *Hand a task to another open harness tab and return its answer.
     > Call this ONLY when the user explicitly asked for a task to be
     > delegated to another tab or harness (e.g. "send this to Claude Code",
     > "use the OpenCode tab for this"). Never call it on your own
     > initiative — for work you decide to offload yourself, use
     > `offload_task`, which you may call automatically whenever you judge
     > it useful.*
     The two tools are thereby distinguished by **who decides**: the user
     (`delegate_task`) vs the harness (`offload_task`, including facade
     backends). The requesting harness sees a harness tab, by name.
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
   - `Manual` — user toggles via the tab context menu ("Read-only"); sticky;
     persisted in `AiToolTabConfig.read_only: bool` (default false).
   - `Auto` — set by the engine when a delegation starts on the tab, cleared
     when it ends; controlled by the global setting
     `delegation.auto_read_only` (default **on**), exposed as a checkbox in
     Settings.
   Enforcement is in the backend write path (`pty_write` refuses with
   `AppError::ReadOnly { tab, reason }` — *manual* vs *driven by <tab>*), the
   xterm widget is only a courtesy gate (input swallowed + toast naming the
   reason). Engine writes bypass the lock by construction (they do not enter
   through `pty_write`). A manual read-only tab **can** still be a worker —
   read-only governs the user's keyboard, not the engine.
5. **Permission and question prompts unlock the keyboard.** When a driven
   tab raises `awaiting_permission` / `awaiting_question` (`state/manager.rs`
   `TabState`), the auto lock relaxes for that prompt only and the driver's
   wait is extended while the prompt stands; a notification ("worker <tab>
   is waiting for your permission") fires through the existing notifications
   path. *Why:* answering a prompt the worker addressed to the user is not
   "using the tab by mistake" — it is the only way the delegation completes.
   The lock re-engages on `PermissionPromptResolved`.
6. **Take-over is always available.** Context menu on a driven tab offers
   "Take over (cancel delegation)": the engine stops waiting, the driver
   receives `cancelled: user took over`, the lock clears. cImp never sends
   Escape or any other key into the worker on cancel/timeout — the worker
   finishes what it is doing, visibly.
7. **Tab icon, tri-state, next to the shield.** A lock-style glyph on AI
   tabs with `title` + click → context menu, derived in testable TS
   (`latch.ts::protectionTint` precedent):
   - *exposed* — tab is a registered delegation target / facade backend,
     idle (outline glyph);
   - *driven* — a delegation is in flight (filled glyph, accent colour; the
     title names the driver tab);
   - *locked* — manual read-only (filled glyph, neutral colour).
   A tab that is both manually locked and exposed shows *locked* with
   "exposed" in the title; *driven* always wins while in flight. Status-bar
   gets one chip, `delegation`, counting in-flight delegations
   (`sandboxChip.ts` precedent).
8. **Opt-in per tab, both directions.** A tab is a valid target only if the
   user enabled it: `AiToolTabConfig.delegation_target: bool` (explicit
   mode) and/or a `HarnessTab` backend entry referencing it (facade mode).
   Neither the `delegate_task` tool nor the facade backend exists for a tab
   the user did not mark. A tab with no target tabs configured does not get
   `delegate_task` in `tools/list` at all (no dead tool).
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
    for the reply). The attribution (2a) is client-side display only;
    nothing typed into the worker is used as a correlation marker — the task
    text is typed verbatim.
11. **Reply goes through V32 screening.** The worker's text is wrapped by
    `detection::wrap_external_result` (`offload/detection/mod.rs:695`) with
    the worker tab named as the source before it is returned to the driver —
    the same boundary every external tool result crosses. A harness's output
    is model-generated text entering another model's context; it is not
    trusted because it came from a sibling tab.
12. **Pre-flight, or refuse.** A delegation starts only if the worker tab
    is: open and its process alive; idle (no `ClaudeOutputStarted` burst in
    progress, no pending permission/question prompt); has an empty input
    line (no partial user-typed text per `note_typed_input`); and has a
    completion signal available (`chp::served(agent, tab, EV_ASSISTANT_TEXT)`
    or a live reader). Any failing condition → immediate structured refusal
    naming the condition; the engine never types into a tab it cannot read
    back from. *Why:* an unreadable worker would silently swallow the task.
13. **Empty is not absent.** A completed turn whose extracted text is
    non-substantive (whitespace, or only tool-call scaffolding) is returned
    as `error: worker produced no text` — never as an empty success.
14. **Every outcome is an Events row** in a new `delegation` lane
    (start / done / refused / timeout / cancelled / takeover / worker-exited),
    each row naming driver tab, worker tab, mode, and duration. The Tools
    tab's existing per-call logging gets `delegation` as a tool class.
15. **No new spawn-baked setting.** `delegate_task` rides the child proxy's
    live `tools/list` (+ V37 `list_changed` pulse); the facade rides
    `offload_task`'s live description. Toggling a target or adding a facade
    backend takes effect on the next turn without restarting either tab.
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

- `offload/mcp.rs` `tools/list`: `delegate_task` appended when the consumer's
  tab has ≥1 enabled target tab other than itself; description lists
  targets by tab name + harness kind, rendered live from `GET /describe`.
- `target` resolves by exact tab name, then by harness kind when exactly one
  target of that kind exists; ambiguity → refusal listing candidates.
- Result shape mirrors `offload_task` (text + meta: worker, duration,
  screening verdict) so harness-side guidance needs no special casing.

### Facade backend

- `OffloadBackendKind::HarnessTab { tab: String }` alongside `Local`/`Remote`
  (`settings/schema.rs:3592`); `Backend` impl (`offload/mod.rs:78`):
  `is_ready` = preflight conditions minus idleness (idle is "free slot"),
  `n_ctx` = declared_context or a generous default, `slots = 1`,
  `tool_scope = All`. New match arms at the sites the survey listed
  (`service.rs:1882/2257/2682`, `supervisor.rs:445`, `mcp.rs:2129`,
  `outbound.rs:886`) — the `Remote => 1` slot hardcode becomes a match.
- `agent.rs` gets a bypass: for a `HarnessTab` backend the worker loop is
  replaced by one `drive()` call (instructions + context become the typed
  request; the worker's own tools do the work). The `schema`/`profile`
  options are honoured by appending the same format instruction the worker
  loop would have used.
- Health checker (V37) treats a `HarnessTab` backend as healthy iff the tab
  is open and readable; transitions mint the usual rows.

### Read-only + UI

- `TabState.read_only: Option<ReadOnlySource>` (`Manual | Driven { by }`);
  `pty_write` checks it before any side effect; IPC `tab_set_read_only`.
- `TabContextMenu.svelte`: "Read-only" (checkbox), "Take over" (only while
  driven), "Delegation target" (checkbox, AI tabs). `Tab.svelte`: the glyph
  per decision 7, state derived in `src/lib/delegation.ts` (pure, tested).
- Settings → Tools: `delegation.auto_read_only`, `delegation.default_timeout_s`
  (default 600), `delegation.max_depth`; facade backends are created in the
  existing offload backend editor by choosing kind "Harness tab" and a tab.

## Failure modes (adversarial)

- **Worker exits mid-task** (`pty-exit`): driver gets `worker_exited`, lock
  clears, row minted. The tab stays open with its exit banner.
- **Worker stalls on a permission prompt nobody answers**: decision 5
  unlocks + notifies; the deadline still runs; on timeout the driver gets
  `timeout (worker awaiting permission)` and the prompt is left standing —
  never auto-answered.
- **Driver tab closes while waiting**: the worker continues visibly; the
  reply is dropped with a `done (driver gone)` row; lock clears on
  completion as usual.
- **Worker replies with injection**: screened (decision 11); a blocking
  verdict returns the V32 refusal envelope, not the text.
- **Model invokes `delegate_task` unprompted**: the description restricts it
  to user-directed use, and the tab glyph + Events row make every use
  visible. Accepted residual — same class as any tool the model may
  over-call; the opt-in per target bounds the blast radius.
- **Two drivers race for one worker**: registry insert is atomic; the loser
  is refused `busy`. Facade: the router sees in-flight=1 and routes
  elsewhere or returns `NoBackendReady`.
- **User types during the paste window** (before the lock engaged): the lock
  engages *before* the write, so the window is closed by ordering; a test
  pins it.
- **Completion signal regresses** (hook not installed, reader dead): caught
  by preflight — refused, not hung. A mid-flight loss surfaces as timeout.
- **App restart mid-delegation**: nothing persists; on start no tab is
  `Driven`; manual read-only is restored from settings.
- **Worker is itself exposed to its driver as a facade and loops**: the
  acyclic check (decision 9) refuses at start and names the chain.

## Out of scope

- Driving a harness without a visible tab (explicitly rejected — decision 1).
- Streaming partial worker output to the driver (turn-granular only).
- Multi-slot workers / queueing (decision 9 refuses instead).
- Driving Shell or Preview tabs.
- Cross-machine delegation (worker tab in another cImp instance).

## Phases

- **A — Read-only mode + glyph**: `ReadOnlySource`, server-side refusal in
  `pty_write`, context-menu toggle, persisted manual flag,
  `delegation.auto_read_only` setting, glyph + chip (`locked` state only).
  *(Independently shippable; useful on its own.)*
- **B — Engine + explicit tool**: `delegation/` module, preflight, write +
  completion correlation for both harnesses, screening, Events lane,
  `delegate_task` in the proxy, `driven`/`exposed` glyph states, take-over.
  Plus the plugin-layer half: `InputProfile` for Claude and OpenCode, the
  `delegation.worker` (Any) and `*.input.profile` registry rows, their L2
  probes, the `CAP_DELEGATION_WORKER` gate + frontend mirror, MAINTENANCE.md
  drift-table rows.
- **C — Facade backend**: `HarnessTab` kind, `Backend` impl, router/agent
  bypass, health integration, backend editor UI.
- **D — Live-verify** (fresh tabs not required — decision 15 — but one
  fresh-tab pass confirms that claim).

## Live-verify

1. Mark Claude tab "Delegation target"; in an OpenCode tab ask "send this to
   Claude Code: summarise src/lib/latch.ts". Claude tab shows the
   attribution banner + local-echo line naming the OpenCode tab, the typed
   request (verbatim, no header — confirm in the Claude transcript JSONL) and
   its answer; OpenCode receives the answer; Events row
   `done`; glyph went *driven* → *exposed*; keyboard refused during flight
   with the reason toast.
2. Same in reverse (Claude → OpenCode).
3. Facade: add backend "lan-worker-2" kind Harness tab → Claude tab; from
   OpenCode call `offload_task` (quality tier). Claude tab performs it;
   OpenCode's result names `lan-worker-2`, never the tab.
4. Permission prompt during a delegation: notification arrives, keyboard
   accepted for the prompt, lock re-engages, delegation completes.
5. Take over mid-flight: driver gets `cancelled`, worker keeps running.
6. Cycle: make the worker a target of itself via the driver → refused with
   the chain named.
7. Timeout: `default_timeout_s=30` against a long task → `timeout`, worker
   finishes visibly, no keys sent.
8. Manual read-only survives app restart; `Driven` does not.
9. Toggle a target off → `delegate_task` gone next turn without a restart;
   spawn_inject_sig unchanged (test + the save-time restart hint must NOT
   fire).
10. Gate: break a harness's input profile probe (or point a tab at a harness
    dir without `input.rs`) → the tab is absent from `delegate_task`'s target
    list and from the facade router, and preflight names the gate reason.

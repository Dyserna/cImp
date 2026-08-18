# V37 — MCP Management

**Status:** SPEC — not yet coded (2026-08-18). GitHub: #78 (investigation + full
decision log in dated comments; this doc is the distillation and, from now on,
the design authority).
**Origin:** investigation session 2026-08-18. Everything below was locked on
#78 before this doc was written; nothing here is speculative.
**Sequencing:** V37 designs and lands **before** V38
([MILESTONE-V38-tool-plugin-framework.md](MILESTONE-V38-tool-plugin-framework.md)) —
V38's MCP tier and its per-tool exposure story build on this milestone's
management and propagation machinery.

## Motivation

cImp has two remote MCP servers configured today (ddg + context7, reached
through the single proxy). The intent is to add many more — but every
configured server's tools land in `tools/list` and therefore in every
session's context, needed or not. There is no way to manage exposure. V38
makes this sharply worse: audit/security tool plugins arriving as MCP servers
multiply the surface. Unmanaged exposure is a non-starter; this milestone is
the gate for adding any further MCPs.

## Locked decisions

1. **One management surface, split internal / external.** *Internal* = MCP
   servers cImp hosts, supervises, and administers (the #41 cimp-mcps half;
   its lifecycle/health/update affordances live in this surface when #41
   lands). *External* = everything cImp does not host — **user-administered,
   even when running on the same machine**. The boundary is administration
   responsibility, not network locality; a third-party server on localhost is
   external. Naming deliberately matches the V32 taxonomy (`toolclass.rs`
   EXTERNAL default). #41 and this milestone share the surface; they ship
   independently.
2. **Categories are user-created.** The user creates a category (e.g. "web
   research", "Java") and adds MCP servers to it. A category **is** the
   profile — the group enabled/disabled as a unit (the earlier abstract
   "profiles" concept is absorbed by this). Category is orthogonal to the
   internal/external axis: a category may mix both.
3. **Toggle model.** Enable/disable exists at the **category level** and at
   the **server level within a category**. All configuration is **global**;
   the **per-project override surface is the enable/disable state** (category
   and server level) via the `.cimp/config.json` overlay. (Symmetric inverse
   of V38, where activation-relevant config — paths/variables/params — is
   what varies per project.) *Recorded interpretation:* "tool level" = server
   level; per-tool granularity **within** a server is feasible via the
   `lean_filter` precedent and can be added if wanted, but is not in scope
   until asked for.
4. **Enforcement point = the single proxy.** Consumers already see only what
   `cimp-offload` advertises; enable/disable is a proxy-side `tools/list`
   filter (precedent: `LEAN_HIDDEN` / `lean_filter()` in
   `src-tauri/src/graph/mcp.rs`). No consumer configuration changes, ever.
5. **Live propagation via debounced `tools/list_changed`.** A toggle notifies
   the harnesses:
   - **Claude leg exists**: the stdio relay (`src-tauri/src/offload/mcp.rs`)
     already declares `tools.listChanged: true` and relays the app's `change`
     pulse (SSE `/events` → `emit_list_changed`).
   - **OpenCode leg to build**: the HTTP endpoint must gain a server→client
     notification stream (Streamable HTTP). OpenCode honors the notification
     upstream since Dec 2025 (PR #5913, both transports).
   - **Debounce is mandatory**: one pulse per toggle *action* (a category
     switch toggling N servers emits ONE pulse, not N). Upstream OC issue
     #34867 (CPU churn on notification storms) is the cautionary tale.
   - **Expected UX, documented honestly**: OpenCode refreshes same-session;
     Claude Code picks changes up **next turn** (upstream support is partial —
     across-turns only, GH #31893/#13646; mid-turn is their bug, not ours).
     Restart-tab remains the documented fallback. Watch item: Claude Code
     fixing those issues upgrades the Claude leg for free.
6. **Observability — all in the Events tab:**
   - Server **enabled but unavailable** at connect/use → error event.
   - **Periodic health check** per enabled server; a failed check or a server
     going offline → error event. A **recovery event** follows when it comes
     back, so an error is never the lane's last word about a healthy server.
     (Precedents: the `offload_server` lifecycle kind with its own retention
     lane; the supervisor + stdout-EOF crash watcher.)
   - **Every call to an MCP server is logged.** The existing `mcp` Events
     lane (own retention cap) already logs proxied MCP traffic; extend rows
     with server identity + category. Health state is also shown live on the
     management UI's server rows.
7. **Model-surface protection carried over from the V38 log:** the
   `quality_audit`/`security_audit` umbrellas stay the default model surface
   for audit tooling; exposing an **external** server's tools directly to the
   model via a category is allowed but its tool **descriptions** enter model
   context — they must pass the same detection screening as tool output
   (screening gap identified during investigation; closed here, where the
   exposure decision lives).

## Architecture

### Registry and settings shape

- Global settings gain an **MCP registry**: server entries (id, transport,
  endpoint/command, internal|external, auth ref) and **category** entries
  (name, ordered member server ids). One stable container in the schema —
  category/server churn never forces a schema migration.
- Project overlay (`.cimp/config.json`) carries only an **activation map**:
  `{category id → enabled?, server id → enabled?}` deltas over global state.
- Effective exposure for a session = global registry ∩ activation state,
  resolved at `tools/list` assembly time in the proxy. Unknown/absent entries
  in the overlay inherit global.

### Propagation

- Any activation change (UI toggle, overlay edit detected on project switch)
  marks the surface dirty; a debounce window (single timer, ~250–500 ms —
  exact value fixed at implementation with a test pinning ONE pulse per
  action) emits `change` on the SSE `/events` stream. The stdio relay path is
  untouched; the OpenCode leg subscribes the HTTP session to the same pulse.
- `SurfaceFingerprint` (graph/mcp.rs precedent) generalizes: recompute the
  advertised-surface fingerprint; suppress the pulse when the effective
  surface did not actually change.

### Health checker

- One lightweight periodic task for all enabled servers (cadence a setting,
  default O(1 min); per-check timeout well under the cadence). A check is
  transport-appropriate: stdio = process liveness; HTTP = `initialize`d
  session ping or cheap `tools/list` HEAD-equivalent.
- State machine per server: `unknown → healthy ↔ unhealthy`, transitions mint
  Events rows (error on `→ unhealthy`, recovery on `→ healthy`), steady
  states mint nothing (no heartbeat spam in the lane).
- Disabled servers are not checked and are not "unhealthy".

### UI

- Evolution of Settings → Tools' existing live-reload MCP editor into the
  management surface: servers grouped by category (and badged
  internal/external), category and server toggles, health chips, per-field
  scope visibility for the per-project activation overrides (explicit
  inherited-vs-overridden, with revert — the `command_allowlist` lesson).
- Placement follows the Tools-sub-tab consolidation precedent; a brand-new
  top-level tab needs stronger justification than this.

## Failure modes (adversarial)

- **Toggle during in-flight call**: the call completes under the surface it
  started with; the new surface applies from the next `tools/list`. Never
  kill in-flight work on a toggle.
- **Pulse lost / consumer ignores it** (old OC build, Claude mid-turn): the
  surface is still enforced at `tools/call` — a call to a disabled server's
  tool is refused with a clear error naming the disabled state. Advertisement
  is a courtesy; enforcement is at dispatch. This is the invariant that makes
  propagation eventual-consistency safe.
- **Health check flap**: transitions require the state to actually change;
  a single failed probe on a healthy server may debounce (N-strikes, design
  detail at implementation) but an offline server must not oscillate rows.
- **Overlay names an unknown server/category**: ignored with one warning
  event, not an error loop (stale project files must not spam the lane).

## Out of scope

- Hosting/supervising internal servers (start/stop/update machinery) — #41,
  same surface, ships separately.
- Per-tool toggles within a server (feasible, deferred until asked).
- V38's plugin framework and its MCP tier (consumes this milestone).

## Phases

- **A — Registry + activation model**: settings container, project overlay
  activation map, proxy-side `tools/list` filtering, dispatch-time
  enforcement, refusal error. *(Independently shippable: management without
  live propagation — restart-tab semantics.)*
- **B — Propagation**: surface fingerprint, debounced pulse, OpenCode HTTP
  notification leg, one-pulse-per-action test.
- **C — Health + observability**: health checker, state machine, error +
  recovery events, per-call logging extension (server + category on `mcp`
  lane rows), UI health chips.
- **D — Management UI**: category CRUD, toggles, internal/external badging,
  scope-visible per-project overrides.
- **E — Screening for exposed descriptions**: external tool descriptions
  through detection screening when a category exposes them to the model.

## Live-verify (fresh tabs, per standing discipline)

1. Toggle a server off → Claude tab: tool gone next turn; OpenCode tab: gone
   same session. Call attempt against a stale surface → refusal error names
   the disabled state.
2. Category toggle spanning 2+ servers → exactly ONE pulse observed.
3. Kill an enabled server's endpoint → error event within one health cadence;
   restore → recovery event. Disabled server stays silent.
4. Per-project override: same server enabled in project A, disabled in
   project B; both tabs live simultaneously behave per their project.
5. `mcp` lane rows carry server + category identity for calls from both
   harnesses and the offload worker.

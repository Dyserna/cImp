# V37 implementation plan — MCP Management

**Written 2026-08-19.** Source of truth for the agent briefs of this run.
Branch: `feature/v37-mcp-management` (off develop). The milestone spec is
`MILESTONE-V37-mcp-management.md` (distilled from GH #78); this file records
what the 2026-08-19 seam investigation found to be *false or thin* in that
spec, and the contracts the orchestrator has fixed so parallel lanes cannot
drift. Run structure: Opus implementation agents, Fable seam review.

## Corrections to the spec (verified 2026-08-19, every claim cited)

| Spec says | Reality |
|---|---|
| Decision 5: "OpenCode leg to build: the HTTP endpoint must gain a server→client notification stream (Streamable HTTP)" | **False premise. OpenCode reaches the proxy over stdio**, spawning the same `cimp --offload-mcp --consumer opencode` child as Claude (`harness/opencode/config.rs:488-512`, `type: local`). The stdio relay already declares `tools.listChanged: true` (`offload/mcp.rs:359`) and `emit_list_changed` exists exactly as named (`offload/mcp.rs:1756`, dispatched on the SSE `change` frame at `:1697`). There is **no HTTP MCP endpoint anywhere** — `offload/server.rs` is the llama-server supervisor; `loopback.rs` is the bearer-token internal API; the only Streamable-HTTP code is cImp as *client* (`mcp_host.rs:1373`). **Phase B is re-scoped: no new transport.** OpenCode's same-session refresh over stdio (upstream PR #5913 covers both transports) becomes live-verify item 1; if it fails live, building an HTTP server leg is a NEW milestone decision, not a silent addition. |
| "Settings → Tools' existing live-reload MCP editor", "Tools-sub-tab consolidation precedent" | **The Settings window has never had a Tools section** — `src/lib/settingsPointers.test.ts` is a tripwire against exactly this phrase. The MCP editor is sidebar section id `'mcp'`, label "MCP servers" (`SettingsApp.svelte:1356`, markup `:5704-5840`), inline in the 9,168-line file. V37 extends section `'mcp'` and extracts a component (precedent: `src/lib/settings/ChecksEditor.svelte`). |
| "enable/disable is a proxy-side `tools/list` filter (precedent: `lean_filter`)" | `lean_filter` filters **built-in graph specs in the child process** (`graph/mcp.rs:53`). The proxied MCP surface is assembled in the **app** process: `McpHost::tool_defs_filtered` (`mcp_host.rs:871`), shared by Claude, OpenCode AND the offload worker. Filtering in the child would leave the worker's surface unfiltered. Enforcement lands in `mcp_host.rs`. |
| "`SurfaceFingerprint` (graph/mcp.rs precedent) generalizes" | It exists (`graph/mcp.rs:391`) but hashes only graph/check settings and gates a **statistics memo** (`surface_stats` for the Overview poll), not the pulse. The live pulse-suppression precedent is `service.rs:2184-2193` (ready-set comparison in `spawn_health_watch`). V37 builds a NEW fingerprint over the proxied surface in `mcp_host`. |
| "Project overlay carries only an activation map `{id → enabled?}`" | The overlay is an untyped deep-diff of the whole `Settings` tree; `deep_merge` (`persistence.rs:1131`) merges objects key-by-key but **replaces arrays wholesale** (`:1130`), and `strip_overlay_banned` (`:735`) drops `llm_pricing`/`harness_versions`/`sandbox` keys. The activation map MUST be JSON objects (`BTreeMap`) keyed by name — never a `Vec` — and must not live under a banned key. |
| "per-field scope visibility … the `command_allowlist` lesson" | `command_allowlist` has **no** inherited-vs-overridden UI anywhere. The real precedent is the tab-override block: `activeTabHasOverrides` (`SettingsApp.svelte:1899`) + "Apply tab overrides to global" (`:3234-3239`). |
| "a category switch toggling N servers emits ONE pulse" | No debounce exists on any pulse path, and the UI does one `settings_update` + one `offload_reload_mcp` **per checkbox** (`setMcpAccess`, `SettingsApp.svelte:1086-1095`); each `reconcile` signals (`mcp_host.rs:865`). Fix BOTH sides: UI batches a category toggle into ONE save + ONE reload; backend debounces the pulse. |
| "extend `mcp` lane rows with server identity" | `ActivityEntry` (`activity.rs:421`) has no server field and identity is NOT recoverable by splitting `tool` on `__` (`mcp_host.rs:928-931` documents why). Real schema change: new required positional args on `ActivityEntry::new` (`:501`; the `:492-499` rule forbids defaulted identity columns) + `#[serde(default)]` for old JSONL rows. |
| Implied single proxied surface | A **third** MCP server exists: `audit/mcp.rs:622` (`cimp-code-audit`, spawned per-tab by both harnesses). **Out of scope for V37** (V38 territory) — stated here so nobody "helpfully" filters it. |

Also verified: schema version constant is `CURRENT_SCHEMA_VERSION: u8 = 31`
(`schema.rs:163`); migration template = `migrate_v30_to_v31` (structure,
`migration.rs:2419-2444`) + `migrate_v29_to_v30` (data-transforming body,
`:2391`); `ddg`/`context7` are pure user config, not in code.

## Locked contracts (orchestrator-owned; briefs use these names verbatim)

### C1 — Identity: the server `name` IS the id
No parallel id space. `McpServerConfig.name` (`schema.rs:3135`) is already
unique (`uniqueMcpName`, `SettingsApp.svelte:1018`) and is the namespace
prefix of every advertised tool. Categories reference member servers by name;
activation maps are keyed by name. Renaming a server is a new identity
(documented in the UI copy). Category names are likewise their own ids,
unique, user-created.

### C2 — Registry shape (schema v31 → v32)
- `McpServerConfig` gains: `origin: McpOrigin` (`internal | external`,
  default `external` — matches the V32 `toolclass.rs` EXTERNAL default) and
  `enabled: bool` (default `true`).
- `OffloadSettings` gains: `mcp_categories: Vec<McpCategory>`
  (`{ name: String, servers: Vec<String>, enabled: bool }` — global list;
  a `Vec` is fine HERE because the registry is global-only) and
  `mcp_activation: McpActivation` —
  `{ categories: BTreeMap<String, bool>, servers: BTreeMap<String, bool> }` —
  the ONLY per-project surface (maps, per the wholesale-array correction).
- Migration `migrate_v31_to_v32`: stamps literal `32`, additive only;
  **invariant: the effective surface of every existing config is unchanged
  after migration** (no categories, all servers enabled, empty activation).
  Test in the house style (`migration.rs:3428-3438`).

### C3 — Effective-enable predicate (single function, single owner)
One pure function in `mcp_host.rs`, unit-tested, used by BOTH advertisement
and dispatch:

```
enabled(server) :=
  (global server.enabled, overridden by overlay servers[name] if present)
  AND ( categories_containing(server) is empty
        OR exists category c containing server with
           (c.enabled, overridden by overlay categories[c.name] if present) )
```

Uncategorized servers ride the server toggle alone — this is what makes the
C2 migration invariant hold. A server whose categories are all disabled is
off even if its own toggle is on. Disabled ⇒ not advertised, not
health-checked, not connected (reconcile treats it as absent), and
`call_for_consumer` refuses.

### C4 — Enforcement is at dispatch; advertisement is a courtesy
`McpHost::call_for_consumer` (`mcp_host.rs:904`) gains a refusal BEFORE the
`owns` check, distinctly worded and machine-recognizable, naming the disabled
state and level: `server 'X' is disabled (category 'Y' is off)` /
`server 'X' is disabled (server toggle)`. Precedent wording pattern at
`:916-922`. This is the invariant that makes propagation
eventual-consistency-safe; the containment matrix row at
`loopback.rs:17389-17391` already promises it.

### C5 — Propagation (no new transport)
- New `McpSurfaceFingerprint` in `mcp_host.rs`: hash over the effective
  advertised surface per consumer (server names + their tool names + access
  flags). Recomputed on reconcile/toggle; pulse suppressed when unchanged
  (precedent: `service.rs:2184-2193`).
- Debounce: ONE timer in the service layer (window 300 ms; the test pins ONE
  pulse per action, not the constant), coalescing `signal_change` bursts.
- UI side (Phase D, not B): a category toggle = ONE `settings_update` + ONE
  `offload_reload_mcp`, regardless of member count.
- The stdio relay path (`offload/mcp.rs`) is untouched. The user-facing
  "restart the tab" copy at `SettingsApp.svelte:5746-5748` is updated to the
  honest per-harness story (OpenCode same-session / Claude next-turn /
  restart as fallback).

### C6 — Health checker
- One periodic task for all ENABLED servers; cadence a setting
  (`mcp_health_interval_secs`, default 60; per-check timeout well under it).
  Transport-appropriate probe: stdio = process liveness; HTTP = cheap
  `tools/list` on the initialized session.
- Per-server state machine `unknown → healthy ↔ unhealthy`; `→ unhealthy`
  requires N=2 consecutive failures (flap guard); transitions mint Events
  rows, steady states mint nothing. Disabled servers: no checks, no state.
- New `ActivityKind::McpHealth` (`mcp_health`), own lane + cap — the exact
  five-site edit `offload_server` made: variant (`activity.rs:194` template),
  `as_str` (`:215`), cap const (`:85`), `kind_cap` arm (`:240`),
  `TOTAL_CAPACITY` (`:136`); the compile-time assert at `:159` enforces the
  set. Recovery event follows every error event when the server returns — an
  error is never the lane's last word about a healthy server.

### C7 — `mcp` lane rows carry server + category
`ActivityEntry` gains `server: Option<String>` and `category: Option<String>`
as REQUIRED positional args on `ActivityEntry::new` (per the `:492-499`
rule), `#[serde(default)]` on read so old JSONL rows still parse. Every
call site updated (compiler finds them). `McpHost::call_recorded`
(`mcp_host.rs:1079-1091`) populates both from routing knowledge — never by
string-splitting the tool name.

### C8 — UI lives in section `'mcp'`, extracted
Extend section `'mcp'` ("MCP servers"); no new top-level section. Extract
`src/lib/settings/McpManagementEditor.svelte` + pure logic in
`src/lib/settings/mcpEditor.ts` with vitest (precedent:
`ChecksEditor.svelte` / `checksEditor.test.ts`). Servers grouped by category,
badged internal/external, category + server toggles, health chips (data
already in `ServiceStatus.mcp_servers`), per-project activation override with
explicit inherited-vs-overridden + revert (precedent: the tab-override block,
NOT `command_allowlist`). Never write the phrase "Settings → Tools" anywhere
(`settingsPointers.test.ts`).

### C9 — Description screening (Phase E)
Screen tool **descriptions** with `detection::screen()` directly
(`detection/mod.rs:414`) at `tools/list` parse time in the host —
`mcp_host.rs:1829`, once per connect — NOT `wrap_external_result` (its
headers/envelope would corrupt a description string). Policy: a flagged
description ⇒ that TOOL is dropped from the surface (server stays; other
tools unaffected) + one error event in the `mcp` lane naming tool + server.
Applies to `origin: external` servers only. Re-screen on reconnect (surface
may change server-side).

### C10 — Out of scope (stated to every agent)
`audit/mcp.rs`'s surface (V38); per-tool toggles within a server (deferred
until asked); hosting/supervising internal servers (#41); any new HTTP MCP
transport (see corrections).

## Environment constraints (every brief carries these)

- A second agent may share this working tree: **scope every `git add` to
  explicit paths**; never `git add -A`/`-u`, never `cargo fmt` repo-wide,
  never `git checkout --`/`reset` beyond your own files.
- Checks: `run_check {name: cargo-check|cargo-test|tsc, changed_only: true}`
  via MCP where available; otherwise `cargo test --locked --bin cimp`
  (`--bin cimp` is mandatory — no lib target) and `npx vitest run`.
- vitest does NOT type-check — run `tsc`/`npm run check` for TS changes.
- Commit on `feature/v37-mcp-management` with `feat(V37): …` /
  `fix(V37): …` subjects; one commit per phase lane.
- Report back: assumptions made, contracts relied on, discretionary
  decisions, and anything that contradicts this plan — surfaced, not
  silently chosen.

## Phases, dependencies, sequencing

```
A (registry+enforcement) ──► B (propagation, backend-only) ──► C (health+observability) ──► E (screening)
        │
        └────────────► D (UI, frontend-only — parallel with B/C)
```

- **A — Registry + activation + enforcement** (C1–C4): schema v32 +
  migration, effective-enable predicate, `tool_defs_filtered` filter,
  dispatch refusal, reconcile skips disabled. Baselines captured FIRST
  (cargo test + vitest counts at branch point). *Independently shippable —
  restart-tab semantics.*
- **B — Propagation backend** (C5 minus UI): fingerprint, debounced pulse,
  suppression test, one-pulse-per-reconcile-burst test.
- **D — Management UI** (C8 + C5's UI half + copy update): runs parallel
  with B/C (disjoint trees: B/C are `src-tauri`, D is `src/`; C's small
  `EventsView`/`activity.ts` touches are disjoint from D's files).
- **C — Health + observability** (C6, C7): checker, state machine, events,
  row columns, `EventsView`/`activity.ts` filter support. After B: both
  rework `mcp_host.rs`.
- **E — Screening** (C9): small, last, touches `mcp_host.rs` connect path.

Fable seam reviews: after A (before B/D launch) and after E (whole-run seam
pass: who reads what, toggle-during-flight, rerun/partial-failure), findings
closed or consciously deferred with an owner.

## Live-verify (fresh tabs; gates milestone close alongside #78)

1. Toggle a server off → OpenCode tab: gone same-session **over stdio** (this
   verifies the Phase-B re-scope; if it fails, the HTTP leg becomes a new
   decision); Claude tab: gone next turn. Stale-surface call → refusal names
   the disabled state and level.
2. Category toggle spanning 2+ servers → exactly ONE settings write, ONE
   reconcile, ONE pulse.
3. Kill an enabled server's endpoint → error event within one cadence + N-1
   extra probes; restore → recovery event. Disabled server stays silent.
4. Same server enabled in project A, disabled in project B, both tabs live →
   each behaves per its project overlay.
5. `mcp` lane rows carry server + category for calls from Claude, OpenCode
   AND the offload worker.
6. Migration: pre-v32 settings file loads with an unchanged tool surface.

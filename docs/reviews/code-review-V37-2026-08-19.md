# V37 close-out seam review — 2026-08-19

Run record for the V37 MCP Management implementation
(`feature/v37-mcp-management`; design authority
`docs/IMPL-PLAN-V37-mcp-management.md`). Two seam reviews ran: after Phase A,
and after Phase E over the whole diff `9515c6d..54ef238`. This file is the
decision log; every finding below is CLOSED or consciously deferred with an
owner.

## Phase commits

| Phase | Commit | Suites after |
|---|---|---|
| Plan | `9515c6d` | — |
| A — registry + activation + enforcement | `9fce295` | cargo 2355/0/6, vitest 629 |
| B — fingerprint + pulse gate + F1–F5 | `eea79e0` | cargo 2366/0/6 |
| D — management UI | `499b3a9` | vitest 652, check 336/0/0 |
| C — health checker + Events | `098e197` | cargo 2380/0/6, vitest 663 |
| E — description screening + recovery retry | `54ef238` | cargo 2391/0/6 |
| Fix — `withheld` row status (E-2) | see git log | — |

Branch-point baselines: cargo 2343/0/6, vitest 629 (32 files), svelte-check
333/0/0.

## Phase-A review (mid-run)

C1–C4 PASS. Findings F1 (disabled_owner false-refusal vs a live
`git__extra`-style owner, MED), F2 (refusal leaked existence to ungranted
consumers, LOW), F3 (overstated in-flight claim, doc), F4 (shutdown left
`disabled` stale, LOW), F5 (registry arrays not actually global-only —
overlay pinned them wholesale, MED) — **all five fixed in Phase B**
(`eea79e0`). F6 (cosmetic readers + SSRF endpoint allowlist ignore `enabled`)
ACCEPTED: labels aren't enforcement; configuring an endpoint is the trust
statement, deactivating is not distrust.

**D-1 (decision record):** `any_claude_mcp()` / `any_opencode_mcp()` /
`mcp_host_needed()` deliberately consult `*_access` and NOT `enabled`.
`*_access` is a structural spawn-baked grant; `enabled` is live activation
state. Gating child injection on `enabled` would make re-enable dead in any
tab spawned during a disabled period (defeating C5) and would replace the C4
disabled-refusal with "unknown tool" in the last-server-disabled edge. The
defense lives as doc-comments on all three functions.

**D-1 AMENDED by Phase F (2026-08-19, user-approved scope addition).** The
same hazard was found one level up: `*_access` had it too. A tab spawned with
zero grants got no `cimp-offload` child at all, so there was no
`tools/listChanged` relay for the C5 pulse to travel on and a later grant still
needed a fresh tab — hit live by the user. The fix is not a better predicate but
the removal of the spawn-time decision: **the proxy child is now unconditional in
every AI tab** (`harness/claude/overlay.rs`, `harness/opencode/config.rs`), and
`advertises_offload_to_{claude,opencode}` are deleted. Consequences: both
`spawn_inject_sig` `"mcp"` slots lost their offload element and `"channels"` lost
its conjunct, so an MCP access flip no longer nags open tabs to restart; the
spawn-baked injection-hygiene paragraph lost its advertise gate (a live-changing
input may not gate a baked artifact) and now rides every AI tab under the
consumer-hygiene switch alone; and `events_relay` announces
`tools/list_changed` on every (re)connect, so a child that spawned before the
loopback existed refreshes when a grant brings it up. The three functions survive
for host-lifecycle decisions only — D-1's "do not add an `enabled` term" warning
is now MOOT FOR INJECTION and still live for `mcp_host_needed`.

**Residual (accepted, reported):** on a bare install — offload, graph, Code Audit
and every grant off — the FIRST grant flips `Settings::loopback_needed()`
false→true, which really does change what a fresh Claude tab writes (the
Notification / PermissionDenied / Stop / SubagentStop shims appear), so the
restart hint fires once on that one edge. The MCP tools themselves still reach
the running tab. Pinned by
`spawn_inject_sig_tracks_spawn_time_settings`, which allows exactly `notify_hooks`
to move there and nothing else.

**F-V37-4 (MED) — CLOSED by the sandbox-grant fix.** Making the proxy child
unconditional widened a pre-existing V33 Phase B gap. A sandboxed AI tab
(`sandbox.tabs`, AppContainer) is granted the project root, its harness's own
state and nothing else — so the `cimp --offload-mcp` child that every AI tab
must now spawn could neither be launched (no grant on `cimp.exe`) nor find the
app (no grant on the discovery data beside it, which is how it resolves the
loopback port + token). Every sandboxed tab's MCP child would have failed:
loudly, as a denial row, and still broken. **Decision: three file-scoped grants,
never the exe directory.** `<exe-dir>` is cImp's portable root —
`settings.json` with the app's auth tokens and API keys, `tool-activity.jsonl`,
the detection stores — and V33's posture is that no secret cImp holds may reach
a child, so the read+execute directory grant that would have been the one-line
fix is refused. The tab gets `cimp.exe` (read+execute, FILE),
`<exe-dir>/.cimp-offload.json` (read-only, FILE) and `<exe-dir>/.cimp-discovery/`
(read-only, directory — discovery entries only). The discovery TOKEN is readable
by design: it is the credential the child authenticates to its own app with, and
every tab carrying this child has always read it. The rows are shared by both
harness tables because the child is unconditional in both, and
`the_proxy_child_gets_the_binary_and_its_discovery_and_nothing_wider` pins the
three paths and widths *and* that the exe DIRECTORY, `settings.json` and
`tool-activity.jsonl` are never rows. Two residuals are written into
`sandbox::tabs::cimp_child_rows`: the legacy `.cimp-offload.json` row is
optional, so a tab prepared before the app first wrote that file loses only the
legacy fallback (the granted `.cimp-discovery/` entry is authoritative and its
ACE is inheritable); and a sibling DLL imported at LOAD time would be unreadable
for the same reason — the shipped layout has none (`DirectML.dll`, the one
non-system load-time import, resolves from `System32`). Linux untouched: V33
Phase D deliberately does not sandbox tabs there. No live-behavior change on this
install — `sandbox.tabs` defaults OFF and protection is globally off — so this is
correctness before anyone enables it, and item 13 below is its gate.

## Whole-run review (post-E)

All seven seams PASS: pulse chain (no producer bypasses `run_pulse_gate`;
`Backend` pulses never surface-suppressed; D's action = one save + one
reconcile), toggle-during-flight windows (disabled-before-teardown +
live-owner-first dispatch + mid-sweep guard + `swap_recovered`'s
disabled/ptr_eq re-checks — every retry-vs-reconcile interleaving keeps
exactly one connection), overlay (write-through + one-time heal + delete-key
revert are consistent), Events lanes (caps self-accounting via the
compile-time assert), E's contract deviations (screening in `connect_server`
is the single funnel; retry candidacy = `Unhealthy` keeps every recovery row
answering a real error row), the C10 fence (audit/mcp.rs untouched; stdio
relay untouched), and migration/old-data paths (all invariant-asserting
tests).

### Findings

- **F-V37-1 (MED) — CLOSED by the fix commit.** Screen-withheld rows
  (`kind:"mcp"`, `source:"screen"`) rendered as "Call failed"; `flagged`
  would have been worse ("nothing was blocked" is false here). New
  `withheld` RowStatus with honest wording.
- **F-V37-2 (LOW) — ACCEPTED.** A hand-edited global file carrying an
  activation entry that a project overlay null-deletes fails the typed parse
  and falls back to global for the session (warn logged). No code path
  writes activation into the global file; pre-existing diff/merge-engine
  semantics. This paragraph is the decision record.
- **F-V37-3 (LOW) — ACCEPTED.** A retry sweep with several hung stdio
  candidates can exceed the cadence (sequential connects, 30 s timeout
  each). Self-limiting single task; concurrent retries are the storm the
  design refuses.

### Decisions

- **E-1 — detection-toggle changes do not re-screen a live surface.
  DEFERRED, owner: V38 (#77), as a named requirement.** The host never
  re-lists tools mid-connection, so the only gap is retro-vetting
  already-seen descriptions after a rules update. A drop-only re-screen
  needs interior mutability on `McpServer.tools`; putting detection config
  in `config_sig` is a reconnect storm (including on every auto-updater
  bundle install). Window closes on any config edit, reconnect, or restart.
  V38's tool-plugin trust framework re-does this surface and owns the
  re-screen path.
- **E-2 — FIXED NOW** (F-V37-1).
- **E-3 — timed retry teardown can kill a stdio child mid-call. ACCEPTED.**
  Retry fires only on `Unhealthy` servers whose tools are already withdrawn
  from every surface, so healthy in-flight calls a timer could kill are
  essentially nonexistent; identical teardown to reconcile's, new trigger
  only.

Also recorded from the run: Phase B's F5 heal snaps per-project **access
flag** overlays to global on first launch (documented in
`persistence.rs:1076-1082`; per-project variation is `mcp_activation` only —
if per-project access flags are ever wanted, that is an activation-map
extension, a new decision). `mcp_health_interval_secs` is a per-project
overlay-able scalar (accepted; heal covers arrays only). Phase E screening
covers tool names + descriptions; input schemas out of scope (documented in
`tool_screen_text`).

## Live-verify — the plan's six items plus these

7. **Flap-guard negative:** one failed probe (pause/resume) withdraws
   nothing and mints nothing; only the second does.
8. **Retry non-resurrection:** kill server → `unhealthy` row → toggle OFF →
   restore endpoint → no `reconnected` row, absent from every surface.
9. **Revert = inherit, live:** set a project activation override, revert
   (key deleted from `.cimp/config.json`), then a global toggle flows
   through to that project.
10. **Legacy-overlay heal:** launch in a project whose overlay carries a
    pre-V37 `mcp_servers` copy → promoted + stripped on first save; later
    global registry edits visible in that project.
11. **Screen drop end-to-end:** local rule flags a test tool's description →
    tool absent from `tools/list`, one `mcp`-lane row `source:"screen"` with
    server/category, `withheld` chip (not "Call failed").
12. **Cadence edit live:** `mcp_health_interval_secs` 0 → checker silent;
    back to 5 → resumes next tick, no restart.
13. **Sandboxed tab keeps its MCP child (F-V37-4):** with `sandbox.enabled` +
    `sandbox.tabs` ON, a Claude tab and an OpenCode tab each still list the
    `cimp-offload` tools and can call one; the Sandboxing lane shows the three
    file-scoped grants (`cimp.exe`, `.cimp-offload.json`, `.cimp-discovery`) and
    **no** grant row for the exe directory itself. Shares the V33 #72
    live-verify session.

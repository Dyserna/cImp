# Milestone V8-02: Remote Backends + Capability-Aware Routing (the offload pool)

> **Release tag:** TBD at ship time (placeholder label). Direct sequel to **[MILESTONE-V8-01](MILESTONE-V8-01-local-offload.md)** (single-local offload). **Additive** — V8-01 was built behind a `Backend` seam so this milestone slots in without re-architecting the agent loop, MCP host, budget, or capability registry.
>
> **No separate spike.** Like V8-01, validation is taken **inline during implementation** (no Phase-0 gate).

## Purpose

Generalize V8-01's single local `llama-server` into a **backend pool** and route each `offload_task` to the right backend. Motivating setup: a main PC running the big model (Qwen3.6-35B-A3B, ~150k window) plus a **second machine with an RTX 3070 (8 GB)** on the LAN that can only run a small model with a small window — perfect for *very simple* offloads, freeing the big backend (and avoiding the queue) for heavy work. Optionally a **cloud** OpenAI-compatible endpoint too.

Three capabilities land here:
1. **Remote backends** — a LAN box or a cloud API, configured by `base_url` (+ auth), health-checked and used over HTTP (no local process, so no tab).
2. **Capability-aware routing** — pick **one** backend per task by required context, model strength, tool-need, and live availability; Claude can bias with a `tier` hint.
3. **Per-backend tool scoping** — different backends expose **different tool sets** (the user's point): a cloud model must not get your local filesystem/git/exec tools (privacy), and a weak small model gets a trimmed surface (it's bad at multi-tool loops).

## Relation to V8-01 (what's reused vs. new)

**Reused unchanged from V8-01:** the agent loop (`offload/agent.rs`), MCP host (`offload/mcp_host.rs`), MCP server toward Claude (`offload/mcp.rs`), native tools, context-budget machinery, dynamic thinking, the read-only Offload Server tab (for *local* backends), and the capability registry. The loop already targets a `base_url` over HTTP — a remote backend is just a different one.

**New in V8-02:**
- `offload/backend.rs` — the `Backend` abstraction over `backends: Vec<OffloadBackend>` (V8-01's `LlamaServer` becomes the **Local** impl; a thin **Remote** impl is added).
- `offload/router.rs` — per-task backend selection.
- Per-backend **tool allow-lists** (scoping) layered onto the MCP host + native tools.
- The **`tier`** arg on `offload_task`, union capability reporting, and cloud privacy gating.
- A small **additive schema migration**: `server_command` + `autostart` (V8-01) → `backends: Vec<OffloadBackend>` (one Local entry).

## Decisions locked (from discussion)

- **Backends = a pool (Local | Remote-LAN | Remote-Cloud).**
  - **Local** — cImp owns the process: V8-01's `server_command` + read-only tab + autostart + Start/Stop/Reset. Capabilities (`n_ctx`/`np`/model) discovered from `/props`.
  - **Remote (LAN or cloud)** — cImp holds a `base_url` (+ optional `auth_token`); it **health-checks and connects**, cannot start/stop it. **No tab** (no local process) — surfaced as a Settings **status line**. Capabilities discovered from `/props` when the endpoint exposes it (a remote llama-server does), else **declared** in config (`declared_context`, `declared_model`) for endpoints that don't (many cloud APIs).
  - Each backend has its **own** budget (`n_ctx/np × high_water`), concurrency gate (`np`), and health — exactly V8-01's per-server machinery, now per-backend.
- **Routing is per `offload_task`, never per step.** One backend runs the whole task (conversation state lives on one server's slot — no mid-loop migration). Selection:
  1. **Tool-need is a hard filter** — the task's required tools must be a subset of the backend's allowed tools (see scoping). A "read these local files" task is ineligible for a cloud backend that lacks local-file tools.
  2. **Required context** — estimated task input vs. each eligible backend's per-slot budget; a 100k-token ingest can't go to an 8 GB/16k box.
  3. **Complexity / model strength** — trivial single-pass work (summarize/extract/classify/format) → the small/fast backend; real reasoning → the large one.
  4. **Availability** — if the preferred backend's slots are full (`in-flight == np`) and the task fits another eligible one, **spill** instead of queuing; **fail over** when a backend is down.
  - **Claude biases via `tier: "auto" | "fast" | "quality"`** on `offload_task` (Opus knows if a task is trivial); `auto` lets the router decide. One enabled backend → router is a no-op.
  - **Uncertainty errs toward capability.** Required-tool/required-context estimation is heuristic (you don't know a file's size until you read it). When unsure, route to a **more-capable** backend; if a routed backend overflows or the model requests a disallowed tool, the agent degrades (truncate/map-reduce) or the router **re-routes/fails over** rather than returning garbage.
- **Per-backend tool scoping (privacy + capability).** Each backend carries an **allow-list** over the global pool (native tools + configured MCP servers); only allowed tools are put in the `tools` array sent to that backend's model. Defaults by kind:
  - **Local** → all tools.
  - **Remote LAN** (e.g. the 3070) → all tools by default (trusted network) — *note:* cImp still **executes** tools on the cImp host, so a local file the model reads travels over the **LAN** to that box; data stays on the user's network.
  - **Remote Cloud** → **web/docs only by default** (`duckduckgo`, `fetch`, `context7`); **`read_file`/`code_search`/`run_command`/`filesystem`/`git` are denied** so local file contents / command output never leave the machine. The user can opt a cloud backend into local-data tools **explicitly, with a warning**.
- **Cloud = data leaves the machine.** A cloud backend is gated behind an **explicit consent toggle**, labeled distinctly from LAN-remote in the UI, and documented in README/`NOTICE`. It still protects the *Opus* context, but breaks the local/offline property. The LAN 3070 case keeps data on the user's network.
- **Capability reporting becomes a union** across enabled backends, kept coarse: e.g. *"Backends: large (150k ctx, all tools) + fast (16k ctx, web/docs only). Pass `tier` to bias; local-file tasks run on the large backend."* So Opus knows a fast tier exists and that it can't read local files. Re-rendered on `tools/list` and on `notifications/tools/list_changed` (now also fired on backend health/membership change), per V8-01's mechanism.
- **Tabs stay local-only.** Each *local* backend keeps its read-only Offload Server tab; remote backends have no PTY to render and appear only as Settings status lines.

## What This Milestone Delivers

**Phase A — Backend abstraction + Remote impl + schema migration**

1. `offload/backend.rs` — `Backend` over `backends: Vec<OffloadBackend>`. V8-01's `LlamaServer` becomes the **Local** impl (command + tab + lifecycle). A **Remote** impl health-polls (`/health`) + discovers (`/props`, or uses `declared_*`) + connects over HTTP; no process ownership, no tab. Each exposes `base_url()`, `is_ready()`, `n_ctx()`, `slots()`, `in_flight()`, `allowed_tools()`, `tier()`.
2. Schema migration (additive): `OffloadSettings.server_command` + `autostart` → `backends: Vec<OffloadBackend>` with one `Local` entry; old files migrate in the cascade (mirrors prior additive migrations). `OffloadBackend { name, enabled, kind: Local{ server_command, autostart } | Remote{ base_url, auth_token, is_cloud }, declared_context, declared_model, tier_label, tool_scope }`.

**Phase B — The router**

3. `offload/router.rs` — `select(task) -> &Backend`: filter by **allowed-tools ⊇ required-tools** (heuristic required-tools from the instruction + a capability fallback), then by **per-slot budget ≥ estimated context**, then by **tier/complexity** and Claude's `tier` hint, then **availability** (spill on full, fail over on down). One enabled backend short-circuits. Emits a routing-decision log line (surfaced in Settings/status: "task → fast backend").
4. Agent-loop hook: the loop asks the router for a backend, runs entirely against it, and on a hard mismatch mid-run (overflow, or model requests a disallowed tool) either degrades or asks the router to **re-route** the task once.

**Phase C — Per-backend tool scoping**

5. `tool_scope` per backend (`All | Only(names) | AllExcept(names)` over native-tool + MCP-server names). The MCP host + native dispatch **filter the `tools` array per backend** at loop start. Defaults: Local = All; LAN = All; Cloud = web/docs only. Cloud opt-in to local-data tools requires the explicit warning acknowledgement.
6. Routing consumes `tool_scope` as the hard filter (Phase B step 3).

**Phase D — Capability union + `tier` + description**

7. Extend the capability registry to a **union across enabled backends** with per-backend coarse labels (ctx, tier, tool-scope summary). Render the `offload_task` description from it; add **`tier: "auto"|"fast"|"quality"`** to the tool schema; fire `tools/list_changed` on backend health/membership changes too.

**Phase E — Settings surface**

8. A **backends editor**: add/remove/enable; **Local** = command + autostart + tab (as V8-01); **Remote** = `base_url` + auth + `is_cloud` flag + declared context/model/tier-label; per-backend **tool-scope** picker; per-backend status (health, `n_ctx`, `in-flight/np`, last-routed count). Cloud backends show the **data-leaves-the-machine consent** toggle and a distinct badge.

**Phase F — Docs**

9. `DESIGN.md` (pool + router diagram), `README.md` (adding a LAN/cloud backend, routing behavior, the cloud privacy note), `MAINTENANCE.md` (example remote configs), `NOTICE`/`CHANGELOG` (cloud data-egress disclosure; Added: remote backends + routing).

## What This Milestone Does NOT Do

- **Per-step / mid-loop backend switching.** One backend per `offload_task`; a task's whole loop runs there. (A single overflow-triggered re-route restarts the task, it doesn't split it.)
- **Be a general LLM gateway.** The router is purpose-built for offload (context/complexity/tool-scope/availability), not a configurable LiteLLM-style proxy with arbitrary policies, rate-limit accounting, or cost optimization.
- **Deeply validate every provider.** `llama-server` (local or remote LAN) is the validated path; cloud / Ollama / LM Studio / vLLM / hosted APIs work via `base_url` + declared capabilities, best-effort (per-provider tool-calling / `/props` / thinking-flag quirks are a followup).
- **Cross-machine tool execution.** Tools always execute on the **cImp host**; results travel to whichever backend ran the task. (A remote box running its *own* MCP servers is a followup.)
- **GGUF auto-download or write/edit tools** — unchanged from V8-01 (still out).

## Test Plan

- **Phase A** — Unit: schema migration turns a V8-01 `server_command` file into one `Local` backend; Remote impl health-polls and uses `declared_context` when `/props` is absent. Manual: add a LAN llama-server by URL → it reaches `ready` with discovered `n_ctx`; no tab is created for it.
- **Phase B** — Unit: router picks the small backend for a tiny single-pass task and the large one for a 100k-context task; honors `tier="quality"`/`"fast"`; spills to a second eligible backend when the first is at `in-flight==np`; fails over when the preferred is down; single-backend → no-op. Manual: two offloads, one trivial one heavy → trivial lands on the 3070, heavy on the main PC; Settings shows where each went.
- **Phase C** — Unit: a cloud backend's `tools` array excludes `read_file`/`filesystem`/`git`/`run_command` by default; a task that requires local files is **not routed** to a cloud backend; opting a cloud backend into local tools requires the consent flag. Manual: route a "read local file" task with only a cloud backend enabled → it refuses/falls back with a clear reason, never silently sends the file to cloud.
- **Phase D** — Unit: capability description reflects the union and updates on a backend going down/up (`tools/list_changed`). Manual: Opus sees both tiers in the description and uses `tier` appropriately.
- **Phase E/F** — Manual: backends persist across restart; cloud consent is required before a cloud backend is usable; redacted `Debug` hides `auth_token`. Build: `cargo build` + `npm run build` succeed.

## Files Most Likely Touched

- `offload/backend.rs` (new — Local | Remote impls), `offload/router.rs` (new)
- `offload/mcp_host.rs`, `offload/agent.rs` — per-backend tool filtering at loop start; router hook + single re-route
- `offload/mcp.rs` — `tier` arg on `offload_task`; union capability description; `tools/list_changed` on backend changes
- `settings/schema.rs` — `backends: Vec<OffloadBackend>` (+ `tool_scope`, `is_cloud`, `declared_*`); migration from `server_command`/`autostart`; redacted `Debug` for `auth_token`
- `ipc/commands.rs` — per-backend status (health/n_ctx/in-flight/last-routed); backend add/remove/start/stop/restart
- `SettingsApp.svelte`, `lib/offload.ts`, `lib/settings/{types,store}.ts` — backends editor (Local/Remote/cloud-consent + tool-scope picker), routing/status display
- Docs: `DESIGN.md`, `README.md`, `MAINTENANCE.md`, `NOTICE`, `CHANGELOG.md`

## Risks and Open Questions

- **Routing correctness + the small backend's limits.** Mis-routing is the main failure mode: a big-context or multi-tool task on the 8 GB/16k 3070 truncates or tool-calls badly. The router must be conservative — small/weak backend only when context fits *and* the task is single-pass/simple (or `tier="fast"`). Required-context/tool estimation is approximate, so the agent must still degrade gracefully or re-route on overflow. Keep routing legible and surfaced (Settings shows where each task went).
- **Per-backend tool scoping must be airtight for cloud.** The whole privacy guarantee rests on the cloud `tools` array genuinely excluding local-data tools *and* the router never sending a local-data task to a cloud backend. Test both the tool-array filter and the routing filter; default-deny, opt-in-with-warning.
- **Remote reliability + latency.** A LAN/cloud backend can be slow, flaky, or mid-restart. Health-poll before routing, fail over to local when down, count network time against `offload_timeout_secs`. Remote capability discovery is weaker — a cloud endpoint may mis-declare its window; trust-but-verify against `usage.prompt_tokens` and clamp.
- **Cloud data egress.** Even with scoping, the *instructions* and any `context` Opus passes go to the cloud model. The consent toggle covers this; document that offloading to cloud sends the task text (not just tool results).
- **Schema migration.** `server_command`→`backends` must be a clean additive migration that preserves the user's V8-01 command as the first Local backend; round-trip an old file in tests.
- **Global concurrency across backends.** Per-backend `np` gates are clean, but a global "how many offloads in flight total" view (and the warm-pool shared semaphore from V8-01's open decision) matters more here — the warm-pool target design is the natural home for the pool + router.

## Followups (FUTURE-FEATURES candidates)

- **Per-provider hardening** of non-llama.cpp endpoints (Ollama / LM Studio / vLLM / hosted APIs) — provider-specific tool-calling / thinking-flag / `/props` quirks.
- **Finer routing** — per-tool / per-capability backend selection (a coder backend for code, an instruct backend for research) and cost/latency-aware policies.
- **Remote-host-local MCP servers** — let a remote box run its own MCP servers (tools executing near that model) rather than all tools executing on the cImp host.
- **Multimodal routing** — route image/PDF offloads to a vision-capable backend.

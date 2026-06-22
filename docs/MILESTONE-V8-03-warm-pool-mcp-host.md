# Milestone V8-03: Warm Pool + MCP Host (the long-lived offload service)

> **Release tag:** TBD at ship time (placeholder label). Direct sequel to **[MILESTONE-V8-02](MILESTONE-V8-02-remote-backends-routing.md)** (backend pool + routing) and the completion of the deferred Phase C of **[MILESTONE-V8-01](MILESTONE-V8-01-local-offload.md)** (the MCP host). **Additive** — V8-01/V8-02 were built behind the `Backend`/router seams so the loop's *home* can move without re-architecting routing, scoping, the capability union, or the schema.
>
> **No separate spike.** As with V8-01/V8-02, validation is taken **inline during implementation** (no Phase-0 gate). The one genuinely new surface — the loopback IPC + its auth — is the thing to get right early (see Risks).

## Purpose

Promote the offload machinery from the **per-call `--offload-mcp` child** (the V8-01/V8-02 MVP) to a **warm, long-lived service inside the Tauri app**, and finally build the **MCP host** (V8-01's Phase C, never implemented) so the offload worker can reach the user's tool servers (`duckduckgo`, `fetch`, `context7`, `git`, `filesystem`) — kept warm across calls.

This closes three concrete limitations the MVP architecture forced, all of which bite harder now that V8-02 added a *pool*:

1. **Global concurrency is impossible from the child.** Each `--offload-mcp` child is a fresh process blind to every other offload (other Claude tabs, other backends), so it reports `in_flight == 0` — which is exactly why V8-02's **spill-on-busy never fires in production** (the router sees every backend as free). Only the long-lived app sees all in-flight offloads and can enforce a real global gate and feed honest `in_flight` to the router.
2. **Capabilities can't be health-accurate from the child.** The child only knows *configured* backends/servers before its first call, so the `offload_task` description and `notifications/tools/list_changed` reflect config, not live health. The app knows which backends and tool servers are actually up *now*, and can push `tools/list_changed` when one crashes or recovers.
3. **MCP tool servers cold-start on every task.** `npx`/`uvx` servers are slow to spawn; a per-call child pays that tax per offload. The app keeps a **warm connection pool**. (This was the original motivation for the target design in V8-01, and is moot until the MCP host exists — which is why the host lands *in this milestone*.)

The `--offload-mcp` child does not go away: it stays as the **MCP-stdio bridge toward Claude** (Claude Code only speaks MCP to a subprocess) and as a **self-contained fallback** when the app isn't running. It shrinks to a thin proxy.

## Relation to V8-01 / V8-02 (what's reused vs. new)

**Reused unchanged:** the `Backend` trait + `LlamaServer`/`RemoteBackend` impls (`offload/server.rs`, `offload/remote.rs`), the router (`offload/router.rs` — a pure function over `BackendView`, so it runs app-side untouched), the agent loop (`offload/agent.rs`), the OpenAI wire types, per-backend `ToolScope` + the cloud-privacy filters, the `backends` schema, the capability-union renderer, and the `--mcp-config` injection seam (`tabs/config.rs`). The supervisor already holds the live local-backend state + `in_flight`.

**New in V8-03:**
- `offload/service.rs` — the **app-owned offload service**: resolves the pool from *live* supervisor/host state, owns a **global concurrency semaphore**, routes (real `in_flight`), and runs the loop. This is V8-02's `mcp.rs::run_offload` relocated into the app with live state instead of per-call config probing.
- `offload/loopback.rs` — an **authenticated loopback endpoint** (127.0.0.1, ephemeral port + token) the child proxies to, plus a session-scoped **discovery file** (`{port, token}`) under the portable root (mirrors the [[project_statusline_context_bar]] "never seed `~/.claude`" discipline).
- `offload/mcp_host.rs` — the **MCP client/host** (V8-01 Phase C): warm connections to the configured tool servers, `tools/list` aggregation + namespacing, read-class filter, `filesystem` confinement, per-server health + isolation, and the live **capability registry**.
- `offload/mcp.rs` becomes a **proxy + fallback**: `tools/list` → app `GET /describe`; `tools/call` → app `POST /run`; subscribe to `GET /events` to emit `notifications/tools/list_changed`; **degrade to the self-contained V8-02 path** when the app is unreachable.

## Decisions locked (from discussion)

- **The app is the single owner of the warm pool + the global semaphore.** The agent loop, the backend pool, the router, and the MCP-host connections all live in the long-lived Tauri app. `llama-server` (and any local backend) is the app-owned process in this layout, exactly as before.
- **The child is a proxy *with* a self-contained fallback — not a hard dependency.** When the app's discovery file is present and the endpoint answers, the child forwards everything. When it's absent/unreachable (app not running, headless cron, mid-restart), the child falls back to V8-02's self-contained path (read settings → probe → route → run) so **offload still works without the app**, just without the warm pool, global concurrency, or live health. The two paths call the **same core** (`run_offload` factored so the loop body is shared) so they can't drift.
- **Loopback-only + token auth, via a discovery file.** The endpoint binds `127.0.0.1` on an **ephemeral port**, requires a **per-launch bearer token**, and advertises `{port, token}` in a discovery file written to the **portable root** (not `~/.claude`), created when offload is enabled and **removed on exit**. The token rotates every app launch. File permissions are tightened where the OS allows. This protects against another local process driving offloads or reading task text in flight.
- **`tools/list_changed` is pushed app → child → Claude.** The app detects a backend/tool-server health or membership change, emits an event on `GET /events` (SSE/long-poll); the child — which holds the stdio pipe to Claude — relays it as `notifications/tools/list_changed`. The child therefore serves stdio **and** subscribes to `/events` concurrently (two tokio tasks). The injected system-prompt addendum stays static; only the live tool *description* updates (unchanged contract from V8-01).
- **The MCP host is read-class only, confined, and isolated.** Only read/query tools are exposed (write/edit/destructive tools from any server — `filesystem` write, `git` commit — are filtered out); `filesystem` is confined to `allowed_roots`; each server is spawned with a timeout and isolated so one hung/crashed server can't wedge the whole offload. Per-server health surfaces in Settings. Tools are namespaced `<server>__<tool>` and **filtered by the chosen backend's `ToolScope`** (V8-02 already scopes by MCP-server name — cloud backends never see a local-data MCP server).
- **Global gate + per-backend gates coexist.** Each backend keeps its `np`-sized gate (V8-01); the app adds a single **global semaphore** ("how many offloads in flight total") so a busy pool queues coherently and the router's spill/fail-over decisions run on **honest** `in_flight` across all sessions.
- **Capabilities are health-accurate.** The registry reflects backends + native toggles + **healthy** MCP servers (a configured-but-down server is not advertised). `GET /describe` renders from live health; `/events` fires on any change.

## What This Milestone Delivers

**Phase A — App-side offload service (move the loop + pool + router into the app)**

1. `offload/service.rs` — `OffloadService`, app-owned (held in `AppState` beside the supervisor). Exposes `run(instructions, context, thinking, tier) -> AppResult<String>` and `describe() -> String`. It builds `BackendView`s from **live** supervisor state (real `in_flight`, discovered `n_ctx`), runs `router::select`, and executes the agent loop against the chosen backend with its scope/auth/budget — i.e. V8-02's `mcp.rs::run_offload` relocated, now reading live state instead of re-probing per call. Factor the shared loop body so the child's fallback reuses it.
2. **Global concurrency semaphore** sized from config (default: sum of per-backend `np`, capped). `run` acquires a global permit *and* the chosen backend's slot; honest `in_flight` per backend feeds the router so **spill-on-busy and fail-over finally work in production**.

**Phase B — Loopback proxy channel**

3. `offload/loopback.rs` — a minimal authenticated loopback HTTP service (reuse `reqwest`/the existing async runtime; a tiny `hyper`/`axum`-free hand-roll or the lightest available server). Endpoints (all require the bearer token): `POST /run` (`offload_task`), `GET /describe` (capability description), `GET /events` (SSE for `tools/list_changed`). Binds `127.0.0.1:0`; writes `{port, token, pid}` to a session-scoped discovery file under the portable root when `offload.enabled`; removes it on exit.
4. `offload/mcp.rs` becomes the **proxy + fallback**: read the discovery file; if present and healthy, forward `tools/call`→`POST /run`, render `tools/list`←`GET /describe`, and spawn a task subscribing to `/events` to emit `notifications/tools/list_changed`; **else** run the self-contained V8-02 path. One shared `run_offload` core behind both.

**Phase C — MCP host (warm tool servers) — completes V8-01 Phase C**

5. `offload/mcp_host.rs` — an MCP **client** owned by `OffloadService`. Per `offload.mcp_servers`: spawn/connect (stdio `command`+`args`+`env`, or HTTP `url`), run `initialize` + `tools/list`, **namespace** tools (`ddg__search`, `git__log`), **read-class filter**, confine `filesystem` to `allowed_roots`. Connections are **kept warm** across calls. At loop time, merge native + MCP tools into the `tools` array (then filter by the backend's `ToolScope`) and route each `tool_call` to the owning server's `tools/call`. Time out + isolate a misbehaving server; track per-server health.
6. Capability registry: native toggles + **healthy** MCP servers + enabled backends → coarse capability labels, consumed by `describe()`.

**Phase D — Live capabilities + `tools/list_changed`**

7. On any backend health/membership change (a server crashes/recovers, a backend goes down/up, a toggle flips), the service emits a capabilities-changed event on `/events`; the child relays `notifications/tools/list_changed`. `describe()` always renders from **live** health.

**Phase E — Concurrency + status surfacing (Settings)**

8. Settings shows a **global "offloads in flight"** readout and now-honest **per-backend `in-flight / np`**; **per-MCP-server health** rows (connected/healthy/down, tool count); and a warm-pool indicator. The per-backend editor from V8-02 is unchanged.

**Phase F — Docs**

9. `DESIGN.md` (the *target* architecture becomes the real one; warm pool + MCP host + loopback), `README.md` (configuring MCP tool servers; warm pool note), `MAINTENANCE.md` (discovery-file location + loopback security model; example `mcp_servers` configs), `CHANGELOG.md` (Added: warm offload pool + MCP host; Fixed: cross-backend spill now works).

## What This Milestone Does NOT Do

- **Remove the per-session child.** Claude Code only speaks MCP to a subprocess, so `--offload-mcp` stays — as the stdio bridge and the headless fallback. It just stops doing the heavy lifting when the app is up.
- **Cross-machine tool execution.** Tools still execute on the **ccImp host**; results travel to whichever backend ran the task. A remote box running its *own* MCP servers stays a followup.
- **Write/edit tools.** The offload worker stays read-only (search/read/web/docs/git-read/allowlisted-run). Write/destructive tools are filtered out even when a server offers them.
- **Per-provider hardening** of non-llama.cpp endpoints (Ollama / LM Studio / vLLM / hosted APIs) — still best-effort, a followup.
- **Streaming partial results** into Opus — one final string per call, unchanged.
- **A general LLM gateway / arbitrary IPC API.** The loopback endpoint is purpose-built for offload (`run`/`describe`/`events`), token-gated and loopback-only — not a public local API.

## Test Plan

- **Phase A** — Unit: `OffloadService::run` routes against live `in_flight` (a backend at `in_flight == np` is spilled; with all busy the global gate queues then times out at `offload_timeout_secs`); the shared loop core produces identical results whether driven by the service or the fallback. Manual: fire two offloads with one slot → one runs, one queues, and the **in-flight count is honest across two Claude tabs**.
- **Phase B** — Unit: the discovery file round-trips `{port, token}`; a request without the token is rejected; the child uses the proxy when the file is present and **falls back to self-contained** when it's absent/unreachable. Manual: run an offload with the app up (served by the warm service) and with the app down (served by the fallback child) → both return a coherent answer; the discovery file is gone after the app exits.
- **Phase C** — Unit: the host namespaces tools (`ddg__search`), drops write/destructive tools, confines `filesystem` to `allowed_roots`, and a hung server times out without wedging the loop; cloud-backend scope excludes a local-data MCP server. Manual: with `duckduckgo`+`fetch` configured, an offload does a real web search+fetch via the **warm** connections (no per-call cold-start); killing a server mid-session drops it from the capability description.
- **Phase D** — Unit: a backend/server going down/up fires a capabilities-changed event and the child emits `tools/list_changed`; `describe()` reflects **health**, not config. Manual: `/mcp` in a Claude tab shows the live capabilities; disabling a server mid-session updates the description promptly.
- **Phase E/F** — Manual: Settings shows global in-flight, honest per-backend in-flight/np, and per-MCP-server health; the loopback endpoint is unreachable from a non-loopback address; the token never appears in logs. Build: `cargo build` + `npm run build` succeed.

## Files Most Likely Touched

- `offload/service.rs` (new — app-owned loop + pool + global semaphore), `offload/loopback.rs` (new — authenticated endpoint + discovery file), `offload/mcp_host.rs` (new — warm MCP client pool)
- `offload/mcp.rs` — becomes proxy + self-contained fallback over one shared `run_offload` core
- `offload/supervisor.rs` — expose live pool state to the service; global-semaphore plumbing; per-MCP-server health for status
- `offload/agent.rs` — host-aware `ToolRouter` (native + MCP tools, scoped) replacing the native-only path
- `main.rs` — construct `OffloadService` + start the loopback endpoint when `offload.enabled`; write/remove the discovery file; stop on `CloseRequested`
- `ipc/commands.rs` — global-in-flight + per-MCP-server-health in the status payload
- `settings/schema.rs` — likely no new fields (reuse `mcp_servers`, `backends`); maybe a global-concurrency cap (additive)
- `SettingsApp.svelte`, `lib/offload.ts` — global in-flight readout + per-MCP-server health rows
- Docs: `DESIGN.md`, `README.md`, `MAINTENANCE.md`, `CHANGELOG.md`
- `Cargo.toml` — possibly a lightweight loopback server dep if hand-rolling on `tokio` stdio/`reqwest` proves fiddly (justify before adding)

## Risks and Open Questions

- **Loopback auth is the security boundary.** The endpoint must bind loopback-only, require a per-launch token, and keep the discovery file readable only by the user (tighten perms where the OS allows). A malicious local process that reads the file could drive offloads or observe task text — mitigate with ephemeral tokens, loopback bind, file perms; document the residual local-trust assumption (same threat model as any localhost dev server).
- **Two code paths (proxy vs. fallback) must not drift.** Factor the loop body into one shared core both call; test that they produce identical results. The fallback is the headless/cron path, so it has to stay first-class.
- **`tools/list_changed` plumbing.** The child must serve stdio *and* subscribe to `/events` concurrently without deadlock, and Claude Code must actually honor `list_changed` mid-session (a standing V8-01 assumption — verify promptness). If the client ignores it, the description is still accurate at each `tools/list`.
- **MCP host is an external surface.** `npx`/`uvx` servers hang, crash, and drift in version; warm connections mean a leaked/zombied child outlives a call. The host must time out, isolate, restart, and surface per-server health; the app must reap them on exit (same orphan caveat as `llama-server`).
- **Lifecycle races.** App restart while a child is mid-call; a stale discovery file (hard-killed app) the child trusts and then fails to reach → it must detect quickly and fall back. The discovery file should carry the `pid` so a child can sanity-check liveness.
- **Warm-pool footprint.** Several persistent MCP-server processes + connections add memory/process weight even when idle. Consider lazy-connect (spin up a server on first use, idle-evict after N minutes) as a refinement.
- **Global vs. per-slot budget interaction.** The global semaphore caps total in-flight; per-backend `np` still divides each backend's window. Make sure the router's spill doesn't oversubscribe a backend's slots once `in_flight` is honest.

## Followups (FUTURE-FEATURES candidates)

- **Remote-host-local MCP servers** — let a remote backend run its own MCP servers (tools executing near that model) instead of all tools executing on the ccImp host.
- **Per-provider hardening** of non-llama.cpp endpoints (tool-calling / thinking-flag / `/props` quirks).
- **Idle-evicting warm pool** — lazy-connect MCP servers on first use and drop them after an idle window to cut the resident footprint.
- **Streaming offload progress** surfaced in ccImp's UI (now feasible — the loop runs in the app, so a transient panel can show the worker's tool round-trips live).
- **Write/edit offload tools** behind an explicit safety design (diff preview, confirmation).

# Architecture Notes — per feature / milestone

How the non-obvious parts of cImp actually work: the design decisions, the
invariants, and the "why it is built this way" for each milestone's feature
area. These notes were carved out of `MAINTENANCE.md` so that document can stay
what its name says — *what to check during a maintenance run*.

The split:

- **This file** — how-it-works narrative, per milestone. Read it before changing
  code in one of these areas, or to understand a maintenance finding.
- **`MAINTENANCE.md`** — the maintenance-run procedure and run log, the
  dependency & component inventory, the deep-dive "what to check on bump" notes
  (`ort`, `whisper-rs`, the Claude Code / OpenCode CLI contracts), the
  per-feature residual limitations to periodically re-check, the open-spike
  table, and the hand-run live-verify recipes.

Where a section here has a maintenance counterpart, the counterpart says
"Architecture: see ARCHITECTURE.md § …".

Not to be confused with **`DESIGN.md`**, which is the whole-app architecture
reference ("how cImp is"). This file is narrower and deeper: the per-milestone
mechanics, invariants and review-pass findings that used to be interleaved with
the maintenance checklists.

## Contents

| Milestone | Section |
|---|---|
| V8 | [Offload — backends, warm pool, loopback & MCP host](#offload--backends-warm-pool-loopback--mcp-host-v8) |
| V9-02 | [Code graph grammars & tags queries](#code-graph-grammars--tags-queries-v9-02) |
| V10 | [Code Intelligence — Context Engine](#code-intelligence--context-engine-v10) |
| V11 | [Code Intelligence — Token Efficiency](#code-intelligence--token-efficiency-v11) |
| V17 | [Code Intelligence — Token Efficiency II](#code-intelligence--token-efficiency-ii-v17) |
| V12 | [Code Intelligence — Agentic Inner Loop](#code-intelligence--agentic-inner-loop-v12) |
| V22 | [Code Intelligence — run_check Generalization](#code-intelligence--run_check-generalization-v22) |
| V23 | [Code Audit — Aggregated Security Scanning](#code-audit--aggregated-security-scanning-v23) |
| V25 | [Code Quality — Language-Gated Linters](#code-quality--language-gated-linters-v25) |
| V13 | [Workbench — Vibe-Coding Guardrails](#workbench--vibe-coding-guardrails-v13) |
| V14 | [Workflow & Visibility](#workflow--visibility-v14) |
| V15 | [Code Graph Parity](#code-graph-parity-v15) |

---

## Offload — backends, warm pool, loopback & MCP host (V8)

### Backend pool configuration (V8-01 / V8-02)

The offload pool lives under `offload.backends` in `settings.json`. A V8-01
single-server config (`offload.server_command` + `offload.autostart`) migrates
to one Local backend automatically (v1.16 → v1.17); the legacy scalar fields
still work as a fallback when `backends` is empty.

Example pool — a big local model, a small LAN box, and an optional cloud API:

```jsonc
"offload": {
  "enabled": true,
  "backends": [
    {
      "name": "main",
      "enabled": true,
      "tier": "quality",
      "tool_scope": { "mode": "all" },
      "kind": {
        "type": "local",
        "server_command": "llama-server --model C:\\models\\Qwen3.6-35B-A3B-Q4.gguf --port 8080 --jinja -ngl 99 --ctx-size 150000 --flash-attn",
        "autostart": true
      }
    },
    {
      "name": "lan-3070",
      "enabled": true,
      "tier": "fast",
      "tool_scope": { "mode": "all" },          // trusted LAN → all tools
      "kind": {
        "type": "remote",
        "base_url": "http://192.168.1.50:8080",  // a llama-server on the LAN box
        "auth_token": "",
        "is_cloud": false,
        "cloud_consent": false
      }
    },
    {
      "name": "cloud",
      "enabled": false,                          // off until you opt in
      "tier": "quality",
      "declared_context": 128000,                // cloud APIs rarely expose /props
      "declared_model": "some-cloud-model",
      "tool_scope": { "mode": "allexcept", "tools": ["read_file","code_search","run_command","filesystem","git"] },
      "kind": {
        "type": "remote",
        "base_url": "https://api.example.com/v1",
        "auth_token": "sk-...",                  // redacted in Debug logs
        "is_cloud": true,
        "cloud_consent": true                    // REQUIRED for a cloud backend to be usable
      }
    }
  ]
}
```

How the pieces behave:

- **Routing** is `offload/router.rs::select` — a pure function over `BackendView`
  snapshots (readiness → tool-need → context budget → tier/availability). The
  unit tests there encode the expected behavior; update them when changing the
  selection order.
- **Remote capabilities**: a remote `llama-server` exposes `n_ctx` via `/props`;
  cloud APIs usually don't, so they rely on `declared_context`. The probe treats
  any HTTP response from a cloud `/health` as "reachable" (cloud endpoints often
  lack `/health`); a LAN llama-server must answer `/health` 2xx.
- **Cloud privacy** rests on two independent checks: the router never routes a
  local-data task to a cloud backend (`required_tools ⊄ allowed`), and the agent
  loop's `NativeRouter` filters the `tools` array by scope *and* refuses a
  disallowed call. Keep both — they're tested in `router.rs` and `agent.rs`.

### Warm pool, loopback endpoint & MCP host (V8-03)

When the app is up, the offload service (`offload/service.rs::OffloadService`,
held in `AppState`) is the single owner of the warm pool, the global concurrency
gate, and the MCP host. The per-session `cimp --offload-mcp` child forwards to it
over a small authenticated loopback HTTP endpoint.

**Warm pool vs. fallback child.** When the app is running it owns the loop +
pool + router + global gate + MCP host (`offload/service.rs`), and the
`cimp --offload-mcp` child is a thin proxy to it. Only the app sees all
in-flight offloads, so cross-backend spill/fail-over works there. The child
still carries the **self-contained fallback** (the V8-02 path) for when the app
is down — keep it first-class (headless one-shot and cron invocations of a
harness CLI depend on it), and keep the shared `router`/`agent` code shared so
the two paths can't drift.
The child probes the loopback endpoint per request and **falls back** to that
self-contained path on any transport failure (stale discovery file from a
hard-killed app, app mid-restart, app not running). *(This is the single
explanation of the fallback child; `MAINTENANCE.md` § Offload (V8) points here.)*

**Loopback endpoint + discovery file (`offload/loopback.rs`).**

- Binds `127.0.0.1:0` (ephemeral port) and requires a **per-launch bearer token**.
  Routes: `POST /run`, `GET /describe`, `GET /events` (SSE). Purpose-built for
  offload — not a general local API.
- Advertises `{port, token, pid, root}` in **two** places under the portable
  root (next to `settings.json` — *never* `~/.claude`), both written at start and
  **removed on graceful exit**: the per-instance file
  **`<exe-dir>/.cimp-discovery/<pid>.json`**, which is the authoritative one, and
  the legacy single file **`<exe-dir>/.cimp-offload.json`**, kept in step
  (last-writer-wins) for anything that only knows that path. Readers resolve
  root-aware through `read_discovery_for(hint)` → `select_discovery`: the
  **deepest** per-instance `root` that is an ancestor of the hint wins (nested
  checkouts resolve to the closest instance, same-root duplicates tie-break on
  pid); with no hint and no match, a sole surviving entry is taken, and only then
  does it fall back to the legacy file. **Both paths still resolve — a reader
  that models only one of them is wrong** (this is what made review finding
  F-26's one-file repro fail). Hard-killed instances leave their `<pid>.json`
  behind; `sweep_stale_discoveries` probes and deletes them at the next instance
  start. The token rotates every launch; on Unix the files are `chmod 600`
  (best-effort; Windows ACL tightening is a TODO).
- **Security model / residual risk:** loopback-only bind + token auth keep another
  local process from driving offloads or reading task text *in flight*. A
  malicious local process that can read the discovery file could still do both —
  the same trust assumption as any localhost dev server. Mitigations: ephemeral
  token, loopback bind, file perms. Don't log the token; don't widen the bind off
  loopback.

**MCP host (`offload/mcp_host.rs`).** Warm client pool over `offload.mcp_servers`
(the conventional `mcpServers` object shape). Per server: `initialize`+`tools/list`,
namespacing `<server>__<tool>`, a **read-class filter** (leading-verb heuristic on
the first two name segments — see `is_read_class`/`WRITE_VERBS`, unit-tested), and
`filesystem` confinement (the configured `allowed_roots` are appended as the
server's allowed dirs). stdio is fully warm (a reader task demuxes JSON-RPC by id);
HTTP `url` servers are best-effort single-POST. A crashed/hung server is isolated
(its tools drop from the capability set) and surfaces in *Settings → Offload → MCP
tool servers*. Example config is in `README.md`.

**Live capabilities / `tools/list_changed`.** `OffloadService` exposes a change
channel fed by (a) MCP-host connect/drop pulses and (b) a periodic health watch
that compares the ready-backend set. The loopback `/events` stream relays each as
a `change` event; the child (which holds the stdio pipe to Claude) emits
`notifications/tools/list_changed`. `describe()` always renders from live health.

**Global concurrency.** `offload.global_concurrency` (optional) caps total
offloads in flight; `null` auto-sizes from the summed per-backend slot counts,
clamped to 32. The gate is created at app launch — changing the cap needs a
relaunch.

---

## Code graph grammars & tags queries (V9-02)

The code graph extracts symbols/calls via a generic tree-sitter `tags.scm`
engine (`src-tauri/src/graph/tags.rs`). **To add a language:**

1. Add its `tree-sitter-<lang>` grammar crate to `src-tauri/Cargo.toml` (must be
   ABI-compatible with `tree-sitter = 0.26` — exposes a `LANGUAGE` `LanguageFn`;
   check with `cargo add <crate> --dry-run` + a build).
2. Add a `Lang` variant + file extensions in `graph/model.rs` (`from_path`,
   `tag`, `from_tag`) and a `language_for` arm in `graph/builder.rs`.
3. For a **code** language: vendor a query at `src-tauri/queries/<lang>/tags.scm`
   (prefer the grammar's upstream `queries/tags.scm`; trim over-broad
   `@reference.call` patterns that rely on `#is-not?`/`#not-match?` predicates —
   the base `Query` engine doesn't enforce them). Add an `include_str!` arm to
   `tag_spec` and route the variant through the engine in `parse_file`. For a
   **markup/data** language, skip the query — registering it in `language_for`
   already enables `graph_struct_search`.
4. Add the tag to the default `languages` list (`settings/schema/graph.rs` +
   `lib/settings/types.ts`) if it should index by default, and to the Settings
   "Supported" hint in `SettingsApp.svelte`.
5. Add a fixture test in `graph/tags.rs`; the `every_vendored_query_compiles`
   test already guards that every vendored query compiles against its grammar
   (catches a node/field name that drifted in a grammar update).

The engine derives containment and caller attribution purely from byte spans, so
a `tags.scm` whose `@definition.<kind>` capture sits on the actual construct node
(not an enclosing scope) works with no engine changes. Capture suffixes map to
`SymbolKind` in `kind_from_suffix`.

---

## Code Intelligence — Context Engine (V10)

The "Code Graph" tab is renamed **Code Intelligence** (internal tab id
`graph-monitor` and the `graph` settings key are unchanged) and its view
(`src/lib/CodeIntelligenceView.svelte`) routes five sections: Index / Activity /
Memory / Context / Analyses.

Since #130 that file is the ROUTER plus the Memory and Context sections; the other
four live in `src/lib/codeIntel/` (`UsageOverview`, `AnalysesSection`,
`TracePathSection`, `ArchitectureSection`), and the rules more than one of them
needs are in `src/lib/codeIntel/codeIntel.css` — an unscoped sheet keyed on the
`.graph-monitor` class the router puts on its root, so a child picks them up
through the DOM rather than losing them to Svelte style scoping. Each child is
mounted unconditionally and gates its own markup on an `active` prop: the state
behind a section (a selected session, a typed trace, a scan result) has always
outlived a section switch, and an `{#if}` around the component would not let it.

**Schema versioning & migration.** `graph/schema.rs::GRAPH_SCHEMA_VERSION` stamps
the derived-relation shape. On open, `GraphIndex::migrate_schema` compares it
against a `schema_meta` singleton (which is **not** in `RELATIONS`, so it survives
`reset()`); a mismatch drops+recreates the derived relations, and the service's
normal rebuild repopulates them from source. This runs once, transparently, on
the first launch after an upgrade — bump `GRAPH_SCHEMA_VERSION` whenever a
`RELATIONS` column changes.

**Memory relations are rebuild-safe.** `session` / `mem_event` / `mem_note` are
ensured by `ensure_memory_relations` **outside** `RELATIONS`, because a full
index rebuild calls `reset()` (drops every `RELATIONS` relation) and memory is
runtime event data, not derived from source — it must survive a rebuild.

**Memory event sources are per-agent.** Claude records in-process via the
transcript tap (`harness/claude/read.rs::record_tool_events`, beside `update_agents`;
session id = the `<id>.jsonl` stem), wired through `OobContext.mem` from
`pty/manager.rs`. OpenCode's OOB SSE stream has no tool events, so its memory
comes from the injection plugin's `tool.execute.after` hook POSTing to
`/memory/event`. Each harness's plugin declares the mapping (the `memory_kind`
column of `harness/<id>/tools.rs`), and core reads it through
`harness::native::memory_kind` with the request's SOURCE — so `Edit` and `edit`
are answered from their own vocabularies and an unidentifiable source records
nothing (V40 Phase A, locked decision 16; it used to be one
`graph::classify_tool` `match` over both).

**Memory-tool session scoping — per-agent, then per-tab (V28).** The
`context_recall` / `context_note` / `context_notes` MCP tools have no session
argument (no harness passes session identity into an MCP server's tool-call
context), so they resolve a session from `graph.db`. The first layer scopes to
the *calling agent*: the MCP child's `--consumer` — a **registered** harness's
consumer token, resolved through `HarnessId::from_consumer` — flows
`offload/mcp.rs::proxy_graph` → `/graph_run` (`GraphRunBody.consumer`) →
`run_graph_tool` → `dispatch_recorded` `source` → `mem_agent(source)` →
`GraphIndex::mem_current_session_for(Some(agent))` (and the app-down fallback
`handle_call(params, consumer)` does the same). A token nobody registered
resolves to `graph::mcp::UNKNOWN_SOURCE` and filters to no sessions; it is never
served a default harness's scope (V40 Phase A, locked decision 2). `source` is
also the activity ring's badge, so each harness's graph/context calls read as
its own registry id — the frontend types that field as a bare `string` since
V40 Phase F (a call from a harness this build has not heard of must render, not
fail to type-check) and colours it from the harness's declared `accent`.

**V28 (issue #13) closes the per-agent layer's gap** — two tabs of the *same*
agent sharing (and stealing) one memory scope. Identity rides the **spawn argv**,
since the `cimp --offload-mcp` child is per-tab and cImp composes its whole
command line: each harness's plugin bakes `--tab <tab-id>` into whatever MCP
server entry it writes at spawn (`HarnessPlugin::pre_args` / `compose_env` /
`write_artifacts`; the per-harness mechanism is in that plugin's README). The
child forwards it as
`GraphRunBody.tab` on `/graph_run` — and, since V32 Phase B, as
`McpCallBody.tab` on `/mcp/call` too: external servers still hold no cImp memory
scope, but the proxy's taint latch is keyed by the same tab identity on both
tool-serving routes. `handle_graph_run` resolves it at call
time through `GraphService::live_session_for_tab(tab, agent)` (the V24
live-session registry: exact key match, agent must match, TTL-filtered, never a
guess — NC-2's resolver discipline), and threads the resulting session id down
`run_graph_tool` → `dispatch_recorded` → `run_tool`, where the `context_*` tools
(and `graph_repo_map`'s session boost) use it instead of the recency lookup.

Everything on that path **fails open to the pre-V28 behavior**: no `--tab` (a
child spawned before the upgrade), an unknown or TTL-stale tab key, or a blank
value all fall back to `mem_current_session_for(agent)`. A tool call never errors
for lack of identity, so there is no restart hint owed — and because the tab id
is not Settings-derived and cannot change while a tab runs, it needs no
`spawn_inject_sig` entry.

**Per-tab identity is pinned at spawn where the harness's CLI allows it (V34,
2026-08-09).** A harness whose CLI can be told which conversation to run under
declares the flags that select one (`HarnessPlugin::session_selector_flags`,
V40 locked decision 26) — that is how cImp tells *the user pinned a session*
from *cImp may pin one*. Where it may, the plugin generates a UUID at spawn and
passes it, so the fallback reader follows exactly that session's artifact
instead of racing for the newest one and the tab's binding is provable no matter
how many tabs share a project. The id is generated beside the `OobSpec` that
carries it (`HarnessPlugin::resolve_oob`), so the argv flag and the artifact the
tap follows cannot drift; `graph_tab_session` exposes tab → session so the Code
Intelligence Overview can follow the focused tab rather than the
most-recently-active session. An empty `session_selector_flags` is a first-class
answer: *this harness selects its session some other way*, and no pin is
attempted.

**A pin is a claim, verified against the artifact's existence.** Passing the
flag does not mean the tab runs under it — that was observed in the field.
Until the pinned artifact exists the tab publishes no identity and behaves
exactly as described next; the tap keeps watching and upgrades if it appears. A
tab whose own args already select a conversation is never pinned at all, and a
new-session command rolls a verified tab's session over and returns it to the
same fallback. The per-harness mechanics — the flag names, the artifact path,
the verification step — are in that plugin's README § *Memory scoping*.

**The unpinned case is not isolatable, and degrades to unscoped rather than
guessing (H1, 2026-08-05 review).** Without a pin, a fallback reader whose
artifact root is derived from the project directory alone has no per-process
discriminator — it follows whichever session under that root wrote last. Two
such tabs of the same harness on the SAME project dir therefore prove nothing
about which session is theirs. Each running tap declares its root
(`GraphService::mark_live_tab_root`, refreshed per poll tick, RAII-cleared on
tab exit), and the single predicate `graph::service::tab_binding_is_ambiguous`
— running tabs only, never merely configured ones, and never a PINNED one —
makes the registry withhold both answers while ≥2 tabs share a root:
`live_session_for_tab` → `None` (⇒ fail-open to the pre-V28 recency lookup,
never a tool error) and `live_sessions_for(harness)` drops the pair, so NC-2's
permission resolver refuses instead of flipping a badge/TTS on the wrong tab.
The pin exemption is deliberately one-sided: it clears the pinned tab only,
since an unpinned neighbour is still the one guessing and may still grab the
pinned tab's artifact. Tabs on different dirs are unaffected, and so is a
harness whose reader is *told* a session id rather than deriving a directory to
watch. The spawn dir feeding both the artifact root and the ingress cwd fallback
has one definition (`tabs::config::ai_working_dir`).

Every harness stamps the registry per tab from its own reader, and **which key
space that stamp lands in is declared, not inferred** (`HarnessPlugin::
session_key_space`, V40 Phase D, locked decision 20): `SessionKey::Tab` for a
harness whose live session is keyed by the cImp tab id, `SessionKey::Session` —
the default, and the fail-closed one — for a harness that hands cImp its own
session id. An id from an undeclared harness therefore lands in the session
space, where it cannot name a cImp tab, which is the C-2 hazard the declaration
removes: `LiveKey` carries the space, so a session id and a tab id are not the
same key even when they are the same string. The loopback `/memory/event` path
keeps its *separate* session-keyed entries (the Usage "live now" badge reads
them), and OpenCode's ids are accepted into the session space **even when they
collide with a tab id** — tab-keyed and session-keyed entries coexist because
the key space is part of the key. Sub-agent sessions are excluded by each reader
from its own stream, so a tab always resolves to its current **main** session;
sub-agent tool calls arrive through the same per-tab child and therefore share
the tab's scope by design. The offload worker (`offload` consumer) has no tab
and keeps its project-wide, agent-`None` scope.

**Context injection** (opt-in, `graph.context_injection`). `graph/context.rs`
ranks files (symbol/reference/doc hits + session working set) and budget-packs
outline digests — synchronous, no per-prompt embedding. The block reaches a
model over **that harness's own extension mechanism**: the plugin writes
whatever artifact its harness loads and composes whatever flags or environment
carry it (`HarnessPlugin::write_artifacts` / `pre_args` / `compose_env`), and it
declares what that mechanism is called as an affordance the window renders
(`HarnessAffordances::inject_mechanism`). Core never names either. Every such
artifact is **spawn-baked**, so a Settings change that alters it moves that
harness's `spawn_sig` and raises the restart hint; the file names, the launch
flags that would disable the mechanism, and the ignore-file bookkeeping live in
that plugin's README.

**New local loopback routes** (`offload/loopback.rs`), same authenticated-
localhost trust model as `/graph_run`: `POST /context/retrieve` (gated on
`context_injection`) and `POST /memory/event` (OpenCode's memory ingress —
its BODY is `harness/opencode/hook.rs` since V40 Phase I; core keeps the route
and the recording and receives a neutral `plugin::MemoryEvent`).

---

## Code Intelligence — Token Efficiency (V11)

**Schema bump to v3 — one rebuild for the whole V11–V14 roadmap.**
`graph/schema.rs::GRAPH_SCHEMA_VERSION` moved 2 → 3 for a single column change:
`symbol.is_test` (provisioned for a later milestone, unused by anything in
V11). That's the *only* `RELATIONS` shape change, so it's the only thing that
forces the migrate-on-open rebuild described in the V10 section above. Every
other new store this milestone adds is **additive, create-if-missing, and
needs no version bump**: `code_chunk` (added to `RELATIONS` directly — the
code-embedding source text) plus `digest` and `code_vec`, both ensured lazily
the first time they're needed (`GraphIndex::put_digest` /
`ensure_code_vector_store`), the same pattern V10 used for `session` /
`mem_event` / `mem_note`. **`injected` (the Phase C dedup state) is *not* a
relation** — it's an in-memory `HashMap<session_id, InjectState>` on
`GraphService` (`graph/service.rs`), so it never survives a restart and needs
no schema entry; a restart just re-injects fresh on the next turn, which is
the intended fail-safe.

**Harness ingress is a plugin seam (V40 Phase C, locked decisions 15 and 22).**
Core's loopback router holds **no harness path literal**. A harness that reaches
cImp over its own ingress registers those routes from its plugin —
`HarnessPlugin::routes()` returns a `&'static [Route]` of `(method, path,
handler)` — and `offload/loopback.rs` matches every CHP-neutral arm **first**,
then falls through to `harness::ingress::route(method, path)`; a path nobody
serves is a `404` either way. The handler answers a `HookReply` (a status and a
body) that core **serializes without reading**, because "this harness answers
hook-output JSON and that one answers `{"ok":true}`" is not something core may
know (`harness/plugin.rs::HookReply`). Four tests hold the seam:
`ingress::tests::no_two_plugins_claim_one_route` (a wire boundary may not depend
on registry order), `no_plugin_route_shadows_a_core_route` (core's `match` wins,
so a shadowing handler would simply never run),
`every_declared_timeout_outlasts_the_budget` and
`every_inverted_wire_default_names_a_route_that_exists`.

*Which* of a harness's own events map onto which of its routes — the matcher
strings, the payload fields, the reply envelope, the version floor — is that
harness's business and lives in its plugin directory. For Claude Code the table
is [`src-tauri/src/harness/claude/README.md` § *Hook routing*](../src-tauri/src/harness/claude/README.md#hook-routing);
the wire contract every route rides on is [`docs/CHP.md`](CHP.md).

The **legacy `/context/*` routes stay**, and they are the harness-neutral CHP
bodies: a tab open across an upgrade is still running the artifact an older
build wrote, so both transports of each capability meet at **one shared core**
— which is what keeps them from drifting into two behaviours. cImp's own retired
dispatch flags survive in `main.rs` as tombstones that drain stdin and exit 0,
so an old artifact is inert rather than launching a second cImp GUI.

**Every ingress route fails open, and the app's budget is derived rather than
hand-set.** A harness declares how long its out-of-process caller waits before
abandoning cImp's reply (`HarnessPlugin::hook_reply_timeout`; `None` = "this
harness never waits", and it does not participate). Core takes
`min(every declared timeout) − harness::ingress::HOOK_REPLY_MARGIN` as the time
it may spend before answering — `ingress::hook_reply_budget()`, 1800 ms with the
two shipped plugins, pinned by
`the_derived_budget_is_the_1800_ms_the_shipped_plugins_imply`. The ordering is
the whole mechanism: the harness starts the tool the instant its own timer
fires, so cImp's answer has to land first or the app is staging into a call it
believes it gated. A refused connection, a timeout and any non-2xx are
non-blocking on every one of these routes.

**Every ingress request carries its cImp tab** (#48, finding M-7). A harness's
own hook payload names its session and its working directory and nothing that
identifies a cImp tab, so the identity travels **outside the body**:
`HarnessPlugin::identity_of_request(route, req)` answers the `(chp, agent, tab)`
triple for the routes its harness owns, and `None` — the default — means *read
the CHP envelope*, which is what core does for every ordinary caller. The four
`/context/*` routes need a tab to resolve the V32 taint-latch scope against, and
three of them (`compaction`, `should_read`, `post_edit`) gate on it: under an
EXTERNAL latch `post_edit` will not run the project's configured checks and
`should_read` will not return source text, each answering with its own fail-safe
rather than an error. A caller that sends no tab resolves no scope and is
admitted — the same locked fail-open every tool-serving loopback route takes.
Which of its hooks a harness emits at all is that harness's own gating, computed
from the same Settings its `spawn_sig` covers, so a toggle the user flips raises
the restart hint.

**Permission detection is push-primary, scrape-fallback (NC-2).** A harness's
notification ingress has no toggle and no schema entry of its own; it is emitted
whenever `Settings::loopback_needed()` holds — i.e. whenever the loopback it
POSTs into actually runs (offload / graph / Code Audit MCP). That gate is
load-bearing (H2, 2026-08-05 review): without it a default install did work per
notification whose POST had nowhere to land, so the *primary* signal was dead
and silent. The gate is structural as well as deliberate — an artifact bakes its
URL at spawn, so with no loopback there is nothing to emit — and the consequence
is unchanged: **a feature-less install runs scrape-only permission detection.**
The injection carries a `spawn_inject_sig` entry so enabling one of those
features raises the restart hint.

**What reaches core is an EDGE, not a payload** (V40 Phase C, locked decision
21). Which of a harness's notification types or TUI footers means *a prompt is
on screen* is that harness's own grammar: the classification lives in
`harness/<id>/`, along with the rows the neutral detector engine
(`processing::permission::PermissionDetector`) matches on — those are a
transcription of somebody else's terminal chrome and belong beside the harness
they were transcribed from (`HarnessPlugin::permission_patterns`,
`patterns_doc_note`, `legacy_permission_patterns`). Core receives
`PermissionEdge::{Detected, Resolved}` and a tab, which is the same pair the
scrape detector produces — which is why a pushed edge and a scraped edge
collapse to one signal at the state manager instead of being two features. The
tab mapping is the harness's too, and it never guesses: a candidate that cannot
be resolved uniquely is DROPPED. Both producers feed the one idempotent
`awaiting_permission` flag, so the push simply usually wins the race and the
scrape path still covers a dropped or missed event. A pushed *resolve*
additionally force-clears that tab's scrape latch
(`ProcessorControl::ClearPermissionLatch` → `PermissionDetector::force_clear`)
and re-scans, because the detector is edge-triggered: an auto-denial landing
while a real approval prompt is still on screen would otherwise clear the badge
with nothing able to re-raise it (M11).

**Compaction route's side effects are unconditional.** `GraphService::
compaction_context` (`graph/service.rs`) always clears the session's
`injected` dedup map and marks it `post_compaction` — even when
`compaction_context` is off or the rendered block is empty — because those
two effects are what keep Phase C (dedup) and Phase E (read advisor) correct
across a compaction regardless of whether the block itself is gated on. Only
the returned working-set/notes text is gated.

**Two ingress output contracts are unverified against the pinned harness
build** — the `TODO(spike)` rows D0 (does a compaction block reach the model?)
and E1 (does a read refusal's reason reach it?). Both are `Dep::Behavior`: no
payload reveals the answer, so they stay manual spikes with a recorded outcome
rather than probes, and each harness declares its own row as permanently
unprobed with the reason (`HarnessPlugin::declared_unprobed`). They are tracked,
with their pass/fail recipes and where the outcome is recorded, in
`MAINTENANCE.md` § Open spikes & unverified contracts, and the per-harness
detail is in that plugin's README § *Open spikes & unverified contracts*. Design
posture in both cases: the server-side effects are correct regardless of whether
the harness reads the emitted field, so the feature degrades safely — worst case
the block never reaches the model, and for E1 the milestone spec says to cancel
the feature rather than ship a bare refusal (`read_advisor` defaults off).

**Read advisor staleness check uses content hash, not mtime.** `should_read`
(`graph/service.rs`) compares the current file's FNV hash against the indexed
`file.hash` — the same check `graph_snippet`'s `stale` flag uses — rather than
comparing a stored mtime against the memory event's timestamp. A code-review
fix (see the `fix(V11)` commit): mtime comparison is vulnerable to filesystem
clock skew on network shares / WSL2 bind-mounts, which could wrongly suppress
a real edit and hand the agent stale content.

**Digest jobs are demand-driven, slot-gated, and local-only.**
`context_llm_digests` only digests files that actually ranked into an
injection and have no outline (docs/configs/long scripts) — not the whole
repo. `GraphService::enqueue_digest` single-flights by `(root, file,
content_hash)` (an `InflightGuard` removes the key on `Drop`, so a panicked
digest task can't permanently leak a slot) and caps concurrent jobs at 32.
The compute itself goes through `OffloadSupervisor::run_internal` — a
non-streaming, tools-off, thinking-suppressed completion that **only
considers backends already running locally** (`self.running`, not the full
pool/router), so a digest can never route to a remote or cloud backend
regardless of `allow_remote_worker_access`. Injection never blocks on this: a
cache miss falls back to the V10 outline/empty digest and the result lands in
`graph.db`'s `digest` relation for the next retrieve.

**Code-embedding backfill rides the doc-embedding pass, strictly after it.**
`embed_backfill` (`graph/service.rs`) embeds `doc_chunk`s first (cheaper, and
doc search stays useful even with code embedding off), then — only when
`embed_code_bodies` is on — embeds pending `code_chunk`s into `code_vec` under
the same epoch/dim/model. `graph_semantic_code` is advertised (`graph/mcp.rs
tools()`) only when **both** `semantic_search` and `embed_code_bodies` are on
(a code-review fix — the backfill that actually populates `code_vec` only
runs when `semantic_search` is on, so gating the tool on `embed_code_bodies`
alone would advertise a tool that could never return results). No full-text
fallback exists for code chunks the way `graph_search_docs` backs
`graph_semantic_docs` — a miss degrades to a clear "unavailable, try
`graph_find_symbol`/`graph_struct_search`" message instead of silently
re-running as a keyword search.

**`file_centrality` counts distinct inbound edges, not join rows** (a
code-review fix). `graph_repo_map`'s ranking signal is inbound call-edge
count per file; the initial implementation joined `edge` against `symbol`
without deduping, so a callee name defined N times in one file inflated that
file's centrality by N×. Fixed in `graph/index.rs::file_centrality`, with a
regression test alongside it.

---

## Code Intelligence — Token Efficiency II (V17)

**No schema bump, no new hooks/routes/CLI subcommands.** The read-advisor
escalation (diff-substitute, shell interception, first-read tier) is all
in-memory session state on `GraphService` and reuses the V11 `--read-hook` shim
+ `/context/should_read` route; the graduation rules read existing `mem_event`;
the first-read tier reads the existing `digest` relation. New settings are all
additive, `#[serde(default)]` (`read_advisor_diffs=true`,
`read_advisor_shell=true`, `read_advisor_first_read_kb=0`, `lean_tools=false`).

**Snapshot-store constants (not settings), in `graph/service.rs`.** The
diff-substitute snapshot LRU is bounded by three consts — promote to settings
only if field data demands:
- `SNAP_ENTRY_MAX = 512 KiB` — a single file's snapshot is retained only when
  the content is ≥ `read_advisor_min_lines` lines **and** ≤ this size.
- `SNAP_TOTAL_MAX = 16 MiB` — whole-store byte budget; on overflow the
  oldest-touched snapshots are dropped (set `snapshot: None`, hash/turn kept —
  eviction forgets the *content*, never the *observation*).
- `READ_SEEN_MAX_ENTRIES = 4096` — a row-count backstop on the `read_seen`
  map itself (independent of the byte budget; not in the original plan, added
  during Phase A so an all-tiny-files session can't grow the map unbounded).
- `READ_REMIND_CAP = 3` — a changed file re-arms an already-reminded slot only
  while its remind `count` is below this; at cap it passes. An *unchanged*
  reminded file never re-reminds regardless of count.

**B5 — bypass-canary interplay (shell interception).** `check_bypass`
(`graph/service.rs`) has a skip-guard: when `read_advisor_shell` is on and
`shellread::whole_file_read(command)` matches, it returns *before* scoring —
the command was either intercepted-and-denied (the remind was already recorded
by `should_read`) or verdict-passed (not a bypass), so without the guard every
intercepted `cat` would *also* count as a bypass and poison
`drift.read_bypass.v1`. The canary itself is untouched: with interception live
its rate should **fall**. A persistently high `drift.read_bypass.v1` now means
the agent found a **residual escape route** the strict parser deliberately
rejects — `sed -n`, `head`, `tail` — not the plain `cat`/`Get-Content` the
overlay now catches. The `RULE_DRIFT_READ_BYPASS` rationale in `advisor.rs`
says so.

**F2 — `e1_pass` is stricter than `!e1_blocked()`.** The `adopt.read_advisor.v1`
graduation rule gates on `Signals.e1_pass`, which is
`harness_versions.e1_status` trimmed/lowercased `== "pass"` — **not** merely
`!e1_blocked()`. `e1_blocked()` is false for both `"pass"` *and* `"unverified"`
(it only fails closed on an explicit non-pass/non-unverified value), but
"verified OK" for auto-graduating a hook we've never seen work means *proven*:
an `unverified` E1 must not flip `read_advisor` on by itself. This is the one
intentional bare `"pass"` string comparison outside
`HarnessVersions::status_blocks`.

Hand-run smoke recipes for the diff-substitute, shell-interception, first-read
and tool-surface behaviors live in `MAINTENANCE.md` § Live-verify recipes.

---

## Code Intelligence — Agentic Inner Loop (V12)

**No schema bump — every V12 store is additive, create-if-missing.**
`symbol.is_test` (Phase C) is the one column that would normally force a
version bump, but it rode V11's v2 → v3 bump for free (the column already
existed, unused, in that migration — see the V11 section above), so
`GRAPH_SCHEMA_VERSION` stays at 3 for all of V12. Every other new store is a
plain relation created on first use, the same pattern V10/V11 used for
`session`/`digest`/`code_chunk`: `commit_touch` (Phase D, file churn),
`project_fact` (Phase E, durable facts), `session_distilled` (Phase E, an
idempotency marker per session id), and `meta` (Phase F, a small generic
key/value store backing the analyses-auto trigger's last-seen counts). An
older `graph.db` opens against these with zero migration step — they simply
don't exist until the first write.

**A fourth ingress capability joins the V11 three, on the same seam.** The
auto-check tap is `POST /context/post_edit`'s core, reached either by the
harness-neutral CHP body or by a harness's own post-edit ingress route — the two
transports meet at one core, as every other capability's do. Its emission is
gated on `context_injection && auto_check`, independent of the other three
context toggles, and which hook a harness wires it to is that harness's own
declaration (`HarnessPlugin::routes()`, `chp_event_for_route()`); the mapping is
in that plugin's README § *Hook routing*.

**`TODO(spike F0)` — a third unverified output contract, same posture as
V11's D0/E1.** Which field of a post-edit hook's reply actually reaches the
model as additional context is unconfirmed against the pinned harness build; the
emitted shape and the spike's status are the plugin's, recorded in its README
§ *Open spikes & unverified contracts*. Degrades safely either way: the
server-side effects (debounce
clock, baseline update, parked-block bookkeeping) run regardless of whether
Claude reads the field, and a parked block still drains via the next
`/context/retrieve` call (`GraphService::drain_auto_check`) — worst case the
block just arrives a turn later instead of inline. `auto_check` defaults off,
so nothing is affected until this is confirmed and a project opts in. Tracked
in `MAINTENANCE.md` § Open spikes & unverified contracts.

**The `checks/` module is a dependency surface of its own.** `checks::parsers`
has one parser per shipped `ParserKind` (`cargo-json`, `tsc`, `eslint-json`,
`pytest`, `generic-gcc`); each is regex/JSON-shape coupled to that tool's
*current* output format. Fixture upkeep is a maintenance-run item — see
`MAINTENANCE.md` § Check parsers & fixtures (V12 / V22).

**`graph_impact` / `is_test` / `graph_tests_for` are all approximate by the
same name-keyed-call-graph limitation `graph_references` already documents.**
None of these resolve dynamic dispatch, trait objects, higher-order callbacks,
or reflection-based test discovery — they walk the same reverse/forward
`calls` edges the rest of the graph does, which are name-keyed, not
type-checked. `graph_impact`'s dependent tree and `graph_tests_for`'s test
list are both labeled candidates in their tool descriptions (`graph/mcp.rs`),
same honesty convention as dead exports: an empty result reads as "found
none", not "verified none exist." Test detection itself (`graph/builder.rs`'s
`is_test` walkers) has no bit at all for languages without a bespoke walker or
a path-convention fallback — again accurate-but-incomplete rather than wrong,
matching V10's `visibility` precedent.

**The 4-agent code-review pass (`fix(V12)`, commit `aa120c3`) is worth reading
directly** — it caught several correctness bugs that would otherwise degrade
silently: `git status --porcelain` collapsing a brand-new untracked directory
into one `?? dir/` line (both `graph_impact` and `changed_only` now use
`-z --untracked-files=all` and NUL-split, shared between `graph::impact` and
`checks::gitls`); a `changed_only` site filter that could drop a just-edited
file's occurrence when a diagnostic already had ≥5 sites elsewhere (fixed by
filtering the *uncapped* site list before `cap_sites` truncates — see
`checks::mod::run`'s doc comment); a check that fails to spawn previously
vanished from the report indistinguishably from "ran clean" (now surfaced as
`"⚠ check `<name>` did not run: <err>"`, `checks::auto::spawn_failure_line`);
`is_cfg_test` missing `cfg(any(test, …))`/`cfg(all(test, …))`; a
`DistillGuard` in-flight-session-id guard preventing two concurrent
distillation sweeps (a full rebuild and a watcher-batch reindex can both pick
up the same idle session) from double-distilling it into duplicate facts; the
project-fact ranking boost requiring a whole-word, ≥4-char, non-generic-stem
match (the initial version was a raw substring match, so `mod`/`index`/
`context` spuriously boosted unrelated files); and `parse_unified_diff` only
treating a `+++ ` line as a new file header when it immediately follows a
`--- ` line (otherwise an added line whose *content* starts with `++` can be
misread as a header). The same pass also de-duplicated the two modules' git
spawn helper into `graph::gitcmd::run_git` (shared by `graph::impact` and
`graph::gitmeta`; `checks::gitls` keeps its own async twin on purpose — see
that module's doc comment) and bounded `graph_recent_changes` at the Datalog
level (`:order -last_ts :limit`) instead of scanning the whole `commit_touch`
relation per retrieve.

---

## Code Intelligence — run_check Generalization (V22)

**No schema involvement.** `CheckDef`'s new fields (`cwd`, `env`, `report_file`,
`pattern`, `auto`) are all `#[serde(default)]`, so an old `.cimp/config.json`
overlay deserializes unchanged; detection / auto-configure state is settings +
in-memory, nothing touches `graph.db`.

**The Rust `ParserKind` enum and the TS `ParserKind` union must stay in
lockstep — a tripwire enforces it.** `checks/mod.rs`'s tripwire test
`include_str!`s `src/lib/settings/types.ts` and asserts every `ParserKind` wire
name (its kebab-case serde rename, *derived* from serde — not a second hand-kept
list) and every `CheckDef` field key appears in the file. Adding a Rust variant
or field without mirroring it in `types.ts` fails `cargo test`. `all_parser_kinds()`
(same test module) is an exhaustive `vec![…]` over every variant, so it's a
compile error until a new variant is listed — the tripwire can't silently skip
one.

### Adding a `run_check` parser

Same shape as adding a graph language (above): fixture-first, and the exhaustive
match forces the wiring.

1. **Capture a fixture** from the *real* tool's output (stdout, or the report
   file for a file-reading parser), warts and ANSI codes included, into the test
   module of `checks/parsers.rs` — the existing per-parser tests are the
   template. Add a truncated/garbage-input case too (must yield zero diags, no
   panic — the spec requires it).
2. **Write `parse_<kind>`** in `checks/parsers.rs` (ANSI-strip first via the
   existing `strip_ansi`; keep severities and the dedup key consistent with the
   V12 machinery) and add its **`ParserKind` variant** in `checks/mod.rs` with the
   kebab-case `#[serde(rename)]`. If the parser needs a new `CheckDef` input (as
   `regex-custom` needs `pattern`, or the file-readers need `report_file`), add
   that field `#[serde(default)]` too — the tripwire will then require it in
   `types.ts` as well.
3. **Extend `all_parser_kinds()`** (`checks/mod.rs` test module) and route the
   variant through `parsers::parse`. The exhaustive `vec!` / `match` won't
   compile until the new variant is listed, so this step is forced, not optional.
4. **Mirror the wire name in `src/lib/settings/types.ts`** (the `ParserKind`
   union) — the tripwire (above) fails `cargo test` until you do.
5. **Add it to the editor dropdown.** `PARSER_KINDS` in
   `src/lib/settings/checksEditor.ts` is a **hand-maintained** ordered list
   (mainstream → SARIF/long-tail → regex/generic), with a matching `PARSER_LABELS`
   entry and, if the parser reveals `pattern`/`report_file`, an arm in
   `showsPattern` / `showsReportFile`. It is **not** derived from the union, and
   there is **no tripwire on it** (the TS type would still accept the variant), so
   a new parser is invisible in the UI until you add it here — double-check this
   step.
6. **Run `cargo test` (tripwire + fixtures green) and `npm run check` + `npm
   test`.** The parser then appears in the editor dropdown automatically. Because
   the detect/preset catalog (`checks/detect.rs`) is a separate data table, wire
   the new parser into a preset there only if language auto-detection should
   *propose* it for some ecosystem — otherwise it stays a manual-only choice.

**`cwd` / `report_file` are confined under the project root, the same way
offload's `ToolCtx::confine` confines a path.** Absolute or `..`-escaping paths
are rejected at settings validation *and* at run time; a `report_file` that's
missing after the run is an explicit error diag, never empty success.
`report_file` is resolved **relative to the check's working directory** (`cwd`
if set, else the project root) — matching where a tool run in that dir actually
writes (so `detect.rs`'s nested-module presets seed an unprefixed
`target/surefire-reports` correctly). For back-compat with pre-fix configs that
were written root-relative *with* a `cwd`, resolution tries cwd-relative first
and falls back to root-relative only when the file exists solely there
(cwd-relative wins when both exist). `env`
values are redacted in `CheckDef`'s `Debug`. `regex-custom`'s `pattern` is
compiled and its mandatory named groups checked at save time
(`parsers::validate_pattern`, surfaced through the `checks_validate_pattern`
IPC) so a bad pattern is a UI error, not a silent zero-diagnostics run.

Parser fixtures rot when the underlying tool changes its output — see
`MAINTENANCE.md` § Check parsers & fixtures (V12 / V22).

### Adding a harness plugin

Same genre as the two how-tos above, one layer down: this is what it costs to
support a **new CLI** (or absorb a change in one), after V35 Phase K moved the
whole harness surface into `src-tauri/src/harness/`. The full treatment — layers
as modules, the tier model, CHP, the registry, and a step-by-step developer guide
for both adding a harness and changing an existing plugin — is
[HARNESS-PLUGIN-LAYER.md](HARNESS-PLUGIN-LAYER.md). The in-tree twin of this
section is `src-tauri/src/harness/README.md`; the design is
`DESIGN-harness-plugin-architecture.md` (§ 4 for the tree, § 4.1 for the tests
below, § 6 for this cost table).

**Everything cImp knows about a harness lives in one directory**, and four
tests in `harness/layering.rs` keep it that way:

- `no_harness_literals_outside_harness` — a string a harness OWNS
  (`hookSpecificOutput`, `message.part.delta`, the TUI permission footer) may not
  appear in production code outside `harness/`. The needle list is *derived from
  the capability registry's* `depends_on`, so declaring a new dependency
  automatically widens what the scan refuses to see elsewhere. Exceptions are an
  explicit, commented `LITERAL_ALLOWLIST` — **one file today** (`graph/index.rs`,
  a word collision rather than a dependency) — re-checked by
  `every_literal_allowlist_entry_is_still_earning_it` so an entry cannot outlive
  the literal it was written for. The scan reads each file with its
  `#[cfg(test)]` items removed, line-ending-blind and brace-matched via
  `crate::rustsrc`, and two further tests guard the guard
  (`the_literal_scan_reads_the_same_code_on_every_platform`,
  `executable_text_ignores_line_endings_and_cuts_at_every_test_item`).
- `no_harness_identity_outside_registry` (V40 Phase A, locked decision 10(a)) —
  a harness's own NAME. Every descriptor id, reserved tab id, binary stem and
  consumer token is a needle, derived from the registry, and none of them may
  appear in production code outside `harness/`. Core may *hold* a `HarnessId`
  and pass it to the registry; it may not spell one and it may not branch on
  one. Exceptions are `IDENTITY_ALLOWLIST` — **two files today**
  (`settings/schema/mod.rs`, `state/manager.rs`), both for persisted wire forms —
  re-checked by `every_identity_allowlist_entry_is_still_earning_it`. The
  frontend has its own half: `src/lib/harnessIdentity.test.ts` runs the same
  scan over `src/`, with its own allowlist and its own both-directions check.
- `harness_modules_do_not_import_capabilities` — the dependency direction is
  L1 → L2 only. A module under `harness/` may not reach into `crate::graph`,
  `crate::tts`, `crate::workbench` or `crate::delegation`
  (`layering::CAPABILITY_MODULES`; `crate::usage` left that list in V40 Phase D,
  because the whole usage data path moved *below* the seam into
  `harness/claude/usage.rs` behind `usage_source()`). The fallback readers that
  still import upward are a declared list (`UPWARD_EXEMPT`, **six entries**),
  each with its reason, and the test asserts in **both** directions — an
  exemption that stops being needed fails the build, so the list cannot rot into
  padding.
- `every_registry_entry_is_fully_wired` (V40 Phase A, locked decision 10(b); it
  absorbed `every_harness_dir_declares_its_capabilities`) — one
  `HarnessDescriptor` row is a promise about a dozen places, and this is what
  makes forgetting one of them a red build. It checks the directory set in
  **both** directions (a `harness/<id>/` directory the registry does not declare
  fails as loudly as a descriptor with no directory; `_`-prefixed directories
  such as `_retired/` are data a retired harness left behind, not a harness),
  then per descriptor: capability rows exist, a CHP hello is declared
  (`chp::EV_HELLO` appears in the directory), identity is complete (binary, tab
  id, consumer, label), exactly **one** `<id>.input.profile` capability row
  exists and the plugin really answers an `input_profile()`, `spawn_sig` is not
  null, every declared setting is unique/labelled/well-typed and every
  `scoped_features()` row names a `Bool` field the schema declares, the sandbox
  grant table is non-empty, a *Harness health* panel row appears (plus the
  neutral `Harness::ANY` one), `MAINTENANCE.md` names every capability id, and a
  harness declaring `FileArtifact` has `fixtures/harness/<id>/goldens/`.

**The steps, and the test that fails until you do each one:**

1. **`harness/<id>/mod.rs`** with an `impl HarnessPlugin`, plus `pub mod <id>;`
   in `harness/mod.rs` and **one `HarnessDescriptor` row** in
   `harness/registry.rs` (id, label, binaries, reserved tab ids, consumer token,
   `expects_chp`, `env_strip`, `features`, `plugin`). It is deliberately **not**
   a new enum variant: `HarnessId` is an opaque newtype, so there is no
   `HarnessId::Claude` for a `match` in core to grow an arm for.
   → `every_registry_entry_is_fully_wired`, `ids_are_unique_and_non_empty`,
   `every_reserved_tab_id_resolves_to_exactly_one_harness`.
2. **`settings_schema()`** — your own `ext` fields, stored under
   `Settings.harness[<id>].ext` and never named by core. An empty table is an
   ordinary answer (empty Settings section, no ext keys, no work anywhere).
   → `every_registry_entry_is_fully_wired` (duplicate key, missing label,
   default the declared kind rejects), and
   `info::tests::the_committed_registry_fixture_matches_the_registry` for the
   frontend mirror.
3. **`routes()`** if the harness pushes over its own ingress rather than posting
   plain CHP bodies, plus `identity_of_request()` if its identity rides outside
   the body, `chp_event_for_route()`, `drift_vocabulary()` and
   `hook_reply_timeout()`.
   → `ingress::tests::no_two_plugins_claim_one_route`,
   `no_plugin_route_shadows_a_core_route`,
   `every_inverted_wire_default_names_a_route_that_exists`,
   `the_drift_vocabulary_is_declared_and_deduplicated`,
   `every_declared_timeout_outlasts_the_budget`.
4. **`native_tools()`** — the tools this harness serves ITSELF, with `class`,
   `mutates_fs` and `memory_kind` per row, plus `memory_arg_keys()` for the
   argument spellings its payloads use. **Empty fails closed and loudly**: every
   call would be treated as mutating and none recorded as a memory event.
   → `native::tests::every_registered_harness_declares_its_natives`,
   `an_unidentified_source_fails_closed`,
   `each_harness_answers_in_its_own_vocabulary`. Add the matching section to
   `HARNESS-NATIVE-TOOLS.md`.
5. **`input.rs` + `input_profile()`** if this harness may be a delegation
   worker, **and** the `<id>.input.profile` capability row that states what the
   profile depends on, **and** that row's `declared_unprobed()` reason. All
   three or none: a profile with no row is a Tier-D behaviour nothing records
   and nobody can mark verified, and the `delegation.worker` gate reads the
   recorded spike outcome **per harness** (`contract::gate_for`), so a harness
   with no profile answers `None` and is simply not a valid worker.
   → `every_registry_entry_is_fully_wired` (exactly one row, and a row implies a
   profile), `contract::tests::the_delegation_gate_resolves_the_workers_own_row`,
   `the_delegation_worker_gate_fails_closed_on_anything_unrecognized`.
6. **`instructions()`** — every string cImp puts in front of this harness's
   model, rendered in its vocabulary, and **`tool_for_role()`** for the one thing
   a neutral sentence cannot avoid naming (`GRAPH_GUIDANCE` says "prefer
   `graph_outline` → `graph_snippet` over a full *Read*", and OpenCode's
   rendering says `read` / `bash` because that is what OpenCode serves).
   → `instructions::tests::every_harness_declares_every_slot`,
   `nothing_ships_with_an_unfilled_placeholder`,
   `the_graph_nudge_speaks_each_harnesss_own_vocabulary`,
   `registry::tests::every_declared_tool_role_names_a_native_tool`.
7. **`preflight()`** — whether a tab of this harness may be enabled right now,
   with the install hint the UI appends to a refusal. Claude's "not gated, it is
   the app's own front end" is a declared `Ok`, so *not gated* is on the record
   rather than an exemption a third harness inherits by accident.
8. **`spawn_sites()`** — your rows in the external-process spawn ledger, because
   its tripwire scans the whole tree and consumes core's rows and every plugin's
   together. → `spawn_ledger::tests::the_spawn_ledger_is_exhaustive`.
9. **`config_writer()`** if cImp can write this harness's local-provider
   configuration, and the descriptor's matching `LocalProviderConfig` feature.
   → `info::tests::a_declared_config_writer_exists` (both directions),
   `local_provider_vars_name_declared_ext_keys`.
10. **The descriptor's `features` and the plugin's `affordances()`** — what core
    mounts beyond the neutral path, and every user-facing string the window used
    to hard-code (label, default command, accent, state dirs, install hint,
    attachment format, attribution template, inject mechanism, status-line rows).
    A harness that declares nothing renders with cImp's own wording and no
    accent: a visible absence, never another product's copy under this one's
    name. → `a_declared_usage_push_has_a_source`,
    `accents_are_distinct_where_declared`,
    `every_harness_declares_what_the_window_prints`, and the vitest suite
    *registry parity (locked decision 11)* in `src/lib/harness.test.ts`.
11. **A CHP hello** — `serves` / `cannot`, built from the *same* booleans that
    decided what the artifact actually wired, so the declaration cannot claim
    something the artifact does not do. Event ids come from `chp::EV_*`.
12. **`canaries()` + fixtures** under
    `src-tauri/fixtures/harness/<id>/<version>/` — the L1 assertions that a
    recorded payload still produces *substantive* output, run every `cargo test`
    and inside the shipped binary whenever this harness's version changes. Write
    the negative twin: a positive canary that never ran passes just as green as
    one that did. → `canary::tests::canaries_and_the_matrix_agree`,
    `embedded_canaries_are_exactly_the_declared_ones`,
    `every_fixture_version_dir_has_a_manifest`.
13. **`probe()`** (plus `probes()`, `probes_share_one_child()` and
    `declared_unprobed()`) — the L2 half, driven against the installed CLI.
    `harness/probe.rs` stays core: it owns the runner, the report shape and the
    **declared report order** — but since V40 Phase I the order is a
    CONCATENATION of each plugin's own `probes()` in `drive_order()`, not a
    hand-kept list of both harnesses' ids in core. → `contract::tests::probes_and_the_matrix_agree`,
    `every_silent_degradation_has_a_canary_or_a_probe_or_a_waiver`.
14. **Goldens if the artifact is a file** — declare `HarnessFeature::FileArtifact`
    and commit `src-tauri/fixtures/harness/<id>/goldens/`, so a change to what the
    harness loads is a reviewable byte diff.
15. **Capability rows** in `harness/contract.rs` (or, for rows whose contract is
    a sentence about this product, `HarnessPlugin::capabilities()`), with
    `wired_in` naming your files — **and a drift row in `MAINTENANCE.md` in the
    same commit**. → `wired_in_paths_exist`,
    `contract::tests::matrix_matches_maintenance_doc`.
16. **A fallback reader** (`<id>/read.rs`) *only* if the harness cannot push;
    declare `activity_source()`, `usage_source()` and `session_key_space()` to
    match. Tier C stays possible, contained and declared rather than ambient. It
    will need L4 types, so add it to `UPWARD_EXEMPT` with the reason and the
    condition that retires it.

Everything **outside** `harness/<id>/` is neutral and consumes the plugin through
the interface. Two claims this section used to make were false and are replaced
by the tests that now enforce the truth:

| Old claim | What is actually true |
|---|---|
| "no new enum variant outside `harness/`" | Still true, and now **checked**: `HarnessId` is an opaque newtype with no per-product constants, and `no_harness_identity_outside_registry` fails the build if core spells a harness name at all. |
| ~~"no new `match` arm in `tabs/config.rs`"~~ | The *file* is no longer exempt from the identity scan — its last per-harness residual left in V40 Phase C — so a new arm there would fail `no_harness_identity_outside_registry`, not merely be discouraged. |
| ~~"no frontend mirror"~~ | **False.** There IS a frontend mirror and it is pinned: `harness_list` serves the roster over IPC, `info::tests::the_committed_registry_fixture_matches_the_registry` writes `src-tauri/fixtures/harness/registry.json` and fails when the committed file differs, and vitest asserts the TypeScript unions in `src/lib/harness.ts` cover it (*registry parity (locked decision 11)*). A descriptor field, a feature or a harness added in Rust without its TS mirror is a red `npm test` rather than a runtime `undefined`. |
| ~~"a bespoke gate constant"~~ | Still true: `contract::gate_for(id, settings, harness)` is the one query, and `no_gate_blocks_outside_the_declared_list` / `every_gated_capability_can_actually_block` hold both directions. |

If a step forces something outside `harness/<id>/`, the seam is in the wrong
place — raise it rather than adding it.

Two standing constraints. **cImp does not load harness plugins it did not
ship** (design D7): there is no drop-in directory and no manifest format,
because the plugin is inside the TCB — cImp only *computes* the V32 Phase H
verdict, and the enforcement is a `throw` inside the plugin's own
`tool.execute.before`, which no cImp-side test can verify ran. And
`opencode/plugin.rs` / `opencode/tools.rs` are **security controls, not data
pipes**: the native-tool gate, the taint beacon and the pre-mutation checkpoint
all execute inside the generated file, and the registry marks those rows in its
`controls` column.

---

## Code Audit — Aggregated Security Scanning (V23)

**No schema involvement, nothing bundled.** `CodeAuditSettings` is additive
(`#[serde(default)]`, feature off by default), the tab is a reserved
app-rendered dashboard gated on `code_audit.enabled`, and every tool resolves
ebin → PATH → per-tool override at scan time — the release ships no scanner
binaries. The runner (`src-tauri/src/audit/`) normalizes each tool's SARIF
through the V22 `checks::parsers` `sarif` parser, so the audit path grows no new
diagnostic parsing; the one audit-only extra is the scan-coverage line, a second
best-effort pass (`runner::parse_scanned_artifacts`) over osv-scanner's raw SARIF
`runs[].artifacts` that deliberately does *not* extend the shared parser.

**Audit tools are exit-code-inverted vs `run_check`.** `0` = clean, a code in
the tool's declared `findings_exit_codes` = findings present (a SUCCESS),
anything else = a genuine tool error — `adapters::classify_exit` owns this, and
it's the one place V22's checks model (non-zero = failure) doesn't fit.

**Since V38 the fourteen tools are DATA, not code.** They live in
`src-tauri/src/plugins/builtin/cimp-audit.json`, an embedded manifest read
through the same loader, validator, registry and runner a dropped-in plugin
goes through (`docs/TOOL-PLUGINS.md`). `AuditToolId`, `AuditToolConfig` and the
`static Adapter` table are gone; `audit/adapters.rs` keeps only what is not
per-tool configuration (`Category`, `Transport`, `classify_exit`). Adding a
scanner is now a manifest entry — no rebuild for a user's own, and no new
control flow for one of ours.

**Offline degrades — and the failed chip must say why.** osv-scanner queries the
OSV API / deps.dev and semgrep downloads its rules on first run, so an offline
scan can fail. `runner::exit_error_message` appends a trimmed tail of the tool's
own stderr (falling back to stdout) to the `exited with code N` message, surfaced
as the failed chip's tooltip — a bare `exited with code N` with no tail means the
tool printed nothing, not that the excerpt was dropped.

The SARIF fixtures were hand-built rather than captured, and the live-verify
pass is where real output gets substituted — see `MAINTENANCE.md` § Code Audit
& Code Quality scanners (V23 / V25) and § Live-verify recipes.

---

## Code Quality — Language-Gated Linters (V25)

**Builds directly on V23 and nothing is bundled.** The eleven quality tools are
eleven more entries in the same definition as V23's trio — since V38 that is the
embedded `cimp-audit` manifest — sharing one runner, one ebin → PATH → override
resolution, and one `checks::parsers` decoding path. The four non-SARIF tools
name a findings decoder (`typos-jsonl`, `eslint-json`, `knip-json`,
`machete-text`); those decoders live in `checks::parsers` like every other one,
reached through the findings-namespace `AuditParser` in `audit/runnable.rs`.
The one genuinely new mechanism is the **census** (`audit/census.rs`) — a
bounded, ignore-respecting walk (20 000 entries / 2 s, cached ~60 s) that decides
which tools apply.

**Flags/exit-codes were web-verified at implementation and deviate from the spec
in two places — trust the code, not the spec.** (1) **cppcheck** writes its report
to **stderr**, not stdout, and exits **0 even with findings**; the adapter uses
`--output-file=<tmp>` with `Transport::ReportFile` and an empty `findings_exit_codes`
(a clean exit-0 run with a populated report is the normal findings path), needs
**≥ 2.16** for SARIF, and runs with `--enable=warning,style`. (2) **cargo-machete**
emits a header line + tab-indented crate names on stdout (the `MacheteText` parser
matches that), exit 1 on unused deps. Other exit codes: **typos** = 2, **PMD** = 4
(5 is a real error), everyone else 1. `Adapter::classify_exit` owns all of this.

**Node tools resolve project-local first.** eslint and knip carry
`Adapter::project_local_bin`; `resolve_audit_binary` (`audit/mod.rs`) tries a
non-empty override verbatim → `<root>/node_modules/.bin/<tool>` (the `.cmd`/`.bat`
shim on Windows) → ebin → PATH. `dotnet-analyzers` resolves `dotnet`,
`semgrep-quality` reuses the `semgrep` binary.

**Upgrade reconcile.** A pre-V25 `settings.json` persisted only the three Security
tools; the lenient `tools` deserializer keeps a present array verbatim, so
`persistence::reconcile_audit_tools` appends any missing built-in on load
(`integrity_check`) and on the live settings-update round-trip, preserving every
existing entry and its order. Unknown/stale ids are still dropped by the lenient
enum.

**Post-release redesign (2026-07-16): ONE tab, two sub-tabs.** The separate
`TabId::CodeQuality` reserved tab is retired — `CodeAuditView.svelte` hosts
**Security | Quality** sub-tabs (Code Intelligence section pattern; both
`AuditPanel` instances stay mounted, the inactive one is display-hidden so a
running scan keeps streaming). Settings **schema v22 → v23** drops any persisted
`code-quality` tab entry (`migrate_v22_to_v23`); the wire id now parses as a
plain Shell id and never reaches the runtime.

**Quality auto-selection (`code_audit.quality_auto_select`, default on).** Each
QUALITY tool's `enabled` checkbox follows the census automatically:
its manifest's `enabled_by_default` AND applicable
(`audit::runner::auto_select_quality` — dotnet-analyzers and semgrep-quality
declare `enabled_by_default: false` and so stay opt-in; security tools and user
plugins are never touched). Applied at every scan start and by the
`audit_refresh_census` IPC (tab mount + Settings open take a real census, ≤60s
cache, so gating/hints/selection work before the first scan). Since schema v34
the flags it writes live in `tool_plugins`, under the built-in audit plugin's
key. A manual quality checkbox edit (Settings → Tool Plugins) flips the setting
to manual mode; the **Auto-select for this project** button (Settings → Code
Audit, shown in manual mode) re-applies and re-enables auto. Frontend mirror:
`qualityAutoSelection` in `codeAudit/logic.ts`.

The per-tool install/scan walkthrough is in `MAINTENANCE.md` § Live-verify
recipes.

---

## Workbench — Vibe-Coding Guardrails (V13)

**No graph-schema change, no new MCP tool.** The whole feature is a reserved
app-rendered tab (`TabId::Workbench`, same pattern as Code Intelligence)
backed by spawned `git` (diff parsing, worktrees) and a self-contained
`.cimp/shadow.git` store (checkpoints) — `GRAPH_SCHEMA_VERSION` stays at 3.
Diff/worktree operations need `git` on `PATH`; checkpoints work in a project
with no `.git` at all (the shadow repo is self-contained), which is
deliberate — it's what makes checkpoints useful *before* `git init`.

**Shadow-repo trust model — one audited chokepoint.** `workbench::git::GitCtx`
(`git.rs`) has three optional fields mapping 1:1 onto `GIT_DIR` /
`GIT_WORK_TREE` / `GIT_INDEX_FILE`; `run`/`run_with_stdin` always **set or
remove** all three explicitly before spawning `git` — never leaving one
inherited from the parent process's environment — which is the actual safety
property (a spawned `git` child could otherwise silently inherit a stray
`GIT_DIR` and operate on the wrong repo). `GitCtx::discover` (all `None`)
targets the user's own repo; `GitCtx::shadow(root)` points `GIT_DIR` at
`.cimp/shadow.git`, `GIT_WORK_TREE` at the project root (shared with the
user's tree — checkpoints see real on-disk content), and `GIT_INDEX_FILE` at
the shadow repo's own index so staging for a snapshot never touches the
user's index. Every shadow git call in `shadow.rs`, `diff.rs` (the
non-git/checkpoint-diff fallback), and `worktree.rs` routes through this one
constructor pair — there is no second way to spawn a shadow `git` process.
Regression-tested directly (`git.rs`'s unit tests assert the exact env-var
overrides for both `discover` and `shadow`, plus that `discover`'s overrides
are all `None`).

**Checkpoints are orphan commits, deduped by tree sha, not by a "did
anything change" flag.** `shadow::snapshot` always runs `stage_and_write_tree`
first (needed to see untracked files even for the dry-run dedup check), then
compares the freshly-computed tree sha against the latest `cp-<seq>` tag's
`<tag>^{tree}` — equal shas skip the commit. This replaced an earlier
`changed_since_index`-based dedup guard (removed in the V13 code-review pass)
that could wrongly report "unchanged" against a stale index; tree-sha
comparison sidesteps the whole index-staleness question. Each checkpoint is a
parentless `commit-tree` (`git commit-tree` with no `-p`) tagged `cp-<seq>` —
no branch ever advances in the shadow repo, so `git status` inside it
permanently reads "unborn HEAD vs a fully-staged index"; that's expected, not
a bug. `next_seq`/`latest_checkpoint_tag` both derive from a `tag -l cp-*`
scan rather than a counter file, so they can't drift out of sync with what
tags actually exist.

**Restore-safety invariants are the one place in this milestone worth
double-checking on every touch.** `shadow::restore` (`shadow.rs`) always: (A)
takes a `Trigger::PreRestore` snapshot of the current state *before* touching
anything, so every restore is itself undoable; (B) re-creates files present
at the target but deleted since; (C) computes `created_since` (files present
in the pre-restore state but absent from the target) and leaves them alone
**unless** the caller passes `delete_new: true` (default `false` at every call
site — untracked new work survives a restore by default); (D) only deletes
`created_since` paths when `delete_new` is explicit. `restore_round_trip_is_
byte_faithful_including_crlf` and `restore_keeps_new_files_by_default_
deletes_only_with_delete_new` in `shadow.rs`'s test module are the direct
regression coverage; re-run both after touching this function. The user's own
`.git` is never opened by `restore` — it operates entirely through the
`GitCtx::shadow` context above.

**Per-hunk revert reconstructs a single-hunk patch and applies it with `git
apply --reverse --unidiff-zero -`** (`diff.rs::revert_hunk`/
`build_hunk_patch`) — never a partial apply; a failure (stale `hunk_hash`,
mid-merge/-rebase `readonly` guard, or `git apply` itself rejecting the patch)
leaves the file untouched. `hunk_hash` is recomputed from the hunk's own
content each time a diff is built, so a hunk that shifted or changed since the
UI last saw it fails the hash check rather than reverting the wrong lines. A
checkpoint (when checkpoints are on) is taken before the `git apply` call,
matching Feature 1's restore-is-always-undoable posture. `is_special_state`
checks for `MERGE_HEAD`/`REBASE_HEAD` and flips the whole diff summary
`readonly` — no hunk reverts while the index is mid-merge/-rebase.

**The `fs-batch` event is a new, shared primitive — not workbench-private.**
`WorkbenchService::publish_fs_batch` (`mod.rs`) broadcasts a capped path list
on the `fs-batch` Tauri event whenever the graph watcher's own debounce thread
hands over a batch; both the Diff pane (`workbenchDiff.ts`, 500 ms debounce +
5 s poll fallback that skips itself while the watcher is on) and the burst
checkpoint trigger (`handle_fs_batch_for_burst`) subscribe to the same
broadcast channel, so a project with `graph.enabled` off still gets live diff
refresh and burst checkpoints — the watcher requirement is soft, not hard.

**Merge never leaves a half-merged main tree — verified, not just attempted.**
`worktree::merge` refuses up front on a dirty main tree or a main branch that
doesn't match the worktree's recorded base; on a `git merge` conflict it runs
`git merge --abort` and, critically, checks *that* command's own exit status
(a V13 code-review fix) — if the abort itself fails, the error message says so
explicitly ("main working tree may be left half-merged... resolve manually")
rather than claiming a clean abort it can't confirm. `discard` only removes
worktrees whose `.cimp/worktrees/<slug>.meta.json` sidecar cImp itself wrote,
double-confirmed in the UI. `merge_conflict_aborts_cleanly_and_leaves_main_
tree_untouched` in `worktree.rs`'s test module asserts `MERGE_HEAD` absence,
unchanged `HEAD`, and a clean `git status` after an aborted merge — the
regression coverage for the "never half-merged" guarantee.

**The 3-agent code-review pass (`fix(V13)`, commit `010a14e`) is worth reading
directly** — same posture as V12's, and it caught one **critical data-loss
bug**: `diff_vs_now`'s `git add -A` used to leave the shared shadow index
matching disk, so `restore`'s own pre-restore safety snapshot (Invariant C,
above) could dedup against a now-*stale* tree sha and skip taking a real
undo point — a restore could then destroy uncommitted edits with nothing to
recover them from. Fixed by giving `diff_vs_now` its own scratch index (zero
side effect on the dedup-relevant index state); regression test
`restore_after_a_dry_run_diff_preserves_uncommitted_edits`, verified
fail-without/pass-with. The same pass added the `git merge --abort`
exit-status check described above, fixed a `parse_unified` panic on an empty
hunk-body line, moved the checkpoint min-gap gate from global to per-root
(it was swallowing other projects' checkpoints), excluded
checkout-untouched paths from `RestoreReport.changed`, and wired the
non-git-project diff pane (`DiffSource::Shadow` — diff vs the latest
checkpoint) that Feature 2's design called for but Phase B initially missed.

---

## Workflow & Visibility (V14)

**Two different schema numbers move this milestone — don't conflate them.**
The **graph** schema stays at `GRAPH_SCHEMA_VERSION = 3` (`graph/schema.rs`):
the new `usage_stat` relation (`graph/index.rs`) is additive/create-if-missing,
the same pattern every V10–V13 store used. The **settings** schema, by
contrast, bumps `CURRENT_SCHEMA_VERSION` 20 → 21 (`settings/schema/mod.rs`) — the
first schema move this file's V10–V13 sections haven't had to talk about,
because it's the first milestone in the series to add a new *tab kind*
(`TabConfig::Preview`) rather than a graph-side capability. The migration
step itself (`settings/migration.rs`'s `migrate_v20_to_v21`) is a pure
version-stamp, no data transform: every new field this milestone adds
(`preview_last_url`, `preview_allow_remote`, `prompt_templates`,
`templates_seeded`, `advisor_dismissed`) is `#[serde(default)]`/`Option`, so
an older `settings.json` round-trips through it with nothing to migrate.

**Usage/X-ray is the fifth hook-free area in Code Intelligence.** Of the tab's
six sections (Index / Activity / Memory / Context / Analyses / Usage), only
**Context** needs a harness-side hook at all (the ingress seam described in the
V11 section above) — Index, Activity, Memory, Analyses, and now **Usage** all
ride existing plumbing with no hook of their own. The usage tap extends the OOB
Claude-transcript reader
that already exists for TTS and memory (`harness/claude/read.rs::record_usage`, called
from the same `drain_new_lines` loop as `record_tool_events`): `parse_usage_line`
pulls `message.usage.{input_tokens,output_tokens,cache_read_input_tokens,
cache_creation_input_tokens}` keyed by `message.id` (an UPSERT-by-`msg_id`,
so a later line with firmed-up numbers overwrites an earlier zeroed one
in place), and `extract_tool_results` sums `tool_result` content chars,
attributed to a tool name via a small per-session `tool_use_id → name` ring.
Unlike `record_tool_events`, this tap does **not** skip sidechain (sub-agent)
lines — sub-agent token spend counts toward the parent session's totals.

**Sub-agent transcripts live in TWO places across CLI vintages — the tap
handles both, and a canary watches for a third.** Claude Code 1.x wrote a
sub-agent's traffic inline in the parent transcript as `isSidechain:true`
lines (covered by the paragraph above). The 2.x CLIs (observed 2.1.207)
instead write one file per agent at
`~/.claude/projects/<slug>/<session_id>/subagents/agent-<id>.jsonl` (plus an
`agent-<id>.meta.json` we don't read), renamed the launcher tool `Task` →
`Agent` (`harness/claude/read.rs::AGENT_TOOL_NAMES` matches both), and the parent
transcript carries **zero** sidechain lines. `SubagentState` (same file)
tails those per-agent files each poll tick, feeding ONLY `record_usage` and
`record_commit_events` under the parent session id — a sub-agent's tokens
and commits are the parent's spend/output, but its reads/prompts/text stay
out of the working set, turn clocks, avatar state, and TTS, exactly the
split the inline contract had. If the contract moves again,
`SubagentState::drift_tick` records a `subagent_drift` Activity event
(once per session, after the condition holds ~3 ticks) and the advisor
surfaces it as `drift.subagent_transcripts.v1`: either "transcripts moved"
(an agent completed but its traffic showed up in neither location — token
spend is being dropped) or "launcher tool renamed" (`subagents/*.jsonl`
exist but no recognized launch `tool_use` — usage still counts, but the
agents-active avatar hold is blind). A simultaneous rename **and**
relocation is invisible from this vantage; if sub-agent-heavy sessions ever
look cheap again with no canary firing, diff a live session's transcript
dir against these two known layouts first.

**OpenCode usage is `est_only` — `TODO(spike C3)`, resolved as "absent."**
`harness/opencode/read.rs`'s module doc records the spike outcome directly: OpenCode's
`/event` SSE stream's `message.updated.properties.info` object was captured
exhaustively and carries only `{id, role, time}` — no token/usage fields on
the pinned OpenCode version — so this file adds no usage tap at all. The
actual OpenCode-side usage recording happens where OpenCode's memory events
already land, `offload/loopback.rs::handle_memory_event` (`POST
/memory/event`), which estimates chars from a tool call's *input* args (the
same blind spot the memory tap already had — tool output isn't visible
there either) and records a `ToolResult` usage event from that estimate.
`est_only` is derived **structurally and harness-neutrally**, not from a
tracked flag and no longer from the session's agent: a session shows the badge
exactly when it has no recorded `turn` usage row at all
(`GraphIndex::usage_session_has_turn`, read by both `usage_all_sessions` and
`usage_session_row` so the two paths cannot answer differently). A harness that
starts reporting real per-turn tokens therefore stops being `est_only` by
producing them, with no per-product condition to update. Revisit if a future
OpenCode release adds token fields to `message.updated`;
`harness/opencode/read.rs`'s doc comment names the exact field path to
re-check.

**`TODO(spike E0)` — WebView2 child-webview capture compiles clean but has
never run against a live instance.** The Preview tab's capture path
(`preview/capture.rs`) reaches `ICoreWebView2::CapturePreview` through
`Webview::with_webview` → `PlatformWebview::controller()`, verified to
type-match this crate's own `webview2-com = "0.38"` dependency (pinned to the
same 0.38.2 wry 0.55 resolves to transitively, confirmed via `Cargo.lock` —
no COM-GUID-compatible-but-distinct-type risk) — and it compiled cleanly on
the first attempt against the exact pinned dependency graph. What's still
unverified, because no live app was available to drive it from: whether the
captured PNG is actually pixel-correct (right viewport bounds, true
CSS-pixel — not HiDPI-inflated — scale, correct timing relative to paint);
z-order/coexistence with the xterm panes during an actual tab drag; and
focus/keyboard isolation in practice (no hold-Alt-bypass-equivalent was
added, on the assumption — not the measurement — that WebView2 child
webviews don't fight the host window's accelerator table the way the AI-tab
PTY mouse capture did). See the `TODO(spike E0)` comments in both
`preview/mod.rs` and `preview/capture.rs` for the exact call sites; do a live
pass before relying on Snapshot → compose for anything precision-sensitive.

**The embedded-webview path is a new, Windows-only native dependency
surface.** `tauri = { version = "2", features = ["protocol-asset",
"unstable"] }` — `unstable` gates `Window::add_child`, the multi-webview API
the Preview tab is built on (a Tauri naming quirk, not a claim about API
risk: it's the documented, doctested multi-webview shape). Capture adds
`webview2-com = "0.38"` and `windows = { version = "0.61", features =
["Win32_System_Com", "Win32_UI_Shell"] }`, both pinned to match what wry
0.55 already resolves to. All three are load-bearing only on Windows —
`preview/capture.rs`'s `#[cfg(not(windows))]` stub always returns a clear
"only implemented on Windows today" error rather than attempting webkit2gtk
capture, matching the milestone's non-blocking allowance for Linux.

**Preview nav-policy security model — two independent allowlists, one
documented gap.** `preview::is_allowed_preview_host` (pure, unit-tested)
gates which **hosts** the embedded webview may navigate to directly:
`localhost` (name) or a loopback/RFC-1918-private IP literal
(`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `127.0.0.0/8`, `::1`)
unless `preview_allow_remote` is on, checked via `url::Host` (not string
matching, so `http://localhost@evil.com`-style userinfo tricks resolve to
the real host). Separately, `preview::is_externally_openable` gates which
**schemes** may ever reach the OS system-opener (`tauri_plugin_opener`) —
`http`/`https` only; this is the Follina-style RCE-vector fix from the
`fix(V14)` review pass (below). **KNOWN LIMITATION** (documented directly
in `preview/mod.rs`'s module doc, `// KNOWN LIMITATION` comment): both
policies apply only to the **main frame** — wry exposes no
subframe-navigation hook, so a policy-allowed page (a legitimate localhost
dev server) that embeds `<iframe src="https://some-remote-host">` can load
that remote content inside the Preview tab without either check ever
running. Accepted for a localhost dev-preview surface (the threat model is
"don't let the tab casually reach hosts you didn't ask for," not "sandbox
untrusted third-party content") — revisit if wry grows subframe-navigation
events, or by reaching `CoreWebView2Frame::NavigationStarting` directly if
this ever needs to be airtight.

**The `fix(V14)` review pass (commit `820319e`) is worth reading directly** —
same posture as V12's and V13's, three agents, one HIGH-severity data-loss
bug and one HIGH-severity RCE-vector bug:
- **`settings_update` template-clobber (HIGH, data loss).** The generic
  settings-save IPC used to do a near-wholesale overwrite of the persisted
  `Settings` (preserving only `layout`/`session` from live state before
  applying an incoming snapshot). `prompt_templates`/`templates_seeded` are
  written **out-of-band** by the dedicated `compose_templates_global_set` IPC
  (straight read-modify-write against the physical global `settings.json` —
  see the Prompt Library note in `FEATURES.md`/`CHANGELOG.md`), so a Settings
  window snapshot taken before a template edit could roll that edit right
  back the next time *any* unrelated setting saved. Fixed by
  `apply_incoming_settings` also preserving `prompt_templates`/
  `templates_seeded` from live state, same as `layout`/`session`; regression
  test simulates a stale/empty incoming snapshot and asserts templates
  survive.
- **`open_external` scheme allowlist (HIGH, RCE vector).** Before this fix, a
  Preview tab's rejected-navigation path and `on_new_window` handler forwarded
  *any* URL straight to `tauri_plugin_opener::open_url`, which ultimately
  calls OS shell APIs — a `file:`, `data:`, or (the Follina-class case) a
  registered custom protocol handler like `ms-msdt:` had no meaningful "host"
  for `is_allowed_preview_host` to reject, so it sailed through untouched to
  the OS. Fixed by `is_externally_openable`, gating `open_external` to
  `http`/`https` only — see the security-model note above.
- **`attach.rs` TOCTOU (correctness).** `save_png`/`reserve_path` used to pick
  the next `n.png` index (a `read_dir` scan) and then create the file as two
  separate steps; two genuinely concurrent writers (a clipboard paste racing
  a Preview snapshot, both allocating from the same session's attach dir)
  could observe the same "next index" and collide, silently dropping one
  image. Fixed with a process-wide `ATTACH_ALLOC_LOCK` mutex serializing
  index-pick-and-create in a shared `allocate_and_write` helper, plus
  `OpenOptions::create_new` (O_EXCL-equivalent) with retry-on-collision as a
  second line of defense; regression test spawns two barriered threads and
  asserts both payloads land intact in distinct files.
- **Advisor proposal bounds (correctness).** `RULE_MIN_SCORE` gained a
  `MIN_SCORE_CEILING` (12) so repeated applies of "raise `context_min_score`"
  can't climb the floor high enough to silently turn off injection
  altogether; `RULE_TURN_BUDGET` now only proposes when its formula computes
  a genuine reduction (`proposed < current`) — the previous `.max(1_000)`
  floor could otherwise propose *raising* (or no-op'ing) an already-small
  budget, directly contradicting a rule whose entire premise is "lower the
  budget." Both guarded by dedicated tests in `advisor.rs`.
- The same pass also fixed a webview-leak (a Preview child webview is now
  destroyed by the backend's own `close_tab` and drained on app exit, not
  solely by the frontend's `onDestroy`, which a renderer crash or HMR reload
  could skip), added a 5s timeout to `capture_to_png` (a concurrent tab-close
  could otherwise hang the capture's completion callback forever) with
  stray-0-byte-file cleanup on any failure path, scoped `effectiveness_totals`
  to the calling project root's own sessions (it was previously summing
  process-wide, misattributing another project's chars in a multi-project
  session), and fixed a `PreviewToolbar` Back-button history bug (a
  non-pure history model that could oscillate between two entries).

---

## Code Graph Parity (V15)

**The graph schema moves 3 → 4 — the first graph-side bump since V11.** V15
Feature 3 adds a `confidence` value column to two relations (`ref` and `edge`,
`graph/schema.rs`), which is a *shape* change CozoDB can't `ALTER`, so it trips
the existing reset-migration: on first launch after upgrade an old `graph.db`
is `reset()` and fully re-derived from source (every row is re-derivable, so no
data is lost). Both columns carry a `default 'inferred'` so a partially-written
row is never silently `Extracted`. If you add another graph relation column,
bump the version again and note it in `MAINTENANCE.md` § Schema versions —
graph & settings.

**Confidence is a two-layer computation — don't look for `Ambiguous` at parse
time.** The bespoke walkers and the tags engine only ever stamp `Extracted`
(same-file target, or a structural/import/doc edge) or `Inferred` (cross-file,
name-keyed) — that's all a single-file parse can honestly know
(`FileGraph::classify_confidence`, `graph/model.rs`). `Ambiguous` is applied at
**query time**, the only place a name's global candidate count is visible:
`callers`/`references` downgrade to `Ambiguous` when `symbol_count(name) > 1`;
`callees` when a callee name resolved to more than one row; `dependents_transitive`
and `shortest_path` fold it in via `multi_candidate_names()` and carry the
*weakest* link along a chain (`Confidence::weaker`). If you add a new
name-keyed consumer, apply the same override or it will over-claim certainty.

**`graph_path` and `graph_architecture` are idx-only, settings-aware tools.**
They're special-cased in `graph/mcp.rs::dispatch_recorded` (like `graph_impact`)
so they can read `path_max_hops` / `arch_*` from settings — they do *not* fall
through to `run_tool` (which has no settings handle). Both build their adjacency
in Rust from a handful of relation scans (the `transitive`/`dependents_transitive`
pattern), not Datalog recursion. Architecture clustering is deterministic label
propagation (id-sorted, bounded iters) — approximate and honestly labelled
"heuristic"; there is **no** warm-index cache in V1 (computed on demand each
call), so if a large repo makes it slow, add caching keyed off the index epoch.

**The Graph view is a section inside the Tool Activity tab** (it was its own
reserved tab until schema v26; the v25 → v26 migration plus the integrity
check's `RETIRED_TAB_IDS` prune drop old `graph-view` entries). Gated by
`graph.graph_viz` (default off); `ToolActivityView.svelte` mounts it lazily on
the first section visit and then keeps it alive hidden (display:none) so the
laid-out simulation survives section switches. The visualization
is a **self-contained** Canvas 2D force graph in `src/lib/GraphView.svelte` — no
three.js / d3 dependency was added, keeping the bundle lean and offline. Live
activity is a 1.5 s poll of `graphHistory()` (there's no push event for
individual tool calls), matching `GraphCall.target` to rendered nodes; a real
traversed-edge highlight isn't reconstructable because `GraphCall` carries only
a single `target` string, so callers/callees calls approximate it via the node's
incident call edges. If a tool-call push event is ever added, switch the poll
to it.

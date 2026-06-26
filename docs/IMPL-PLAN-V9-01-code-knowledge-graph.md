# Implementation Plan — V9-01 Code Knowledge Graph

Execution plan for [MILESTONE-V9-01](MILESTONE-V9-01-code-knowledge-graph.md), grounded in the actual ccImp codebase (single `ccimp` crate, edition 2021, Tauri 2 + Svelte). This is the *how/sequence*; the milestone doc is the *what/why*.

## Guiding principles

- **Mirror existing patterns exactly.** The `offload` module is the template for almost everything: settings (`OffloadSettings`), reserved tab (`OFFLOAD_SERVER_TAB_ID` / `TabId::OffloadServer`), app-owned service (`Arc<OffloadService>` via `app.manage`), MCP server (`offload/mcp.rs`), loopback (`offload/loopback.rs`), native tools (`offload/tools/`), IPC (`#[tauri::command] -> AppResult<T>`), frontend non-PTY tab (`OffloadServerView.svelte` + `isOffloadTab` branch in `Pane.svelte`).
- **Stage the heavy dependencies.** `cozo` and `tree-sitter` dominate build time and risk. Land the *wiring* first (no heavy deps, fast `cargo check`), then the *parser* (tree-sitter), then the *store* (cozo). Each stage has a green checkpoint.
- **`cargo check` from `src-tauri/` is the iteration loop** (default features, no GPU SDK, any shell). Full builds only when needed. `npm run check` for the frontend.

## Dependency staging

| Stage | Crates added | Build cost | Gate |
|---|---|---|---|
| 1 (wiring) | none | trivial | `cargo check` green with the module skeleton + settings + error variants |
| 2 (parser) | `tree-sitter`, `tree-sitter-rust` (+ TS/JS/Py/MD later) | moderate (C compile, one-time) | builder unit test: a fixture Rust file → expected symbols/edges |
| 3 (store) | `cozo` (`storage-sqlite`) | **heavy** (large dep tree, first build slow) | index round-trip test: build → query `find_symbol` from a real `graph.db` |
| 4 (search) | `ast-grep-core` | light | `struct_search` matches an AST pattern |
| 5 (watch) | `notify`, `ignore` | light | incremental re-index test |
| 6 (embed) | none (reuse `reqwest`) | — | semantic round-trip against a local `/v1/embeddings` |

> **cozo risk:** the first `cargo check` after adding `cozo` may take many minutes and could surface a toolchain issue on Windows/MSVC. Validate it in isolation (add dep, `cargo check`, nothing else) before writing code against it. SQLite backend is bundled (rusqlite `bundled`) — *not* the RocksDB/`cozorocks` C++ backend.

## Phase → file map (grounded)

**Phase A — schema + builder (one language, full build)**
- `error.rs` — add `GraphNotReady(String)`, `Graph(String)` variants (mirror `OffloadNotReady`/`Offload`).
- `settings/schema.rs` — add `GraphSettings` (mirror `OffloadSettings`: `#[serde(default)]` + manual `Default`; no secrets → derive `Debug`), `pub graph: GraphSettings` on `Settings` + its `Default`, and `GRAPH_MONITOR_TAB_ID` const. Additive — no `schema_version` bump.
- `graph/mod.rs` — `mod model; mod builder; mod schema; mod index; mod query; mod service;` + re-exports. Register `mod graph;` in `main.rs` (after `mod error;`).
- `graph/model.rs` — the language-independent IR: `Lang`, `SymbolKind`, `EdgeKind`, `Symbol`, `Reference`, `Edge`, `DocChunk`, `FileGraph`, plus `symbol_id()` and `Lang::from_path()`. (Stage 1, pure Rust, unit-tested.)
- `graph/builder.rs` — `parse_file(path, src, lang) -> FileGraph` via tree-sitter. (Stage 2.) Start with **Rust** node-kind extraction (functions/structs/enums/traits/impls/consts + call/use edges + doc-comments), generalize to `tags.scm` in Phase E.
- `graph/schema.rs` + `graph/index.rs` — CozoDB relation DDL + `GraphIndex` (open/create `<root>/<db_subdir>/graph.db`, full build, `stats()`). (Stage 3.)

**Phase B — query API + first MCP tool**
- `graph/query.rs` — typed Datalog queries: `find_symbol`/`references`/`callers`/`callees`/`imports`/`transitive`/`outline`/`neighborhood`/`search_docs`/`struct_search`. Token-bound every result.
- `offload/mcp.rs` — add `graph_*` tool descriptors to `tools/list`, dispatch in `handle_tools_call` (resolve project root from cwd → query). End-to-end in a real `claude` session.

**Phase C — app-owned service + warm index + loopback**
- `graph/service.rs` — `GraphService` (`AppState`-managed via `app.manage`), `project_root → GraphIndex` map, warm handles. Construct + manage in the setup hook (mirror `start_offload_runtime`).
- `offload/loopback.rs` — add `("POST", "/graph/query")` arm (reuse the bearer-token auth + `handle_conn` dispatch).
- `offload/agent.rs` — add `graph_*` to the native tool router (`NativeRouter`) for the offload worker.
- `main.rs` — construct `GraphService`, manage it, close handles on `CloseRequested` (alongside `shutdown_all`).

**Phase D — watcher** — `graph/watcher.rs` (`notify`, debounced), per-root, honoring ignore rules; staleness check on open.

**Phase E — more languages + docs** — TS/JS/Py grammars + `tags.scm`; stack-graphs `tsg` where available; Markdown → `doc_chunk`.

**Phase F — settings + status** — `ipc/commands.rs`: `graph_status`, `graph_rebuild`, `graph_pause_watch`, `graph_rebuild_embeddings`, `graph_test_query` (register in `generate_handler!`). `SettingsApp.svelte` graph section; `src/lib/settings/{types,store}.ts` + `src/lib/graph.ts`.

**Phase G — semantic search** — `graph/embed.rs` (OpenAI `/v1/embeddings`, opportunistic back-fill, epoch tagging); CozoDB HNSW + `graph_semantic_*`; full-text fallback.

**Phase H — schema/deps/docs** — finalize `Cargo.toml`, third-party-licenses, DESIGN/README/MAINTENANCE/CHANGELOG.

**Phase I — monitor tab** — `graph/events.rs` (`GraphEvents` bus + throttled snapshot); `graph_monitor_subscribe/unsubscribe` IPC; `GRAPH_MONITOR_TAB_ID` reserved tab (`TabId::GraphMonitor`, `default_graph_monitor_tab()`, `reconcile_graph_monitor_tab()` in `persistence.rs`); `GraphMonitorView.svelte` + `isGraphTab()` branch in `Pane.svelte`.

## Verification per stage

- Rust: `cargo check` then `cargo test graph::` from `src-tauri/`.
- Frontend: `npm run check` (svelte-check) + `npm run test` (vitest) where applicable.
- Manual: enable graph in Settings, launch a Claude tab, confirm `graph_find_symbol` answers from the index; the Code Graph tab renders (Phase I).

## Session progress

- [x] Milestone doc committed (7a93e21).
- [x] **Stage 1 (wiring)** — settings/error/IR scaffold, committed (ea0bf82), `cargo test graph::` green.
- [x] **Stage 2 (parser)** — tree-sitter Rust extraction, committed (a09edd2), 6 tests green.
- [x] **Stage 3 (store)** — CozoDB (sqlite+rayon, no graph-algo/rocksdb) `GraphIndex`: open → `index_file_graph` → `find_symbol`/`stats`, idempotent per-file replace. Round-trip test green. This proves Phase A + the first slice of Phase B end-to-end.
- [x] **Stage 3b (query API)** — `callers`/`callees`/`references`/`imports`/`outline`/`transitive`(recursive Datalog)/`search_docs` + `open_existing`, all unit-tested green (8 graph tests).
- [x] **Phase B (MCP wiring)** — `graph/mcp.rs`: `graph_*` tool descriptors + dispatch, advertised only when `graph.enabled`, resolving the project root from cwd and opening `graph.db` read-only (the self-contained path). Wired into `offload/mcp.rs` `tools/list` + `tools/call`. Compiles green.
- [ ] **Next: app-side indexer** — nothing builds `graph.db` at runtime yet (only tests do), so the MCP tools currently report "no code graph — index this project". Wire a `GraphService` (Phase C) that builds the index on project open + a `graph_rebuild` IPC, so the tools have data end-to-end. Then: ast-grep `struct_search`, loopback route, watcher (D), more langs (E), settings UI (F), semantic (G), monitor tab (I).
- [ ] Also pending: extend the `--mcp-config` injection gate in `tabs/config.rs` so the offload-mcp server is injected when `graph.enabled` even if offload is off (today graph tools ride on the offload server, so they're only reachable to Claude when offload is also enabled).

> **cozo build note (resolved):** use `default-features = false, features = ["storage-sqlite", "rayon"]`. Default features pull `graph-algo` → `graph_builder 0.4.1`, which fails to compile against the current `rayon` (a real upstream incompatibility). We don't need cozo's built-in graph algorithms — transitive/reachability are plain recursive Datalog.

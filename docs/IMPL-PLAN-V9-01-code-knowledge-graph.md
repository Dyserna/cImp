# Implementation Plan — V9-01 Code Knowledge Graph

Execution plan for [MILESTONE-V9-01](MILESTONE-V9-01-code-knowledge-graph.md), grounded in the actual cImp codebase (single `cimp` crate, edition 2021, Tauri 2 + Svelte). This is the *how/sequence*; the milestone doc is the *what/why*.

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

- [x] Milestone doc committed (334c15e).
- [x] **Stage 1 (wiring)** — settings/error/IR scaffold, committed (6b2e901), `cargo test graph::` green.
- [x] **Stage 2 (parser)** — tree-sitter Rust extraction, committed (717d07a), 6 tests green.
- [x] **Stage 3 (store)** — CozoDB (sqlite+rayon, no graph-algo/rocksdb) `GraphIndex`: open → `index_file_graph` → `find_symbol`/`stats`, idempotent per-file replace. Round-trip test green. This proves Phase A + the first slice of Phase B end-to-end.
- [x] **Stage 3b (query API)** — `callers`/`callees`/`references`/`imports`/`outline`/`transitive`(recursive Datalog)/`search_docs` + `open_existing`, all unit-tested green (8 graph tests).
- [x] **Phase B (MCP wiring)** — `graph/mcp.rs`: `graph_*` tool descriptors + dispatch, advertised only when `graph.enabled`, resolving the project root from cwd and opening `graph.db` read-only (the self-contained path). Wired into `offload/mcp.rs` `tools/list` + `tools/call`. Compiles green.
- [x] **App-side indexer (Phase C core)** — `graph/service.rs`: `GraphService` (`app.manage`d beside the offload service) builds `<root>/<db_subdir>/graph.db` at runtime. `spawn_rebuild` runs the build on a dedicated worker thread (off the async runtime) with `idle/building/ready/error` status bookkeeping + a `graph-status` event; the build core is the free `build_tree` fn (gitignore-respecting `ignore::WalkBuilder` walk → lang/size filter → `parse_file` → `index_file_graph`), preceded by `GraphIndex::reset()` so deleted files leave no stale rows. Startup builds the launch root when `graph.enabled` (+ a settings watcher kicks one on a runtime false→true enable); `shutdown()` drops warm handles on close. IPC: `graph_status` (list known-root statuses) + `graph_rebuild(root?)` registered in `generate_handler!`. **End-to-end now closed:** the app builds the store the MCP tools read. New dep: `ignore = "0.4"` (ripgrep's walker). `cargo check --bins` clean; `cargo test graph::` 9 passed.
- [x] **Injection gate (graph-independent)** — `tabs/config.rs` `build_pre_args` now injects the `--offload-mcp` `--mcp-config` when `offload.enabled || graph.enabled` (graph tools reach Claude even with offload off), and appends a `GRAPH_GUIDANCE` system-prompt nudge listing the `graph_*` tools when `graph.enabled`. 13 `tabs::config` tests green (2 new: graph-only mcp inject + graph guidance).
- [x] **Watcher (Phase D)** — `graph/watcher.rs`: per-root `notify` recommended watcher → a debounce thread (coalesces a burst of saves over `watch_debounce_ms`, default 300ms; skips `Access` events) → `GraphService::reindex_paths`. Incremental apply: re-parse created/modified files (gitignore-filtered via `build_gitignore` + lang/size filter shared with the full walk through `lang_for`), drop rows for deleted ones via new `GraphIndex::remove_file` (factored out of `index_file_graph`), then refresh status counts. A coarse `write_lock` serializes watcher batches against full rebuilds (no write mid-`reset()`). Watcher handle owned in `GraphService.watchers` (kept alive; dropped on shutdown → debounce thread exits). `start_watch` (idempotent) wired into startup + runtime-enable beside `spawn_rebuild`. New dep: `notify = "6"`. 11 graph tests green (added `remove_file` drop-all + `lang_for` filter). **Live-unverified:** real `notify` events + debounce only smoke-tested by unit logic, not against the running app yet.
- [x] **Offload-worker graph access (rest of Phase C, consumer #2)** — the local offload worker can now query the graph too, completing the "queryable by BOTH consumers" milestone goal. `graph/mcp.rs` refactored to a shared core: `tool_specs()` (single source of truth) + `run_tool()` (dispatch+format) + `offload_query(roots, name, args)`; the MCP descriptors and the worker's `ToolDef`s both derive from `tool_specs()` (parity-tested). `offload/tools/graph_tools.rs` builds the worker `ToolDef`s + dispatches; routed in `tools::dispatch` (`graph_*` arm). **Privacy gate (user-decided):** local worker always gets graph tools; a **remote** backend (LAN *or* cloud) only when `graph.allow_remote_worker_access` is opted in — `worker_graph_allowed(enabled, is_remote, allow_remote)` computed in `service.rs::run_on` (new `PoolEntry.is_remote`), tools added to `native_defs` only when allowed, and re-gated in `HostRouter::call` as defense-in-depth (new `allow_graph` field). No `LOCAL_DATA_TOOLS` rework needed. **Settings UI:** new "Code graph" section in `SettingsApp.svelte` — Enable toggle, Rebuild-index + Refresh-status buttons with a live counts readout, and the remote-worker-access checkbox behind a ⚠ Privacy warning. Frontend `GraphSettings` type + `defaultSettings.graph` + `src/lib/graph.ts` (`graphRebuild`/`graphStatus`). Tests: backend 352 passed (added `worker_graph_gate_truth_table` + `defs_mirror_the_shared_specs`); `npm run check` 0 errors.
- [x] **Phase E (docs slice) — Markdown chunking + ignore globs + index_docs** — `builder.rs::parse_markdown` chunks Markdown into `doc_chunk`s by ATX heading (fence-aware so `#` inside ```` ``` ```` isn't a heading; GitHub-style slugs, de-duped `foo`/`foo-1`; pre-heading preamble → file-stem anchor), so `graph_search_docs` now covers READMEs/design docs, not just Rust doc-comments. `build_tree` now honors the `graph.ignore` globs (an `ignore::overrides::OverrideBuilder`, each pattern `!`-prefixed since overrides are whitelists) and the `index_docs` toggle (off → skip markdown; the watcher's `reindex_paths` skips it too). 13 graph tests green (added `markdown_chunks_by_heading` + `rebuild_indexes_markdown_docs_and_honors_index_docs_toggle`).
- [x] **Phase E (languages) — TS/JS/Python extraction** — `builder.rs` now extracts TypeScript, JavaScript (incl. JSX), and Python, so the graph covers the whole repo, not just Rust. Added grammar crates `tree-sitter-typescript 0.23.2`, `tree-sitter-javascript 0.25.0`, `tree-sitter-python 0.25.0` (build clean against ts 0.26 — they expose `LANGUAGE*` via the `tree-sitter-language` shim, so the grammar versions don't need to track core). JS/TS walk (`walk_js`): functions/classes/methods/interfaces/enums/type-aliases + `const f = () => …` arrow/function consts, `export …` unwrapping (JSDoc attaches across the export), import edges (unquoted specifier), call edges (`member_expression` → property), class→method containment, `/** */` JSDoc. Python walk (`walk_py`): `def`/`class` (+ `@decorated`), method-vs-function by class nesting, docstring → doc, `import`/`from … import` edges, `attribute` call names. Shared `emit_symbol`/`take_doc` helpers. `.tsx` uses `LANGUAGE_TSX`. 16 graph tests green (added `extracts_typescript`/`_javascript`/`_python`).
- [x] **Phase G (semantic search)** — embed client + CozoDB HNSW vector store + epoch + opportunistic backfill + `graph_semantic_docs` (full-text fallback) + status. Committed `a2b8bb7`.
- [x] **Phase I (monitor tab)** — reserved app-rendered Code Graph tab + `GraphMonitorView`. Committed `1b94b35`.
- [x] **Phase F (settings UI)** — full graph config panel. Committed `7e7371f`.
- [x] **Stage 4 (structural search)** — implemented as `graph_struct_search` via **tree-sitter's native Query API** over the existing grammars (NOT ast-grep — avoided the dependency/version conflict; tree-sitter queries are LLM-writable and the engine re-exports `StreamingIterator`). `builder.rs`: `language_for(lang)` + `struct_search(lang, pattern, files, max_rows, max_snippet)` (compiles the query once, runs over each file, bounded). `index.rs`: `files_for_lang`. `mcp.rs`: `graph_struct_search` spec in the shared set (both consumers), `run_struct_search` reads the indexed files of a language from disk (root threaded via `open_project_index → (root, idx)`) and runs the query. `model.rs`: `Lang::from_tag`. 361 tests pass (added `struct_search_matches_by_ast_shape` — `#eq?` predicate finds exactly the `.unwrap()` calls + malformed-query error).
- [ ] **Remaining (optional polish):** loopback `POST /graph/query` warm path for the cloud MCP child (avoids the read-only sqlite double-open during a rebuild — rare); stack-graphs precise name resolution (upgrade over current name-level call edges). **The milestone is otherwise feature-complete (A–I); next step is the holistic live test in the running app.**

> **cozo build note (resolved):** use `default-features = false, features = ["storage-sqlite", "rayon"]`. Default features pull `graph-algo` → `graph_builder 0.4.1`, which fails to compile against the current `rayon` (a real upstream incompatibility). We don't need cozo's built-in graph algorithms — transitive/reachability are plain recursive Datalog.

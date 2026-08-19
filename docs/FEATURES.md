# cImp — Feature Inventory

A scannable, one-line-per-feature inventory of everything cImp does, grouped by
area. Current as of **v0.30.0**. Companion to `MAINTENANCE.md` (which covers the
dependency/component breadth) — this is the *capability* breadth.

## Core App & Terminal
- Multi-tab terminal with a recursive split-pane layout tree (horizontal/vertical)
- Draggable tabs that re-parent between panes while keeping xterm state, scrollback, and PTY
- Resizable splitters with min-size clamping and per-split stored ratios
- Focus management — avatar, audio, and compose all follow the focused pane's active tab
- Named layout presets: save / restore / rename / manage, with a recent-presets list
- PTY management with per-tab scrollback persistence across restarts
- Platform shell detection (Git Bash on Windows w/ registry probe, `$SHELL` on Linux)
- Windows console-window suppression for spawned subprocesses

## AI Tabs
- Three AI-tool tab types, each running its tool's native fullscreen TUI
- Cloud Claude tab (subscription OAuth or `ANTHROPIC_API_KEY`)
- Claude (local) tab with `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_MODEL` injected at spawn
- Claude (local) targets a local Anthropic-compatible proxy (LiteLLM bridging Ollama / LM Studio / vLLM / llama-server)
- OpenCode tab — manages its own providers/credentials; cImp injects only MCP tools + TTS/offload/graph guidance
- "AI tabs enabled" checkboxes (Claude / Claude (local) / OpenCode); Claude only on a fresh install
- Fullscreen interaction: mouse kept shell-like (select-copy, Shift+right-click paste) with a hold-`Alt` bypass to the TUI
- Per-type tab duplication via the `+` button, cloning live config, auto-named & closable
- Tab context menu: Configure / Restart / Rename / TTS toggle
- Compose submits to the focused pane's active tab

## Shell Tabs
- Custom shell tabs (`Ctrl+T` or end-of-bar `+`) with command/args/cwd config
- Restart-shell without losing the tab; closed-shell overlay with restart button
- Reduced notification set (error / exited) for shells
- Alternative shells documented (WSL, pwsh, cmd, zsh)

## Text-to-Speech
- Local Kokoro-82M ONNX synthesis (Apache 2.0), offline
- Master enable that loads/unloads the Kokoro model (frees memory when off; distinct from mute), built off the async runtime
- Out-of-band sourcing: Claude transcript JSONL tail + OpenCode `/event` stream (no terminal scraping, no `[[TTS]]` markers)
- Markdown reduced to speakable prose (code/tables/tool output/reasoning dropped), sentence-segmented and deduped
- Per-tab speak toggle (gates whether a tab reads its assistant prose), read live; `Esc` stops the current burst
- Voice-pack selection auto-discovered from `models/voices/`
- Speed and volume controls
- Speak-selection gesture (`Ctrl`+right-click)
- WebGPU GPU backend (shipped, vendor-agnostic) with automatic CPU fallback; optional CUDA (not shipped)
- GPU/CPU selectable at runtime in *Settings → Audio* ("Process on"); switching reloads the model, no restart (supersedes the old `CIMP_GPU` env var)

## Speech-to-Text (Dictation)
- Offline dictation via whisper.cpp (MIT), selectable GGML models (default `small`)
- Enable toggle loads/unloads the Whisper model (frees memory when off) and gates the record button + push-to-talk
- Toggle-button and push-to-talk (hold `Ctrl+Shift`) capture, with debounce + abort-on-other-key
- Microphone device selection; VAD / silence handling; rubato resample to 16 kHz mono
- Transcript appended to the compose overlay for review
- Vulkan GPU backend (shipped, vendor-agnostic) + optional CUDA; CPU default

## Avatar & Visualizer
- Picture/Video avatar with per-state overrides; animated frame-based sprite avatars (default `impSprites`)
- Five states: Idle / Listening / Thinking / Speaking / Error, with crossfade transitions
- Visibility, position, size, opacity controls; nearest-neighbor pixel scaling
- Real-time waveform visualizer (playback + mic), configurable color/width/glow/opacity
- Idle optimization that parks the animation frame when silent

## Compose Overlay
- Slide-up textarea (`Alt+Enter`), submit `Ctrl+Enter`, min/max height, draft persistence

## Task Offload (V8)
- Offload subtasks from cloud Opus to a local/remote llama-server via the `offload_task` MCP tool
- Backend pool with fast/quality tiers; local, remote-LAN, and consent-gated cloud backends
- Router: readiness → tool-need → context budget → tier, with spill-on-busy and fail-over
- Global + per-backend concurrency gates, slot tracking, fast-reject backpressure
- Per-backend tool scope (all / web+docs only); cloud data-egress consent enforced
- Honest in-flight counts published into the tool description
- Warm-pool MCP host (stdio + Streamable-HTTP servers), tool namespacing, read-class-only filter, filesystem confinement, per-server health isolation
- Native tools `read_file` / `code_search` / `run_command` with command security policies
- Loopback endpoint + discovery file with per-launch bearer token; self-contained fallback for headless runs
- `offload_task` + graph tools exposed to both Claude and OpenCode tabs (OpenCode via a `--consumer opencode` child)
- Read-only Offload server dashboard (an "Offload server" sub-tab of the Tool Activity tab): per-backend metrics, queue depth, throughput, request history

## Code Knowledge Graph (V9-01)
- Per-project incremental graph in `.cimp/graph.db` (CozoDB + SQLite)
- Languages (V9-02): full symbol/call graph for Rust, TypeScript, JavaScript,
  Python, Go, Java, C, C++, C#, PHP, Bash, Scala, OCaml, Ruby, Haskell, Kotlin,
  Swift, SQL, Erlang, R, Perl, Ada (generic tree-sitter `tags.scm` engine);
  struct-search for HTML, CSS, JSON, YAML, XML, assembly; Markdown for docs
- Symbol extraction, call/import graphs, transitive reachability, cross-file name binding
- Markdown/doc-comment parsing + full-text search; semantic search via Qwen3-Embedding + HNSW
- Structural AST pattern search; FS watcher for incremental re-index
- MCP graph tools: `graph_find_symbol`, `graph_callers`, `graph_callees`, `graph_references`, `graph_imports`, `graph_outline`, `graph_transitive`, `graph_search_docs`, `graph_struct_search`
- Symbol visibility bit (Rust `pub`/`pub(crate)`, JS/TS `export`, Python `_` convention, Go capitalization)
- Reserved **Code Intelligence** tab (renamed from Code Graph): Overview (usage) / Memory / Context / Analyses / Trace path / Architecture; the index dashboard + rebuild actions moved to **Tool Activity → Graph index**, the activity feed to **Tool Activity → Activities**

## Code Intelligence — Context Engine (V10)
- Session/action memory: per-project record of files each session read/edited/queried + agent notes; `context_recall` / `context_note` (pinnable) / `context_notes`; survives index rebuilds; Memory tab section (working set, notes, sessions, clear)
- Per-agent memory scoping: a Claude tab and an OpenCode tab on the same project keep separate sessions, so recall/notes don't cross between them
- Automatic context injection (opt-in): ranks prompt-relevant files and prepends a budget-bounded outline digest — Claude via a `UserPromptSubmit` hook, OpenCode via a generated dependency-free `.opencode/plugin`; session-hot files first; live preview + per-file/per-turn budgets + min-score gate
- Packaged analyses: dead exports (candidate unused public symbols) + import cycles; `graph_dead_exports` / `graph_cycles` tools + Analyses tab section; UI states which languages each analysis covers (dead exports: Rust/JS/TS/Python/Go; cycles: JS/TS/Python/Rust)
- Activity history distinguishes the caller (claude / opencode / offload) per graph & memory tool call
- Stale-schema safety: an older `graph.db` is transparently rebuilt on the app side; read-only consumers (MCP child, offload worker) are told to rebuild rather than served an emptied index
- Local loopback surfaces: `POST /context/retrieve`, `POST /memory/event` (same authenticated-localhost trust model as `/graph_run`)

## Code Intelligence — Token Efficiency (V11)
- `graph_snippet`: fetch one definition's body (by symbol, or `file`+`line`) instead of reading the whole file; ambiguous names return a disambiguation list, whole-file symbols fall back to an outline hint; byte-capped (`max_body_bytes`, default 16 KiB) and flagged `stale` when the on-disk hash has drifted from the index
- `graph_repo_map`: a budget-bounded (`repo_map_budget_chars`) map of the project's most call-central files and their top exported signatures, for orienting without exploring; session-hot files rank higher; agent-pullable any time, plus an opt-in once-per-session injection on a session's first prompt (`repo_map_on_session_start`)
- Injection dedup: a file already injected in full is demoted to a one-line "unchanged" reminder on later turns until it changes (`(updated)` tag) or `context_dedup_ttl_turns` (default 10) elapses; per-session, in-memory
- Compaction survival: a Claude `PreCompact` hook (`cimp --precompact-hook` → `POST /context/compaction`) feeds the compactor the session's ranked working set + pinned notes and clears the dedup state so the next turn re-injects fresh (`compaction_context`, default on)
- Redundant-read advisor (opt-in, off by default): a Claude `PreToolUse` hook on `Read` (`cimp --read-hook` → `POST /context/should_read`) denies a re-read of an unchanged file with its outline (`advise`) or outline + relevant symbol body (`substitute`) as the reason — never a bare refusal; one reminder per file per session; passes everything right after a compaction (`read_advisor` / `read_advisor_min_lines` / `read_advisor_mode`)
- Local-model context digests: for files with no useful outline (docs/configs/long scripts), the local offload backend caches a ≤3-line digest (never routed off-box); `context_llm_digests`, needs a ready local backend
- Code embeddings + `graph_semantic_code`: symbol-level semantic code search mirroring the doc-embedding pipeline, returning `file:line · kind · signature · distance` (never bodies) to chain into `graph_snippet`; gated on `embed_code_bodies` **and** `semantic_search` (shared embedder); `semantic_code_max_chunks` caps the backfill
- Index card shows cached code-embedding coverage (`code: N/M chunks`) and cached digest count (`N context digests cached`)

## Code Intelligence — Agentic Inner Loop (V12)
- `run_check`: run one of the project's configured checker commands (`checks: [{name, cmd, parser, timeout_secs}]` in `.cimp/config.json`) and get back deduplicated, structured diagnostics grouped by severity + code + normalized message (≤5 sample sites each) instead of a raw dump; shipped parsers `cargo-json`, `tsc`, `eslint-json`, `pytest`, `generic-gcc`; `changed_only` filters to git-changed files; a model-supplied `name` only selects among the project's configured checks, never a raw command; doesn't require the code graph; exposed to both cloud tabs (MCP) and the offload worker's native tool set
- `graph_impact`: blast radius of the current working-tree diff vs HEAD (or an explicit `symbols` list) — changed symbols → transitive dependents (name-keyed, approximate), depth-labeled, plus a file-level rollup and changed-but-unindexed files called out separately; `include_tests: true` appends the affected tests; also an Analyses-section button ("Impact of working-tree changes")
- Test↔symbol mapping: `symbol.is_test` populated per language (Rust `#[test]`/`#[tokio::test]`/`rstest` + `#[cfg(test)]` modules including `any(test)`/`all(test)`; JS/TS `*.test.*`/`*.spec.*`/`__tests__`; Python `test_*` in `test_*.py`/`tests/`; generic path heuristics elsewhere); `graph_tests_for { symbol | file }` returns the transitive callers filtered to tests — candidates, not guarantees (dynamic-dispatch caveat)
- Git-aware context: a `commit_touch` relation (90-day `git log` window, file-level only) boosts recently-churned files in context-injection ranking (+3 within 7 days, +1 within 30) and adds a `last change: "…" (3d ago)` trailer to digests and `graph_find_symbol` rows; `graph_recent_changes { days?, path_prefix? }` tool surfaces churn-ranked files with subjects; not a git repo → feature simply absent
- Memory distillation (off by default, `memory_distillation`): an idle-session sweep (quiet > 24h) sends the session's working set + notes to the **local-only** offload path (never remote) to extract ≤3 non-obvious durable `project_fact`s, capped at 100 live facts (oldest unpinned archived first); facts surface in `context_recall`, boost retrieve ranking on a whole-word file-stem mention, and get a Memory-section **Facts** list (pin/edit/delete/add manually); opt-in `promote_pinned_facts` appends only pinned facts to the launch-time guidance payload so durable knowledge arrives with zero tool calls
- Proactive automation (opt-in, off by default — same posture as the V11 read advisor): a Claude `PostToolUse` hook on `Edit`/`Write`/`MultiEdit` (`cimp --postedit-hook` → `POST /context/post_edit`) debounces edit bursts (`auto_check_debounce_s`, default 5s), runs the configured checks `changed_only`, and injects only new/worsened diagnostics since the session's last run; appends a two-line blast-radius note when the edited symbol has ≥ `auto_impact_min_dependents` (default 10) dependents; check runs are single-flight per project root so concurrent Claude/OpenCode tabs share one run; dead exports/import cycles also re-run after every completed index pass (`analyses_auto`, default on) and badge the Analyses section on change

## Workbench — Vibe-Coding Guardrails (V13)
- Reserved **Workbench** tab (default on, `Settings → Workbench`), sectioned Diff / Timeline / Worktrees, same left-rail pattern as Code Intelligence
- Live diff pane: working-tree diff vs `HEAD` (spawned `git`), re-diffed on the shared `fs-batch` event (500ms debounce, 5s poll fallback); virtualized file list, unified/side-by-side toggle, intra-line word-diff; non-git projects diff against the latest checkpoint when checkpoints are on
- Per-hunk **Revert** (`git apply --reverse`; refuses mid-merge/-rebase and on a stale hunk hash), **Copy**, and **Send to agent** (hunk + `file:line` into the compose overlay, targeted at the focused AI tab)
- Status-bar `±N` changed-files badge; click focuses/opens the Workbench tab
- Cross-agent checkpoints (off by default): automatic snapshots into a separate `.cimp/shadow.git` store that never touches the user's `.git`; triggers are per-prompt (tapped from the same shim as context injection, records the triggering agent), a debounced file-activity burst, and manual "Checkpoint now"; works even before `git init`
- Timeline section: snapshot list (trigger, agent, files changed) with **Diff vs now** / **Restore**; restore always takes a "pre-restore" safety snapshot first, re-creates deleted files, and deletes files created since only with an explicit opt-in checkbox (default off)
- Worktree manager: "New Claude/OpenCode tab in worktree…" creates `git worktree add .cimp/worktrees/<slug> -b cimp/<slug>` and spawns the AI tab with `cwd` set there (`⑂ slug` tab title); Worktrees section shows ahead/behind, diff vs base, **Merge** (refuses on a dirty/wrong-branch main tree, aborts cleanly on conflict), **Discard** (double-confirmed, cImp-created worktrees only), **Open shell here**; `git worktree prune` on app start
- Merge-readiness chip (soft-dep on V12 `run_check`): latest `changed_only` check result per worktree, advisory only

## Workflow & Visibility (V14)
- Prompt library: parameterized compose templates, global (`settings.json`) + per-project (`.cimp/config.json` overlay, shadows same-named global); `{selection}`/`{clipboard}` resolve on insert, other `{name}` placeholders become tab-stops; `/`-on-empty-textarea fuzzy picker (also a 📋 button and a rebindable shortcut, default `Alt+/`); managed under *Settings → Compose*; 4 starter templates (deletable)
- Image paste/drop into compose: Tauri-clipboard paste or drag-drop of image files → an attachment chip → submit appends the local path(s) to the message (both Claude Code and OpenCode read local image paths); pasted images land in a per-launch temp dir (`%TEMP%/cimp-attach/<launch-id>/n.png`), age-pruned (>3 days) at startup and on graceful exit; dropped files are referenced in place
- Token/cost X-ray: a **Usage** section (6th, after Analyses) in Code Intelligence — per-turn stacked bars (input/cache-read/output/est. tool-result), a top-consumers table, per-session totals + cache-hit ratio, and an effectiveness panel (chars injected / suppressed-by-dedup / displaced-by-read-advisor); transcript `usage` fields are exact, everything else is labeled `est.`; sourced from the OOB Claude transcript tap into a new `usage_stat` relation (additive, no schema bump) — OpenCode sessions report `est_only` (its event stream carries no token fields); a status-bar session-tokens line
- Budget-tuning advisor: an Advisor card atop the Usage section proposes measured, propose-and-confirm changes to the V10/V11 knobs — raise `context_min_score` (capped) when injected files go unused, raise `read_advisor_min_lines` when reminders are usually followed by a full re-read anyway, lower `context_turn_budget_chars` (only as a real reduction) when unused-injection is high and turns are budget-maxed; each rule has its own minimum-sample gate; Apply writes through the normal settings path and then holds that rule quiet for 3 further sessions (the rates are cumulative, so an instant re-proposal would be judging pre-apply data), Dismiss is remembered per rule at a 10%-bucketed rate and re-fires only on a material shift; rules are versioned and listed in the card's tooltip. `context_min_score` is enforced per file (a below-floor file is dropped even when the turn's top match clears the gate), so raising it genuinely trims the marginal tail
- Localhost preview tab: a user-creatable **Preview** tab — an embedded WebView2 child webview (Tauri's multi-webview API) with a URL bar, back/reload, device-width presets (mobile/tablet/desktop), and **Snapshot → compose** (captures the viewport to PNG straight into the compose attachment); auto-reload on a ~1s quiet period after the shared `fs-batch` event; navigation restricted to localhost/loopback/RFC-1918-private hosts unless `preview_allow_remote` is on; `target="_blank"`/`window.open()` always exits to the system browser; not a general browser (no history UI, no profiles)

## Code Intelligence — Code Graph Parity (V15)
- Edge confidence (always on): every call/reference/edge is tagged `extracted` (same-file or structural — parser-certain), `inferred` (a single cross-file name-keyed guess), or `ambiguous` (the name resolves to >1 definition, applied at query time); badges on `graph_callers`/`graph_callees`/`graph_references`/`graph_impact`; impact adds a confidence split summary and an optional `min_confidence` filter; forces one graph rebuild (`GRAPH_SCHEMA_VERSION` 3→4)
- `graph_path { from, to, kinds?, symmetric?, max_hops? }`: shortest path between two entities ("how does X reach Y?") across call/import/containment edges, each hop labelled with its edge kind and confidence; directed by default, `symmetric` for a plain relatedness walk; endpoints accept a symbol name, `file:line`, or file path; reports honestly when there's no path; bounded by `path_max_hops` (default 8); a **Trace path** section in the Code Intelligence tab
- `graph_architecture`: once-per-project orientation map — god nodes (highest-degree hubs), subsystems (deterministic label-propagation file communities, named by common path prefix), and surprising connections (cross-subsystem edges = candidate accidental coupling); topology only, no LLM/embeddings, honestly labelled heuristic; `arch_max_communities` (12) / `arch_min_community_size` (3); an **Architecture** section in the Code Intelligence tab
- Graph view (stretch, opt-in `graph_viz`, off by default): a section of the Tool Activity tab (formerly its own reserved tab, retired in schema v26) drawing a bounded subgraph as a live 2D/3D force graph (node color = subsystem, size = degree; edge color = kind, dash = confidence), self-contained Canvas 2D (no three.js), pulsing nodes as agents read/edit/query the codebase (cloud-agent vs local-offload colors); responsive DPR-aware canvas, pause-when-hidden, orbit/zoom/pan, legend; capped at `graph_viz_max_nodes` (1500)
- Both new tools reach the cloud Claude session and the local offload worker; mirrored as Tauri commands for the tab

## Code Intelligence — Token Efficiency II (V17)
- Diff-substitute for changed-file re-reads (`read_advisor_diffs`, default on within the advisor): re-reading a file *after it changed* is answered with a line-level unified diff against the snapshot of what the agent actually last read, not the whole file — exact (a diff vs the last-read snapshot can't mislead), so it's safe on the edit-then-verify loop that dominates real sessions; falls back to a normal read when no snapshot survives (small file / >512 KiB / LRU-evicted from the ~16 MiB in-memory budget) or the rendered diff would exceed half the new content; a content change re-arms the file for up to 3 reminders/file/session; Activity marks these `(changed — diff substituted)` so the Effectiveness tooltip can split diff reminds from outline reminds
- Shell-read interception (`read_advisor_shell`, default on within the advisor): a second `PreToolUse` **Bash** matcher on the same `cimp --read-hook` shim intercepts a whole-file shell read (`cat FILE`, `Get-Content FILE`, `type FILE`, `gc -Raw FILE`) of an already-reminded file the same way a `Read` is intercepted — closing the V16 detect-only loop (a bypass used to cost the remind *plus* the whole file); strict parser — only a provable pure whole-file read of one file is intercepted, anything with a pipe/redirect/glob/second-path or a partial-read verb (`sed -n`, `head`, `tail`) runs untouched; the deny reason is prefixed `answered without running the command —`; the `drift.read_bypass.v1` canary now measures *residual* escape routes; zero overlay delta when off; Claude-only (OpenCode rides the pending V16 `tool.execute.before` spike)
- First-read digest tier (`read_advisor_first_read_kb`, default 0 = off): the *first* whole-file `Read` of a large **non-code** file (log, lockfile, generated JSON, data dump — no parsed symbols) at or above the KiB threshold is answered with the cached local-model digest plus a first/last ~40-line sample instead of the full content; source files (any parsed outline) never qualify and a sliced `Read({file, offset, limit})` always passes; needs a digest cached for the current content hash — a miss enqueues one and passes, so protection begins on the next (cross-session) encounter; no snapshot retained (generated-file diffs are useless); proposed starting value 256
- Test-run parsers for `run_check` (V12 grouping extended to the last big raw dump): two new `ParserKind`s — `cargo-test` (stable-toolchain text: `FAILED` lines upgraded with their `---- stdout ----` block truncated to ~15 lines + resolved `panicked at file:line:col`, a counts `Note` on the tail line so a clean run renders `ok — N passed` instead of silence, and a compile-error-before-tests fallback so a broken build still surfaces the rustc error) and `jest-json` (`jest --json` / `vitest --reporter=json` — per-failure diag from `testResults[].assertionResults[]`, ANSI-stripped messages, `testFilePath` relativized against the run cwd, counts `Note`); no new settings — add a check to `.cimp/config.json` (`{ "name": "test", "cmd": "cargo test", "parser": "cargo-test", "timeout_secs": 300 }`, or `"cmd": "npx jest --json"` / `"npx vitest run --reporter=json"` with `"parser": "jest-json"`); `GRAPH_GUIDANCE` now nudges the agent to prefer a configured test check over running the test command in Bash
- Tool-surface accounting: the Effectiveness card prices the advertised graph-tool surface (serialized chars + count of `tools()` post-lean-filter, `est.` tokens, cache-written once per session — the fixed per-session cost of offering the tools) via a new `SurfaceStats` field on `UsageSnapshot`
- Lean tool surface (`lean_tools`, default off): hides the cold-tail `graph_cycles`, `graph_dead_exports`, `graph_struct_search`, `graph_path`, `graph_architecture` from the advertised surface (Claude + offload worker) — advertisement-only, each still ANSWERS if an agent calls it by name (dispatch is name-driven); the `surface.lean.v1` advisor rule proposes turning it on after ≥10 sessions with zero calls to any hidden tool (a single call silences it); one-time editorial pass tightened the wordiest tool descriptions (worker surface ~11,891 → ~11,343 chars)
- Graduation rules (the V11 promise, made executable — propose-and-confirm, no silent flips): `adopt.read_advisor.v1` proposes enabling `read_advisor` when it's off, E1 is verified `pass`, and session memory shows ≥3 redundant large same-file re-reads/session over ≥10 sessions (suppressed while any `drift.*` read rule fires); `adopt.read_advisor_substitute.v1` proposes switching `read_advisor_mode` to `substitute` when the advisor is on in `advise` mode, the remind→full-reread rate is ≤0.2 over ≥20 samples, and bypass is low; both use the standard Advisor card, per-rule dismiss memory, and the V16 apply cooldown
- No `graph.db` schema bump, no new hooks/routes/CLI subcommands — all new state is in-memory session state or reads existing relations; the read-advisor escalation (diffs/shell/first-read) is gated under the existing `read_advisor` opt-in

## Code Intelligence — run_check Generalization (V22)
- `run_check` parser coverage extended from three ecosystems to twelve `ParserKind`s: `cargo-json`, `cargo-test`, `tsc`, `eslint-json`, `jest-json`, `pytest`, `generic-gcc` (existing) plus `sarif` (SARIF 2.1 — ruff/clang-tidy/golangci-lint/semgrep/ESLint-SARIF/CodeQL and most modern lint/security tools, one physical location per result), `go` (`go build`/`go vet` text, `# package` headers, severity-less lines → Error), `go-test-json` (`go test -json` event stream — one diag per failed test + a counts summary), `dotnet` (MSBuild canonical `file(line,col): error|warning CSnnnn:` for C#/F#/VB), `junit-xml` (Surefire/Gradle/Kotlin/PHPUnit report files via quick-xml — one failed `<testcase>` ⇒ one diag), and `regex-custom` (universal escape hatch — a user regex with named `file`/`line`/`message` groups + optional `col`/`severity`, validated at save time by `parsers::validate_pattern` so a bad pattern is a UI error, not a silent zero-diagnostics run)
- `CheckDef` gains `cwd` (relative, confined under the project root — fixes nested manifests / monorepos without `--manifest-path` hacks), `env` (forced at spawn, values redacted in `Debug`), and `report_file` (confined, resolved relative to the check's working directory — `cwd` if set, else the project root, with a root-relative fallback for old configs — the parser reads that file's content after the run instead of stdout, for tools that write a report — a missing file ⇒ explicit error diag, not empty success); all `#[serde(default)]` so old `.cimp/config.json` overlays deserialize unchanged; effective-cwd passthrough + `reroot_diags` keep diagnostic `file` paths project-root-relative when `cwd` is nested
- Rust↔TS drift tripwire: a `checks/mod.rs` unit test `include_str!`s `src/lib/settings/types.ts` and asserts every `ParserKind` wire name and every `CheckDef` field key appears there — adding a Rust variant/field without the TS mirror fails `cargo test` (closes the pre-existing drift where `cargo-test`/`jest-json` were missing from the TS union)
- Checks editor (`src/lib/settings/ChecksEditor.svelte`): add / edit / delete configured checks; a parser dropdown (all kinds) that reveals the `pattern` field for `regex-custom` (live-validated via a `checks_validate_pattern` IPC) and `report_file` for `junit-xml`/`sarif`; `cwd` and `env` (reuses `EnvEditor`); per-check **Test** button that dry-runs through `checks::run` and shows exit status + parsed diagnostic count + the first few diagnostics, with an explicit "wrong parser?" warning when a nonzero-exit command produced output but zero diagnostics; writes land as per-project `.cimp/config.json` overlay diffs
- Exposure status line — "run_check exposed: MCP ✓ / offload worker ✓" when checks exist, or "not exposed — no checks configured" with the Detect button right there — so the empty-`checks` gate (which leaves `run_check` unadvertised on every surface) is never silent again
- **Detect & configure**: language auto-detection merges code-graph per-language file counts with a depth-2 marker-file scan (`Cargo.toml`, `package.json`+`tsconfig.json`, `go.mod`, `pyproject.toml`/`setup.cfg`, `pom.xml`/`build.gradle(.kts)`, `*.sln`/`*.csproj`, eslint/ruff/jest/vitest configs; ignores `node_modules`/`target`/etc.) → a data-driven preset catalog (Rust / TS / Go / Python / .NET / JVM) proposes candidate `CheckDef`s; each proposal `{check, ecosystem, evidence, valid, reason}` is PATH-validated (the tool binary must resolve via the pty resolver) and evidence-checked before it's offered, invalid ones shown greyed with the reason; applying merges by `name` (`merge_auto` — `auto=true` entries are updatable, `auto=false` never touched, no duplicates)
- `checks_auto_configure` setting (default **false**): when on, validated proposals are applied automatically on first graph index; auto-added entries carry a `CheckDef.auto` marker so re-detection updates them but never fights a user-edited check (editing an auto entry clears the flag). A one-time Code Intelligence nudge chip ("N suggested checks for this project", with the auto-configured set reported when auto mode applied it) surfaces when a graph index completes with `checks` empty; dismissal is remembered per project (`checks_suggestion_dismissed`); a settings deep-link (`open_settings_window_to_section`) opens straight into the editor
- New IPC surface: `checks_detect`, `checks_apply_proposals`, `checks_suggestion`, `checks_dismiss_suggestion`, `checks_test`, `checks_validate_pattern`; the dropdown/label/conditional-field logic is extracted to `src/lib/settings/checksEditor.ts` (`PARSER_KINDS`, `PARSER_LABELS`, `showsPattern`/`showsReportFile`) for plain-vitest unit testing

## Code Audit — Aggregated Security Scanning (V23)
- **Code audit** section inside the Tool Activity tab (opt-in `code_audit.enabled`, off by default; formerly its own reserved dashboard tab, retired in schema v27): user-triggered scan of the project root, per-tool progress, one aggregated findings table, select + copy-to-agent — no build step, scans manifests/lockfiles on a fresh clone
- Three unbundled tools with distinct roles: **osv-scanner** (dependency CVEs + known-malicious `MAL-*` packages across 19+ lockfiles/manifests via deps.dev), **gitleaks** (secrets in the working tree + git history), **semgrep** (SAST of first-party code — requires Python, Windows support beta, lights up only when resolvable)
- Nothing is bundled — each tool resolves **ebin → PATH → per-tool path override** (the same unbundled model as rustnet/broot), managed in a dedicated **Settings → Code Audit** category with per-tool enable/path/Detect/Browse/extra-args and a scan timeout
- **Detect** button (`audit_detect_tool` IPC) resolves the tool honoring the override and probes `<tool> --version`, shown inline (`✓ <version> — <path>` / not found) — display-only, never writes the stored path
- All three emit SARIF, normalized through the V22 `sarif` parser into one merged findings table (severity · tool · rule · `file:line` · message) with severity-threshold / per-tool / text filters and a Graph view ⌖ jump (Tool Activity → Graph view)
- Findings-present is a non-zero exit for every tool — the runner classifies exit 0 = clean, 1 = findings (success), anything else = tool error; a failed chip's tooltip carries the tool's own stderr tail so offline failures are self-explanatory
- Tools run concurrently against the root with a per-tool wall-clock timeout; a single scan is in flight at a time, Cancel kills the children, results stream per tool via the `audit-status` event (findings capped on the wire, full set fetched via `audit_snapshot`)
- **Copy-to-agent** flow: per-row selection + Select-all-visible / Deselect / Copy selected writes agent-ready markdown (severity, rule id, project-relative `file:line`, message) to the clipboard for pasting straight into a Claude Code / OpenCode prompt
- **Scan-coverage honesty line**: after a scan the tab shows the lockfiles/manifests osv-scanner reported actually scanning (`Cargo.lock ✓ · package-lock.json ✓`, from its SARIF `runs[].artifacts`), so a "0 findings" run over an unscannable ecosystem isn't mistaken for a clean bill of health
- Network-reality hint in the view (osv-scanner queries the OSV API / deps.dev; semgrep downloads rules on first run — offline scans degrade); scans recorded in the tool-activity store (kind `audit`): one row per scanner run, plus one roll-up `security_audit`/`quality_audit` row per agent call with consumer attribution (`claude`/`opencode`/`offload`) in the Activities feed; results live in managed state (survive tab switch, not persisted across restarts in v1)

## Code Quality — Language-Gated Linters (V25)
- Reserved **Code Quality** dashboard tab, a second half of the audit surface split off from **Code Audit**: Code Audit keeps the three *security* tools (osv-scanner / gitleaks / semgrep), Code Quality hosts eleven *quality* tools (linters, dead-code / unused-dependency detection, spell-checking) — separate tabs, each with its own chips, Scan/Cancel button, status line and findings table; both gated on the single `code_audit.enabled` flag
- The eleven quality tools, their language gate, output format, and parser:

  | Tool | Language gate | Output → parser | Notes |
  |---|---|---|---|
  | **oxlint** | `.js/.ts/.jsx/.tsx/.mjs/.cjs` | SARIF stdout → `sarif` | single Rust binary, zero-config |
  | **golangci-lint** | `go.mod` or `.go` | SARIF stdout (v2 `--output.sarif.path stdout`) → `sarif` | |
  | **ruff** | `.py` | SARIF stdout → `sarif` | single Rust binary |
  | **cppcheck** | `.c/.cc/.cpp/.cxx/.h/.hpp` | SARIF report file (`--output-file`, needs ≥ 2.16) → `sarif` | writes to stderr otherwise; **exits 0 even with findings** |
  | **typos** | *always applicable* | JSONL stdout → `TyposJsonl` | the one tool valuable on every project |
  | **eslint** | `eslint.config.*` / `.eslintrc*` marker | JSON stdout → `EslintJson` | resolved project-local-first |
  | **PMD** | `.java` | SARIF stdout (`-R rulesets/java/quickstart.xml`, overridable per-tool via the Settings `ruleset` field) → `sarif` | `pmd.bat` on Windows, needs a JRE |
  | **Roslyn analyzers** (`dotnet-analyzers`) | `*.sln` / `*.csproj` | SARIF report file (`dotnet build /p:ErrorLog=`) → `sarif` | **default-disabled** — runs a real build (restores packages, writes obj/bin); longer timeout recommended |
  | **knip** | `package.json` marker | JSON stdout → `KnipJson` | resolved project-local-first |
  | **cargo-machete** | `Cargo.toml` marker | text stdout → `MacheteText` | near-instant unused-dep scan |
  | **semgrep (quality)** (`semgrep-quality`) | *always applicable* | SARIF stdout (`--config p/r2c-best-practices`, slug overridable per-tool via the Settings `ruleset` field) → `sarif` | **default-disabled** — separate id so quality rulesets never enter the Security section; needs network |

- **Language gating** keeps a tool off a project it doesn't apply to (no PMD chip in a Rust repo): a bounded, `.gitignore`-respecting **census** of the root (`ignore` crate walk, stopped at 20 000 entries / 2 s — a truncation only ever hides a tool, never invents one; cached ~60 s so tab-open and scan share one walk) collects lowercase extensions + a fixed marker set (`go.mod`, `Cargo.toml`, `package.json`, `*.sln`, `*.csproj`, `eslint.config.*`, `.eslintrc*`); a tool applies if ANY of its gate extensions OR markers is present. Each tab shows only applicable tools; a muted "**n tools hidden — not applicable to this project**" line accounts for the rest. Before the first scan (empty census) nothing is hidden
- **Since V38 the scanners are configured in Settings → Tool Plugins**, not in a Code Audit tool list: they are an embedded plugin (`cimp-audit`, badged **built in**) rendered by the same pane as anything you drop in the `plugins\` folder, split into its **Security** / **Quality** categories, each row with the same enable / path / Detect / Browse / extra-arguments / timeout / ruleset controls. The Code Audit section keeps the feature itself — master switch, scan timeout, quality-selection mode, MCP exposure
- **Binary resolution** per tool: a non-empty per-tool **path override** is used verbatim → else, for a node tool (eslint / knip), the project's own **`node_modules/.bin/<tool>`** (the `.cmd`/`.bat` shim on Windows) → else **ebin → PATH** on the bare command name; nothing is bundled. `dotnet-analyzers` resolves `dotnet`, `semgrep-quality` reuses the `semgrep` binary
- **Per-tool timeout override** (blank = the global scan timeout): a tool that runs a real build wants a longer budget — **~1200 s recommended for `dotnet-analyzers`**; a tool exceeding its budget is killed and reported `error` while the others are unaffected
- **One scan at a time globally** across both tabs (the cheapest correct model): while one tab scans, the other tab's Scan button is disabled with a muted "waiting — <other> scan running" note
- Findings-table behaviors are inherited from V23 unchanged, per tab: severity / per-tool / text filters, severity-descending sort, Graph view ⌖ jump, 500-findings-per-tool wire cap (full set via `audit_snapshot`), not-installed → Settings deep-link, and Select-all-visible / Copy-selected agent-ready markdown (Quality tab copies under a `## Code quality findings` heading)
- Exit-code semantics are per-tool, declared by each tool rather than assumed: most quality tools treat exit 1 as findings (success); **typos** uses exit 2; **cppcheck** exits 0 with findings (the report file carries them); PMD's 4 is findings, 5 a real error. Snapshots carry a `census` block and a `skipped_not_applicable` per-tool state. Since V38 the roster needs no seeding at all — it IS the shipped manifest, so a tool a settings file never mentions simply takes its declared defaults, and a fresh install and an upgraded one agree without a reconcile step to keep in step. The schema v32 → v33 migration moves an existing `code_audit.tools` array into the `tool_plugins` container, preserving every enable, path, timeout, ruleset and extra argument

## Theming & Appearance
- 12 bundled terminal palettes + custom 22-color ANSI palette editor; per-tab palette override
- UI theme selector: TUI Orange (default) + TUI Grey, ratatui-style; external theme/palette files
- Terminal background: solid color or image (opacity/blur/size/tint), global presets + per-tab override
- Configurable terminal font family and size

## Settings & Config
- Portable global `settings.json` next to the exe; per-folder `.cimp/config.json` overlay inside the project's `.cimp` data dir (auto-deleted when empty)
- Per-tab settings: command, CLI flags, TTS speak toggle, notification text, appearance
- Migration system with timestamped backups + per-key load validation
- Capability guidance injected on launch when relevant — offload + code-graph tool hints (Claude `--append-system-prompt`, OpenCode instructions file)

## Notifications, Shortcuts & Status Bar
- Tab-state announcements (idle / awaiting-permission / question / error; shells: error / exited)
- Done-while-away indicator, focused-tab announcement toggle, global mute
- Fully rebindable, context-aware keyboard shortcuts with conflict detection
- Status bar: mute, announcements toggle, volume, record button, usage meter, layouts popover
- Claude usage meter hides (and stops polling) when the Claude tab is disabled

## Integration, Monitoring & Build
- Claude Code permission detection, hook-primary: `Notification` + `PermissionDenied` hooks (`cimp --notify-hook` → `POST /permission/event`) flip the awaiting-permission badge/announcement, injected whenever the loopback runs (offload / graph / Code Audit MCP — a feature-less install stays regex-only), with the TUI footer matcher kept as the fallback for anything the hook drops (unmappable session, older CLI)
- Statusline subcommand (`cimp --statusline`) with context-window bar + `--settings` overlay injection
- System monitor panel (CPU/mem/GPU via sysinfo + nvml) with graceful degradation
- Anonymous usage tracking with retry/backoff
- Rolling file logs (tracing) with env-filter levels
- Portable Windows zip (single binary, models bundled) + slim no-models variant; release-hosted models (`scripts/fetch-models.ps1`) w/ checksum verification; CI release workflow

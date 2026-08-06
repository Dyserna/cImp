# Changelog

All notable changes to cImp are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.50.1] — 2026-08-06

### Fixed

- **Usage Overview flapping / slow load on large graph stores.** The
  Code Intelligence Overview's sessions list cycled between loaded and
  "0 sessions" (~30 s rhythm) and took tens of seconds to first paint on
  months-old stores. Four defects compounded: every session-keyed
  CozoScript query bound the session as a post-filter, full-scanning the
  relation (rewritten to inline prefix binds — `usage_all_sessions`
  measured 27.6 s → 1.4 s on a real 166 MB `graph.db`); the 2 s Overview
  poll had no in-flight or ordering guard, so slow passes piled up and
  applied out of order (both guards added, drill-in refetch included);
  store errors were silently swallowed into empty lists that rendered as
  a healthy "0 sessions" (`UsageSnapshot.store_error` now surfaces them —
  the UI keeps last-good data and shows a transient store-busy notice);
  and the warm-index cache keyed roots by raw path spelling, so the
  loopback's `\\?\` form opened a second in-process cozo storage over
  the same SQLite file (cache key now canonicalized — one file, one
  handle).

### Changed

- **30-day session-detail retention.** Sessions idle longer than 30 days
  are purged from the graph store at warm open: the session row plus its
  `usage_stat`, `mem_event`, unpinned `mem_note`, and `session_distilled`
  rows. Pinned notes and Workbench `session_commit` provenance
  deliberately survive. Read-only store consumers (the `--offload-mcp`
  child) never run the sweep.

## [0.50.0] — 2026-08-05

### Added

- **V30 — MCP session channels & session push (#15).** The offload MCP
  child grows an out-of-band session-push pipeline: settings-gated
  channel registration at the MCP host (Phase A), a tab-addressed
  session-push bus over the loopback `/events` stream (Phase B),
  completion producers with native-task auto-backgrounding (Phase C),
  and OpenCode session-push fanout via `noReply` message injection
  (Phase D; needs OpenCode ≥ 1.18.13 for the OpenCode leg).
- **V29 — xterm 6.0 migration (#20).** Terminals move to xterm 6 with
  the WebGL renderer as the fast path and the in-core DOM renderer as
  fallback (upstream deleted the canvas renderer at 6.0). WebGL
  contexts are held only by *visible* terminals, bounding context count
  by pane count — WebView2's ~16-context cap can no longer trigger
  eviction freeze waves at 17+ terminal tabs.
- **V28 — per-session MCP identity (#13).** The `context_recall` /
  `context_note` / `context_notes` memory tools now resolve the calling
  **tab**'s live session (`--tab` baked into the MCP child argv + the
  live-session registry), so two tabs of the same agent no longer share
  one memory scope. Fail-open by design.
- **Hook-driven permission detection (#5).** Claude permission prompts
  are detected primarily via the `Notification` / `PermissionDenied`
  hooks (`cimp --notify-hook` → loopback `/permission/event`); the
  TUI-regex scanner is demoted to fallback. Both paths feed the same
  idempotent awaiting-permission flag.
- **Context bar + live cache stats from the statusline push (#14).**
  The Claude context-window bar and cache read/creation split are fed
  from the statusline JSON's `context_window` block, and the quota
  widget reads the `rate_limits` push — the throttled network poller is
  retired.
- **Rust CI (#27).** First Rust CI: a clippy job linting the shipped
  `tts-webgpu` feature set, a test job (vitest + `cargo test --bin
  cimp`), and a compile-time guard turning the `tts-cuda`+`tts-webgpu`
  feature combo into a hard error instead of a silent CPU-only build.
- **Security-advisory workflow (#21, #22).** osv-scanner pinned as the
  primary advisory source (RustSec + GHSA superset); `cargo audit`
  becomes a cross-check with an accepted-risk baseline at
  `src-tauri/.cargo/audit.toml`.
- **OpenCode 1.18.13 upgrade prep (#9)** — MCP SDK v2 smoke,
  `subagent_depth`, anomalyco URLs, `--mini` guard.

### Changed

- **Dependency batches from the 2026-08-04 maintenance run
  (#17, #18, #19, #25).** MSRV 1.88 + cargo resolver v3; Tauri 2.11.5 +
  dialog 2.7.2; rodio 0.22 / cpal 0.17 in lockstep; portable-pty 0.9,
  vte 0.15, which 8, notify 8, base64 0.23, thiserror 2; npm bump-now
  batch.
- **ort WebGPU prebuilt re-gated behind `tts-webgpu` (#23).** A default
  (CPU) build now downloads the plain CPU ORT dist instead of the
  WebGPU one.
- **Claude MCP auto-backgrounding disabled in the spawn env (#3)**; the
  MCP host adopts protocol-version negotiation and legible `-32022`
  errors (#8, #12).
- **Settings window is created hidden** and revealed only after theme
  registry + settings init settle (white-flash fix).
- Pristine `patterns.json` installs are reconciled with new shipped
  defaults (#5).

### Fixed

- **Offload context budget under `--kv-unified` (#26)** — the per-slot
  budget divides by `-np` only in the unified-KV case, driven by the
  flag parsed from `server_command`.
- **Usage push file format 2** with per-slot aging and honest context
  attribution.
- **2026-08-05 full-branch review hardening** — V30 push hardening, V28
  identity guard, and the permission pipeline (2 HIGH / 16 MEDIUM
  findings, all fixed; report in `docs/reviews/`).
- **Permission footer matched by grammar** rather than literal TUI
  chrome (#5).
- **Clipboard read-image capability** granted for compose image paste.
- Transcript tap logs skipped lines and pins the format-tolerance
  contract (#7); the Claude `--settings` overlay is pinned to the
  2.1.214 contract (#6); the missing `--jinja` warning is demoted to a
  debug note (#10).

## [0.49.1] — 2026-08-04

### Added

- **Nippon dark + light themes and terminal palettes.** Two new TUI-style
  themes built from the colorpoint.io "nippon" palette — `nippon-dark`
  (teal surfaces, gold accent) and `nippon-light` (pale sage surfaces,
  teal accent) — each with a matching terminal palette.
- **Usage Overview dashboard card.** A collapsible Dashboard card at the
  top of Code Intelligence's Overview with a session/sub-agent token
  donut (per-kind inner ring) and a cost donut, following live/drill-in
  selection like the other cards.

### Changed

- **The four `tui-*` themes collapsed into one built-in `tui` theme with
  a user-picked accent.** tui-orange/-blue/-green/-grey differed only in
  accent; they are replaced by a single `tui` theme compiled into the
  binary, with the accent family derived from the new Settings → Theme →
  Accent color picker (`ui.tui_accent`, presets for the four classic
  accents plus a free color swatch). Settings migrate automatically
  (schema v28); on-disk themes are unchanged.
- **The Usage Effectiveness card is collapsible** like the other Usage
  cards, open by default, state persisted.

### Fixed

- **Cards and status colors ignored theme/palette changes** (#2). Two bug
  classes: ~90 raw status/surface hexes (GitHub-dark-baked badges, pills,
  borders, feed accents) replaced with the semantic theme tokens, and
  ~70 `var()` uses of tokens that were never defined (`--text`,
  `--border`, `--panel`, …) — where the dark fallback always won —
  remapped to the real token set. Switching theme (esp. dark ↔
  `nippon-light`) and terminal palette now live-updates the Tools
  sub-tabs, Code Intelligence, Workbench, and Settings cards.
- **New `--surface-card` token: cards and sectioned areas are slightly
  darker than the body background.** Defined in all theme blocks and
  derived from `--surface-0` via `color-mix`, so it follows terminal
  palettes; TUI-style themes fill `section` panes with it and every
  `.card` surface uses it.
- **The accent-picker hint no longer slides up under the color
  swatches** (the default hint style's -8px label pull-up applied where
  there was no label).

## [0.49.0] — 2026-07-17

### Fixed

- **semgrep (quality) ran into a dead registry ruleset.** Semgrep silently
  removed `p/best-practices` from its registry (HTTP 404 → semgrep exit 7
  before scanning), so the tool always errored. The default is now the
  surviving canonical slug `p/r2c-best-practices` — same rule pack under its
  original r2c-era name; no newer replacement pack exists. The security
  semgrep's `--config auto` was unaffected.

### Added

- **Per-tool "Ruleset" override for the audit tools that select one.** The
  two semgrep tools (`--config <slug>`) and PMD (`-R <ruleset>`) no longer
  bake their ruleset into the binary: each Settings row gains a Ruleset field
  (blank = the built-in default: `auto` / `p/r2c-best-practices` /
  `rulesets/java/quickstart.xml`). If an upstream-owned default breaks again,
  the fix is a settings edit instead of waiting for a release. The field
  counts toward the per-tool global/local scope badge and survives old
  settings files untouched. (`extra_args` can't do this for semgrep — it
  *merges* repeated `--config` flags rather than replacing the dead one.)

### Changed

- **Quality-audit sweep: the repo's own audit reports 0 findings.** All 113
  findings triaged and resolved — real fixes (unused `ndarray` dependency
  dropped, a misspelled art asset renamed to `Transition.mp4`, unused catch binding,
  `new Array(n)` → `Array.from`, useless spread fallback, Svelte 5 `$effect`
  dependency reads wrapped in `void`) plus a new `_typos.toml` allowlisting
  the false positives (audit-parser test fixtures, short identifiers/CSS
  classes, crate names, camelCase-split shrapnel), each entry documented with
  its reason.

## [0.48.0] — 2026-07-17

### Changed

- **Code Intelligence settings split into four sub-tabs.** The one long
  scroll is now Code graph (rebuild/status, indexing, ignores, tool
  surface, architecture & path tracing, offload worker access) ·
  Semantic search (toggle plus the embedding server config, now always
  visible instead of hidden behind the toggle) · Token efficiency
  (context injection, read advisor, local-model digests) · Graph view
  (enable, max nodes, tuning, edge colors).
- **Code Audit settings split into three sub-tabs.** Settings (feature
  toggle, scan settings, MCP exposure) · Security tools · Quality
  tools, using the same sub-tab nav as Code Intelligence.
- **Offload backend template libraries are now truly machine-global.**
  Saved server-command / remote-backend templates write through to the
  global settings.json and never pin into a project overlay; templates
  stranded in an existing overlay are promoted into the global baseline
  once on load. Mid-session promotions stick — the settings diff
  baseline now tracks the physical global file instead of being frozen
  at launch.
- **MCP tool-server editor rows stack into card-like groups.** Each
  server lays out as name + Remove, a full-width URL field, and the
  access checkboxes, with clear spacing between server groups.

### Fixed

- **`cimp --help` / `--version` no longer launch the GUI.** Both (plus `-h`,
  `-V`, and a leading `help`) print usage/version and exit — previously any
  unrecognized invocation fell through to a full app launch, so an AI agent
  probing the CLI (`cimp --help`, `cimp code-audit --help`) opened real
  windows. All other args still forward to the Claude tab (the drop-in
  `claude` contract, e.g. `cimp --resume <id>`).
- **Code-audit / graph MCP tools no longer strand with "cImp is not running"
  when offload is off.** The loopback endpoint now starts whenever ANY
  feature that advertises an MCP server needs it (offload, graph, Code Audit
  exposure) — previously it keyed on offload alone, so an audit-only or
  graph-only project advertised tools whose endpoint never came up. A
  tripwire test keeps the advertise and serve gates aligned.
- **Multi-instance MCP routing.** Discovery is now per-instance
  (`.cimp-discovery/<pid>.json` next to the exe, each entry carrying its
  launch root) and children resolve the instance whose root contains their
  cwd — previously the single last-writer-wins `.cimp-offload.json` could
  route project A's audits/graph/hook calls to project B's instance. Stale
  entries from hard-killed instances are swept at startup, the legacy file
  is kept in step as a fallback, and `/audit/run` additionally rejects a
  misrouted child with a clear "this instance serves X" error instead of
  scanning the wrong project.
- **Audit scanner paths now apply machine-wide.** Configured scanner exe
  paths (`code_audit.tools[].path`) were silently saved into the active
  project's overlay, so paths set up in one repo resolved to nothing in
  every other repo. Paths now live in the global settings file (legacy
  overlay paths are promoted once at load); the per-project overlay keeps
  only the enable flags and extra args.
- **TUI themes: settings editor buttons unwrapped.** Under the TUI themes
  (whose "[ Save ]" bracket convention wraps every settings-section button
  in `[ … ]` pseudo-elements) the compact × remove buttons in the Checks,
  environment, and args editors wrapped three lines tall, and their text
  buttons (Test, Detect & configure, + Add …) drew a box around the
  brackets. The × buttons now use the themes' `icon` opt-out and keep their
  compact box; the text buttons moved their visuals to element-level
  selectors so the TUI reset can flatten them into proper bracket buttons.
  Modern themes render pixel-identical to before. A dev-only harness
  (`src/dev/checks-harness.html`) renders the checks editor under both
  theme families for future settings-UI work.

### Added

- **Code Audit tool-list scope controls.** Save to global / Load from
  global / Clear all buttons on both scanner sub-tabs, plus a per-tool
  global/local badge (comparing enabled, extra args, and timeout; exe
  paths stay machine-scope). Edits still default to the project
  overlay; Save promotes the tool config to the global file, Load
  re-adopts it and drops the project copy.
- **"Restart the AI tab" hint when MCP exposure changes.** Enabling Code
  Audit (or flipping any setting that changes which MCP servers are
  advertised) now shows a toast in the main window and a Settings hint —
  servers are injected at tab spawn, so a running Claude/OpenCode session
  can't gain them mid-flight.
- **Restart warnings on every spawn-baked setting.** The restart-hint toast
  now fires for ALL settings that only reach an AI tab at launch, not just
  the MCP server set: capability guidance (offload nudge, graph, semantic
  search, pinned facts), the Claude `--settings` overlay (status line,
  context-injection / checkpoint prompt hook, compaction hook, read advisor
  + its shell matcher, post-edit auto-check), the OpenCode plugin flags and
  injected `local-llama` provider, and the local-provider `ANTHROPIC_*` env
  (only when a Claude tab actually opted in). Settings hints were added or
  corrected to match: offload enable/guidance, the MCP tool-server editor
  (whose "changes apply live" only ever applied to the warm host — AI tabs
  capture their tool list at connect), semantic search, the shell-read
  matcher, Workbench checkpoints, the Local LLM provider fields, and Add to
  OpenCode. Flipping a tab's "Use local LLM provider" now also trips the
  per-tab Restart Required badge.
- **Distinct "misconfigured" audit-tool status.** A tool whose CONFIGURED
  path doesn't resolve now reports `path-invalid` ("configured path not
  found: <path> — fix it in Settings", with a Settings link in the chip)
  instead of the misleading "not installed"; the MCP report summary counts
  it separately.

## [0.47.0] — 2026-07-16

### Changed

- **Tool Activity tab renamed to "Tools", sub-tabs reordered.** The tab strip
  now shows "Tools" (the tab id stays `tool-activity`; existing layouts pick
  up the new name automatically), and its sections run Activities · Offload
  server · Offload tools · Graph index · Graph view · Graph tools ·
  Code audit.
- **Tools sections fill the pane.** The Activities feed card and the
  Graph/Offload tools reference lists now size to the tab (scrolling
  internally) instead of stopping at a fixed height, and the two reference
  lists are always expanded — the collapse toggle is gone.
- **Code Audit tab retired — the Security | Quality panels are now a
  "Code audit" sub-tab of Tool Activity.** The separate V23 reserved tab is
  gone; the audit surface renders as a section inside the Tool Activity tab
  (still gated by `code_audit.enabled`, off by default; a Settings pointer
  shows when disabled), mounted lazily on the first visit and kept alive
  across section switches so a running scan keeps streaming. Settings schema
  v26→v27 drops persisted code-audit tab entries, and the id joins the
  integrity check's retired-tab prune for stale overlays.
- **Agent code-audit calls now land in the Activities feed.** Every
  `security_audit`/`quality_audit` MCP call (Claude Code, OpenCode, or the
  offload worker) records one roll-up row in the persistent tool-activity
  store with consumer attribution, duration, finding count, and the full
  report as the captured response — alongside the existing per-scanner rows.
  Refused calls (busy runner, feature disabled) are recorded as failed rows.
- **Graph View tab retired — the live force graph is now a "Graph view"
  sub-tab of Tool Activity.** The separate V15 reserved tab is gone; the
  visualization renders as a section inside the Tool Activity tab (still
  gated by the `graph_viz` setting, off by default), mounted lazily on the
  first visit and kept alive across section switches so the laid-out
  simulation survives. The Workbench diff and Code Audit ⌖ jumps now reveal
  Tool Activity and flip it to the Graph view section. Settings schema
  v25→v26 drops persisted graph-view tab entries (offload-server-retirement
  precedent, including the integrity-check retired-id prune for stale
  overlays).
- **Graph index dashboard moved to Tool Activity.** The Code Intelligence
  tab's Overview → Index group (Rebuild index / Rebuild embeddings / Test
  connection / Pause watch, the embedder probe, and the per-root index cards
  with language census + semantic-search status) now renders as a "Graph
  index" sub-tab of Tool Activity, next to Graph tools. The Code Intelligence
  Overview keeps the Usage cards; Memory / Context / Analyses / Trace path /
  Architecture are unchanged.
- **Offload Server tab retired — the dashboard is now an "Offload server"
  sub-tab of Tool Activity.** The separate V8-03 reserved tab is gone; the
  live per-backend dashboard (Local Start/Stop/Reset, slots, throughput,
  request history, raw server log) renders as a fourth section inside the
  Tool Activity tab, next to Activities / Graph tools / Offload tools.
  Settings schema v24→v25 drops persisted offload-server tab entries
  (code-quality-retirement precedent).

## [0.46.0] — 2026-07-16

### Added

- **Code Audit over MCP (V26).** A new `cimp --code-audit-mcp` stdio server
  (`cimp-code-audit`) exposes two zero-argument tools — `security_audit` and
  `quality_audit` — to Claude Code and OpenCode, proxied to the running app
  over the loopback `/audit/run` NDJSON stream; the offload worker gets the
  same tools natively (offered to **local** backends only, since reports carry
  repo paths and code quotes). Agent-triggered scans reuse the exact UI
  semantics and stream live into the Code Audit tab. Per-consumer exposure
  toggles (Claude / OpenCode / offload) live in a Settings "MCP exposure"
  group (schema v23→v24) and are re-enforced at every run.
- **Census-driven quality auto-select.** `code_audit.quality_auto_select`
  (default on) keeps each Quality tool enabled iff it is factory-default-
  enabled AND applicable to the project's language census; applied at scan
  start, on tab mount/re-show, and on Settings open (real census, ≤60 s
  cache), so chip gating and "not applicable" hints work before the first
  scan. A manual quality-checkbox edit flips to manual mode so choices stick;
  "Auto-select for this project" in Settings re-applies and re-enables auto.

### Changed

- **Code Quality tab retired — Quality is now a sub-tab of Code Audit.** The
  separate V25 reserved tab is gone; the Code Audit tab hosts
  **Security | Quality** sub-tabs. Both panels stay mounted, so a scan keeps
  streaming into the hidden sub-tab, and persisted filters survive. Settings
  schema v22→v23 drops persisted code-quality tab entries.

### Fixed

- **Executable pickers accept `.cmd`/`.bat`/`.com` launchers.** The Browse
  dialogs (audit tool path overrides, bottom-bar external tools, tab command)
  filtered to `*.exe` only, hiding npm bin shims (`eslint.cmd`, `knip.cmd`)
  and Java launchers (`pmd.bat`) that the resolver and spawn path already
  handle fine.
- **Active-tab flapping.** Two `set_active_tab` round-trips in flight at once
  could re-arm each other through the `ActiveTabChanged` forward-sync,
  flipping the focused pane back and forth for seconds. Applying a broadcast
  is now terminal and can never generate another push.
- **Non-AI tabs left-align their titles.** Only AI-tool tabs light the status
  dot, so only they keep the reserved indicator slot; dashboard and shell
  tabs drop it instead of centering their label around a phantom dot.
- **Usage chart zoom resets per session** and the S/A lane spans the full
  scroll width instead of stopping at the visible card edge.

## [0.45.0] — 2026-07-16

### Added

- **Usage chart: horizontal scroll + wheel zoom.** The "This session" stacked
  bar chart no longer drops the oldest turns to fit the card width: once bars
  hit their minimum width the chart scrolls horizontally (the S/A lane pans
  with it), the mouse wheel zooms the bar width around the cursor
  (shift+wheel pans), and the view stays pinned to the newest turns unless
  you've scrolled back into history. Zooming out fully returns to
  fill-the-card mode; only a hard 1000-turn render cap remains.
- **S/A lane color pickers.** The chart legend's "S session" / "A agent"
  swatches are now color inputs like the five segment swatches, persisted in
  settings (`graph.usage_color_session` / `usage_color_agent`); the agent
  color also tints the sub-agent bars' outline.
- **Code Quality — language-gated linters (V25).** A new reserved **Code
  Quality** dashboard tab splits the audit surface in two: **Code Audit** keeps
  the three security tools (osv-scanner / gitleaks / semgrep), Code Quality
  hosts eleven quality tools — **oxlint**, **golangci-lint**, **ruff**,
  **cppcheck**, **typos**, **eslint**, **PMD**, **Roslyn analyzers**
  (`dotnet-analyzers`), **knip**, **cargo-machete**, and **semgrep (quality)**.
  Both tabs share the one `code_audit.enabled` flag, run one scan at a time
  globally (the idle tab's Scan shows "waiting — <other> scan running"), and
  reuse V23's findings table, filters, Graph ⌖ jump, and copy-to-agent. Nothing
  is bundled — each tool resolves an override → project-local
  `node_modules/.bin` (eslint / knip) → ebin → PATH, and the four non-SARIF
  tools get small audit-local parsers (typos JSONL, eslint/knip JSON, cargo-
  machete text). `dotnet-analyzers` and `semgrep (quality)` are
  **default-disabled** (a real build / network-fetched rulesets).
- **Language gating.** A bounded, `.gitignore`-respecting census of the project
  root (20 000 entries / 2 s, cached ~60 s) decides which tools apply — no PMD
  chip in a Rust repo. Each tab shows only applicable tools with a muted
  "n tools hidden — not applicable to this project" line; Settings always lists
  all eleven (split into Security / Quality groups) and, after a scan, marks the
  gated-off ones with a "not applicable to the current project" hint.
- **Per-tool timeout override** (`AuditToolConfig.timeout_secs`, blank = the
  global scan timeout) — a longer budget for tools that run a real build
  (~1200 s recommended for `dotnet-analyzers`).

### Fixed

- **Drilled-in sessions refresh live.** Selecting a session in the Sessions
  card froze the "This session" card at the click-time snapshot — selecting
  the *current* session meant re-clicking it to see new turns. The card now
  refetches the selected session's detail whenever its row advances on the
  Overview poll; idle historical sessions still fetch exactly once.
- **Upgraded installs gain the Quality tools.** A settings file persisted before
  V25 carried only the three security entries; a load-path reconcile now appends
  any missing built-in audit tool (preserving every existing entry — enabled /
  path / extra_args / timeout — verbatim and in order), so the Code Quality tab
  and its Settings group are populated on first launch instead of staying empty.

## [0.44.0] — 2026-07-15

### Security

- **Audit-fix pass over the v0.43.0 Code Audit findings.** Frontend: vite
  5 → 8.1.4 (three CVEs; also drops the vulnerable esbuild 0.21.5 entirely
  and dedupes vitest's nested vite 8.0.10), svelte → 5.56.5 (four CVEs),
  devalue → 5.8.1, with `@sveltejs/vite-plugin-svelte` → 7.2.0 and a
  regenerated `package-lock.json` (`npm audit`: 0 vulnerabilities). Rust:
  tauri → 2.11.1 (origin-confusion IPC CVE), openssl → 0.10.81 (three
  CVEs), quick-xml → 0.41.0 (two DoS advisories; junit parser moved to the
  new `normalized_value` API), plus quinn-proto/anyhow/crossbeam-epoch
  patch bumps. Release workflow: the "Resolve tag" step now reads
  `inputs.tag`/`github.ref_name` through `env:` indirection
  (shell-injection hardening) and every action `uses:` is pinned to a full
  commit SHA. New root `.gitleaks.toml` allowlists `cimp.<name>.v<N>`
  storage-key constants (the one false-positive "secret"). Accepted, with
  reasons: `lz4_flex` 0.10 / `bincode` 1.x (pinned by cozo's `swapvec ^0.3`
  chain — local disk-swap of our own data), wayland-scanner's quick-xml
  0.39 copy (Linux-only build-time macro, trusted input), the gtk 0.18
  family + `unic-*`/`adler`/`fxhash`/`proc-macro-error`/`serial`
  unmaintained-only advisories (transitive, no fixes exist).

## [0.43.0] — 2026-07-15

### Added

- **Session Usage Insights (V24).** The Code Intelligence Usage group now
  answers *who* spent the tokens and *what a session actually cost*. Every
  recorded turn carries an `origin` (main **session** vs sub-**agent**; graph
  store schema 4 → 5 with a crash-safe stage-and-swap migration of
  `usage_stat` — existing usage history is preserved, old rows read
  `session`), tagged at the Claude tap from the sub-agent transcript drain and
  `isSidechain` lines. The "This session" bar chart grows an **S/A grouping
  lane** (contiguous same-origin runs, labeled when wide enough) plus a subtle
  accent outline/desaturation on agent bars, in both tokens and est-cost
  modes. **Clicking a session row now drills in** instead of opening the old
  cost popup: the card swaps to that session's turn series and top consumers
  (new `graph_session_usage` command — the data was always persisted, only
  the current session was ever queried), the title shows
  `agent · date time · id` with a copy-id button (session ids are directly
  resumable: `claude --resume <id>` / `opencode -s <id>`, shown as a hint),
  and a **Live** pill returns to the current session.
- **Per-model Cost card (V24).** A collapsible **Cost** card under "This
  session" prices each model in the session separately (a Fable main session
  with Opus agents shows two rows), each row with its token totals, an S/A
  share line, and a what-if pricing select — auto-matched from the pricing
  table by `model_prefix`, with **Custom…** rates and a **Free ($0)** option.
  Selections are stored as stable provider+model keys (never table positions),
  so Settings pricing edits can't silently repoint a row; a vanished row falls
  back to auto-match. Live mode tracks the session as turns accumulate; the
  grand total sums per-model costs, fixing the old popup's mixed-model
  single-rate mispricing. The cost popup is gone.
- **Active-session markers (V24).** Session rows that are live right now —
  an open Claude/OpenCode tab (tap-registered, RAII-cleared on close) or any
  session with activity in the last 5 minutes — get a theme-accent edge and a
  pulsing dot (reduced-motion honored), all of them at once, coexisting with
  the selected highlight.
- **Real OpenCode token usage (V24).** The generated OpenCode plugin now
  forwards per-assistant-message token usage (input/output+reasoning/cache
  read/write, model id) from the `message.updated` event into the loopback
  `/memory/event` ingress as real Turn rows — OpenCode session rows stop
  showing all-zero totals and price like Claude ones. Task-tool child
  sessions roll up to their parent with `origin: agent` (mirroring the Claude
  sub-agent contract), and child tool events no longer fabricate phantom
  token-less session rows. The `est` badge is now data-driven: it marks
  sessions with no recorded turns at all (pre-V24 OpenCode history keeps it;
  plugin-reporting sessions lose it). Note: the plugin regenerates on app
  launch, so OpenCode tabs report tokens after the next restart.

- **Code Audit — aggregated security scanning (V23).** A new opt-in
  (`code_audit.enabled`, off by default) reserved **Code Audit** dashboard tab
  and a **Settings → Code Audit** category run three unbundled security
  scanners against the project root and merge their output into one findings
  table. **osv-scanner** covers dependency CVEs *and* known-malicious packages
  (OSV `MAL-*`) across 19+ lockfiles/manifests, **gitleaks** covers secrets in
  the working tree + git history, and **semgrep** (detect-if-present — Python,
  Windows support beta) covers first-party SAST. Nothing is bundled: each tool
  resolves ebin → PATH → an explicit per-tool path override, with a **Detect**
  button (`<tool> --version` probe) and Browse in Settings. All three emit
  SARIF, normalized through the V22 `sarif` parser into a merged table
  (severity · tool · rule · `file:line` · message) with severity/tool/text
  filters. Findings-present is a non-zero exit for all three, so the runner
  classifies exit 0 = clean, 1 = findings (a success), anything else = a tool
  error; tools run concurrently with a per-tool timeout, Cancel kills the
  children, and progress streams via the `audit-status` event. Findings are
  selected and **copied as agent-ready markdown** (severity, rule id,
  project-relative `file:line`, message) to paste straight into a Claude Code /
  OpenCode prompt. A **scan-coverage line** lists the lockfiles/manifests
  osv-scanner reported actually scanning (from its SARIF `runs[].artifacts`) so
  a "0 findings" run over an unscannable ecosystem isn't read as a clean bill
  of health, and a network-reality hint plus the failed chip's own stderr tail
  make offline degradation self-explanatory. Scans are recorded in the
  tool-activity store (kind `audit`); results live in managed state and are not
  persisted across restarts in v1.

## [0.42.2] — 2026-07-15

### Fixed

- **OpenCode tab: mouse-wheel scrolling worked only while holding Alt.** The
  wheel forwarded to a fullscreen AI TUI was synthesized at a fixed cell
  (1;1); OpenCode (≥1.18) hit-tests the wheel by coordinate and scrolls the
  pane under the pointer, so top-left-corner wheels landed on non-scrolling
  chrome and were dropped (only the hold-Alt passthrough, which encodes real
  coordinates, scrolled). The synthesized sequence now reports the terminal
  cell under the pointer (legacy X10 encoding capped at its 223-coordinate
  maximum). Claude, which scrolls the transcript regardless of coordinate,
  is unaffected.

## [0.42.1] — 2026-07-15

### Fixed

- **Graph stuck in "building": the index's own store writes re-triggered
  rebuilds forever.** A full rebuild commits one transaction per indexed file
  into `<root>/.cimp/graph.db`, and the SQLite journal churn from a large
  project (observed: 885 files ≈ >4096 fs events) overflowed the watcher's
  bounded channel while the debounce thread sat blocked on the store
  write-lock the rebuild itself held — and the overflow "recovery" is a full
  rebuild, whose writes overflowed the channel again, looping indefinitely
  (~75 s per cycle, status pinned at `building`). The watcher callback now
  drops events under the graph's store subdir at the source (like `.git`/
  `target`/`node_modules`), and an overflow-triggered rebuild is logged at
  INFO so the next such loop is visible in default logs.

### Added

- **Settings → Code Intelligence: "Ignored files & folders" editor.** The
  `graph.ignore` globs (previously config-file-only) are now editable in the
  Settings UI: add/edit/remove gitignore-style rows manually, or pick a file/
  folder via the native explorer dialog ("Add file…"/"Add folder…", new
  `graph_ignore_pick` command on `rfd`) — picks land as root-relative anchored
  globs (`/docs/gen/`). Changes apply to the live index immediately without a
  full rebuild: a resync pass drops newly-ignored files' rows and (hash-skip)
  indexes newly un-ignored ones, converging on the last edit even under rapid
  changes. Also fixed on the way: the incremental watcher path now honors
  `graph.ignore` (previously only `.gitignore`), so saving an ignored file no
  longer silently re-indexes it.

## [0.42.0] — 2026-07-13

### Fixed

- **19 verified fixes from the V17/V21/V22 code review.** Highlights: a
  settings v21→v22 migration backfills `list_dir` into persisted "web-only"
  offload tool scopes (with a tripwire tying future tool growth to a
  migration); a char-boundary panic in the offload marker stripping on
  non-ASCII final lines; URL answers now verify fully (`file://` kept as a
  path claim, other schemes excluded from path verification); stateful tools
  (everything but pure `graph_*` lookups; MCP tools by default) re-execute on
  repeated identical calls instead of serving stale cache; `run_check`
  `report_file` resolves cwd-relative with a root-relative fallback (nested
  Maven/Gradle auto-detect shipped broken) and SARIF `file://` URIs handle
  RFC 8089 authority/UNC forms; checks auto-configure also triggers from
  incremental reindex. Plus a shared `fsutil` path-confinement module
  replacing three divergent copies, consolidated skip lists, and +32
  regression tests.
- **Usage: sub-agent token spend was invisible under Claude Code 2.x.** The
  2.x CLIs (observed 2.1.207) moved sub-agent transcripts out of the parent
  session file (no more inline `isSidechain` lines) into per-agent files at
  `<session>/subagents/agent-<id>.jsonl`, and renamed the launcher tool
  `Task` → `Agent` — so the Usage section counted only the orchestrator (in a
  real V17 build session: 64 Fable messages counted, 731 Opus sub-agent
  messages with ~289k output tokens dropped) and the agents-active avatar
  hold never engaged. The OOB tap now tails the per-agent files (usage +
  commit provenance only, attributed to the parent session — same split the
  inline contract had) and matches both launcher names. A new
  `drift.subagent_transcripts.v1` advisor canary fires if the contract moves
  again (transcripts in neither known location after an agent completes, or
  transcript files present with no recognized launcher tool).

### Added

- **Offload worker grounding & abilities (V21).** The local offload worker is
  now grounded and verifiable: a native `list_dir` tool (confined, capped), a
  verified-facts system prompt, `[Tn]` evidence citations checked by a
  deterministic answer verifier (one corrective "verify" turn, taint footer on
  residual violations), and a `verified: fully|partially` marker with an
  optional one-shot fast→quality escalation on partial verification
  (`offload.escalate_partial`). It also gains `run_check` as a worker-native
  tool, a curated read-only command preset (git, cargo metadata/tree) via a
  new `allowed_subcommands` allowlist, an identical-call short-circuit,
  a thinking guard (thinking Off still thinks on the final turn of tool-using
  runs), and grammar-enforced structured output (a `schema` param mapped to
  llama-server `json_schema` on the final turn only).
- **`run_check` generalization (V22).** Checks now run for any stack, not
  just Rust/JS: six new parsers (`sarif`, `go`, `go-test-json`, `dotnet`,
  `junit-xml`, `regex-custom`), per-check `cwd`/`env`/`report_file` (confined
  under the project root), language auto-detect with a preset catalog and a
  `checks_auto_configure` flow, and a ChecksEditor UI in Settings with a
  dry-run Test button and per-check exposure status.
- **Graph View: wider zoom-out + folder spacing up to 50.** The "Spacing
  between folders" tuning knob now goes to 50 (was 5), and the camera's
  zoom-out ceiling is doubled and stretches with that knob so a spread-out
  graph can still be framed in one view.
- **Read advisor: diff-substitute for changed-file re-reads (V17).** Re-reading
  a file *after it changed* is now answered with a line-level unified diff
  against a snapshot of what you last read, instead of the whole file — exact,
  so it's safe on the edit-then-verify loop. Falls back to a normal read when no
  snapshot survives (small file / over-cap / evicted) or the diff would be more
  than half the new file; a content change re-arms the reminder up to 3× per
  file per session. On by default within the `read_advisor` opt-in
  (`read_advisor_diffs`).
- **Read advisor: whole-file shell-read interception (V17).** A second
  `PreToolUse` Bash matcher intercepts a whole-file shell read
  (`cat` / `Get-Content` / `type` / `gc -Raw`) of an already-reminded file the
  same way a `Read` is intercepted — closing the loop V16 could only detect (a
  bypass used to cost the reminder *plus* the whole file). Strict: only a
  provable pure whole-file read of one file is caught; anything with a pipe,
  redirect, glob, second path, or a partial-read verb (`sed`, `head`, `tail`)
  runs untouched. On by default within the advisor (`read_advisor_shell`);
  Claude-only for now.
- **Read advisor: first-read digest tier for huge non-code files (V17).** The
  first read of a large non-code file (log, lockfile, generated JSON, data
  dump) can be answered with the cached local-model digest plus a head/tail
  sample instead of the full content. Off by default; enable by setting a KiB
  threshold (`read_advisor_first_read_kb`, try 256). Needs a cached digest — the
  first encounter enqueues one and passes.
- **`run_check`: test-run parsers (V17).** Two new parsers extend `run_check`'s
  grouped-diagnostics to test runs: `cargo-test` (stable-toolchain text —
  failures with resolved `panicked at file:line`, a truncated stdout block, and
  a passing-count line; a compile error before tests still surfaces) and
  `jest-json` (`jest --json` / `vitest --reporter=json`). Add a check to
  `.cimp/config.json`; the agent is nudged to prefer a configured test check
  over running the test command in Bash.
- **Usage: tool-surface accounting + lean surface (V17).** The Effectiveness
  card now prices the advertised graph-tool surface (serialized chars + count,
  cache-written once per session). A new `lean_tools` toggle (off by default)
  hides the cold-tail tools (`graph_cycles`, `graph_dead_exports`,
  `graph_struct_search`, `graph_path`, `graph_architecture`) from the
  advertised surface — they still answer if an agent calls them by name — and a
  `surface.lean.v1` advisor rule proposes enabling it after ≥10 sessions with
  zero calls to any hidden tool. The wordiest tool descriptions were tightened.
- **Advisor: read-advisor graduation rules (V17).** Two propose-and-confirm
  rules turn the V11 "graduate from field data" promise into Advisor proposals:
  `adopt.read_advisor.v1` proposes enabling the advisor when it's off, E1 is
  verified, and session memory shows repeated redundant large re-reads; and
  `adopt.read_advisor_substitute.v1` proposes switching to `substitute` mode
  when reminders are rarely followed by a full re-read. No silent default flips.

## [0.41.4] — 2026-07-12

### Added

- **Harness-contract hardening (V16).** The Claude Code / OpenCode
  integration contracts are now actively monitored instead of assumed:
  a version tripwire surfaces untested CLI upgrades (with a "Mark
  verified" flow), new `drift.*` advisor canaries fire when the hook,
  injection, or usage surfaces stop behaving (silent read hook, unseen
  injections, vanished usage fields, malformed payloads, shell-read
  bypasses), the read-hook shim validates its payload contract, and a
  failed E1 hook check hard-blocks the dependent features rather than
  letting them run silently broken. The read advisor's trust is now on
  a TTL, re-verified as sessions pass.
- **Usage: tokens | est. cost toggle.** The Usage cards can render as
  estimated dollar cost using the price table (models auto-matched by
  prefix; mixed-model sessions labeled "est · mixed"), and the
  Effectiveness line now compounds cache-read savings.
- **Settings UI: token-efficiency controls.** The V11 read-advisor
  knobs (master toggle, min lines, advise/substitute mode) and LLM
  digests are editable in Code Intelligence settings; the
  injection-gated knobs (dedup TTL, repo map on session start,
  compaction context) nest under the Context injection toggle.
- **Settings UI: full-schema gap closure.** Every user-facing setting
  now has an editor: all bindable shortcuts (tab 3–9, new shell tab,
  close tab, pane focus/split/close), per-tab env vars via a shared
  key/value editor, terminal scrollback (ring size / persist / restore),
  background snapshot lines, preview_allow_remote, offload global
  concurrency and per-backend declared model.

### Fixed

- **Advisor: context_min_score is enforced per file.** The score floor
  now applies inside context packing (not just to the top match), so
  marginal tail files no longer ride in under a strong #1 — they were
  the bulk of the injected-but-never-touched waste.
- **Advisor: Apply starts a 3-session cooldown.** Applying a proposal
  no longer risks an immediate re-proposal judged on data collected
  under the old value; unlike a dismissal, the cooldown expires on its
  own.
- **Shell-tab dialogs no longer wipe env vars.** Configure Tab and New
  Shell Tab previously saved `env: {}`, silently discarding hand-edited
  per-tab env vars.

## [0.41.3] — 2026-07-12

### Added

- **Offload: "Show command on start" confirm dialog.** A new per-Local-
  backend checkbox (Settings → Offload, under the server command) makes
  the Offload Server tab's Start button open the `llama-server` command
  in an editable confirm dialog first. The edited command applies to
  that launch only — it is validated by the same parse as the configured
  command (errors render inline) and is never written back to Settings.
  Starting with an edited command while the server is already running is
  an explicit error instead of a silent no-op.

### Fixed

- **Offload routing follows the running server.** When the local
  backend is up, offload routing and the dashboard now use the URL the
  server was actually launched with rather than re-parsing the saved
  command, so a one-shot command override (or an edited-but-not-yet-
  restarted command) can't point tasks at the wrong host/port.
- **Keep-alive follow-ups (0.41.2):** the Code Intelligence embedder
  reachability probe re-runs when the tab returns (it no longer sticks
  on "unreachable" after starting the embedder while the tab was
  hidden); the Diff pane stops issuing graph-status queries while the
  Workbench tab is off-screen; Worktrees diff-panel expansion and the
  Timeline's open "Diff vs now" (including per-file expansion) now
  persist like the sibling sections; the per-commit diff-expansion
  memory is capped instead of growing for the app's lifetime.

## [0.41.2] — 2026-07-12

### Fixed

- **App tabs no longer reset when switched away, hidden, or restored.**
  The app-rendered dashboards (Workbench, Code Intelligence, Tool
  Activity, Graph View, Offload Server, Note) are now kept alive behind
  the scenes instead of being rebuilt on every activation — selections,
  expanded rows, scroll, and the Graph View's laid-out 3D graph all
  survive tab switches, hide/un-hide, and moving the tab between panes.
  While off-screen their polls idle; data refreshes the moment they
  return.
- **View state survives app restarts.** The selected sub-tab in
  Workbench / Code Intelligence / Tool Activity, the Usage cards' and
  tool-reference lists' open state, the Diff pane's expanded files,
  full-file toggles and Unified/Side-by-side layout, the Git graph's
  selected commit, and Session commits' expanded commits are persisted
  per machine.
- **Git graph:** switching the selected commit no longer leaks the
  previous commit's full-file content or file expansion into the newly
  selected one.

## [0.41.1] — 2026-07-12

### Added

- **Workbench: git-graph commit details.** Clicking a commit in the Git
  graph expands the same detail a Session-commits row shows — the full
  message body plus the per-file diff against its first parent.
- **Workbench: diff ↔ full-file toggle.** Every diff surface (live Diff
  pane, Session commits, Timeline, Worktrees, git-graph detail) can switch
  a file between the normal 3-line-context diff and a whole-file view with
  the changes still highlighted.
- **Workbench: git graph auto-refresh.** The graph polls every 5 s while
  visible; background refreshes are flicker-free, keep the last good graph
  on transient git failures, and skip re-rendering when nothing changed.

### Changed

- Workbench sections reordered: Git graph · Diff · Session commits ·
  Timeline · Worktrees.
- **New-install defaults:** UI theme `tui-blue` paired with the
  `OpenCode Grey` terminal palette (the embedded last-resort fallback and
  the statusline palette fallback follow); avatar opacity 50%; waveform
  overlay off. Existing settings files keep their persisted values.

### Fixed

- Code Intelligence: the Sessions card's cache-write/cache-read/out
  columns misaligned after the token reorder — cache-write overlapped
  cache-read and a wide gap sat before "out".

## [0.41.0] — 2026-07-11

### Added

- **Persistent Tool Activity history.** The Activities feed survives app
  restarts: entries are mirrored to `tool-activity.jsonl` next to the
  executable. Clicking a row opens a popup with the actual request and
  response (graph tool args + output; offload instructions + synthesized
  answer, payloads truncated at 16k/24k chars). Rows can be deleted
  individually and the whole history cleared (two-step confirm).
- Retention is per feed kind (graph 400 / offload 100), so chatty graph
  telemetry can never evict the rarer offload run history.
- **Session commits + git graph Workbench tabs.** Per-session commit lists
  with live commit provenance (transcript tap ∪ time-window) and a
  railway-style git graph view.
- **tui-blue and tui-green theme variants.**
- Code Intelligence: the session cost popup shows the session's model, and
  token ordering matches the Claude UI.

### Changed

- The Activities feed's offload rows are now one per completed
  `offload_task` run (with payloads) instead of the per-slot llama-server
  request records; the slot-level history remains on the Offload Server
  tab's backend cards.
- `graph_history` / `activity_list` accept a `since_ts` high-water mark;
  the Graph View pulse feed uses it, so its steady-state 1.5s poll no
  longer re-fetches the whole history.

## [0.40.1] — 2026-07-10

### Added

- **Tool Activity tab.** A new reserved, read-only tab that gathers tool
  usage in one place, with three sections: **Activities** (a unified,
  newest-first feed merging the code-intelligence graph-call history with
  every offload backend's request history), **Graph tools**, and **Offload
  tools** (the two tool reference lists, moved here from the Code
  Intelligence and Offload Server tabs). Gated by the new
  `ui.tool_activity_tab` setting (default on) with a checkbox in
  Settings → Tabs.

### Changed

- **Code Intelligence tab restructured.** The Index, Usage, and Activity
  subtabs are consolidated: a single **Overview** subtab now stacks the
  status groups top-to-bottom (Index, then Usage). The activity feed and the
  graph-tools reference moved to the Tool Activity tab; Memory / Context /
  Analyses / Trace path / Architecture are unchanged.
- The Offload Server tab's tools reference and the per-backend **History**
  block moved to the Tool Activity tab (the collapsible **Offload runs** log
  stays on each backend card).

### Fixed

- Toggling **Show the Graph View tab** now materializes/removes the tab
  live — previously the live settings-update path never mirrored
  `graph.graph_viz` into the runtime, so the tab only appeared (or
  disappeared) after an app restart. The Graph View tab was also missing
  from the read-only write guard, the non-closable guard, and the
  frontend's no-PTY skip list; all now match the other reserved tabs.

## [0.40.0] — 2026-07-10

### Added

- **Code Graph Parity (V15).** Closes four code-only gaps benchmarked against
  Graphify, all on top of the existing per-project graph — no multimodal
  ingestion, no external services, everything local.
  - **Edge confidence** — every call/reference/edge is now tagged
    `extracted` (same-file / structural — the parser is certain), `inferred`
    (a single cross-file name-keyed guess), or `ambiguous` (the name resolves
    to more than one definition). Surfaced as badges on `graph_callers`,
    `graph_callees`, `graph_references`, and `graph_impact` (which gains a
    confidence split summary and an optional `min_confidence` filter), so the
    agent can tell a certain caller from a probable one. Always on — a
    correctness property, not a toggle. Forces one graph rebuild
    (`GRAPH_SCHEMA_VERSION` 3 → 4).
  - **`graph_path`** — trace the shortest path between two entities ("how does
    X reach Y?") across call/import/containment edges, each hop labelled with
    its edge kind and confidence. Directed by default; `symmetric` for a plain
    "are these related at all?" walk; `kinds` restricts edge types; bounded by
    the new `path_max_hops` setting (default 8).
  - **`graph_architecture`** — a once-per-project orientation map: **god nodes**
    (highest-degree hubs), **subsystems** (deterministic label-propagation file
    communities, named by common path prefix), and **surprising connections**
    (edges crossing subsystem boundaries — candidate accidental coupling).
    Topology only, no LLM/embeddings. New settings `arch_max_communities`
    (default 12) and `arch_min_community_size` (default 3).
  - **Graph View tab (stretch)** — an opt-in reserved tab that draws a bounded
    subgraph as a live 2D/3D force graph (node color = subsystem, size =
    degree; edge color = kind, dash = confidence) and pulses nodes as agents
    read/edit/query the codebase. Off by default (`graph_viz`), capped at
    `graph_viz_max_nodes` (default 1500).
  - Both new tools are exposed to the cloud Claude session and the local
    offload worker, and mirrored as `graph_path` / `graph_architecture` /
    `graph_viz_snapshot` Tauri commands for the Code Intelligence tab's new
    **Trace path** and **Architecture** sections.

- **Token Efficiency (V11 Context Engine II).** Builds on V10's memory/injection
  core to cut the token cost of a real agentic session: fetching one function
  instead of a whole file, orienting once instead of exploring, and not
  re-sending what the agent already has.
  - **`graph_snippet`** — fetch a single definition's *body* (by symbol, or by
    `file`+`line`) instead of reading the whole file. Ambiguous symbol names
    return a disambiguation list rather than a body; a symbol spanning the
    whole file (a top-level script) falls back to its outline + a "use Read
    with offset/limit" hint. Byte-capped by a new `max_body_bytes` setting
    (default 16 KiB) and flagged `stale` when the on-disk content hash no
    longer matches what was indexed.
  - **`graph_repo_map`** — a budget-bounded map of the project's most
    call-central files with their top exported signatures, for orienting at
    the start of a task without exploring. Session-hot files rank higher.
    Agent-pullable any time, and (opt-in) auto-injected once per session on
    the first prompt when `repo_map_on_session_start` is on. New settings
    `repo_map_budget_chars` (default 4000) and `repo_map_on_session_start`
    (default off).
  - **Injection dedup.** A file injected in full is demoted to a one-line
    "unchanged" reminder on later turns until it changes or a TTL elapses
    (`(updated)` tag when it does change). New setting
    `context_dedup_ttl_turns` (default 10 turns; `0` disables dedup).
  - **Compaction survival.** A new Claude `PreCompact` hook
    (`cimp --precompact-hook` → `POST /context/compaction`) feeds the
    compactor the session's ranked working set and pinned notes so they
    survive the summary, and clears the session's dedup state so the next
    turn re-injects fresh. New setting `compaction_context` (default **on**,
    nested under context injection).
  - **Redundant-read advisor** (opt-in, off by default). A new Claude
    `PreToolUse` hook on `Read` (`cimp --read-hook` →
    `POST /context/should_read`) intercepts a re-`Read` of a file already read
    unchanged this session and denies it with the file's outline (`advise`
    mode) or outline + the most relevant symbol body (`substitute` mode) as
    the reason — always usable content, never a bare refusal. One reminder per
    file per session; everything passes through unchanged right after a
    compaction (the agent may have genuinely lost the content). New settings
    `read_advisor` (default off), `read_advisor_min_lines` (default 300),
    `read_advisor_mode` (`advise` | `substitute`, default `advise`).
  - **Local-model context digests.** For files with no useful outline (docs,
    configs, long scripts), the **local** offload backend writes a cached
    ≤3-line digest instead of falling back to a raw content snippet — never
    routed off-box. New setting `context_llm_digests` (default off; needs a
    ready local offload backend).
  - **Code embeddings + `graph_semantic_code`.** Symbol-level semantic code
    search, mirroring the existing doc-embedding pipeline. Returns
    `file:line · kind · signature · distance` — never bodies — meant to chain
    into `graph_snippet`. Enabled by `embed_code_bodies`, which requires
    `semantic_search` (they share the embedder and backfill pass). New setting
    `semantic_code_max_chunks` (default 20 000).
  - The Code Intelligence tab's Index card now shows cached code-embedding
    coverage (`code: N/M chunks`) and cached digest count (`N context digests
    cached`) alongside the existing doc-embedding readout.

- **Agentic Inner Loop (V12).** Builds on V11's token efficiency to tighten
  the agent's edit → check → fix loop and make it work even when the model
  never asks:
  - **`run_check`** — one tool that runs the project's configured checker
    commands and returns deduplicated, structured diagnostics (grouped by
    severity + code + normalized message, up to 5 sample sites each) instead
    of a raw dump; a 400-error `tsc` run becomes ~30 rows. Configured
    per-project via a new root-level `checks: [{ name, cmd, parser,
    timeout_secs }]` list (rides the `.cimp/config.json` overlay). Shipped
    parsers: `cargo-json`, `tsc`, `eslint-json`, `pytest`, and `generic-gcc`
    (the `file:line:col` fallback). `changed_only: true` filters diagnostics
    to files touched since HEAD. Same security posture as `run_command` — a
    model-supplied `name` only *selects* among the project's configured
    checks, never a raw command. Doesn't require the code graph; exposed to
    both cloud tabs (MCP) and the offload worker's native tool set.
  - **`graph_impact`** — blast-radius analysis for the working-tree diff (or
    an explicit `symbols` list): maps changed line ranges to indexed symbols,
    then returns their transitive dependents (name-keyed, approximate — same
    honesty convention as `graph_references`) with depth, a file-level
    rollup, and changed-but-unindexed files called out separately. Also a new
    Analyses-section button ("Impact of working-tree changes"). `include_tests:
    true` appends the affected tests to the report.
  - **Test↔symbol mapping.** `symbol.is_test` is now populated per language —
    Rust `#[test]`/`#[tokio::test]`/`rstest` plus `#[cfg(test)]` modules
    (including `cfg(any(test))`/`cfg(all(test))`), JS/TS `*.test.*` /
    `*.spec.*` / `__tests__`, Python `test_*` in `test_*.py` / `tests/`, and
    generic path-convention heuristics elsewhere. New tool
    `graph_tests_for { symbol | file }` returns the transitive callers of the
    root filtered to test definitions — candidates, not guarantees (dynamic
    dispatch caveat, same posture as dead exports).
  - **Git-aware context.** A new `commit_touch` relation (file → last commit
    timestamp/subject/90-day touch count, collected from `git log
    --since=90.days`) boosts recently-churned files in `/context/retrieve`
    ranking (+3 within 7 days, +1 within 30) and adds a `last change: "…"
    (3d ago)` trailer to injected digests and `graph_find_symbol` rows. New
    tool `graph_recent_changes { days?, path_prefix? }`. Not a git repo →
    the feature is simply absent, everything else unaffected.
  - **Memory distillation.** An idle-session sweep (session quiet > 24h)
    sends its working set + notes to the **local-only** offload path (never
    remote/cloud) to extract at most 3 non-obvious, durable `project_fact`
    rows, capped at 100 live facts (oldest unpinned archived first). Facts
    surface in `context_recall`, boost `/context/retrieve` ranking when a
    fact mentions a candidate file's stem (whole-word match, generic stems
    excluded), and get a Memory-section **Facts** list (pin/edit/delete/add).
    Off by default (`memory_distillation`; needs a ready local backend). New
    opt-in `promote_pinned_facts` appends only pinned facts to the
    launch-time guidance payload, so durable knowledge can arrive with zero
    tool calls.
  - **Proactive automation** (opt-in, off by default — same posture as V11's
    read advisor). A new Claude `PostToolUse` hook on `Edit`/`Write`/
    `MultiEdit` (`cimp --postedit-hook` → `POST /context/post_edit`) debounces
    a session's edit bursts (`auto_check_debounce_s`, default 5s), runs the
    configured checks `changed_only`, and injects only diagnostics that are
    new or worsened since the session's last run — plus a two-line
    blast-radius note when the edited symbol has at least
    `auto_impact_min_dependents` (default 10) dependents. Check runs are
    single-flight per project root, so a Claude tab and an OpenCode tab
    editing concurrently share one run instead of duplicating a build; each
    session still sees only what *it* hasn't seen. Dead-exports/import-cycles
    now also re-run after every completed index pass (`analyses_auto`,
    default **on** — read-only, badges the Analyses section on change).

- **Vibe-Coding Guardrails (V13).** A new reserved **Workbench** tab (default
  **on**) makes it safe to let agents loose across the whole working tree —
  live diff, undo-anything checkpoints, and isolated worktrees, all local and
  git-native:
  - **Live diff pane.** The working-tree diff vs `HEAD` (spawned `git`,
    parsed unified diff), re-diffed on the same `fs-batch` event the graph
    watcher already emits (debounced 500 ms, plus a 5 s poll fallback when
    the watcher itself is off). Virtualized file list, unified/side-by-side
    toggle, intra-line word-diff. Per-hunk **Revert** (`git apply --reverse`;
    refuses mid-merge/-rebase and on a stale `hunk_hash` if the file changed
    since the hunk was computed), **Copy**, and **Send to agent** (drops the
    hunk as a fenced block + `file:line` header into the compose overlay,
    targeted at the focused AI tab). A status-bar `±N` badge click-opens the
    tab. Non-git projects with checkpoints on diff against the latest
    checkpoint instead.
  - **Cross-agent checkpoints** (off by default). Automatic working-tree
    snapshots into a separate `.cimp/shadow.git` store that never touches the
    user's own `.git` — no stash entries, refs, or reflog noise, enforced by
    a `GitCtx` that always sets *or* removes `GIT_DIR`/`GIT_WORK_TREE`/
    `GIT_INDEX_FILE` before spawning a shadow `git`. Each snapshot is an
    orphan commit tagged `cp-<seq>`, deduplicated by tree sha so a quiet
    period doesn't spam near-identical commits. Triggers: per user prompt
    (tapped from the same POST the injection shim already uses, recording
    the triggering agent), a debounced file-activity burst (covers shell-tab
    edits), and a manual "Checkpoint now". A new **Timeline** section (in the
    Workbench tab) lists snapshots with trigger/agent/files-changed and
    offers **Diff vs now** / **Restore**. Restore always snapshots the
    current state first (a "pre-restore" checkpoint, so restore is itself
    undoable), re-creates files deleted since, and deletes files created
    since only with an explicit opt-in checkbox (default off — untracked new
    work is kept unless asked). Works even before `git init`. New settings
    `workbench.checkpoints` (default off), `checkpoint_max` (100),
    `checkpoint_max_age_days` (7), `checkpoint_burst_files` (5) /
    `_burst_window_s` (60), `checkpoint_min_gap_s` (120).
  - **Worktree manager.** "New Claude/OpenCode tab in worktree…" runs `git
    worktree add .cimp/worktrees/<slug> -b cimp/<slug>` and spawns the AI tab
    with `cwd` set to the new worktree (tab title gets a `⑂ slug` marker). A
    **Worktrees** section lists every worktree with ahead/behind counts,
    **Diff vs base** (reuses the diff viewer), **Merge** (fast-forward or
    merge commit; requires a clean main tree on the worktree's base branch;
    on conflict runs `git merge --abort` and reports failure rather than
    ever leaving a half-merged tree), **Discard** (double-confirmed, only
    for cImp-created worktrees), and **Open shell here**. A merge-readiness
    chip (soft-dep on V12 `run_check`) shows the latest `changed_only` check
    result per worktree — advisory only, never gates the Merge button.
    `git worktree prune` runs on app start.

- **Workflow & Visibility (V14).** Four quality-of-life features sharing one
  theme — see what's happening, and feed the loop faster:
  - **Prompt library.** Saved, parameterized compose templates: a global list
    in `settings.json` plus per-project entries in the `.cimp/config.json`
    overlay, project entries shadowing a same-named global one by name.
    Variables `{selection}` (the focused pane's terminal selection) and
    `{clipboard}` resolve immediately on insert; any other `{name}` stays a
    literal tab-stop, first one auto-selected. In the compose overlay, `/` at
    the start of an empty textarea (or a 📋 button) opens a fuzzy-filter
    picker (↑↓/Enter, `Esc` or continued typing dismisses it into literal
    text — the agent's own slash commands are unaffected); a rebindable
    shortcut (default `Alt+/`) opens compose with the picker already up.
    Managed under *Settings → Compose* — global templates get full
    add/edit/delete; project templates are a read-only list edited via the
    overlay file, so a committed `.cimp/config.json` shares a team's
    templates with no separate export step. Ships 4 starters
    (`review-this-diff`, `write-tests-for`, `explain-selection`,
    `commit-message`), deletable.
  - **Image paste/drop into compose.** Paste an image (Tauri clipboard
    plugin — the WebView2 `navigator.clipboard` denial doesn't apply here) or
    drag-drop image files onto the compose overlay → a chip appears above the
    textarea; on submit, the local path(s) are appended to the message text
    (both Claude Code and OpenCode accept local image paths in prompts).
    Pasted images are written to a per-launch temp dir
    (`%TEMP%/cimp-attach/<launch-id>/n.png`); dropped files are referenced in
    place, not copied. The temp dir is age-pruned (>3 days) at startup and
    again on graceful exit.
  - **Token/cost X-ray.** A new **Usage** section (6th, after Analyses) in
    the Code Intelligence tab: per-turn stacked bars (input / cache-read /
    output / est. tool-result), a top-consumers table (tool × est. tokens), a
    per-session table with totals and cache-hit ratio, and an effectiveness
    panel (chars injected / suppressed-by-dedup / displaced-by-read-advisor).
    Honest measurement throughout — token counts read straight from a
    transcript's own `usage` block are exact; anything derived from
    character counts is labeled `est.`, and there's no fabricated savings %.
    Sourced by extending the existing OOB Claude transcript tap to also
    record `usage` and tool-result sizes into a new `usage_stat` relation
    (additive, no schema bump); OpenCode's `/event` stream carries no
    token/usage fields on the pinned version, so every OpenCode session
    reports `est_only`. A new status-bar session-tokens line surfaces the
    running total.
  - **Budget-tuning advisor.** An **Advisor** card atop the Usage section
    proposes measured changes to the V10/V11 knobs — propose-and-confirm,
    never silent self-modification. Three deterministic Rust rules, each
    gated on a minimum sample size (≥5 sessions, plus a per-rule
    injection/reminder/turn floor): injected files rarely touched again ⇒
    propose raising `context_min_score` (capped at a ceiling so repeated
    applies can't silently kill injection); read-advisor reminders usually
    followed by a full re-read anyway ⇒ propose raising
    `read_advisor_min_lines`; a high unused-injection rate while turns are
    budget-maxed ⇒ propose lowering `context_turn_budget_chars` (only ever
    proposed when it's a real reduction). Each proposal carries the measured
    rationale and an Apply button that writes through the normal settings
    path; Dismiss is remembered per rule at a 10%-bucketed rate, so it
    re-fires only once the underlying rate shifts to a different bucket.
    Rules are versioned (`rule_id` strings) and listed in the card's
    tooltip.
  - **Localhost preview tab.** A new user-creatable **Preview** tab: an
    embedded WebView2 child webview (Tauri's multi-webview API) pointed at a
    dev-server URL, with a URL bar, back/reload, device-width presets
    (mobile/tablet/desktop), and **Snapshot → compose** (captures the
    webview's current viewport straight to a PNG in the compose attach dir
    and opens the overlay with it pre-attached). Auto-reload fires on a quiet
    period (~1s) after the shared `fs-batch` file-activity event. Navigation
    is restricted to `localhost`/loopback/RFC-1918-private hosts unless
    `preview_allow_remote` is on; any `target="_blank"`/`window.open()`
    always leaves the tab for the system browser rather than opening a
    second preview pane. Not a general browser — no history UI, no
    profiles.

### Changed

- The graph store schema bumps to v3 (`symbol.is_test`, provisioned for a
  later milestone — the only column change); an older `graph.db` is
  transparently rebuilt on first launch, same as the V10 migration. Every
  other new relation this adds (`digest`, `code_chunk`, `code_vec`) is
  additive and created on demand — no separate migration.
- Two new loopback routes join `/context/retrieve` under the same
  authenticated-localhost trust model: `POST /context/compaction` and
  `POST /context/should_read`.
- V12 adds one more loopback route the same way: `POST /context/post_edit`.
  V12's schema footprint is entirely additive on top of V11's v3 bump — no
  further version bump (`commit_touch`, `project_fact`, `session_distilled`,
  `meta` are all create-if-missing).
- V13 touches neither the graph schema nor MCP tool set — the Workbench is a
  reserved app-rendered tab (like Code Intelligence) backed entirely by
  spawned `git` and its own `.cimp/shadow.git` store.
- V14 adds the new `usage_stat` relation additively — no graph schema bump
  (still v3). The **settings** schema bumps 20 → 21 for the new Preview tab
  kind (`TabConfig::Preview`) and the `preview_allow_remote` /
  `preview_last_url` / `prompt_templates` / `templates_seeded` /
  `advisor_dismissed` fields; the migration is a no-op data transform (every
  new field is `#[serde(default)]`/`Option`), so an older `settings.json`
  round-trips additively.

### Fixed

- **Workbench module hardening** (multi-agent code review of the whole
  module). Highlights: automatic prompt/burst checkpoints were silently
  broken on Windows (git rejects the `\\?\` canonicalized root the
  triggers passed — verbatim prefixes are now stripped at the spawn
  boundary); several Workbench views mutated plain `Set`/`Map` state that
  Svelte 5 doesn't proxy, leaving the worktree diff panel / check chips /
  checkpoint-diff expansion visually dead (now `SvelteSet`/`SvelteMap`);
  reverting an untracked file's hunk deleted the file with no confirmation
  (now always confirms in explicit delete terms). Also: merges are no
  longer refused over untracked files, discard refuses while an AI tab
  lives in the worktree, failed worktree/shell creates no longer leak a
  queued pane placement, per-file diff fetches guard against out-of-order
  responses, non-UTF-8 filenames survive restore on Linux, `LC_ALL=C` is
  pinned on spawned git, the shadow repo pins `core.hooksPath` and skips
  redundant re-init per checkpoint, and the diff parser prefers explicit
  header paths over the `diff --git` split.

## [0.35.0] — 2026-07-08

### Added

- **Code Intelligence (V10 Context Engine).** The read-only "Code Graph" tab is
  renamed **Code Intelligence** and gains four capabilities beyond structural
  search, folded into a five-section view (Index / Activity / Memory / Context /
  Analyses):
  - **Session memory.** cImp keeps a per-project, rolling record of what each
    agent session reads, edits, and queries (from Claude's transcript in-process
    and OpenCode's tool hooks), plus free-text notes the agent chooses to
    remember. New MCP tools `context_recall` (reload this session's working
    set), `context_note` (remember a decision; pin to keep it across sessions),
    and `context_notes`. The Memory section shows the working set, notes (with
    pin/unpin), and recent sessions, with per-session and project-wide clear.
    Memory is stored in `graph.db` in relations that survive a full index
    rebuild.
  - **Automatic context injection** (opt-in, off by default). When enabled, cImp
    ranks the files most relevant to each prompt and prepends a budget-bounded
    digest (an outline of the top files' signatures, not their full contents) —
    for **Claude** via a `UserPromptSubmit` hook, for **OpenCode** via a
    generated dependency-free `.opencode/plugin`. Session-hot files (from
    memory) rank first. The Context section has a live **preview** so you can
    see exactly what a prompt would inject, and per-file / per-turn character
    budgets and a relevance threshold in Settings.
  - **Packaged analyses.** On-demand **dead exports** (candidate unused public
    symbols — honestly labelled as candidates, since dynamic dispatch / external
    APIs / macros produce false positives) and **import cycles** (loops of files
    that import one another), as the Analyses section and the `graph_dead_exports`
    / `graph_cycles` MCP tools.
- **Symbol visibility.** The graph now records each definition's visibility
  (Rust `pub`/`pub(crate)`, JS/TS `export`, Python underscore convention, Go
  capitalization), which powers accurate dead-export detection.

### Changed

- The graph store schema is versioned; on first launch after upgrading, an older
  `graph.db` is transparently rebuilt from source to pick up the new columns and
  memory relations (cheap — every row is re-derivable).
- Two new local loopback surfaces (`POST /context/retrieve`, `POST /memory/event`)
  join `/graph_run` under the same authenticated-localhost trust model.

## [0.34.2] — 2026-07-06

### Added

- **Interactive language buttons in the Code Graph tab.** The read-only
  per-language file-count list is now a grid of colour-outlined buttons that
  classify every language present in the project: **green** = indexed,
  **yellow** = supported by the engine but not indexed (click to add — it's
  indexed and, when semantic search is on, embedded), **red** = unsupported
  (informational). Clicking a green button removes that language from the index
  (dropping its rows) and turns it yellow. Red buttons use curated display names
  for well-known unsupported programming languages (Zig, Lua, Elixir, F#,
  Fortran, Solidity, …); data/config/unknown files fold into a single "Other"
  bucket. Backed by a new project language *census* that walks the tree with the
  same ignore rules as a rebuild but without the allowlist filter, so it sees
  languages the indexed store never records.

## [0.34.1] — 2026-07-06

### Added

- **Start/Stop/Reset for the local offload server in the Offload Server tab.**
  The Local section now shows a Start/Stop/Reset button row at the top (below
  the "Local" heading, above the status card), so the local llama-server can be
  controlled straight from the tab without opening Settings. Mirrors the
  per-backend lifecycle controls in Settings → Offload.

## [0.34.0] — 2026-07-06

A per-project Note scratchpad, one-click OpenCode wiring for local offload
backends, and consolidation of cImp's per-project files under the `.cimp` dir.

### Added

- **Note tab — a per-project scratchpad.** A new 📝 button in the bottom bar's
  broot/rustnet group opens a singleton Note tab: a rudimentary plain-text
  editor (old-Notepad style) for jotting commands and ideas. Its content lives
  in `<launch_cwd>/.cimp/cimp.note.txt` (alongside the settings overlay and
  code-graph store) and autosaves — debounced ~800 ms after the last keystroke,
  on a 5 s safety timer, and on tab/app close. Pressing the button opens the
  existing note or creates one; the tab is closable and re-opens to the same
  file. No schema bump (the tab persists as an ordinary Shell-kind entry).

- **One-click OpenCode provider registration for local offload backends.** The
  Offload settings' local-backend card gains an **Add to OpenCode** button (plus
  an *Auto-sync while offload enabled* toggle) that registers the running
  llama-server as OpenCode's `local-llama` provider — base URL and model read
  from the backend command — and selects it as the default model, so a freshly
  opened OpenCode tab is ready to use. Auto-sync re-derives it from the primary
  local backend at launch and on save while offload stays enabled.

### Changed

- **Per-folder settings overlay moved into the project's `.cimp` dir.** The
  per-launch-directory overlay is now `<launch_cwd>/.cimp/config.json` (inside
  the same `.cimp` folder that already holds the code-graph `graph.db`), instead
  of the loose `<launch_cwd>/.cimp.custom.config.json`. This consolidates all
  cImp-specific per-project files under one directory. An existing loose overlay
  is migrated into `.cimp/` automatically on the next launch (best-effort move;
  the old file is still read in place if the move can't happen, so no
  customization is lost). No schema bump.

- **Offload local-backend card layout.** The OpenCode provider controls now sit
  above the Start/Stop/Reset lifecycle row (moved to the bottom of the card),
  with clearer spacing between the two groups.

## [0.33.0] — 2026-07-04

Linux support: cImp now builds, runs, and ships on Linux (x86-64, Ubuntu 24.04+)
as a portable tarball with the same GPU-accelerated TTS/STT as Windows. Plus a
fix for stray `[[TTS]]` markup that could leak into the terminal and speech.

### Added

- **Linux build + portable tarball (Ubuntu 24.04+).** A new `build-linux` CI job
  produces full and no-models `.tar.gz` portable layouts, GPU-accelerated out of
  the box — Kokoro TTS on ort's WebGPU (Dawn→Vulkan) backend and Whisper STT on
  whisper.cpp's Vulkan backend — with automatic CPU fallback, on any GPU vendor
  including Intel. The runtime floor is Ubuntu 24.04 / glibc 2.39 (set by ort's
  WebGPU prebuilt). See `docs/MAINTENANCE.md` (Linux build) and
  `docs/LINUX-VALIDATION.md`.

### Fixed

- **Stray `[[TTS]]` markup leaking into the terminal and TTS.** V20's fullscreen
  TUI retired the `[[TTS]]` marker convention (prose is now spoken from each
  tool's out-of-band transcript/event stream), but a stale `docs/CLAUDE.md` still
  told the model to emit the tags — so they appeared on-screen and were read
  aloud whenever a session touched `docs/`. Removed it.

### Removed

- The vestigial per-tab `tts_injection` free-text instructions field and the
  dead `display.show_tts_markup` toggle — both obsolete since V20's out-of-band
  TTS. Old settings files round-trip unchanged (no schema bump).

## [0.32.2] — 2026-07-03

Runtime GPU/CPU selection for TTS and STT, reusable offload backend templates,
and an at-a-glance tools reference in the Code Graph and Offload Server tabs.

### Added

- **GPU/CPU device selector for TTS and STT.** *Settings → Audio → TTS* and
  *Settings → Speech-to-text* each gained a **Process on** dropdown (GPU / CPU).
  Switching it reloads only that model on the newly-selected device — no app
  restart. **GPU** prefers the compiled GPU backend and auto-falls-back to CPU
  if none is usable; **CPU** forces CPU. On a CPU-only build both run on CPU.
- **Saveable offload backend templates.** Local backends can save/load/delete
  named `llama-server` command templates (a global library); Remote backends
  get named base-URL + auth-token endpoint templates. The Pool editor layout
  was reflowed — inline *Start-on-launch*, Start/Stop/Reset at the card bottom,
  a word-wrapped multi-row *Server command* field, and *Test offload* moved to
  the section bottom.
- **Collapsible Tools reference** in the Code Graph and Offload Server tabs. A
  shared component lists the tools each feature exposes — the `graph_*` MCP
  tools for Code Graph, and `offload_task` plus the worker's native
  `read_file` / `code_search` / `run_command` for the Offload Server — each with
  a one-line description and an example prompt.

### Changed

- **The TTS/STT device is now a setting, not the `CIMP_GPU` env var.** The new
  per-feature **Process on** selector is authoritative; `CIMP_GPU=cpu` is no
  longer consulted for device selection (the setting supersedes it). Existing
  settings files without the field load as **GPU**, preserving the historical
  "prefer GPU, fall back to CPU" behavior — no migration needed.

## [0.32.0] — 2026-07-01

The per-project code knowledge graph now understands ~29 languages through a
generic tree-sitter `tags.scm` engine, and fullscreen AI tabs get working
mouse-wheel scroll and right-click paste.

### Added

- **Multi-language code graph (V9-02).** A generic `tags.scm` extraction engine
  adds full symbol + call graphs for Go, Java, C, C++, C#, PHP, Bash, Scala,
  OCaml, Ruby, Haskell, Kotlin, Swift, SQL, Erlang, R, Perl, and Ada — joining
  Rust, TypeScript, JavaScript, and Python — plus structural search for HTML,
  CSS, JSON, YAML, XML, and assembly. Adding a language is now a vendored query
  plus a grammar crate, not a bespoke walker. Tier-1 code languages index by
  default; markup/data are opt-in via *Settings → Code Graph → Languages*.

### Fixed

- **Mouse-wheel scroll in fullscreen AI tabs** (Claude / OpenCode). The wheel is
  now forwarded as native mouse-report sequences instead of being translated to
  arrow keys, so scrolling works while click/drag selection stays local.
- **Right-click paste in fullscreen AI tabs.** Clipboard access now goes through
  the Tauri clipboard plugin; WebView2 blocks `navigator.clipboard.readText`,
  which had silently broken paste.

## [0.31.0] — 2026-06-30

TTS and STT can now be fully turned off, loading and unloading their models on
the toggle, and the bottom bar reflects each feature's enabled state.

### Added

- **Enable text-to-speech** master toggle (*Settings → TTS*). Turning TTS off
  **unloads the Kokoro ONNX model**, freeing CPU/GPU memory; turning it on
  reloads it. This is distinct from *Mute*, which keeps the model loaded and
  only silences playback. The remaining TTS controls disable while it's off.

### Changed

- **STT enable now loads/unloads the Whisper model.** Previously the
  *Speech-to-text* toggle only hid the record button; it now drops the
  whisper.cpp model on disable (freeing memory) and warms it on enable, via a
  control channel to the transcription worker.
- **Bottom bar reflects feature state.** The TTS controls (volume, mute,
  announcements, selection-TTS transport) hide when TTS is disabled, mirroring
  the record button, which already hides when STT is disabled.
- **Claude usage meter is gated on the Claude tab.** The bottom-bar session /
  weekly quota widget hides and stops polling when the subscription Claude tab
  isn't enabled.

### Fixed

- TTS model construction (the ONNX session build, seconds on a GPU EP) now runs
  on the blocking pool instead of inline on the async runtime, so toggling TTS
  on no longer parks a runtime worker thread.

## [0.30.0] — 2026-06-30

Fullscreen-only AI tabs + out-of-band TTS (milestone V20). Both **Claude Code**
and **OpenCode** now run their **native fullscreen TUI**, and text-to-speech is
sourced from each tool's structured side channel instead of scraping the
terminal. (Version jumps 0.23 → 0.30, skipping the unused 0.24–0.29.)

### Changed

- **AI tabs launch fullscreen.** Removed the two settings that forced an inline
  renderer — OpenCode's `--mini` (which hid commands like `/connect`) and
  Claude's `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN`. Both tools now run their full
  native TUI with the complete command palette.
- **TTS is out-of-band.** Speech no longer comes from scraping `[[TTS]]` markers
  out of the terminal. A new `oob` layer reads assistant prose from structured
  sources and feeds the existing segmenter/synthesizer unchanged:
  - **Claude** — tails the session transcript JSONL
    (`~/.claude/projects/<slug>/<id>.jsonl`), speaking each assistant text block
    (skipping `thinking`/tool blocks).
  - **OpenCode** — taps the fullscreen TUI's `GET /event` SSE stream (cImp
    launches it with a known `--port`), speaking each assistant message on
    completion and **excluding reasoning** parts. The same stream drives the
    avatar Thinking/Idle state.
  - Assistant markdown is reduced to speakable prose (fenced code blocks and
    markup dropped) before segmentation.
- **Mouse stays local in AI tabs.** A fullscreen TUI enables mouse tracking,
  which would route drags/clicks to the app and break local selection. cImp now
  suppresses mouse tracking for AI tabs so drag-to-select, copy-on-select,
  right-click paste, and Ctrl+right-click speak-selection all behave like a
  shell — with a **hold-Alt bypass** to hand the mouse to the app when needed.
  Shell tabs are unaffected.

### Removed

- The terminal **TTS-scraping pipeline** (the `[[TTS]]` marker convention, its
  runtime prompt injection for AI tabs, and the tag scanner). The processing
  layer is now a raw-stream forwarder plus the cell model used by permission
  detection.
- The per-tab **"TTS all output"** (`speak_all` / `tts_all_output`) mode and its
  context-menu toggle — it rode the scrape path and doesn't map onto structured
  sources.

### Fixed

- OpenCode no longer announces a spurious **"idle"** notification on tab open
  (the fullscreen startup paint tripped the byte-burst activity fallback, which
  is now skipped for tabs whose Thinking/Idle is event-driven).

### Migrated

- v19 → v20: strips any stored `--mini` from AI-tab `args` and drops the retired
  `tts_all_output` field; `copy_on_select` and everything else are preserved. A
  `settings.json.v19.bak` backup is written before the rewrite.

## [0.23.0] — 2026-06-30

### Changed

- **Rebrand `ccImp` → `cImp`.** The app now hosts both **Claude Code** and
  **OpenCode** (V19), so a name tied to "Claude Code" no longer fits. `cImp` =
  "**c**ode **Imp**" — an editor-/agent-agnostic name for the same mischievous
  helper. (The fuller "code imp" / "CodeImp" spelling is already taken
  elsewhere, hence the compact `cImp`.) The imp mascot is unchanged. This is a
  clean rename with **no backward compatibility**, mirroring the earlier
  `cctts` → `ccImp` rebrand:
  - Display / brand `ccImp` → **`cImp`**; binary, Cargo crate + `[[bin]]`, npm
    package, and Tauri `mainBinaryName` `ccimp` → **`cimp`** (output is now
    `cimp.exe`); Tauri `productName` → `cImp`; bundle `identifier`
    `com.ccimp.app` → `com.cimp.app`; window titles → `cImp`.
  - GPU env var `CCIMP_GPU` → **`CIMP_GPU`**; `RUST_LOG`/log target → `cimp`;
    daily log files `ccimp.log.*` → **`cimp.log.*`**.
  - Per-folder overlay file `.ccimp.custom.config.json` →
    **`.cimp.custom.config.json`** (the old `.ccimp.*` / `.cctts.*` overlays and
    any `CCIMP_GPU` usage are simply abandoned — re-set them under the new
    names). `settings.json` keeps its generic name. Per-project code-graph dir
    `.ccimp/` → `.cimp/`.
  - Statusline subcommand is now `cimp --statusline`; portable zips are
    `cimp-portable-win-x64-*`.
  - The GitHub repo `Dyserna/ccImp` should be renamed to `Dyserna/cImp`
    **after** this release's CI is green (GitHub auto-redirects old URLs); then
    `git remote set-url origin <new>`. The local clone folder is left as-is.

## [0.22.0] — 2026-06-29

### Changed

- **OpenCode replaces Aider (V19).** The two Aider AI-tool tabs are replaced by
  a **single OpenCode** tab (`opencode`), launched inline via `opencode --mini`
  with its session config injected through a single `OPENCODE_CONFIG_CONTENT`
  env var. Unlike Claude (which needs a separate local tab because the local
  endpoint is set by a launch-time env var), OpenCode addresses many providers
  as `provider/model` and switches between them in-session from global config +
  credentials — so one tab covers cloud and local and cimp injects no provider
  block. OpenCode reaches the same cImp capabilities the Claude tabs use — the
  offload tool, the code knowledge graph, and the web-research MCP servers — via
  the injected `mcp` block pointing at `cimp --offload-mcp --consumer opencode`.
  Unlike the silent Aider tabs, OpenCode is given the TTS-markup convention
  through an instructions file, so the OpenCode tab can speak. cimp does **not**
  bundle OpenCode (~158 MB); install it from <https://opencode.ai/docs> (or drop
  the binary in `ebin/`). A dedicated `opencode_access` per-server flag controls
  which MCP servers OpenCode sees.
- **New `TUI - Grey` theme (OpenCode); `TUI - Red` and `TUI - Aider` removed.**
  The theme set is now `tui-orange` (Claude Code, default) and `tui-grey` — an
  OpenCode-flavored monochrome theme keyed off a cool light-grey accent
  (`#c8ccd0`) and paired with a new `OpenCode Grey` terminal palette. The
  `tui-red` (Imp Red) and `tui-green` (Aider Green) themes and their dedicated
  `Imp Red` / `Aider Green` terminal palettes are dropped. The compiled-in
  last-resort fallback (used only when the on-disk `themes/` folder is empty)
  moves from `tui-red` + `Imp Red` to `tui-orange` + `GitHub Dark`, so the
  embedded fallback now matches the new-install defaults. Settings holding a
  removed theme/palette name keep their string and fall back gracefully at
  load (unknown theme → `tui-orange` chrome; unknown palette → `Default`).

### Migration

- **Schema 18 → 19 (`migrate_v18_to_v19`).** Both reserved `aider` and
  `aider-local` tabs collapse into the single `opencode` tab (id, command, name;
  per-tab `env` preserved; `use_local_provider` reset and stored `--model` args
  dropped; TTS injection enabled; duplicate `opencode` tabs de-duplicated). The
  legacy `aider_local` provider settings are dropped (OpenCode manages its own
  providers). Layout-tree, layout-preset, and active-tab references are rewritten
  and de-duplicated, `enabled_ai_tabs` is remapped, and each MCP server's new
  `opencode_access` defaults to its existing `claude_access`. A `.bak` of the
  v18 file is written before the upgrade.

## [0.21.0] — 2026-06-29

A correctness-and-hardening release: a full-codebase baseline review (security, systemic, and per-area correctness) was triaged and the confirmed issues fixed.

### Security

- **`run_command` short-flag bypass closed.** A glued short flag (`-ccore.hooksPath=/x`) now hits the same denial as the spaced form — previously the glued spelling slipped past the `git -c` guard.
- **Palette path traversal blocked.** The status-line palette name is rejected if it contains a path component, so a crafted `settings.json` can't read outside `palettes/`.
- **`read_file` TOCTOU.** The read is now byte-bounded (not just the pre-stat), so a file growing between the size check and the read can't blow past the limit.

### Changed

- **State signals are must-deliver.** The avatar/permission signal channel is larger and the terminal subprocess-exit edge is delivered with backpressure instead of a silent drop, so a burst can't desync the avatar/permission state machine.
- **Heavy work off the async runtime.** Per-tab content capture runs on a dedicated writer thread, TTS synthesis runs on the blocking pool, and the offload supervisor releases its lock before killing a child — keeping IPC, audio, and amplitude responsive.

### Fixed

- **TTS suppression no longer leaks across tabs.** An Esc-silenced tab stays silent when a different tab produces output; only the active tab's fresh output clears the suppression.
- **Idle-notification edge cases.** The audio idle edge uses `notify_one` (no missed edge), Idle is suppressed while a question is pending (symmetric with permission prompts), and `StopAll` no longer emits a spurious Started→Stopped pair after Esc.
- **Speech-to-text errors surface.** A microphone stream error sets the error state instead of yielding a silent empty transcript, and a mid-hold record-mode flip still releases.
- **Settings reliability.** A rejected settings update rolls back the optimistic UI change, an init-time event race no longer leaves a stale store, a failed overlay removal is no longer reported as success, a backup-write failure no longer wipes settings for the session, and two migration steps were corrected (TTS-injection enable, avatar-margin overlay leak).
- **Offload robustness.** Nested `<think>` blocks strip cleanly, compaction no longer emits consecutive `user` turns, the remote-backend cache key tolerates a trailing slash, the SSE relay reconnects on a half-open socket (and reuses one HTTP client), and the in-flight count stays accurate while the global cap shrinks.
- **Terminal/processing.** The TTS tag scanner advances even with no markers present (fixing an unbounded buffer and O(n²) rescans), cursor-column clamps to a valid column, and multi-parameter cursor-forward sequences emit the right spacing.
- **Frontend.** Fixed Tauri-listener leaks on component churn, a blank-pane layout-repair gap, a `NaN` poll interval that busy-polled, a status-bar drag lockout (pointercancel), a double-click that raced two selection-TTS sessions, a context menu that could open off-screen, and the code-graph monitor's pause-button label.

### Internal

- Graph file-watch and backfill no longer panic or orphan work on thread exhaustion / a lock-gap race; assorted dead code, stale comments, and a swallowed read error were cleaned up.

## [0.20.1] — 2026-06-28

### Added

- **Cancellable offload.** Local offload requests now stream, so interrupting an `offload_task` (or the calling session going away) aborts the in-flight generation and frees the `llama-server` slot immediately, instead of leaving an orphaned request running to completion and blocking the slot.
- **Total token counts in the Offload Server dashboard.** The request history now shows total tokens (prompt + generated) with the generated count broken out (e.g. `41,841 tok · 7,939 out`), and the per-slot bar reflects true context fill (prompt + generated) rather than generated-only.
- **`broot` launches with hidden files shown.** The bottom-bar broot button now runs `broot -g -h`.

### Changed

- **Bottom-bar controls regrouped.** Left-to-right: broot · rustnet | start dictation | play/pause/restart/stop · volume · mute TTS · mute notifications | settings, with dividers between groups. The avatar show/hide toggle was removed from the bar.
- **Leaner no-models update zip.** The slim/no-models zip no longer ships the `avatars/` and `sprites/` folders — the canonical sets are embedded in the app, so it still renders them; the full zip is unchanged.

### Fixed

- **Window resize no longer fires the "idle" chime.** A resize repaints the terminal, which was tripping the avatar's byte-burst activity fallback (Idle → Thinking → Idle) and firing a spurious notification.
- **Offload connection reliability.** The chat client now retries a transient transport (connect/send) error once and no longer reuses idle keep-alive sockets, fixing the `error sending request for url …` failures (including under concurrent requests) caused by the local server closing a pooled connection between requests.
- **No more silent-empty offload results.** When the model ended a turn with no answer (e.g. it reasoned entirely inside a `<think>` block), the agent returned an empty string as success; it now makes one forced-final attempt and surfaces a real answer or an explicit placeholder.

## [0.20.0] — 2026-06-28

### Added

- **Bundled external tools (`ebin/`).** The portable zip now carries a sibling `ebin/` ("external binaries") folder with `broot` (MIT) and `rustnet` (Apache-2.0), so the bottom-bar quick-launch buttons and shell tabs work without the user installing those tools. Command resolution checks `ebin/` first, then PATH; drop any executable into `ebin/` to add a tool. (rustnet needs Npcap installed to capture traffic; aider is not bundled because pip's launcher isn't portable.)
- **External-tool path override (Settings → Bottom bar).** Point `rustnet` or `broot` at a specific executable in any folder (with a file picker), overriding the `ebin/` → PATH lookup. Leave blank to resolve normally.

### Fixed

- **Crash-safe child reaping (Windows).** Offload child processes (`llama-server`, the warm MCP-host servers, `run_command`) are now assigned to a kill-on-job-close Job Object, so the OS terminates them whenever cImp dies for any reason — a crash, `panic = abort`, `taskkill /F`, or the dev hot-reload — not just on a clean exit. Fixes orphaned `llama-server` processes piling up and holding VRAM across dev cycles.
- **Aider tab gating.** Enabling an Aider tab (cloud or local) is now rejected with a clear message when the `aider` command can't be resolved (not in `ebin`, not on PATH), instead of materializing a dead "command not found" tab. Claude is not gated.

## [0.19.0] — 2026-06-27

### Added

- **Code knowledge graph (V9-01).** A per-project graph of code (files, symbols, references, calls, imports) and docs (Markdown + doc-comments), built in-process with tree-sitter and stored in an embedded CozoDB/SQLite database under `.cimp/`. Queryable by both the cloud Claude session (MCP tools) and the local offload worker (native tools): `graph_find_symbol`, `graph_callers`, `graph_callees`, `graph_references`, `graph_imports`, `graph_outline`, `graph_transitive`, `graph_search_docs`, `graph_semantic_docs`, and `graph_struct_search` (tree-sitter structural patterns). Covers Rust, TypeScript, JavaScript, Python, and Markdown.
- **Semantic doc search.** Embeds doc chunks via an OpenAI-compatible `/v1/embeddings` endpoint and ranks them with a CozoDB HNSW vector index (epoch-scoped by model + dimension), degrading to full-text search when the embedder is unreachable.
- **Live re-indexing + monitor tab.** A filesystem watcher incrementally re-indexes on change; a reserved, app-rendered **Code Graph** tab shows index/embedder status and counts, an on-demand embedder **Test connection** probe, and a unified **Recent calls** history (cloud + offload). Full Settings panel for languages, ignore globs, size limits, and the embedding endpoint.
- **Warm query path.** Cloud Claude's graph queries now run against the app's single warm index over the loopback (with a direct read-only fallback when the app isn't running), eliminating a cross-process double-open of the SQLite store and feeding the call history.

### Fixed

- Reserved feature tabs (Offload Server, Code Graph) now materialize/disappear immediately when toggled in Settings, instead of only on the next launch.

## [0.18.0] — 2026-06-26

### Added

- **HTTP MCP tool servers for offload.** The offload tool host now speaks the
  MCP 2025-06-18 Streamable HTTP transport, so HTTP/SSE MCP servers (e.g. a LAN
  DuckDuckGo + Context7) work alongside stdio servers and their read-class tools
  are offered to offloaded subtasks.
- **Live MCP server editor (Settings → Offload → Tools).** Add, remove, and
  enable/disable MCP tool servers (name + url) and have the change applied
  without restarting cImp. A read-only "MCP server status" health section sits
  above the editable list.
- **Offload queue backpressure.** A configurable max queue depth fast-rejects new
  offloads once the pool is saturated and that many tasks are already waiting
  (blank = the old unbounded blocking queue). Live queue depth is shown in the
  warm-pool readout and the per-backend dashboard card.
- **Parallel-offload awareness.** The `offload_task` description now tells Claude
  it can fan out independent subtasks concurrently and reports the live
  concurrent-slot count, so subtasks are issued at once instead of serially.
- **`speak_selection` keyboard shortcut** — read the active terminal's selection
  aloud, the keyboard equivalent of the Ctrl+right-click gesture.

### Fixed

- **Offload queue counter could wedge the queue cap.** The app-wide waiter count
  is now decremented via an RAII guard, so a cancelled/aborted offload no longer
  leaks the counter and permanently triggers the "queue full" fast-reject.
- **An unreachable MCP server could stall parallel offloads.** The HTTP client
  gained a short connect timeout, and an unchanged tool-server config now skips
  the host reconcile entirely, so one dead LAN server no longer serializes every
  in-flight offload behind the reconcile lock or freezes the Settings save.
- **Stale MCP session handling.** The `Mcp-Session-Id` is captured from the
  tools/list response and refreshed on every call, so a server that assigns or
  rotates the id late no longer wedges later calls with a `400`.
- SSE response bodies are read incrementally and return on the first JSON-RPC
  result, so a server that streams progress events first no longer blocks to the
  request timeout. Content-type matching is now case-insensitive.
- A freshly added MCP editor row with no endpoint is no longer connect-attempted
  (no more confusing empty-command error), and text edits persist on blur instead
  of a fire-and-forget write per keystroke that could land a half-typed URL.

### Changed

- **Selection-read diagnostics.** The TTS worker and frontend now log each
  selection chunk's path and warn when a chunk produces no audio or fails, so a
  dropped read can be pinned to a frontend split gap vs a backend skip.
- Drop the `// ` comment prefix on hint text in the tui-green/orange/red themes.

## [0.17.3] — 2026-06-26

### Changed

- **Waveform visualizer no longer burns GPU while idle.** The avatar waveform
  rescheduled its `requestAnimationFrame` render loop unconditionally,
  repainting the canvas (with a `shadowBlur` glow) at display rate even in
  silence. In the WebView2/Chromium compositor that held the GPU at ~10–15%
  while cImp was otherwise idle — unrelated to the loaded TTS/STT/LLM weights,
  which only compute on an in-flight request. The loop now parks itself once
  the buffer drains to a flat line (~1s after audio stops) and restarts the
  instant TTS or microphone audio resumes.

### Fixed

- **Settings window null-safety.** The offload command-policy lookup
  dereferenced the settings snapshot without the null guard its sibling
  helpers use, risking a crash if invoked before settings finished loading.
  It now returns nothing until settings are loaded.
- Two pre-existing `svelte-check` type errors are resolved (the Custom-palette
  resolver write and the snapshot guard above); `npm run check` is green.

## [0.17.2] — 2026-06-25

### Added

- **Command security policies (Settings → Offload → Tools).** The offload
  `run_command` tool's hardening is now a set of visible, editable per-program
  **security policies** instead of a hidden, git-only special case. Each policy
  names a program and the argument flags / subcommands it refuses plus the
  environment variables it forces at spawn; the previous git hardening ships as
  the seeded default. The Offload settings are reorganized into **Pool** and
  **Tools** sub-tabs, and the allowlist now shows, per program, whether a policy
  is hardening it. Existing config files inherit the default git policy
  automatically.

### Fixed

- **`run_command` argument-policy bypass (security).** A value-consuming global
  flag (e.g. git `--namespace x config …`) could shift the "first non-flag
  token" off the real subcommand, slipping a denied subcommand like `git config`
  past the guard and re-enabling arbitrary code execution via repo-local config.
  The default git policy now denies every value-taking global, restoring the
  guard's soundness; custom policies document the same requirement.
- **Custom terminal palette corrupted the bundled Default.** Resolving a Custom
  theme assigned colors onto the shared `defaultPalette()` object in place,
  corrupting the Default for the rest of the session. It now clones before
  overlaying.
- **Read-only offload tool filter — camelCase gap.** A camelCase mutating tool
  name like `configSet` evaded the write-verb filter that `config_set` would
  hit; all filter tiers now split camelCase sub-words.
- **Loopback auth token** is now compared in constant time (timing side-channel).
- **Offload concurrency-cap reconcile** no longer spawns a task per trigger that
  piles up behind the resize lock during a slow shrink; a single runner absorbs
  concurrent triggers and converges to the latest config.
- **Settings migration** no longer re-migrates (and regenerates a backup) every
  launch if a cascade leaves a file under-migrated; the pre-migration backup
  can't be overwritten on name-probe exhaustion.
- **Offload backend handles** are pruned when a backend is renamed, removed, or
  disabled (was a slow per-edit leak).

## [0.17.1] — 2026-06-25

A sweep of 37 confirmed correctness issues surfaced by a multi-agent bug hunt
and verified before fixing (full write-up in `docs/BUG-HUNT-2026-06-25.md`).
Grouped by area below.

### Fixed

#### Offload

- **Rotated auth tokens take effect immediately.** Editing a remote backend's
  bearer token from one value to another no longer silently reuses the old
  token — the cached handle's reuse check now fingerprints the actual token
  value, not just whether one is present, so a rotated key forces a rebuild
  instead of failing health/props probes with the stale credential.
- **Read-only worker boundary tightened.** The tool-name filter that keeps the
  local offload worker read-only now also catches common mutating verbs that
  previously slipped through (`cancel`, `abort`, `force`, `sync`, `evict`,
  `flush`, `upsert`, `amend`, `persist`).
- **`run_command` git hardening.** `git config` (and `--git-dir` / `--work-tree`)
  are now refused — writing `core.pager` / `core.sshCommand` / aliases let a
  later allowlisted `git` invocation execute arbitrary code. Git is also spawned
  with `GIT_PAGER=cat`, an empty `GIT_SSH_COMMAND`, and ambient config disabled
  as defense in depth.
- **Honest concurrency accounting.** The global concurrency gate is reconciled
  under a lock and the cap is published only after permits are added/reclaimed,
  so the in-flight count no longer transiently mis-reports during a resize. A
  remote `llama-server` restarted with fewer slots (`-np`) is now rebuilt rather
  than over-scheduled against a stale larger gate.
- **Context-budget robustness.** Auto-compaction now hard-truncates an oversized
  retained turn (or a huge original context) as a last resort, so it can't loop
  re-sending an over-budget prompt. A git/diff/commit task now requires the real
  `run_command` tool instead of a dedicated `git` MCP server, so a capable local
  backend isn't wrongly refused. `budget_high_water_pct` is clamped to a non-zero
  floor so a bad value can't zero out the working budget.
- **Clearer native-tool feedback.** `read_file` distinguishes an oversized single
  line from an offset past end-of-file, and flags an ambiguous relative path that
  resolves under multiple roots instead of silently picking the first.
- **Offload can be enabled mid-session.** Turning offload on in Settings after
  launching with it off now starts the loopback discovery endpoint (and warm
  host / health watch / metrics poller) instead of requiring a relaunch.

#### Terminal, tabs & settings

- **No more lost settings on concurrent edits.** Tab create/close/rename, layout
  save, preset edits, and active-tab persistence now compose atomically, so a
  layout auto-save (splitter drag) overlapping a tab operation — or an edit in
  the Settings window — can no longer clobber the other and make a tab vanish or
  a layout reset on next launch.
- **No orphan tabs.** If creating a tab fails partway through, the settings and
  registry entries are now rolled back instead of leaving a phantom tab that
  reappears on next launch.
- **First keystrokes into a new tab are no longer dropped** while the state
  manager is still registering the tab.
- **`pty_start` no longer stalls other terminal commands** by holding the tab
  registry lock across a blocking scrollback file read.
- **The last settings edit before quitting is saved.** Settings are flushed
  synchronously on shutdown, so an edit made within the 500 ms debounce window
  isn't lost; failed atomic writes no longer leave orphaned temp files; and a
  rapid re-migration can no longer overwrite the original pre-migration backup.

#### Audio & TTS

- **Read-along highlight stays in sync.** The selection-read "now playing" and
  "done" edges are retried instead of dropped when the state channel is briefly
  full, so the highlight no longer sticks on the last sentence or skips a chunk.
  A mark/queue desync on a same-tick drain-and-enqueue is also fixed.
- **The avatar no longer lip-syncs to frozen audio while paused.**
- **Cleaner sentence boundaries.** Speak-all no longer splits a sentence at an
  abbreviation (`Dr.`, `e.g.`), and a long legitimate sentence is no longer
  truncated at the head by the runaway-buffer guard.
- **Avatar state recovers correctly** after a subprocess exit or error mid-output
  (no longer stuck in *Thinking*), and a cross-tab idle announcement can no
  longer be silently suppressed by a stale echo-suppression guard.

#### Speech-to-text & theming

- A fully bracketed/parenthesized real utterance (e.g. "(parenthesize this)") is
  no longer discarded as a non-speech marker.
- The mic waveform no longer briefly shows the previous recording's tail.
- Palette verification now accepts only valid CSS hex lengths (3/4/6/8 digits).

#### Frontend

- **Escape only stops TTS when something is playing** (no per-keystroke IPC).
- Custom terminal palettes from `settings.json` are validated key-by-key, so a
  malformed value can't corrupt the xterm theme.
- The usage meter distinguishes "not logged in" from a transport error, so it
  appears within one poll after login instead of backing off up to ~5 minutes.
- A stale sprite-animation frame timer is cleared before loading the next
  animation; the terminal rebind fallback spawns at the live geometry; and
  dictation is appended to a trimmed compose buffer (no double spaces / glued
  lines).

## [0.16.2] — 2026-06-24

### Fixed

- **No more empty console windows on Windows.** Windows allocates a console
  window whenever the GUI process spawns a console executable. The
  `llama-server`, MCP server, and `run_command` spawns now pass
  `CREATE_NO_WINDOW` so they run hidden — their output is already captured over
  piped stdout/stderr and surfaced in the Offload Server tab.

## [0.16.1] — 2026-06-24

### Added

- **Per-backend Offload Server dashboard, grouped Local / Remote.** The Offload
  Server tab now shows one live card per enabled backend instead of only the
  local server, split into **Local** and **Remote** sections. A reachable LAN
  `llama-server` gets the full dashboard — slots busy/total, queue depth,
  throughput, context-fill %, per-slot rows, and request history — just like a
  Local backend; cloud and unreachable backends show a compact status row. The
  raw server log stays available for Local backends (cImp owns their process).

### Fixed

- **Remote backend slot count.** A remote `llama-server`'s real parallel
  capacity (`-np`) is now discovered from `/props` `total_slots` instead of
  assuming a single slot. The Settings status line and the dashboard show the
  true slot count, and the concurrency gate grows to match so offloads are no
  longer serialized to a multi-slot box.

## [0.16.0] — 2026-06-24

### Added

- **Offload Server dashboard tab + clearer errors (V8-03).** When offload is enabled
  there's now a read-only, non-closable **Offload Server** tab with a live
  **dashboard** of the local `llama-server`: slots busy/total, queue depth,
  throughput (tokens/sec), context-fill %, a per-slot row with live token counts +
  tokens/sec + progress bars, and a request **history** (start/end, duration,
  tokens, avg speed) — plus the raw server log tucked into a collapsible section.
  It polls the server's `/slots` and `/metrics` endpoints rather than scraping log
  text; add `--metrics` to your server command for the true context-fill %, the
  server-side queue depth, and server-computed throughput.
  A Local backend now also requires a genuine llama.cpp server: if something else is
  serving the port (e.g. an LM Studio instance), it shows a clear **error** with
  guidance instead of a false "ready" with an unknown context window. The tab
  appears/disappears on the next launch after toggling **Enable offload**.
- **Warm offload pool + MCP host (V8-03).** The offload machinery moves out of the
  per-call `cimp --offload-mcp` child and into the long-lived cImp app, which now
  owns the agent loop, the backend pool, the router, and — finally — the **MCP
  host**: warm, long-lived connections to your configured tool servers
  (`duckduckgo`, `fetch`, `context7`, `git`, `filesystem`) so an offload reaches
  real tools without paying an `npx`/`uvx` cold-start per call. Tools are
  namespaced (`ddg__search`), filtered to **read-class only** (write/destructive
  tools are dropped), and `filesystem` is confined to the allowed roots; per-server
  health shows in **Settings → Offload**. The app enforces a single **global
  concurrency gate** across every Claude tab, so the capability description and the
  router's spill/fail-over now run on **honest, health-accurate** state. The child
  shrinks to a thin proxy: when the app is up it forwards over an authenticated
  loopback endpoint (ephemeral port + per-launch token, advertised in a discovery
  file next to the exe — never `~/.claude`) and relays `tools/list_changed` when a
  backend or tool server goes up/down; when the app is **down** (headless cron,
  mid-restart) it falls back to the self-contained path, so offload still works
  without the app.
- **Offload backend pool + capability-aware routing (V8-02).** The single local
  offload server generalizes into a **pool of backends** — a local `llama-server`,
  a LAN machine, and/or a cloud OpenAI-compatible endpoint — and cImp routes each
  `offload_task` to the right one. The router picks a backend by required tools,
  required context, tier (`fast`/`quality`), and live availability (spilling when
  one is busy, failing over when one is down); Claude can bias it with a new
  `tier` argument, and the tool description reports the whole pool so Opus knows a
  fast tier exists. **Remote backends** are configured by URL (+ optional auth)
  and health-checked — no local process, no tab. **Per-backend tool scoping** is
  the privacy boundary: a **cloud** backend defaults to web/docs only (local
  file/search/command/git tools denied at both the routing and tool-array layers)
  and is unusable until you grant explicit data-egress consent; LAN backends keep
  data on your network. Manage it all in **Settings → Offload → Backend pool**.
  Additive settings migration: an existing single `server_command` becomes one
  Local backend in the pool.
- **Local task offload (V8-01).** cImp can now hand token-heavy subtasks —
  broad codebase searches, large-file/log summarization, web research — from the
  main Claude (Opus) session to a local LLM, so the cloud session's context grows
  by a paragraph instead of a megabyte. You point cImp at a `llama-server`
  command (e.g. Qwen3.6-35B-A3B) in **Settings → Offload**; it injects an
  `offload_task` MCP tool into the Claude tabs it launches (session-scoped via
  `--mcp-config`, never touching `~/.claude`), and the local model does the
  searching/reading/summarizing while only the synthesized result returns to
  Opus. The agent loop, the MCP server toward Claude (the hidden
  `cimp --offload-mcp` subcommand), and the native tools (`read_file`,
  `code_search`, allowlisted `run_command`) all live in the single cImp binary —
  no Node/Python sidecar. cImp discovers the server's context window and slot
  count, budgets each task against the per-slot window, and bounds it by step
  count and a wall-clock timeout. Off by default; the model is user-supplied
  (not bundled). File access is confined to configurable `allowed_roots` and
  `run_command` is deny-by-default (allowlist only).

### Fixed

- **Offload per-slot budget (V8-03).** llama.cpp's `/props` reports the *per-slot*
  context window (the total `--ctx-size` already divided by `-np`), but cImp
  divided by the slot count a second time — so every offload got roughly half its
  real working window. Fixed; offloads now budget against the full per-slot window.
- **Cross-backend offload spill now works (V8-03).** Because each per-call
  `--offload-mcp` child was blind to every other in-flight offload, it always
  reported `in_flight == 0`, so V8-02's spill-on-busy and fail-over never fired in
  production — the router saw every backend as free. The long-lived app-side
  service sees all in-flight offloads across every Claude tab and feeds the router
  honest counts, so a busy backend now spills to a free one (and a full pool queues
  coherently behind the global gate) as designed.
- **Claude Code fullscreen renderer disabled.** Recent Claude Code versions default
  to an alternate-screen "fullscreen" TUI that repaints the whole screen and enables
  mouse tracking — both break cImp's core assumption of a linear, append-only output
  stream. The result was leaked literal `[[TTS]]` markers (visible on select), double
  paste and double copy-on-select, and a dead Ctrl+right-click speak-selection. cImp
  now forces the classic inline renderer by setting
  `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` for every Claude tab (overridable via per-tab
  env), restoring all four behaviors.

## [0.15.0] — 2026-06-20

### Added

- **Context-window bar in Claude's status line.** cImp now injects a status
  line into the Claude Code tabs it launches, showing live context usage —
  e.g. `Opus  ▓▓▓▓▓░░░░░ 50% (100k/200k)` — themed to your terminal palette.
  It renders from the new hidden `cimp --statusline` subcommand (no external
  script, no Node/Python/PowerShell dependency) and is wired up via a
  session-scoped `--settings` overlay, so it appears only inside cImp and
  never touches your global `~/.claude` configuration. Enabled by default;
  toggle it in Settings → Bottom bar → Claude context bar.

## [0.14.0] — 2026-06-18

### Added

- **Tool launch buttons in the bottom bar.** Two quick-launch buttons —
  `rustnet` (🌐) and `broot` (🌳) — each open a fresh tab running that tool
  (`rustnet`, `broot -g`). Every press opens another tab, and each is an
  ordinary closable Shell tab with a close `×`, so these situational tools can
  be spun up and torn down freely. The command is resolved via `PATH` at spawn
  time; a missing tool still opens the tab and shows the standard
  "command not found" overlay. Backed by the new `open_tool_tab` IPC.

### Changed

- **Default UI theme is now `tui-orange`** (was `tui-red`) so the chrome accent
  matches Claude Code's orange; the paired default terminal palette is
  `GitHub Dark`. The avatar still defaults to the `impSprites` imp. Existing
  settings keep their persisted theme.
- **Speech-to-text is enabled by default.** The bottom-bar record button now
  shows out of the box (a Whisper `ggml-*.bin` model under `models/` is still
  required to transcribe).

### Removed

- **Persistent `broot` builtin tab.** broot is no longer auto-started on fresh
  installs, and the Settings → Tabs "Utility tabs" toggle (and its
  `set_broot_enabled` IPC) is gone — broot now launches on demand from the
  bottom-bar button like rustnet. The settings schema bumps to v16; the
  v15 → v16 migration drops the old auto-seeded `shell-broot` tab from existing
  files (any closable broot tabs you open from the button are unaffected).

## [0.13.1] — 2026-06-17

### Fixed

- **Imp avatar rendered as garbage.** The `impSprites` manifest declared
  `tile: 20` while its frames are 320×320. The sprite player computes its
  alpha-bbox crop in tile space and reuses those coordinates as the source
  rectangle on the full-resolution frame, so it sampled a ~20px patch from
  each frame's top-left corner and blew it up — every imp animation broke. The
  `work_think1` (Speaking) frames were also 1024×1024, inconsistent with the
  rest. Fixed by downscaling `work_think1` to 320×320 so the set is uniform and
  setting the manifest `tile` to 320 to match the real frame size.

### Changed

- **`tui-red` and `tui-orange` now use the `GitHub Dark` terminal palette**
  (was `Imp Red` and `Tomorrow Night`). Existing settings.json files keep
  whatever palette they were persisted with.

## [0.13.0] — 2026-06-17

### Added

- **cImp imp mascot is now the default avatar.** The `impSprites` set ships
  its first art pass — six pixel-art animations (idle blink, dance bounce, two
  think loops, a burning-tokens loop, and a surprise expression) that cover all
  five avatar states via the manifest's `groups`. New installs default to the
  imp; Clawd (`claudeSprites`) stays selectable in Settings → Avatar.
- **`tui-red` theme — the new cImp default.** A ratatui-style theme keyed off
  the imp's scarlet accent (`#e23c3c`), paired with a new `Imp Red` terminal
  palette. New installs land here.
- **`tui-green` theme ("TUI - Aider").** A selectable theme keyed off Aider's
  terminal green (`#2eb82e`, brightened from Aider's `#14b014` logo green),
  paired with a new `Aider Green` terminal palette — one TUI theme per tool the
  app fronts.

### Changed

- **Default UI theme is now `tui-red`** (was `tui-orange`) and the default
  terminal palette is `Imp Red` (was `Tomorrow Night`). Both prior defaults
  remain selectable. Existing settings.json files keep whatever theme/palette
  they were persisted with.

### Removed

- **Dropped the `modern-dark`, `tui-yellow`, and `tui-purple` themes.** The
  theme set is now `tui-red` (imp / default), `tui-orange` (Claude Code), and
  `tui-green` (Aider). The pre-V1.13 legacy `"tui"` theme value now migrates to
  `tui-orange` (the surviving Gruvbox theme) instead of the removed
  `tui-yellow`. With no remaining native-chrome theme, the custom TUI title bar
  always mounts; the OS-chrome path stays as forward-compat for any future
  `decorations: true` theme.

## [0.12.0] — 2026-06-16

### Added

- **Per-tab "TTS all output" mode.** Right-click a Claude tab → **TTS all
  output** to make that tab speak **all** new terminal output and ignore the
  `[[TTS]]…[[/TTS]]` markers, instead of speaking only the marked segments. A
  speaker icon on the tab shows which tabs have it on. Output is
  ANSI-stripped, sentence-segmented, and deduped; an in-progress trailing
  sentence is held until it completes, and unterminated TUI chrome (spinner /
  status line / box-drawing / input prompt) is dropped rather than spoken.
  `Esc` still stops playback until the next output burst. The toggle is
  per-tab, applies live (no restart), skips the existing backlog, and
  persists in the per-folder overlay (`.cimp.custom.config.json`). Cleanest
  for plain/line-oriented output (e.g. a local-LLM tab with no markers). The
  user's own typed/submitted input is registered and skipped when the TUI
  echoes it (even behind the `> ` prompt prefix), so the question isn't read
  back. Note: it speaks everything Claude prints, including reasoning/thinking
  shown above the answer — for answer-only speech, use the default `[[TTS]]`
  marker mode instead.
- **Manifest-driven animated sprite avatars.** The per-state animation
  mapping moved out of hardcoded app code into each sprite set's
  `manifest.json` `groups` (state → animation rotation list), so a new sprite
  set fully defines its own behaviour without code changes. Ships an
  `impSprites` set scaffolding the project's own imp mascot alongside the
  bundled `claudeSprites`.

### Changed

- **Renamed `cimp` → `cImp`.** The app, binary (`cimp.exe`), crate, npm
  package, window titles, log prefix (`cimp.log`), per-folder overlay
  (`.cimp.custom.config.json`), and GPU env var (`CIMP_GPU` → `CIMP_GPU`)
  all move to the new name — renaming the project after its mascot rather
  than a single feature. Still fully portable (writes only next to the exe).
  Re-set any `.cimp.*` overlay or `CIMP_GPU` usage under the new names.
- **Stronger TTS injection prompt for full-answer coverage.** The default
  runtime prompt now makes "wrap your whole answer, not a summary" the
  headline rule, so Claude marks its entire prose answer for speech instead
  of just a sentence or two. Existing tabs adopt it via *Settings → Tabs →
  TTS markup injection → Reset*, then *Restart Tab*; fresh installs get it by
  default.

### Fixed

- **No spurious "idle" announcement on startup.** A freshly-spawned Claude
  tab no longer speaks its idle notification when the welcome banner settles;
  the idle announcement now fires only after the tab has had real user
  interaction.

## [0.11.0] — 2026-06-15

### Added

- **GPU-accelerated text-to-speech via WebGPU.** Kokoro TTS now runs on ONNX
  Runtime's WebGPU execution provider (Dawn-backed), making GPU TTS **portable
  and vendor-agnostic** — it uses any GPU (NVIDIA/AMD/Intel) and falls back to
  CPU automatically, exactly like the Vulkan STT path. Measured ~5× faster than
  CPU, and it works on GPUs where the old CUDA path couldn't (including Blackwell
  / RTX 50-series). Nothing CUDA-specific is bundled — just three small Dawn
  dylibs. (`CIMP_GPU=cpu` forces CPU.) Source builds default to CPU; build
  `--features tts-webgpu` for the GPU variant. See `docs/features/FEATURE-tts-webgpu.md`.

### Changed

- **TTS GPU is now a compile-time feature, not the `CIMP_GPU=cuda` runtime
  opt-in.** The released binary ships `tts-webgpu`; the old NVIDIA-only CUDA path
  survives only as the optional, non-default `tts-cuda` build (mutually exclusive
  with `tts-webgpu`, and not shipped). `CIMP_GPU=cpu` forces CPU for both TTS
  and STT.

## [0.10.0] — 2026-06-15

### Added

- **Offline speech-to-text (dictation).** Press the new microphone button in
  the bottom bar, or hold the push-to-talk shortcut (default `Ctrl+Shift`), to
  dictate by voice. A fully offline, bundled Whisper model (whisper.cpp)
  transcribes your speech into the compose overlay for review before you send
  it — no cloud, no API key, nothing leaves your machine. Enable it under
  Settings → Speech-to-text, where you can pick the model, input device,
  language, translate-to-English, and the record-button mode (toggle vs hold).
  Drop additional `ggml-*.bin` models into the `models/` folder to switch
  between them. The released portable binary is **GPU-accelerated via Vulkan**:
  it automatically uses any GPU (NVIDIA/AMD/Intel) and falls back to CPU when
  none is present — no install, the only requirement is Windows' built-in
  `vulkan-1.dll`. (`CIMP_GPU=cpu` forces CPU.) Source builds default to CPU;
  build `--features stt-vulkan` for the GPU variant. See `docs/MAINTENANCE.md`.

### Changed

- **Three default shortcuts moved off `Ctrl+Shift`** so they don't collide
  with the new push-to-talk chord: Open compose `Ctrl+Shift+E` → `Alt+Enter`,
  Split pane (vertical) `Ctrl+Shift+\` → `Alt+\`, Close pane `Ctrl+Shift+W` →
  `Ctrl+Alt+W`. These are new-install defaults only — existing settings keep
  your current bindings; re-bind them under Settings → Shortcuts if you want
  the new defaults.
- **Compose overlay: `Enter` now sends**, and `Alt+Enter` (or `Shift+Enter`)
  inserts a newline — a one-handed flow that pairs well with dictation. The
  default `submit_compose` shortcut changed `Ctrl+Enter` → `Enter`; the compose
  box handles these keys directly, so the behavior applies without re-binding.
  Also fixed a flicker where the terminal area briefly shifted down when the
  compose sheet opened.

## [0.9.2] — 2026-06-12

### Changed

- **Internal cleanup — no user-facing behavior change.** Removed dead code
  across the Rust backend (unused functions, methods, enum variants, and the
  unused cell-attribute/row-timestamp bookkeeping in the terminal screen
  model) now that all milestones are complete, and cleared the remaining
  `#[allow(dead_code)]` suppressions. Applied mechanical clippy cleanups and
  de-duplicated a few frontend helpers (`AiTabId`, terminal-palette
  application, error-to-string formatting). Terminal colors are unaffected —
  they have always been rendered by xterm.js from the raw byte stream.

## [0.9.1] — 2026-06-12

### Fixed

- **No more blank window on startup.** The main window is now created hidden
  and revealed only once the UI has mounted and the window chrome has settled,
  so the empty WebView that used to flash for a couple of seconds on launch —
  along with the brief title-bar jump as the TUI themes drop the OS
  decorations — is no longer visible. A short safety-net timeout reveals the
  window regardless if the chrome setup stalls.

## [0.9.0] — 2026-06-10

### Added

- **Default `broot` tab.** Fresh installs now ship a `broot` tab alongside
  the default shell. It launches `broot -g` (the broot file browser with
  git info shown in the tree) with no `cwd`, so it opens in the directory
  cimp was started in. `broot` is resolved via `PATH` at spawn time; if it
  isn't installed the tab shows the standard "command not found" overlay
  until you install it. Existing installs get the tab injected by the
  v14 → v15 settings migration (schema bumped to 15); the frontend's layout
  repair places it in the focused pane on first launch after upgrade.
- **broot enable/disable in Settings → Tabs.** A new *Utility tabs* group
  exposes a `broot (git)` checkbox. While enabled the broot tab is a
  builtin — it has no close `×` and can't be closed from the tab bar or
  context menu; untick the checkbox to remove it (kills its PTY and drops
  its scrollback). Re-ticking re-creates it. Backed by the new
  `set_broot_enabled` IPC, mirroring how the AI tabs are gated by
  `enabled_ai_tabs`.

## [1.3.3] — 2026-05-07

### Added

- **Second Claude Code tab for a local LLM.** A new `claude-local` builtin
  AI tab runs the same `claude` binary as the subscription Claude tab but
  with `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` injected at spawn
  time (and optionally `ANTHROPIC_MODEL`), pointing at a local proxy that
  translates the Anthropic Messages API to your local model. Replaces the
  pre-V1.4-07 Aider tab in the same id slot.
- **Local LLM provider settings group.** New `claude_local: { base_url,
  auth_token, model_alias }` settings group exposed in *Settings → Local
  LLM provider*. The auth-token field is password-masked with a show /
  hide toggle. Helper text links to the LiteLLM docs and notes that cimp
  does not start the proxy itself.
- **Per-AI-tab `Use local LLM provider` toggle.** A checkbox on each AI
  tab in *Settings → Tabs* gates env synthesis from the global
  `claude_local` group. Off by default for the subscription Claude tab;
  on by default for the new Claude (local) tab. When enabled, the
  effective env is shown inline as helper text. Per-tab `env` entries
  always override synthesized values, so power users can still target a
  different provider per tab.
- **AI-tab Configure routes to Settings → Tabs scoped to that tab.** The
  right-click *Configure tab* entry on AI tabs (which previously only
  worked on Shell tabs because the dialog is shell-only) now opens the
  Settings window scrolled and expanded to the matching tab section. New
  IPC `open_settings_window_to_tab(tab_id)` plus a `settings-deep-link`
  event the Settings frontend listens for. Cold-open path uses a backend
  state cell consumed via `consume_settings_deep_link`; hot-open path
  uses the event. Shell tabs continue to use `ConfigureTabDialog.svelte`
  unchanged.

### Changed

- **Per-tab theme and background overrides now reach the right-click
  Configure flow on AI tabs.** Schema and the Settings → Tabs UI already
  exposed `theme_override` / `background_override` on AI tabs (V1.4-01 /
  V1.4-02 / V1.4-04); the right-click Configure entry now also routes to
  that surface so the more discoverable path works for the Claude tabs
  too. The runtime application path (`terminals.ts`'s `effectiveTheme`
  / `effectiveBackgroundMode`) was already kind-agnostic — the gap was
  purely UI routing.
- **`AiToolKindWire` enum collapsed.** Pre-V1.4-07 the schema carried
  `ai_tool_kind: "claude_code" | "aider"` on every AI tab; V1.4-07 drops
  the discriminator entirely. AI tabs are simply Claude Code, with an
  optional `use_local_provider` flag that gates env synthesis. The
  state-side `AiToolKind` enum collapses to the same shape (`TabKind::AiTool`
  with no inner data).
- **Default install layout.** Fresh installs now ship with two AI tabs
  (`claude` + `claude-local`) plus the default Shell tab. The integrity
  check restores any of the three reserved ids if a hand-edit removes
  them. The check also coerces `use_local_provider` to its canonical
  value on each builtin so a hand-edit can't silently flip the
  subscription Claude tab into local-LLM mode.
- **`Ctrl+2` switch label** in *Settings → Shortcuts* now reads "Switch to
  Claude (local) tab" (was "Switch to Aider tab").

### Removed

- **Aider tab kind.** `AiToolKindWire::Aider`, `AIDER_TAB_ID`,
  `default_aider_tab()`, the `AiderFirstLaunchNotice.svelte` overlay,
  the aider-specific TTS-injection no-op warning, and the aider-specific
  install-hint in the tab-error overlay are all gone. Aider permission-
  detection patterns (always empty in practice) are also removed.
- **`docs/features/FEATURE-aider-parity.md`** deleted; the two
  aider-related entries in `docs/FUTURE-FEATURES.md` (TTS injection
  blocked on upstream support, permission-pattern enumeration) moved to
  the historical section as superseded by the Aider removal.

### Migrated

- **v1.7 → v1.8** — adds the global `claude_local` group; drops the
  `ai_tool_kind` field from every AI tab; adds `use_local_provider:
  false` to every AI tab; rewrites the legacy aider tab in place
  (`id` → `claude-local`, `name` → `Claude (local)`, `command` →
  `claude`, `args` → `[]`, `use_local_provider: true`, `tts_injection`
  re-enabled with the runtime prompt as the default instructions, and
  the canonical "Aider …" notification strings rewritten to "Claude
  (local) …" — user customizations to env, theme/background overrides,
  and notification text are preserved). Layout-tree references to the
  legacy `"aider"` id are recursively rewritten to `"claude-local"` in
  `layout.tree`, every `layout_presets[].tree`, and
  `session.active_tab_id`. Backup at `config.json.v1.7.bak.<ts>`.
- A v1.2 file lands at v1.8 in one launch with six backups (v1.2, v1.3,
  v1.4, v1.5, v1.6, v1.7).

### Notes

- The auth token is stored cleartext in `settings.json`. Local proxies
  typically accept dummy tokens, so this is acceptable; OS keychain
  integration is a future enhancement if real Anthropic API keys end up
  in the field.
- TTS markup compliance on the Claude (local) tab depends on the
  underlying model. Smaller local models often don't honor the
  `[[TTS]]…[[/TTS]]` convention reliably; cimp treats missing markup
  as silent (the existing fallback behavior).
- Tool-use (Edit / Write / Bash / etc.) on the Claude (local) tab
  depends on the local model supporting Anthropic-style tool calling —
  test before committing to a particular model.

## [1.3.2] — 2026-05-07

### Added

- **Modern Dark theme.** Refreshed visual language: cool slate-blue surfaces,
  mint/teal accent (`#3eddb6`), coral semantics (`#f06080`), generous rounded
  corners (10/14/pill scale), and soft elevation shadows on dialogs / popovers
  / sheets.
- **Centralized design tokens.** `src/theme.css` defines the full token
  surface (surfaces, text, accent, semantics, borders, radii, shadows,
  spacing, motion, typography). Components reference `var(--*)` everywhere;
  no more component-local hex literals.
- **Settings → Appearance → UI theme.** A theme picker for the cimp chrome,
  distinct from the per-tab terminal palette under Display. Initial release
  ships only "Modern Dark"; the entry exists so future themes (light,
  high-contrast) plug in without UI plumbing churn. Persisted as
  `settings.ui.theme`.
- **Pill-shaped active tabs.** Active tab now reads as an elevated rounded
  pill (`--surface-3` fill on `--surface-2` bar) instead of a flush rectangle
  with a bottom-border accent. The two-tier active-state pattern reserves
  mint accent fill for filter toggles and primary CTAs; section selection
  uses surface elevation.
- **`<Pill>` primitive** (`src/lib/Pill.svelte`) — reusable tag/badge with
  `default | mint | coral | orange | accent-fill` variants and three sizes.
  First use site: the "restart required" indicator in Settings.
- **`prefers-reduced-motion` support.** Hover / focus transitions become
  instant when the OS-level reduce-motion preference is enabled.
- **Tabular numerics** on settings value labels (Speed, Volume, Opacity,
  Glow, Line width) so the label width doesn't jitter as the slider moves.
- **Terminal color themes (V1.4-01).** ~12 bundled xterm.js palettes (Default,
  Dracula, Solarized Dark/Light, Nord, Tomorrow Night, Gruvbox Dark/Light,
  One Dark, Monokai, Tokyo Night, GitHub Dark) plus a 22-color custom editor.
  Selectable globally in *Settings → Appearance → Terminal palette*, with
  per-tab override in *Configure Tab → Appearance*. Override travels with the
  tab through drag-and-drop. Live theme swap via `term.options.theme = ...` —
  no terminal recreation, no scrollback loss.
- **Terminal background image / solid color (V1.4-02 / V1.4-03).** Image or
  solid-color background beneath terminal text, with opacity, blur, size, and
  position controls. Per-tab override (custom config / "use global" /
  "disabled") in the Configure Tab dialog. Backgrounds force the xterm.js DOM
  renderer; only tabs that opt in pay the perf cost. Scrollback survives
  renderer flips: the outgoing xterm's state is captured via
  `serializeAddon.serialize()` and replayed into the new instance with
  `term.write()`.
- **Terminal background presets (V1.4-04 B).** Save the current background
  configuration as a named preset from *Settings → Appearance*; load presets
  from either the global page or the per-tab Custom branch. Manage / rename /
  delete from the Manage presets dialog.
- **Live preview in Configure Tab (V1.4-04 C).** Background changes in the
  Configure Tab dialog apply to the target terminal in real time while the
  dialog is open; closing without Save reverts to the original. Optional
  `terminal.background.preview_category_flips` toggle defers image-path swaps
  and category flips until Save for users with many tabs.
- **Cross-restart scrollback (V1.4-04 D).** Per-tab PTY ring buffer (256 KB
  default) persists to `<config-dir>/scrollback/<tab-id>.bin` on graceful
  exit, replayed via `term.write()` on next launch. Settings group
  `terminal.scrollback` (`ring_bytes`, `persist`, `restore_on_launch`).
  Best-effort recovery — hard kills (SIGKILL / Task Manager) lose the buffer.

### Changed

- **`settings.json` shape (UI chrome).** New top-level `ui: { theme: string }`
  block, defaulted to `"modern-dark"`. Existing v1.3 files load unchanged via
  serde defaults; the field is added on next save. No explicit migration
  required.
- **DropZoneOverlay** — switched from a flat blue fill to a mint dashed
  border with a soft inner glow, more visible against dark terminal panes.
- **Dialog elevation** — dialogs now use `--shadow-lg` and `--radius-lg`
  (14 px corners). Inputs sit on `--surface-sunken` with a mint accent
  border on focus.
- **Status bar toggles** (mute, announcements) — pill-shaped with
  `accent-muted` bg + accent border + accent text when active, indicating
  "filter engaged."
- **Snapshot cap and alt-screen detection (V1.4-04 A).** Renderer-flip
  scrollback capture is bounded by `terminal.background.snapshot_lines`
  (default 2000). When the alt-screen buffer is active (`vim`, `less`,
  `htop`, …) snapshot capture and replay are skipped — the live shell
  survives the rebind, but alt-screen contents are dropped (press Ctrl+L
  in the TUI to redraw).
- **Recreate-debounce stagger (V1.4-04 A).** Mass-recreate (e.g., a global
  category flip with many tabs) staggers across two animation frames at
  60 Hz instead of firing all timers in the same frame.

### Migrated

- **v1.3 → v1.4** — adds `terminal.theme = { name: "Default", custom: null }`,
  stamps `theme_override: null` on every existing tab, removes the dead
  `display.theme` field. Backup at `config.json.v1.3.bak.<ts>`.
- **v1.4 → v1.5** — adds `terminal.background` group (`image`, `color`,
  `opacity`, `blur`, `size`, `position`) and stamps `background_override:
  null` on every existing tab. Backup at `config.json.v1.4.bak.<ts>`.
- **v1.5 → v1.6** — adds `terminal.background.presets: []`. Backup at
  `config.json.v1.5.bak.<ts>`.
- **v1.6 → v1.7** — adds `terminal.scrollback` group and
  `terminal.background.preview_category_flips: true`. Backup at
  `config.json.v1.6.bak.<ts>`.
- A v1.3.0 file lands at v1.7 in one launch with four backups.
- The `ui` block continues to load via serde defaults — no explicit migration
  step is needed for the chrome theme.

### Removed

- **Per-tab avatar configuration** and **Per-tab TTS settings** — both were
  planned as items 3 and 4 of `docs/features/FEATURE-per-tab-overrides.md`
  and slated for V1.4-05 / V1.4-06. Cancelled as a scope decision: cimp
  ships exactly one avatar and one TTS voice, customized globally only.
  The skeleton plans were removed; the feature doc and `FUTURE-FEATURES.md`
  were updated to reflect the decision. No code or schema changes (the
  override fields were never added).

## [1.3.0] — 2026-05-06

### Added

- **Multi-pane layout.** The terminal area is now a recursive tree of panes
  and splits. Drag a tab to a pane edge to tear it into a new split, or to a
  pane center / tab bar to move it. Drag-and-drop uses a custom pointer-event
  handler with a 4 px threshold so clicks still register as clicks.
- **Splitter resize.** Each split has a 4 px draggable line between its two
  children (`col-resize` / `row-resize` cursor). Min-pane sizes (200 px wide,
  100 px tall) clamp during drag; window resize re-clamps visually without
  overwriting the user's stored ratio.
- **Pane-aware keyboard shortcuts.**
  - `Ctrl+\` — split focused pane horizontally with a fresh Shell tab.
  - `Ctrl+Shift+\` — split vertically with a fresh Shell tab.
  - `Ctrl+Alt+Arrow` — move focus to the geometrically-adjacent pane.
  - `Ctrl+Shift+W` — close focused pane (tabs migrate to the surviving
    sibling, then the empty pane collapses).
- **Pane right-click context menu** with Split horizontally / vertically,
  Close pane, and Move all tabs to → submenu.
- **Layout persistence.** The full layout tree and focused pane id persist to
  `settings.json` on a 250 ms debounce. Re-launching restores the exact pane
  arrangement from the previous session.
- **Named layout presets.** Save the current layout under a name from the
  Layouts popover in the bottom status bar; restore via Recent presets or the
  Manage presets dialog (with inline rename and confirm-delete).
- **Per-pane tab bar overflow.** When more tabs fit in a pane's width than
  display, the tab bar scrolls horizontally with thin scrollbars and edge-fade
  gradients. The `+` button stays pinned at the right. Activating an
  off-screen tab (via `Ctrl+N` or click) scrolls it into view.
- **Accessibility:** `role="group"` + dynamic `aria-label` on each pane
  (announces ordinal, total panes, and active tab name). `role="separator"` +
  `aria-orientation` + `aria-label="Resize panes"` on splitters. `:focus-visible`
  outlines on tabs, panes, splitters, and the new-tab button. `aria-hidden`
  on the drag ghost so screen readers don't follow it.

### Changed

- **`Ctrl+1`..`Ctrl+9` are now pane-scoped.** They switch to the Nth tab in
  the **focused pane**, not the Nth tab in the global list. This is the only
  behavior change for v1.2 users — closing or moving a tab shifts higher-
  numbered ones down by one within their pane, just as before, but the
  numbering is per-pane.
- **`Ctrl+T` and `Ctrl+W` are now pane-scoped** (new tab into focused pane,
  close active tab in focused pane).
- **Focused-pane indicator** is a 2 px top accent on the focused pane's tab
  bar (placed at the top so it doesn't merge with the active-tab underline,
  which uses the same accent color at the bottom).
- **Avatar overlay, audio playback, and the compose overlay** now route to
  the **focused pane's active tab** rather than a single global active tab.
  Switching pane focus retargets all three.

### Migrated

- v1.2 → v1.3: settings files without a `layout` key are migrated by
  synthesizing a single root pane containing every tab in order, picking
  active from `session.active_tab_id` (then dropped). A
  `settings.json.v1.2.bak` backup is written alongside before the rewrite.

### Known issues

- `Ctrl+Shift+W` may collide with WebView2's "close window" on some Windows
  configurations. If the close shortcut steals the keypress, remap
  `close_pane` to `Ctrl+Q` or `Ctrl+Alt+W` in *Settings → Shortcuts*.
- `Ctrl+Alt+Arrow` may collide with GNOME / KDE workspace switching on
  Linux. Remap `focus_pane_*` to `Ctrl+Shift+Arrow` if so.
- Tearing a tab into its own top-level window is not implemented — tabs
  always live within the single application window.
- No keyboard equivalent for moving a tab between existing panes; use drag
  or the Move all tabs to → context-menu submenu.

# Changelog

All notable changes to cctts are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
  `vulkan-1.dll`. (`CCTTS_GPU=cpu` forces CPU.) Source builds default to CPU;
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
  cctts was started in. `broot` is resolved via `PATH` at spawn time; if it
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
  hide toggle. Helper text links to the LiteLLM docs and notes that cctts
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
  `[[TTS]]…[[/TTS]]` convention reliably; cctts treats missing markup
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
- **Settings → Appearance → UI theme.** A theme picker for the cctts chrome,
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
  and slated for V1.4-05 / V1.4-06. Cancelled as a scope decision: cctts
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

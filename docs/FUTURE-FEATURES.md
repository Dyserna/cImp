# Future Features

This document tracks deferred work for cImp. It has three sections:

1. **External dependencies** — features we want but can't build until something upstream changes. Strict format: what / blocker / current workaround / what to do when blocker resolves / how to monitor.
2. **Deferred work** — things we *can* build but chose not to in the current version. Looser format: what / why / cost / trigger to act.
3. **Done / historical** — items previously listed here that have shipped. Kept as a breadcrumb so the supersession is auditable.

General wishlist items without a real blocker or a clear "we considered this and chose to defer" rationale don't belong here. They live in your head until they crystallize, or in a separate `WISHLIST.md` if you really want a list.

---

# 1. External dependencies

(none currently — the previous aider-related entries moved to the historical section in V1.4-07 / v1.3.3 when the aider tab was removed)

---

# 2. Deferred work (no external blocker)

## v1.4 candidates from v1.3 deferrals

These came up during v1.3 design as out-of-scope. Listed roughly by impact-to-effort ratio for future prioritization.

### Pane numbering for `Ctrl+Alt+1..9` direct focus

- **What:** Direct keyboard focus to a numbered pane, distinct from the v1.3 geometric-adjacency arrows (`Ctrl+Alt+Arrow`). Each pane gets a number (visible in a corner of its tab bar); `Ctrl+Alt+N` jumps focus there.
- **Why:** With 5+ panes the arrow-based navigation gets slow — users develop muscle memory for "the aider pane is pane 3." Direct numeric focus is the standard pattern in tmux (`prefix N`) and i3.
- **Cost:** Small. Pane numbers are an in-order traversal of the layout tree; assign at render time, no persistence needed (numbers can shuffle as the tree changes — that's actually fine, because users orient by the *number visible in the corner right now*). Need to surface a small number badge on each pane; `Ctrl+Alt+N` handler walks the tree and focuses the Nth leaf. Collides with the existing `Ctrl+Alt+Arrow` shortcut family in the same modifier space, but the keys are distinct so no conflict.
- **Trigger to act:** if anyone (including future-you) reports navigating between many panes feels slow, or if the v1.3 user-test reveals geometric-adjacency arrows aren't intuitive enough.

### Maximize pane / Zen mode

- **What:** A keyboard shortcut (e.g., `Ctrl+Shift+Z`) that temporarily expands the focused pane to fill the entire content area, hiding all sibling panes. Same shortcut toggles back to the previous layout.
- **Why:** Two use cases. (1) You're focused on Claude generating something long and the side panes are visual noise. (2) You want to take a screenshot or share your screen and only the focused pane is relevant.
- **Cost:** Small. Stash the current layout tree in memory (not persisted), replace with a single-pane layout containing only the focused pane's tabs, restore on toggle-off. The terminals portal store handles the DOM movement automatically. Edge case: if the user splits or drags during Zen mode, the stash becomes stale — easiest is to exit Zen mode on any layout-mutating action and restore from the stashed layout if "exit Zen" is the next thing the user does.
- **Trigger to act:** noise feedback from daily use, or as soon as you find yourself manually closing panes to focus.

### Tearing tabs into a new top-level window

- **What:** Drag a tab outside the main window to create a new cImp window containing only that tab (or that pane). Standard browser pattern. Multi-monitor users want this.
- **Why:** The single-window constraint becomes restrictive on multi-monitor setups — most natural arrangement is "Claude on monitor 1, aider + shells on monitor 2" but the v1.3 layout tree is bounded by the main window's dimensions.
- **Cost:** Substantial. Tauri 2.x supports multi-window but everything in cImp assumes a single window: the audio target tab gate (one global `audio_target_tab`), the avatar overlay (one DOM instance), the compose overlay (one), settings persistence (a single `layout` field, not per-window). Each of these needs to become per-window or stay single-source-of-truth with windows competing for it. Audio is the worst — if you have Claude in window 1 and aider in window 2, who plays sound? The v1.3 "single audio gate" decision was deliberate; multi-window forces revisiting it. Possibilities: only the OS-focused window plays audio (matches v1.3 single-pane-focus rule generalized to windows), or audio mixing (rejected in v1.3).
- **Trigger to act:** multi-monitor use becomes painful enough that the workaround (split-tree-only) feels limiting. Probably real if you start using cImp on a 2- or 3-monitor desk daily.

### Per-cwd / per-project layout memory

- **What:** Different layouts based on which directory cImp launched from. Launching from `~/projects/bid-system` restores the layout you used there last time; launching from `~/projects/scratch` restores its own.
- **Why:** Different work shapes for different projects. Bid analysis wants Claude + aider + a shell for grep work. Scratch project wants just Claude. The v1.3 single global layout makes you reset every time you switch contexts.
- **Cost:** Medium. Settings layout becomes keyed by cwd (or by a project ID derived from cwd, to handle moves). Persistence is a `Map<cwd, LayoutPersisted>` instead of a single field. The integrity check from v1.3 (orphan tabs, missing tabs) runs per-cwd. UI: subtle indicator showing which project's layout is active. Decision needed: do tabs persist per-cwd too, or globally with the layout? Probably per-cwd — the tab list is part of the project context, and keeping a global tab list with per-project layouts means tabs from project A leak into project B's layout. Migration path from v1.3 is non-trivial: one global layout becomes the default for all unknown cwds, or specifically for the cwd cImp was last launched from.
- **Trigger to act:** if you find yourself repeatedly setting up the same layout when switching projects, or saving/restoring presets named after projects (which is the manual workaround).
- **Note:** likely superseded by **"Project-local settings file"** below — that approach generalizes per-cwd state to *all* settings (themes, background image, tabs, shortcuts, layout) rather than just layout, and uses filesystem location instead of a keyed map. If the project-local approach lands first, this entry retires.

### Project-local settings file

- **What:** Allow each project directory to carry its own cImp configuration file. On install, the global file is created at `%APPDATA%\cImp\settings.json` (Windows) / the platform-equivalent path on macOS/Linux — same as today. On launch, cImp checks the cwd for a project-local file (e.g., `./.cImp/settings.json`); if found, that file is the source of truth for the session — every setting (TTS, avatar, themes, background image, tabs, layout, presets, shortcuts, all of it) is loaded from and persisted back to it, while the global file is left untouched. If no project-local file exists, cImp prompts: **(a)** use the global config (default behavior), **(b)** create a project-local config here (seeded by copying the current global as a starting point). The choice is remembered per-cwd so the prompt fires only on first launch from a given directory.
- **Why:** Per-project customization that scales beyond layout. The v1.3 architecture already centralizes everything in one JSON file; a per-project file is the same shape, just located differently. Concrete use cases: distinct color themes per project (dark for review work, high-contrast for presentations), distinct background images per project (a logo or motif for client-facing demos), distinct tab sets (each project's "standard" tabs without polluting other projects), distinct keyboard shortcuts (a project that needs `Ctrl+\\` for something else gets to remap without affecting other projects). Especially valuable once the **Terminal color themes** and **Terminal background image** items ship — color/image identity per project is the most immediately *visible* form of project context, and color-coded screenshots become useful for documentation and onboarding ("here's what the bid-system workspace looks like vs. the scratch one"). Also subsumes the per-cwd-layout-memory item above with a simpler implementation: instead of a `Map<cwd, LayoutPersisted>` inside a single global file, just put a whole settings file per directory.
- **Cost:** Medium, mostly path-resolution and a startup-prompt UX. The runtime architecture barely changes — the existing `SettingsHandle` keeps working as-is once `persistence::load` and `persistence::save` operate on a different path.
  - **Path resolution.** `persistence::config_path` becomes `resolved_config_path(launch_cwd)` returning either `<cwd>/.cImp/settings.json` (project) or `<config_dir>/cImp/settings.json` (global) based on a probe + a per-cwd "preference cache." File location: `.cImp/settings.json` (hidden directory, namespaced — same idiom as `.git/`, `.vscode/`). Migration backups (`.v1.2.bak` etc.) live alongside whichever file is active.
  - **The per-cwd preference cache.** A small file in the global dir tracks "user's choice for this cwd" so we don't re-prompt every launch. Shape: `{ cwd_decisions: { "/abs/path": "global" | "project-local", ... } }`. When the prompt fires, the chosen value is recorded; subsequent launches from that cwd skip the prompt. A "reset preference for this directory" entry in the Layouts/Settings menu lets the user un-record and re-prompt (or switch). Storing the decision globally (not in the project file) is correct: the user's choice is global metadata, not project state.
  - **Startup prompt UX.** A small modal on first launch in an unrecognized cwd. Three buttons: **Use global config** (`global` preference, no project file created), **Create project config here** (copies current in-memory global to `<cwd>/.cImp/settings.json`, sets `project-local` preference), **Cancel** (uses global for this session, doesn't record a preference — re-prompts next launch). A "Don't ask again for this directory" checkbox is implied by the explicit choice — picking either option records the preference. The dialog is dismissable, never blocks the app, and only appears on truly first launch in a cwd.
  - **Migration: existing v1.3 users.** First launch after this lands: the migration synthesizes a `cwd_decisions` entry of `"global"` for the current cwd (so no prompt fires for the directory the user is already running in). New cwds prompt as designed. No data loss — the global file remains the source of truth until the user creates a project file.
  - **Switching modes mid-session.** Out of scope. If a user wants to switch a directory from global to project-local, they restart cImp after creating the file (or via a "Create project config here" entry in the Layouts/Settings menu that does the copy + restart). Hot-swap mid-run would require tearing down and re-broadcasting every settings subscriber (TTS pipeline, audio output, tab registry, frontend store) which is doable but not worth the complexity for a workflow that naturally aligns with launching cImp per project.
  - **No upward directory walk.** Only the exact launch cwd is checked. Don't traverse parents looking for `.cImp/`. Two reasons: predictable behavior ("I launched here, so this is what's active"), and avoids picking up a stale config from an unrelated parent directory (e.g., launching from `~/projects/foo/scripts/` accidentally loading `~/.cImp/settings.json`). Document and leave.
  - **Git/VCS interaction.** A project file at `.cImp/settings.json` is naturally project-scoped — some users will want to commit it (so a team shares the same workspace defaults), others will want it gitignored (so personal preferences don't leak). cImp doesn't enforce either; document the trade-off in the README. Optionally: when creating a project file, write a sibling `.cImp/.gitignore` containing `*.bak\n` at minimum so backup files never end up in version control even if `settings.json` itself is tracked. Worth doing — backups are user-machine state regardless of whether config is shared.
  - **Avatar / image paths inside project configs.** Today, avatar image paths in settings are absolute paths. For shared project configs (gitted), users want path *relative-ness* — an asset stored at `.cImp/avatars/idle.png` should resolve regardless of where the repo is cloned. Add: when a settings file is project-local, paths starting with `./` or relative paths are resolved relative to the project dir; absolute paths still work. Same rule applies to background image paths from the **Terminal background image** feature. Document. (Global config keeps absolute-paths-only behavior since there's no project root to resolve against.)
  - **Settings window scope.** The Settings window operates on whatever file is currently active. A small indicator in the title bar shows "Editing project config (`/abs/path/.cImp`)" vs. "Editing global config" so the user knows which file they're modifying. No mode-switching dropdown — the file is fixed at launch.
  - **Layout presets.** Project-local presets only apply to that project. The user gets per-project preset libraries naturally — restoring a "Build mode" preset in project A doesn't see project B's "Build mode" because they're stored in different files. Cross-project preset sharing is a separate feature (would require a third "user-level presets" tier) and is out of scope here.
- **Trigger to act:** strong candidate once both **Terminal color themes** and **Terminal background image** ship — visual identity per project is the killer use case that makes the per-project file *feel* worth the launch-time prompt. The per-cwd-layout-memory item also folds into this implementation, so if either of those triggers fires (recurring layout reset, recurring theme reconfiguration when switching projects), pick this up. Earlier triggers: a multi-client consultancy workflow where each project has its own palette / branding requirement; a shared-team workflow where the project's `.cImp/` is committed and team members all want the same workspace defaults.

### DoneWhileAway indicator on the pane itself

- **What:** When focus is on pane A and a tab in pane B fires `DoneWhileAway`, the tab strip indicator already shows in pane B. Add a *pane-level* indicator too — a soft border or badge on pane B itself, visible across the whole window.
- **Why:** With many panes visible, scanning each tab bar for the indicator gets tedious. A pane-level cue (e.g., a 2px accent border on the pane that has any DoneWhileAway tab) makes "something needs you" visible at a glance.
- **Cost:** Small. Derive a `pane.has_done_while_away` boolean from the union of its tabs' flags; bind to a CSS class on `Pane.svelte`. The visual needs to be distinct from the focused-pane indicator — different color, or different position (left edge vs. top).
- **Trigger to act:** if you notice yourself missing DoneWhileAway events on non-focused panes during multi-pane work, or if the v1.3 polish round (M5) finds this on its own.

### Pane swap

- **What:** Pick two panes and swap their positions in the layout tree. Useful when you've built up a layout via drag-and-drop and want to reorganize without tearing it down.
- **Why:** Achievable via drag (drag the contents of pane A out to a temporary spot, drag pane B's contents to where A was, drag A's to where B was) — but tedious with 3+ tabs per pane.
- **Cost:** Small as a tree operation (swap two leaf nodes, preserve their parents). UX is the question: how does the user pick "this pane and that pane"? A modal? Click-pane-then-click-pane? Probably a right-click menu item "Swap with..." → submenu listing other panes (same as the existing "Move all tabs to" submenu pattern). Reuses M3's UI vocabulary.
- **Trigger to act:** noise from real use.

### Split ratio quick-presets

- **What:** Right-click the splitter line → "50/50", "70/30", "30/70" preset ratios.
- **Why:** Splitter drag is precise but slow when you want a clean even split. A click on a preset is faster.
- **Cost:** Trivial. Tiny popover on splitter right-click.
- **Trigger to act:** if you find yourself fiddling with splitter positions regularly. Low priority overall.

### Custom keyboard shortcuts UI for layout actions

- **What:** Settings UI for editing all v1.3 shortcuts. Currently the shortcuts are hand-edited in `settings.json` (consistent with v1.2's other shortcuts).
- **Why:** Every other modern app has a shortcuts editor. Hand-editing JSON is fine for power users but raises the bar for less-technical users.
- **Cost:** Medium. UI for capturing key combinations (most webview frameworks have library support for this), validation against conflicts, persistence into the existing `shortcuts` settings field. The hard part is the conflict-detection UX: two cImp shortcuts can't bind the same combo, but cImp can't detect OS-level conflicts (`Ctrl+Alt+Arrow` on GNOME) — that's just documentation.
- **Trigger to act:** if the project ever gains non-developer users, or if you yourself forget what a shortcut binding is and find yourself opening settings.json frequently.

### Hide pane tab bar when only one tab is in the pane

- **What:** When a pane contains exactly one tab, hide its tab bar. Saves ~30px of vertical space per pane. The tab bar reappears if a second tab is added.
- **Why:** With 4+ panes visible, each with a 30px tab bar, vertical real estate adds up. Single-tab panes don't need the bar.
- **Cost:** Trivial CSS, but UX implications: the user can't right-click "Split horizontally" via the pane context menu (which lives in the tab bar background area) when the bar is hidden. Workaround: rely on shortcuts (`Ctrl+\\`), or add an alternate right-click target (e.g., right-click anywhere in the empty terminal area triggers the pane menu — but that conflicts with xterm.js's right-click).
- **Trigger to act:** if real use shows screen real estate is the binding constraint, especially on small displays.

### Pane-aware compose overlay

- **What:** Compose overlay submits to a *specific* pane chosen at compose time, not necessarily the currently focused pane. E.g., compose a long message intended for Claude (in pane B) while focused on a shell pane (pane A) where you're checking something during composition.
- **Why:** v1.3's compose-targets-focused-pane rule is simple but means "switching focus mid-compose to look up something" sends your message to the wrong tab. Workaround: don't switch focus during compose. Annoying.
- **Cost:** Medium. Compose overlay needs a target-tab selector (a dropdown showing all tabs across all panes, defaulting to the current focused pane's active tab). The selector locks the target at compose-open time and shows it in the overlay header. UX needs care to avoid confusion (the user doesn't accidentally send to the wrong tab because the dropdown wasn't visible).
- **Trigger to act:** if focus-switching-during-compose actually happens to you in practice, or feedback from real use.

### Audio mixing (multiple panes playing simultaneously)

- **What:** Multiple panes' TTS plays at the same time, mixed into a single audio stream.
- **Why:** Removes the "synthesis happens but audio drops" waste in non-focused panes.
- **Cost:** Substantial, and **probably wrong**. The v1.3 single-audio-gate decision was intentional — having Claude's voice and aider's voice (and a shell error tone) all overlap is incoherent for the user. Audio is fundamentally serial in attention. v1.3's "DoneWhileAway flag + visual indicator" handles non-focused completions gracefully without audio overlap.
- **Trigger to act:** **deferred indefinitely.** Listed here only so the decision is recorded — not as something we plan to revisit.

### Terminal color themes

- **What:** A set of selectable color themes (foreground, background, ANSI 16, cursor, selection) applied to terminal tabs. Bundled set of ~10-12 popular themes (Dracula, Solarized Dark/Light, Nord, Tomorrow Night, Gruvbox Dark/Light, One Dark, Monokai, Tokyo Night, GitHub Dark, Default). Plus a "Custom..." option exposing a 16-color editor for users who want their own. **Inheritance model: global default + per-tab override.** Settings has a global theme that all tabs use unless individually overridden; the per-tab Configure dialog can override with a different bundled theme or a custom one. Per-tab override is a property of the tab, not the pane — a tab carries its theme with it when it moves between panes during drag-and-drop.
- **Why:** Default xterm.js colors are fine but bland; users have strong preferences for terminal palettes (often the same one they use in VS Code / their editor). Theme parity across editor, terminal, and AI assistants reduces visual context-switching cost. Per-tab override enables color-coding tabs by purpose: Claude in dark blue, aider in green, shell tabs in default. With v1.3's multi-pane layouts, tabs frequently sit visible side-by-side, which makes color-coding *useful* (not just decorative) — distinct themes give an at-a-glance "which tab am I about to type into."
- **Cost:** Small to medium.
  - **Bundled theme registry:** a static `frontend/src/themes/index.ts` mapping theme name → `ITheme` object. ~12 entries, each ~24 hex strings. Maintained in source. The `ThemeColors`/`ITheme` shape mirrors xterm.js: `foreground`, `background`, `cursor`, `cursorAccent`, `selectionBackground`, `selectionForeground`, plus `black`/`red`/`green`/`yellow`/`blue`/`magenta`/`cyan`/`white` and the 8 `bright*` variants. All hex strings.
  - **Schema — global:** `terminal.theme.name: string` (bundled theme name, defaults to `"Default"`) and `terminal.theme.custom: ThemeColors | null` (used only when `name === "Custom"`).
  - **Schema — per-tab:** each tab in the `tabs` array gains a `theme_override: { name: string, custom: ThemeColors | null } | null` field. `null` means inherit global (the common case; existing tabs migrate as `null`). Override-set tabs use their own theme regardless of the global setting.
  - **Theme resolution:** at terminal creation and on settings change, resolve `effectiveTheme(tab) = tab.theme_override ?? globalTheme`. Wire through a small `themeFor(tabId)` helper used at the single Terminal-construction site.
  - **Wiring:** `terminals.createForTab(tabId)` reads the resolved theme and passes it to `new Terminal({ theme: ... })`. On theme change at runtime (global or per-tab), walk affected Terminal instances and assign `term.options.theme = newTheme` — xterm.js supports this without recreating the terminal. Global change → walk all tabs without an override. Per-tab change → walk just that tab.
  - **Migration from a no-theme settings file:** add the `terminal.theme` group with `name: "Default"` and `custom: null`. Add `theme_override: null` to each existing tab. Idempotent.
  - **Global UI:** Settings → Appearance section. Dropdown of theme names (with a small color-swatch preview next to each name). When "Custom" is selected, expand a panel with the 16+ color pickers.
  - **Per-tab UI:** the existing Configure Tab dialog (v1.2 / v1.3) gains an "Appearance" section with a theme dropdown that includes a "**Use global default** (current: [Theme Name])" option as the first entry — selecting it sets `theme_override = null`. Other dropdown entries set `theme_override = { name: ..., custom: null }`. A "Custom..." entry opens the same color-picker editor used in global Settings, but writes to `tab.theme_override.custom`.
- **Builtin tabs:** Claude and aider inherit global by default like any tab. The user can override per-tab the same way as Shell tabs. Consistent rule, no special-casing.
- **Theme import (iTerm2 `.itermcolors`, Windows Terminal JSON, etc.):** deferred. Bundled set + custom editor covers ~95% of real need. If shipped, the importer feeds the same `ThemeColors` schema; orthogonal to the global-vs-per-tab question.
- **Trigger to act:** any time. Popular-feature polish item users expect; ship when there's bandwidth. Not blocked on anything.

### Terminal background image

- **What:** A custom image displayed beneath the terminal text. User picks an image file (PNG/JPG/WebP), tunes opacity (dimming overlay alpha) and blur (CSS `backdrop-filter`). **Staged rollout:** the initial implementation ships with a *global* image only — one image that applies to all tabs, configured in Settings → Appearance. The per-tab override schema and resolution logic are in place from day one (so the architecture is right from the start and there's no migration churn later), but the *UI* for per-tab override is deferred to a follow-on release. Once it ships, per-tab override behaves the same as themes: the per-tab Configure dialog can set a different image (or explicitly disable the image for that tab), and the per-tab setting moves with the tab during drag-and-drop. Per-tab is a property of the tab, not the pane.
- **Why:** Visual personalization. Some users like ambient imagery in their workspace (gradient, abstract art, photograph) — terminals running full-screen for hours benefit from being visually distinct from a slab of solid color. Same reason GNOME Terminal, iTerm2, and Windows Terminal all offer background images. The follow-on per-tab override extends the color-coding rationale: distinct background images per tab type (Claude has a starfield, aider has a forest, shells stay plain) give an instant visual identity in multi-pane layouts where tabs sit visible alongside each other — but that's a v2 of this feature, not the initial ship.
- **Cost:** Small implementation, but with a real performance trade-off worth understanding.
  - **The xterm.js renderer constraint.** xterm.js has three renderers: DOM, canvas (default), and WebGL (via addon). The canvas renderer fills the host element with the theme background color opaquely on every frame — a transparent theme bg shows nothing through it because the canvas itself is opaque. To get a transparent terminal that lets a CSS `background-image` show through, you must set xterm.js's `allowTransparency: true` option, which **forces the slower DOM renderer** instead of canvas. WebGL renderer doesn't support transparency at all.
  - **Performance impact.** Canvas → DOM is roughly a 2-5× slowdown depending on workload. For typical cImp use (Claude streaming text at human-readable speeds, shell tabs running interactive commands) it's not noticeable. For high-throughput output (`tail -F` on a fast log, `find /`, `cargo build` with thousands of warnings) the DOM renderer will lag visibly. This is a known and well-documented xterm.js limitation, not a cImp bug.
  - **Per-tab-renderer wrinkle.** Because rendering choice is per-`Terminal`-instance, **only tabs that have a background image (resolved from global+override) need DOM renderer.** A tab with no background image keeps canvas. So: a user with global image disabled + override-image on the Claude tab gets canvas everywhere except Claude. This is good — it limits the perf cost to exactly the tabs that opted in. The renderer is set at `new Terminal({ allowTransparency })` time and can't be changed without recreating the instance, so toggling background image on/off mid-session means recreating that tab's Terminal (lose scrollback unless we replay it from the PTY's frontend buffer — defer that complexity, accept "scrollback resets when toggling background image").
  - **Schema — global:** add a `terminal.background` group:
    ```
    terminal.background.image: string | null   // file path, null = no image (default)
    terminal.background.opacity: number        // 0.0-1.0, alpha of dimming overlay (default 0.4)
    terminal.background.blur: number           // px, CSS backdrop-filter blur (default 0)
    terminal.background.size: "cover" | "contain" | "tile"  // CSS background-size (default "cover")
    terminal.background.position: string       // CSS background-position (default "center")
    ```
  - **Schema — per-tab:** each tab gains `background_override: BackgroundConfig | "disabled" | null`. Three states:
    - `null` → inherit global (default for existing tabs)
    - `"disabled"` → explicitly disable background image for this tab even if global is set (useful for "I want backgrounds on most tabs but not on aider where I'm reading diffs")
    - `BackgroundConfig` (same shape as the global group) → override with a different image/opacity/blur for this tab
  - **Background resolution:** `effectiveBackground(tab) = tab.background_override === "disabled" ? null : (tab.background_override ?? globalBackground)`. Returns either a `BackgroundConfig` or null (no image, use canvas renderer).
  - **Wiring:** `terminals.createForTab(tabId)` reads the resolved background. If non-null, sets `allowTransparency: true`, sets the theme `background` color to `rgba(0,0,0,${opacity})` (or honors the active theme's bg with adjusted alpha), applies `backgroundImage`/`backgroundSize`/`backgroundPosition` on the host `<div>`. If `blur > 0`, wraps the terminal cells layer in a `backdrop-filter: blur(${blur}px)` container. If null, default canvas-renderer construction with no image styles applied to the host.
  - **Settings change at runtime:** on global background change, recreate Terminal instances for tabs without an override. On per-tab override change, recreate just that tab's Terminal. Recreation goes through the existing `terminals.destroyForTab` / `terminals.createForTab` flow plus a PTY-frontend-buffer replay (the PTY itself stays alive — only the xterm.js frontend is recreated). Scrollback before the change is lost; document this. Most users change background settings rarely, so it's fine.
  - **Global UI (initial release):** Settings → Appearance, below the theme picker. File picker for the image, sliders for opacity and blur, dropdown for size mode. Live preview if cheap. This is the only background-image UI surface in the initial ship — the per-tab override fields exist in the schema but cannot be set by the user yet.
  - **Per-tab UI (follow-on release):** Configure Tab dialog → Appearance section, below the per-tab theme override. Three radio options: "**Use global default**" (`background_override = null`), "**No background image**" (`background_override = "disabled"`), "**Custom**" (`background_override = BackgroundConfig`). Custom expands the same controls as the global UI. Because the schema and resolution logic already support these states from the initial release, this is a pure UI addition — no schema migration, no settings file changes for existing users.
  - **Image storage:** reference the user's chosen file by absolute path. Fast, no disk overhead, no copy. If the file becomes invalid later, show a clear error message in the Settings panel ("Image file not found at: /path/...") and treat as `null` for rendering. Alternative — copy into cImp data dir at pick time — adds robustness but adds disk and clutters cImp state. Path-reference is the right default.
- **Builtin tabs:** Claude and aider inherit global like any tab. Once per-tab UI ships, override is available the same way as Shell tabs.
- **Animated/video backgrounds:** out of scope. Performance cost much higher; use case dubious. Static images only.
- **Trigger to act (initial release):** any time. Popular-feature polish; bigger caveat to document than themes (the renderer/perf trade-off + the recreation-loses-scrollback gotcha) but otherwise straightforward.
- **Trigger to act (follow-on per-tab UI):** real use feedback that the global-only constraint pinches — e.g., wanting a clean background on aider while keeping the global image elsewhere. The "explicitly disable per tab" use case is the most likely first ask. If it doesn't come up in real use, the follow-on may never need to ship.

### Restore modern-dark's distinct surface steps

- **What:** Scope the "chrome follows the terminal palette" integration (v0.8.0) to the **tui-\*** themes only, and give **modern-dark** back its original layered surface ramp (`--surface-0` … `--surface-4` as distinct slate steps) instead of collapsing them all onto `--term-bg`.
- **Why:** v0.8.0 repointed `--surface-0/1/2/sunken/deep/input` and the neutral text ramp to `var(--term-bg/--term-fg, …)` in **every** theme block so the tab bar / status bar / sidebar integrate with the terminal colors. For the tui-\* themes that's the whole point (they're meant to read as one flat terminal surface). But modern-dark's design depends on *stepped* elevation — tab bar, panes, dialogs, and popovers sit on visibly different slate shades, with depth coming from those steps plus shadow. Collapsing them onto a single `--term-bg` flattens that look. modern-dark isn't the default (tui-orange is), so the regression was accepted at ship time rather than blocking the release.
- **Cost:** Small, mostly a decision about *mechanism*. Options: (a) revert the `var(--term-bg, …)` / `var(--term-fg, …)` substitutions inside the `:root, [data-theme="modern-dark"]` block only, leaving the literal slate values — simplest, but then modern-dark's chrome no longer follows the terminal palette at all (probably fine; it's a designed theme, not a "wrap the terminal" theme). Or (b) keep modern-dark palette-aware but preserve elevation by deriving each surface step as a `color-mix` of `--term-bg` toward white/black (e.g. `surface-2 = color-mix(in srgb, var(--term-bg) 92%, white)`), so the steps survive on any palette — more work and needs contrast tuning per palette. The text ramp has the same choice. Per the theme-isolation policy each theme block is edited independently, so this only touches the modern-dark block (and possibly a note in `theme.css`). The `--term-bg`/`--term-fg` publishing in `main.ts`/`settings_main.ts` stays as-is (still needed by the tui themes).
- **Trigger to act:** if modern-dark gets real use and the flattened chrome reads as a regression, or anyone asks why modern-dark "lost its depth." Until someone actually runs modern-dark day-to-day, low priority.

### Touch / pen drag-and-drop

- **What:** Drag tabs via touch or pen instead of mouse. Use `pointerdown`/`pointermove`/`pointerup` instead of `mousedown`/etc.
- **Why:** Touchscreen laptops, tablets running cImp, accessibility.
- **Cost:** Small to medium — the M2 drag implementation uses `mouse*` events; switching to `pointer*` events covers mouse + touch + pen with a single code path. The hard parts are touch-specific UX (no hover state for drop-zone preview before commit) and pen pressure sensitivity (probably ignore).
- **Trigger to act:** if you ever run cImp on a touchscreen device, or accessibility need.

## v1.2 deferrals still pending

Items deferred during v1.2 design that didn't get picked up in v1.3:

### Per-shell-tab environment variable UI

- **What:** UI in the Configure Tab dialog for editing the `env: HashMap<String, String>` field. The field already exists in the schema; settings.json hand-editing works.
- **Why:** Common need (set `NODE_ENV=development` for a project shell, `PYTHONPATH=...` for another).
- **Cost:** Small. Add a key/value list editor to the Configure dialog.
- **Trigger to act:** the moment you hand-edit settings.json to add an env var. That's the signal that the friction is real.

### Profiles/templates for shell tabs

- **What:** Saved shell tab configurations (name, command, args, cwd, env) the user can spawn from. Choose "WSL Ubuntu" or "Python venv" from a menu instead of filling in the New Shell Tab dialog every time.
- **Why:** If you create the same kind of shell tab repeatedly, profiles save time.
- **Cost:** Medium. Schema additions, UI to save current tab config as a profile, profile picker in the New Shell Tab dialog. Overlaps conceptually with v1.3's *layout* presets — both are "named templates the user picks from." Could be unified later.
- **Trigger to act:** if you find yourself filling in the same New Shell Tab values repeatedly.

### Shell auto-restart on crash

- **What:** When a shell subprocess exits unexpectedly, automatically restart it (with backoff and a max-retry count) instead of requiring the user to press Enter on the closed-state overlay.
- **Why:** For long-lived shells (e.g., a `tail -F` watching logs) that occasionally die, manual restart is friction.
- **Cost:** Small. A per-tab setting `auto_restart: bool` (default false), plus restart logic with sensible backoff (1s / 2s / 4s / cap at 30s, max 5 attempts before falling back to closed-state UI).
- **Trigger to act:** if you set up a Shell tab as a long-running watcher and find it dying repeatedly.

### Notification text variables beyond `{code}`

- **What:** Additional placeholders for notification text editing: `{name}` (tab name), `{tab_position}` (its index), `{cwd}` (working directory), `{timestamp}`. Currently only `{code}` (subprocess exit code) is interpolated.
- **Why:** Customization. "Shell '{name}' (PID {pid}) exited with code {code}" is more informative than "Shell exited."
- **Cost:** Trivial. Extend the interpolation function.
- **Trigger to act:** if you customize notification text and find `{code}` insufficient.

### History/log of subprocess exits

- **What:** A log UI (probably in Settings) showing subprocess starts, exits, and restart counts over time.
- **Why:** Useful for debugging "why does my shell keep dying" situations.
- **Cost:** Small. Append-to-ring-buffer in memory; persist last N (e.g., 100) entries to settings or a sidecar file. Settings dialog gains a "Subprocess log" tab.
- **Trigger to act:** if a shell tab has flaky behavior and you wish you had a record.

## V6 (speech) follow-ups

These build on the V6-01 offline speech-to-text milestone (`docs/MILESTONE-V6-01-speech-to-text.md` — record button + push-to-talk dictation into the compose overlay, whisper.cpp, portable Vulkan GPU). They escalate that pipeline rather than start fresh.

### No-hands mode (always-on voice control)

> **Design status: broad sketch.** The specifics below — the wake phrase, the command vocabulary, which overlay actions are voiceable — are an *illustration of the idea*, not a spec. They will almost certainly change if/when this is built. Recorded now so the direction and its real costs are captured while V6-01 is fresh.

- **What:** A fully hands-free mode. Speech-to-text is armed continuously instead of held on a key. A **wake phrase** (sketch: "ok, claude") opens the compose overlay and starts capturing dictation; a small set of **spoken commands** drive the overlay instead of the keyboard — e.g. "accept composition" / "send it" acts like pressing Enter (submit), "cancel" closes the overlay, "new line" inserts a break, "scratch that" clears. The user dictates a message and sends it to Claude without touching the keyboard at all. This is the natural escalation of V6-01 from *press-to-talk* to *just-talk*.
- **Why:** True hands-free operation. Two real drivers: **accessibility** (RSI, motor impairment, anyone for whom the keyboard is the barrier) and **ergonomics/ambient use** (dictating while pacing, reading something else, or away from the keys). V6-01 already shipped the hard half — offline transcription, the capture thread, the compose overlay, the `sttState` indicator, and the push-to-talk state machine. No-hands mode is the controller that sits on top of all of it. Since cImp is *already* a voice-forward app (it speaks Claude's replies via TTS), closing the loop so the user can also *speak* to Claude is the coherent end state.
- **Cost:** Substantial — and it's not one feature, it's several distinct hard problems, only the last of which V6-01 already solved:
  - **Low-power always-on capture.** whisper.cpp is a *batch* transcriber, not a streaming/always-listening engine. Running it continuously over the live mic is exactly the constant GPU/CPU burn (and fan noise) we just worked to avoid. Need a cheap front gate: **voice-activity detection** (Silero VAD / WebRTC VAD) so whisper only wakes on speech, and ideally a dedicated **wake-word model** (openWakeWord, Porcupine) so the heavy transcriber runs *only after* the wake phrase fires. That's a new dependency category (VAD / wake-word), separate from whisper, and the gating logic is the bulk of the work. **This is the prerequisite** — without it the feature reintroduces the always-hot-GPU problem.
  - **Self-trigger / echo.** The avatar speaks Claude's replies through the same machine's speakers. An always-hot mic will hear that TTS and can self-trigger the wake word (Claude literally saying "ok" wakes itself) or capture its own voice as dictation. Needs either acoustic echo cancellation or — simpler — gating STT while TTS is playing. The feedback loop is a genuine hazard, not an edge case.
  - **Command-vs-dictation disambiguation.** Once capturing, the system must distinguish "text the user wants typed" from "a command to execute." If "cancel" is both a command *and* a word someone might dictate, you have a problem. Options, all with trade-offs: a reserved command grammar matched post-transcription; a command prefix ("claude, send"); a separate command mode toggled by phrase. This ambiguity is the core UX problem and the main reason the example commands "will probably change."
  - **Listening-state machine + trust UI.** A clear model — asleep → wake-detected → dictating → command → action — with an always-visible indicator of whether the mic is hot. An always-listening microphone is a materially different privacy posture than push-to-talk: it needs an obvious on/off and a prominent live indicator (extend the existing status-bar record button / `sttState` store), and must stay **fully offline** like the rest of V6-01. This is as much a trust feature as a technical one.
  - **Action mapping (the cheap part).** The spoken commands map onto surface that already exists — `submit_compose` / `cancel_compose` in the shortcut dispatcher, and the compose overlay's own edit operations. So once a command is *recognized*, firing it is nearly free. The expense is entirely in the recognition + gating layers above.
- **Trigger to act:** a concrete accessibility need (a user who genuinely can't drive the keyboard) is the strongest. Otherwise: once V6-01 dictation sees enough real use that reaching for push-to-talk becomes the friction. **Gated on first solving low-power always-on capture** (VAD/wake-word) — don't start here until that prerequisite is in hand, or the feature undoes the GPU/fan work from V6-01.

## Auto-detect Blackwell (or any unsupported GPU) and gracefully skip CUDA opt-in

- **What:** When `CIMP_GPU=cuda` is set, probe the GPU compute capability before registering the CUDA EP. If the CC is unsupported by the bundled ORT prebuilt (currently sm_120 / Blackwell), log a clear warning and fall back to CPU instead of letting the user see per-segment `cudaErrorSymbolNotFound` errors and silent output.
- **Why:** Today the `CIMP_GPU=cuda` opt-in is honest but unfriendly on Blackwell — registration succeeds, the session commits, and inference fails per-segment with a cryptic CUDA error. A pre-flight probe gives a single clear message at startup.
- **Costs to weigh:**
  - Querying CC requires loading the CUDA runtime, which we already do indirectly via ort. Either add a tiny `cudarc` (or similar) dependency to call `cudaDeviceGetAttribute`, or shell out to `nvidia-smi --query-gpu=compute_cap --format=csv` (works but ugly and adds a subprocess).
  - The "supported CC list" needs to be maintained alongside ort bumps — a magic list is fine but easy to forget to update. Could instead do a real probe inference (build session, run a 1-token forward pass, catch failure) which is self-validating but slower at startup.
- **Trigger to act:** if anyone besides the dev box reports the "registered but no audio" symptom on Blackwell, OR when `ort` upgrades to a version that adds new GPU support and we want the probe to handle the next-gen-GPU regression class generally.
- **Related:** `MAINTENANCE.md` "ort / ONNX Runtime" entry tracks the underlying ORT 1.20 + Blackwell mismatch.

## Unify TTS and STT on one inference runtime (ORT + WebGPU) — DECIDED: not now

- **Decision (2026-06-15):** **No-go for the foreseeable future. STT stays on `whisper-rs`/whisper.cpp + ggml-Vulkan.** It was just shipped (V6-01), it's fast and accurate, and because ggml's Vulkan backend is already cross-platform, **Linux is not blocked** by leaving STT where it is. Revisit only if a real issue forces it — provisionally re-evaluate in a couple of months once TTS-on-WebGPU has proven the native EP is solid on real hardware.
- **What it would be:** the only viable convergence point is **ONNX Runtime + WebGPU**, not ggml. Kokoro has no mature ggml port (so TTS can't move to STT's stack), but Whisper exports cleanly to ONNX — including an all-in-one encoder+decoder+beam-search graph — and Whisper-on-WebGPU is heavily proven via Transformers.js / onnxruntime-web. So unifying means **STT migrates onto `ort`**, dropping whisper.cpp, after TTS adopts WebGPU.
- **Why it's tempting:** one inference runtime and one GPU backend for both subsystems; and — the strongest pull — it **deletes the entire `stt-vulkan` build saga** (Vulkan SDK + forced Ninja generator + MAX_PATH wall + compiling whisper.cpp from source; see `MAINTENANCE.md`). The ORT WebGPU prebuilt erases all of that.
- **Why we're not doing it:** whisper.cpp is batteries-included and `stt/engine.rs` leans on all of it — mel preprocessing, the encoder→decoder loop, greedy/beam decoding, language auto-detect, segment assembly. ONNX Whisper gives a graph, not a pipeline: you'd either drive the all-in-one beam-search export (whose `BeamSearch` is a contrib op that historically runs on **CPU**, so it's not a cleanly all-GPU path) or re-own the decode loop in Rust. That's real new surface area traded for runtime symmetry, against a working, proven STT path. The native (non-browser) WebGPU EP running the merged model fully on GPU is also less proven than the browser path. Net: not worth regressing a good STT path purely for "same tech."
- **Note:** operationally the two **already converge on Vulkan under Linux** once TTS adopts WebGPU (Dawn for TTS, ggml for STT) — the portability/packaging story is unified even with different runtimes. Sharing the literal same runtime crate is mostly internal-tidiness, not a user-facing win.
- **Trigger to act:** if the whisper.cpp Vulkan build toolchain becomes a recurring CI/maintenance burden (the saga keeps biting on bumps), OR a concrete STT issue on the current stack forces a rethink — and only after TTS-on-WebGPU has demonstrated the native EP is reliable enough to bet STT on. Sequence is fixed: TTS to WebGPU first, STT migration as a gated follow-on, never the reverse.

---

# 3. Done / historical

## ~~Portable GPU TTS via the ONNX Runtime WebGPU EP~~ — shipped (replaces the CUDA-only TTS opt-in)

Kokoro TTS now runs on ONNX Runtime's native **WebGPU EP** (Dawn-backed → D3D12 on
Windows, Vulkan on Linux, Metal on macOS) via the `tts-webgpu` Cargo feature,
which the release builds (`--features stt-vulkan,tts-webgpu`). Portable and
vendor-agnostic with automatic CPU fallback, mirroring `stt-vulkan`. Validated
2026-06-15 on Blackwell (RTX 5090): correct output, genuinely on-GPU, ~5× faster
than CPU, and it runs the `ConvTranspose` that broke DirectML. The old runtime
`CIMP_GPU=cuda` opt-in is gone; CUDA survives only as the optional, non-default,
mutually-exclusive `tts-cuda` build (NVIDIA-only, not shipped). Implementation and
the Phase 0 validation results: `docs/features/FEATURE-tts-webgpu.md`; dependency
notes in `MAINTENANCE.md`; packaging (Dawn dylibs) in `PACKAGING.md`. The separate
"unify STT onto the same runtime" question remains explicitly deferred — see § 2.

## ~~Aider TTS markup injection~~ — superseded by Aider removal (V1.4-07 / v1.3.3)

The aider tab was removed in V1.4-07 (released as v1.3.3) in favor of a second Claude Code tab preconfigured to talk to a local LLM. The original premise of this entry — bring aider to TTS parity with Claude when upstream gains a system-prompt-injection flag — is obsolete; cImp no longer hosts aider. The local-LLM use case that motivated it is now covered by the Claude (local) tab, which uses the same `--append-system-prompt` mechanism as the subscription Claude tab.

If aider support is re-added in the future (community ask, etc.), this entry can be revived.

## ~~Aider permission detection patterns~~ — superseded by Aider removal (V1.4-07 / v1.3.3)

Same context as above. The aider tab is gone; permission-detection patterns for aider's confirmation prompts are no longer relevant. Both AI builtins (subscription Claude and Claude (local)) run the same Claude Code permission-detection patterns since they're the same binary.

## ~~Per-tab avatar configuration~~ — considered, decided global-only (2026-05-07)

Listed as a v1.2 deferral and slated as `MILESTONE-V1.4-05-per-tab-avatar.md`. Cancelled as a scope decision: cImp ships exactly one avatar for the entire app, customized globally. Per-tab variation was speculative ("different sprite for the aider tab"); the user explicitly does not want it. The avatar overlay stays a single global instance reading `avatar.images` directly with no per-tab override resolver.

## ~~Per-tab TTS settings~~ — considered, decided global-only (2026-05-07)

Listed as a v1.2 deferral and slated as `MILESTONE-V1.4-06-per-tab-tts.md`. Cancelled at the same time and for the same reason as per-tab avatar: one TTS voice / speed / volume for the whole app, customized globally. The TTS worker continues reading `settings.tts.{voice,speed,volume}` directly with no per-tab override resolver.

## ~~Espeak fallback for out-of-vocabulary words~~ — shipped (default)

Always on — `misaki-rs` is pulled in with default features, which includes its
`espeak` fallback. espeak-ng is statically linked, so no `libespeak-ng.dll` is
shipped, but `espeak-ng-data/` (~7.5 MB) sits next to `cimp.exe` (auto-copied
by `build.rs`). The compiled binary is GPLv3 (see `NOTICE`); cImp source stays
Apache-2.0. Builds need `libclang.dll` for bindgen — pinned via
`src-tauri/.cargo/config.toml`. Verified end-to-end: `"eBook" → "ˈi bˈʊk."`.

---

# Adding new entries to this document

**External dependencies (section 1)** — strict format. Use this when something has to change in code you don't own (upstream library, OS, hardware support) before you can build. Format:

- **What it is** — one paragraph
- **Status** — clear blocker statement
- **What's blocking it** — specific external thing
- **What current versions ship with** — the workaround or omission
- **What to do when blocker resolves** — actionable steps
- **How to monitor** — where to check

**Deferred work (section 2)** — looser format. Use when you *could* build it but chose not to, and want to record the reasoning. Format:

- **What** — short description
- **Why** — what use case it serves
- **Cost** — implementation effort, design decisions, gotchas
- **Trigger to act** — what observation should make us pick this up

If a deferred item is just a "wouldn't it be cool if..." with no real use case behind it, don't add it here. Keep this list to things that have either a felt need or a known cost-of-deferral.

**Done / historical (section 3)** — items previously listed elsewhere that have shipped. Strikethrough header, brief note, kept as a breadcrumb.

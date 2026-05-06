# Future Features

This document tracks deferred work for cctts. It has three sections:

1. **External dependencies** — features we want but can't build until something upstream changes. Strict format: what / blocker / current workaround / what to do when blocker resolves / how to monitor.
2. **Deferred work** — things we *can* build but chose not to in the current version. Looser format: what / why / cost / trigger to act.
3. **Done / historical** — items previously listed here that have shipped. Kept as a breadcrumb so the supersession is auditable.

General wishlist items without a real blocker or a clear "we considered this and chose to defer" rationale don't belong here. They live in your head until they crystallize, or in a separate `WISHLIST.md` if you really want a list.

---

# 1. External dependencies

## Aider TTS markup injection

### What it is

Inject TTS markup instructions (the `[[TTS]]...[[/TTS]]` convention) into aider's system prompt at launch, the same way the Claude tab does via `claude --append-system-prompt`. This would enable spoken TTS for aider tab output, bringing it to parity with the Claude Code tab.

### Status

**Deferred pending upstream support in aider.**

As of the v2 design phase (Q2 2026), aider does not provide a CLI flag for appending content to the system prompt. The closest available mechanism is `--read <file>`, which adds files as read-only context (chat-level user messages), not as system prompt content. Per aider's own community discussions, instructions delivered as user messages are treated differently by many LLMs and may be ignored or deprioritized — which is why direct system prompt injection is what we want.

### What's blocking it

An open aider feature request exists for a `--system-prompt-extras <file>` flag (or similar):

- aider issue #4817: "Option: --system-prompt-extras to always append file content to system prompt for coder mode"
- The request describes exactly the use case cctts has: append a small instructions file to the system prompt at every request, optionally re-reading the file each turn so users can edit instructions live

If aider adds this flag (or any equivalent mechanism — e.g., `--append-system-prompt`, `--system-prompt-file`, or a config-key override settable via CLI like `-c system_prompt_extras=...`), we should adopt it.

### What current versions ship with

- Aider tab spawns aider as a subprocess in a PTY, exactly like the Claude tab spawns claude
- No system prompt injection — aider runs with its default behavior
- TTS markup tags will not appear in aider's output, so no speech will play for the aider tab (the cctts fallback-silent behavior handles this naturally)
- Tab status indicators, notifications, avatar state, and permission detection (when aider patterns are added) all still function — these are independent of TTS markup
- Documentation in the README explicitly notes that aider tab TTS coverage depends on upstream aider support

### What to do when the blocker resolves

When aider releases a CLI flag for system prompt injection:

1. **Verify the mechanism.** Check aider's release notes and CLI reference for the new flag's exact name, semantics (replace vs. append), and file vs. inline-string semantics. Match against what cctts needs (append, file-based or inline-string, applied at every turn).

2. **Update the aider tab launch logic.** The PTY spawn for the aider tab should pass the new flag with cctts's TTS markup instructions, similar to how the Claude tab passes `--append-system-prompt` today. The implementation should be isolated to a single function (the aider-spawn helper) so this is a one-place change.

3. **Decide on file vs. inline string.**
   - If aider's flag is inline-string-based (`--append-system-prompt "..."`), pass the markup instructions directly. Simplest. Same approach as Claude.
   - If aider's flag is file-based only (`--system-prompt-extras <file>`), cctts writes a small temp file at app launch with the markup instructions, passes the path to aider, and cleans up the temp file when the aider subprocess exits. Use the OS temp directory (`/tmp` on Linux, `%TEMP%` on Windows).
   - If both are available, prefer the inline-string form to avoid temp-file management.

4. **Add a per-tab "TTS injection enabled" toggle to settings.** Default on. Lets the user disable injection if a particular model performs poorly with the markup convention or if it interferes with their workflow. The toggle is per-tab so the Claude tab and aider tab can be controlled independently.

5. **Update the README and design documentation.** Remove the "aider TTS not supported" caveat. Note that TTS markup compliance still depends on the local model's instruction-following ability — smaller local models may not wrap content in tags reliably even when instructed.

6. **Test with both cloud and local models.** The Claude tab uses Claude (good at following system prompts). The aider tab might be configured with anything from GPT-4 to a 7B local model. Verify that markup tags appear in output for at least one capable model (e.g., Claude via API, or GPT-4o), then accept that smaller local models may be inconsistent.

### How to monitor for upstream resolution

- **Periodic check**: every few months, run `aider --help` and look for a new system-prompt-related flag, or visit the aider releases page on GitHub
- **Issue subscription**: subscribe to the GitHub issue (#4817 and related), so notifications fire when the feature lands
- **Search**: a quick search for `aider --append-system-prompt` or similar would surface release notes and tutorials if the flag is added under any name

## Aider permission detection patterns

### What it is

Detect aider's confirmation prompts (e.g., "Apply this edit? (Y)es/(N)o/(D)on't ask again") in the same way cctts detects Claude Code's permission prompts, and trigger the same `AwaitingPermission` avatar state and notification.

### Status

**Deferred pending pattern enumeration.** Not strictly an external dependency — aider's prompt strings exist and are observable — but blocked on someone sitting down with aider's source or live runs and producing a robust regex set covering all confirmation flows (edits, file creation, command execution, etc.).

### What's blocking it

- Aider's confirmation prompts are scattered across multiple code paths and may have rephrased wording across versions
- Some prompts are multi-line; some embed file paths that need wildcarding in the regex
- Risk of false positives is non-trivial — the patterns need to distinguish "asking permission" from aider just printing a question as part of normal output

### What to do when picked up

1. Enumerate all confirmation prompt sites in aider (read the source, or run aider through its main flows and capture prompt strings)
2. Build a regex set, one per prompt type, all alternated into the existing permission-detection processing layer
3. Add tests with captured prompt strings as fixtures
4. Ship behind a per-tab "aider permission detection enabled" setting so users can disable if false positives bite

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

- **What:** Drag a tab outside the main window to create a new cctts window containing only that tab (or that pane). Standard browser pattern. Multi-monitor users want this.
- **Why:** The single-window constraint becomes restrictive on multi-monitor setups — most natural arrangement is "Claude on monitor 1, aider + shells on monitor 2" but the v1.3 layout tree is bounded by the main window's dimensions.
- **Cost:** Substantial. Tauri 2.x supports multi-window but everything in cctts assumes a single window: the audio target tab gate (one global `audio_target_tab`), the avatar overlay (one DOM instance), the compose overlay (one), settings persistence (a single `layout` field, not per-window). Each of these needs to become per-window or stay single-source-of-truth with windows competing for it. Audio is the worst — if you have Claude in window 1 and aider in window 2, who plays sound? The v1.3 "single audio gate" decision was deliberate; multi-window forces revisiting it. Possibilities: only the OS-focused window plays audio (matches v1.3 single-pane-focus rule generalized to windows), or audio mixing (rejected in v1.3).
- **Trigger to act:** multi-monitor use becomes painful enough that the workaround (split-tree-only) feels limiting. Probably real if you start using cctts on a 2- or 3-monitor desk daily.

### Per-cwd / per-project layout memory

- **What:** Different layouts based on which directory cctts launched from. Launching from `~/projects/bid-system` restores the layout you used there last time; launching from `~/projects/scratch` restores its own.
- **Why:** Different work shapes for different projects. Bid analysis wants Claude + aider + a shell for grep work. Scratch project wants just Claude. The v1.3 single global layout makes you reset every time you switch contexts.
- **Cost:** Medium. Settings layout becomes keyed by cwd (or by a project ID derived from cwd, to handle moves). Persistence is a `Map<cwd, LayoutPersisted>` instead of a single field. The integrity check from v1.3 (orphan tabs, missing tabs) runs per-cwd. UI: subtle indicator showing which project's layout is active. Decision needed: do tabs persist per-cwd too, or globally with the layout? Probably per-cwd — the tab list is part of the project context, and keeping a global tab list with per-project layouts means tabs from project A leak into project B's layout. Migration path from v1.3 is non-trivial: one global layout becomes the default for all unknown cwds, or specifically for the cwd cctts was last launched from.
- **Trigger to act:** if you find yourself repeatedly setting up the same layout when switching projects, or saving/restoring presets named after projects (which is the manual workaround).

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
- **Cost:** Medium. UI for capturing key combinations (most webview frameworks have library support for this), validation against conflicts, persistence into the existing `shortcuts` settings field. The hard part is the conflict-detection UX: two cctts shortcuts can't bind the same combo, but cctts can't detect OS-level conflicts (`Ctrl+Alt+Arrow` on GNOME) — that's just documentation.
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
  - **Performance impact.** Canvas → DOM is roughly a 2-5× slowdown depending on workload. For typical cctts use (Claude streaming text at human-readable speeds, shell tabs running interactive commands) it's not noticeable. For high-throughput output (`tail -F` on a fast log, `find /`, `cargo build` with thousands of warnings) the DOM renderer will lag visibly. This is a known and well-documented xterm.js limitation, not a cctts bug.
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
  - **Image storage:** reference the user's chosen file by absolute path. Fast, no disk overhead, no copy. If the file becomes invalid later, show a clear error message in the Settings panel ("Image file not found at: /path/...") and treat as `null` for rendering. Alternative — copy into cctts data dir at pick time — adds robustness but adds disk and clutters cctts state. Path-reference is the right default.
- **Builtin tabs:** Claude and aider inherit global like any tab. Once per-tab UI ships, override is available the same way as Shell tabs.
- **Animated/video backgrounds:** out of scope. Performance cost much higher; use case dubious. Static images only.
- **Trigger to act (initial release):** any time. Popular-feature polish; bigger caveat to document than themes (the renderer/perf trade-off + the recreation-loses-scrollback gotcha) but otherwise straightforward.
- **Trigger to act (follow-on per-tab UI):** real use feedback that the global-only constraint pinches — e.g., wanting a clean background on aider while keeping the global image elsewhere. The "explicitly disable per tab" use case is the most likely first ask. If it doesn't come up in real use, the follow-on may never need to ship.

### Touch / pen drag-and-drop

- **What:** Drag tabs via touch or pen instead of mouse. Use `pointerdown`/`pointermove`/`pointerup` instead of `mousedown`/etc.
- **Why:** Touchscreen laptops, tablets running cctts, accessibility.
- **Cost:** Small to medium — the M2 drag implementation uses `mouse*` events; switching to `pointer*` events covers mouse + touch + pen with a single code path. The hard parts are touch-specific UX (no hover state for drop-zone preview before commit) and pen pressure sensitivity (probably ignore).
- **Trigger to act:** if you ever run cctts on a touchscreen device, or accessibility need.

## v1.2 deferrals still pending

Items from `DESIGN-V3.md` that didn't get picked up in v1.3:

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

### Per-tab avatar configuration

- **What:** Different avatar assets per tab (e.g., a different sprite for the aider tab vs. the Claude tab).
- **Why:** Visual distinction reinforces "which AI am I talking to right now."
- **Cost:** Medium. Settings schema gains per-tab avatar override; the avatar overlay component picks the right asset based on focused pane's active tab. Asset bundling decisions (ship multiple sets, or let user supply paths).
- **Trigger to act:** if you find the single avatar visually ambiguous when switching between Claude and aider.

### Per-tab TTS settings

- **What:** Different voice / speed / volume per tab.
- **Why:** Could match avatars (different voice per AI). Or use case where shell error tones want different volume than speech.
- **Cost:** Medium. Per-tab override in settings; TTS pipeline reads target tab's voice settings before synthesis.
- **Trigger to act:** real use feedback. Low priority.

## Auto-detect Blackwell (or any unsupported GPU) and gracefully skip CUDA opt-in

- **What:** When `CCTTS_GPU=cuda` is set, probe the GPU compute capability before registering the CUDA EP. If the CC is unsupported by the bundled ORT prebuilt (currently sm_120 / Blackwell), log a clear warning and fall back to CPU instead of letting the user see per-segment `cudaErrorSymbolNotFound` errors and silent output.
- **Why:** Today the `CCTTS_GPU=cuda` opt-in is honest but unfriendly on Blackwell — registration succeeds, the session commits, and inference fails per-segment with a cryptic CUDA error. A pre-flight probe gives a single clear message at startup.
- **Costs to weigh:**
  - Querying CC requires loading the CUDA runtime, which we already do indirectly via ort. Either add a tiny `cudarc` (or similar) dependency to call `cudaDeviceGetAttribute`, or shell out to `nvidia-smi --query-gpu=compute_cap --format=csv` (works but ugly and adds a subprocess).
  - The "supported CC list" needs to be maintained alongside ort bumps — a magic list is fine but easy to forget to update. Could instead do a real probe inference (build session, run a 1-token forward pass, catch failure) which is self-validating but slower at startup.
- **Trigger to act:** if anyone besides the dev box reports the "registered but no audio" symptom on Blackwell, OR when `ort` upgrades to a version that adds new GPU support and we want the probe to handle the next-gen-GPU regression class generally.
- **Related:** `MAINTENANCE.md` "ort / ONNX Runtime" entry tracks the underlying ORT 1.20 + Blackwell mismatch.

---

# 3. Done / historical

## ~~Espeak fallback for out-of-vocabulary words~~ — shipped (default)

Always on — `misaki-rs` is pulled in with default features, which includes its
`espeak` fallback. espeak-ng is statically linked, so no `libespeak-ng.dll` is
shipped, but `espeak-ng-data/` (~7.5 MB) sits next to `cctts.exe` (auto-copied
by `build.rs`). The compiled binary is GPLv3 (see `NOTICE`); cctts source stays
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

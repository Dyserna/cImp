# Feature: Per-Tab Visual & Audio Overrides

## Purpose

Four features that all express the same pattern: **a global default, optionally overridden per tab, with the override traveling with the tab through drag-and-drop.** Treating them as one design surface — a single override-resolution helper, a single Configure-Tab UI section, a single migration shape — keeps the four implementations consistent and avoids divergent shapes that fight each other later.

See `FUTURE-FEATURES.md` for per-item rationale and trigger-to-act conditions; this doc captures the shared architecture.

## Scope clarification: this is NOT the cctts UI chrome theme

`FEATURE-ui-modernization.md` covers **the cctts chrome** — tab bar, status bar, dialogs, settings window, overlays. That's a global look-and-feel for the application shell, applied via CSS design tokens and a `data-theme` attribute on `<html>`.

**This feature** is **per-tab visual and audio identity** — the xterm.js terminal palette, the background image behind the terminal text, the avatar asset, the TTS voice. The two are independent and ship independently. The word "theme" appears in both but operates on different surfaces. A user can have:

- A Modern Dark UI chrome (UI modernization) **and**
- A Solarized Light terminal palette in their Claude tab (this feature)
- A different palette in their aider tab (this feature, per-tab override)

…simultaneously, without conflict. The chrome stays consistent across tabs; the per-tab settings give each tab its own identity inside that chrome.

## Items in this group

1. **Terminal color themes** — bundled palette + custom 16-color editor, applied to xterm.js `theme` option.
2. **Terminal background image** — image beneath terminal text, with opacity/blur/size controls. Forces xterm.js DOM renderer (perf trade-off).
3. **Per-tab avatar configuration** — different avatar asset per tab. Avatar overlay reads from focused-pane's active tab.
4. **Per-tab TTS settings** — different voice / speed / volume per tab. TTS pipeline reads target-tab settings before synthesis.

## Shared design

### The override pattern

For every per-tab-overridable property `X`:

- A global `terminal.X` (or `avatar.X` / `tts.X`) settings group holds the default.
- Each tab in the `tabs` array gains an `X_override` field with three possible states:
  - `null` → inherit global (default for existing tabs and the common case)
  - explicit value → use that value regardless of global
  - `"disabled"` (background image only — others don't need this state) → explicitly turn the feature off for this tab even if global is set

A single resolver per property:

```ts
function effectiveTheme(tab):     ITheme           = tab.theme_override ?? globalTheme
function effectiveBackground(tab) = tab.background_override === "disabled" ? null
                                  : tab.background_override ?? globalBackground
function effectiveAvatar(tab)     = tab.avatar_override ?? globalAvatar
function effectiveTtsConfig(tab)  = tab.tts_override ?? globalTtsConfig
```

Resolvers live alongside the consumer that needs them — themes and background near `terminals.createForTab` in `src/lib/terminals.ts`; avatar near `src/lib/avatarConfig.ts`; TTS near the TTS pipeline (in Rust — see below).

### Override travels with the tab

Critical: overrides are properties of the *tab*, not the *pane*. A tab dragged from pane A to pane B keeps its override. This falls out naturally because the tab object lives in `settings.tabs[]`, indexed by tab id; pane membership is a separate concern (`pane.tab_ids[]`). No extra work needed.

### Schema migration

Adding override fields is idempotent and additive. For each existing tab in the migration step (`src-tauri/src/settings/migration.rs`):

```
tab.theme_override      = null
tab.background_override = null
tab.avatar_override     = null    // when item 3 ships
tab.tts_override        = null    // when item 4 ships
```

And add the global groups (`terminal.theme`, `terminal.background`, etc.) with default values. No data is lost; existing behavior is preserved (everything inherits global, which equals "old behavior" before the new global field has a value).

Each item bumps the settings version. Backups (`config.json.v1.X.bak`) follow the v1.2/v1.3 pattern in `migration.rs`.

### UI surface — shared shape

- **Global**: Settings → Appearance section. One picker per property. For Custom variants (theme, background), expand a sub-panel.
- **Per-tab**: `src/lib/dialog/ConfigureTabDialog.svelte` gains an Appearance section with one row per overridable property. Each row has the same shape:
  - First option: "**Use global default** (current: <human-readable summary>)" — sets override to `null`.
  - Subsequent options: pick a specific value, or `"disabled"` (background image), or open a Custom editor.
  - Helper text under each row clarifies which file the override is stored in (relevant once Configuration Scope ships and project-local settings are possible).

Reuse a shared `<OverrideRow>` Svelte component if the four rows look similar enough; otherwise, four bespoke rows with consistent visual structure are fine. Don't over-abstract.

### Builtin tabs (Claude, aider) inherit global

Same rule as Shell tabs. No special-casing. Override available the same way. This is an explicit design choice — the alternative ("builtins always use global") would be a hidden inconsistency and would block per-AI color-coding which is the headline use case for color themes.

## Per-item implementation notes

### 1. Terminal color themes

**Bundled set**: ~12 themes in `src/lib/themes/index.ts` (Dracula, Solarized Dark/Light, Nord, Tomorrow Night, Gruvbox Dark/Light, One Dark, Monokai, Tokyo Night, GitHub Dark, Default). Each entry is an `ITheme` shape from xterm.js: `foreground`, `background`, `cursor`, `cursorAccent`, `selectionBackground`, `selectionForeground`, plus `black`/`red`/`green`/`yellow`/`blue`/`magenta`/`cyan`/`white` and the 8 `bright*` variants.

**Schema**:
- Global: `terminal.theme.name: string` (default `"Default"`), `terminal.theme.custom: ThemeColors | null`
- Per-tab: `tab.theme_override: { name: string, custom: ThemeColors | null } | null`

**Wiring**: `terminals.createForTab(tabId)` reads `effectiveTheme(tab)` and passes to `new Terminal({ theme })`. On runtime change (global or per-tab), assign `term.options.theme = newTheme` — xterm.js supports this without recreation.

**UI**: Settings → Appearance → theme dropdown with color-swatch preview. Configure Tab → Appearance → "Use global default" first entry.

### 2. Terminal background image

**Staged rollout**: ship global-only initially. Per-tab schema (`background_override`) and resolver land at the same time, but the Configure Tab UI for the override is deferred to a follow-on. Rationale in `FUTURE-FEATURES.md`.

**The xterm.js renderer wrinkle (read carefully)**:
- xterm.js canvas renderer (default) fills its host opaquely each frame. Transparent terminal bg won't show through. WebGL renderer doesn't support transparency at all.
- To get a transparent terminal: set `allowTransparency: true` at construction → forces DOM renderer → 2-5× slowdown for high-throughput output.
- Renderer choice is fixed at `new Terminal({ allowTransparency })` time. Toggling background image on/off mid-session means recreating the terminal instance. **Scrollback is lost on recreation; the PTY itself is unaffected.** Document this.
- *Only tabs with a non-null effective background* opt into the DOM renderer. Tabs without an image stay on canvas.

**Schema**:
```
terminal.background.image:    string | null   (file path, null = no image, default null)
terminal.background.opacity:  number          (0.0-1.0, dimming overlay alpha, default 0.4)
terminal.background.blur:     number          (px, CSS backdrop-filter blur, default 0)
terminal.background.size:     "cover" | "contain" | "tile"  (default "cover")
terminal.background.position: string          (CSS background-position, default "center")
```
Per-tab: `tab.background_override: BackgroundConfig | "disabled" | null`.

**Wiring**: in `terminals.createForTab`, branch on `effectiveBackground(tab)`. If non-null, set `allowTransparency: true`, set theme `background` to `rgba(0,0,0,opacity)` (or honor active theme bg with adjusted alpha), apply `backgroundImage`/`backgroundSize`/`backgroundPosition` to the host `<div>`, wrap cells in a `backdrop-filter: blur(...)` container if `blur > 0`. If null, default canvas-renderer construction.

**Image storage**: reference user's chosen file by absolute path. If file becomes invalid, show a clear Settings error and treat as null for rendering. Do not copy into cctts data dir at pick time — that adds disk and clutters state. (Note: when Configuration Scope ships, project-local settings will resolve relative paths against the project root — see that feature doc.)

**Animated/video backgrounds**: out of scope. Static images only.

### 3. Per-tab avatar configuration

**Schema**:
- Global: `avatar.config: AvatarConfig` already exists in v1+ schema.
- Per-tab: `tab.avatar_override: AvatarConfig | null`.

`AvatarConfig` shape is whatever `avatarConfig.ts` already defines — sprite paths, idle/talking/thinking/awaiting/done state assets. The per-tab override is the *whole* config, not a per-state override. Simpler shape, and the use case ("different sprite for the aider tab") works.

**Wiring**: `AvatarOverlay.svelte` already reads `$avatarConfig` and `$focusedTabId`. Change to read `effectiveAvatar(focusedTab)`. The state machine continues to read `$avatarState` derived from focused tab's processing state — same as today, just the visual asset changes.

**Asset bundling decisions**:
- v1+ ships a single bundled set; the per-tab UI lets users supply paths, same as the global config does today.
- Optional follow-on: ship 2-3 bundled sets (e.g., "Claude" set, "aider" set, "default" set) and let the override pick a *named* set instead of supplying paths. Defer until users ask.

### 4. Per-tab TTS settings

**Schema**:
- Global: `tts.{voice, speed, volume}` already exists.
- Per-tab: `tab.tts_override: { voice: string | null, speed: number | null, volume: number | null } | null`.

The override has nullable fields *within* it (so a tab can override only `voice` while inheriting `speed` and `volume`), in contrast to themes/background which override the whole structure. TTS settings are atomic primitives the user often wants to tune individually; themes are not. Diverging the shape here is correct.

**Resolver**:
```ts
function effectiveTtsConfig(tab) {
  const o = tab.tts_override
  return {
    voice:  o?.voice  ?? global.tts.voice,
    speed:  o?.speed  ?? global.tts.speed,
    volume: o?.volume ?? global.tts.volume,
  }
}
```

**Backend wiring**: TTS happens in Rust. The TTS worker resolves per-tab settings at synthesis-request time, not at session start. Where the TTS pipeline reads voice/speed/volume today, replace `settings.tts.X` with a lookup keyed by the source tab id (which is already known — TTS segments carry their origin tab). This lookup needs the tab list and global TTS config in scope; both already are.

**Volume specifically**: the v1+ "audio_target_tab" gate still applies. A non-target tab's volume override doesn't matter; its synthesis is dropped before audio output. Per-tab volume affects only the target tab's playback volume.

## Open questions

- **Theme import** (`.itermcolors`, Windows Terminal JSON, etc.): out of scope for the initial themes ship. Bundled set + custom editor covers ~95% of need. If shipped later, importer feeds the same `ThemeColors` schema; orthogonal to the override question.
- **Per-tab background recreation cost**: if a user has 8 tabs all with a background image and edits the global image, all 8 recreate. Snappy on a fast machine but visibly stutters. Acceptable for a rare action; document if it bites.
- **Avatar override granularity**: whole `AvatarConfig` vs. per-state overrides. Going with whole-config for simplicity; revisit if real use shows users want "same idle sprite, different talking sprite" combos.
- **Migration when item 2 ships before item 1**: the items are independent; ship in any order. The resolver helpers don't depend on each other.

## Milestone recommendation

**Milestones needed**, one per item, picked up at implementation time:

- `MILESTONE-V1.4-XX-terminal-themes.md` — schema additions, bundled registry, Settings UI (global + per-tab), runtime theme swap.
- `MILESTONE-V1.4-XX-terminal-background.md` — schema additions (global + per-tab schema landed together; UI staged), renderer-switching machinery, recreation flow, scrollback-loss caveat documented in README.
- `MILESTONE-V1.4-XX-per-tab-avatar.md` — schema, resolver, AvatarOverlay rewire, Configure Tab UI section.
- `MILESTONE-V1.4-XX-per-tab-tts.md` — schema, resolver in Rust TTS worker, Configure Tab UI section.

The milestones share a structure (schema → resolver → wiring → migration → UI) so the *first* milestone in this group will set the pattern that the others follow. **When implementation starts, the milestone author should re-read this doc, decide which item to ship first, and write that milestone in detail.** A reasonable first pick is themes — it's the most-asked item, has no perf trade-off, and exercises the override pattern cleanly. Background image second because it shares Settings-UI proximity with themes and the renderer wrinkle deserves its own milestone.

If the items are picked up out of order, that's fine — the shared design above is the only pre-coordination needed.

## Files most likely touched

- `src-tauri/src/settings/{schema,migration,persistence}.rs`
- `src-tauri/src/tts/...` (per-tab TTS resolver — exact path varies; whatever module reads voice/speed/volume today)
- `src/lib/themes/index.ts` (new, item 1)
- `src/lib/terminals.ts` (theme + background wiring)
- `src/lib/avatarConfig.ts`, `AvatarOverlay.svelte` (item 3)
- `src/lib/dialog/ConfigureTabDialog.svelte` (per-tab UI for all four)
- `src/lib/settings/{store,types}.ts` and a new "Appearance" Settings tab component

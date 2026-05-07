# Feature: Per-Tab Visual & Audio Overrides

## Purpose

Two features that express the same pattern: **a global default, optionally overridden per tab, with the override traveling with the tab through drag-and-drop.** Treating them as one design surface — a single override-resolution helper, a single Configure-Tab UI section, a single migration shape — keeps the implementations consistent and avoids divergent shapes that fight each other later.

See `FUTURE-FEATURES.md` for per-item rationale and trigger-to-act conditions; this doc captures the shared architecture.

> **Scope note (2026-05-07):** this feature group originally listed four items; per-tab avatar configuration and per-tab TTS settings were cancelled as a deliberate scope decision (avatar and TTS stay global-only). The shared override pattern below applies to the remaining two items: terminal color themes and terminal background image.

## Scope clarification: this is NOT the cctts UI chrome theme

`FEATURE-ui-modernization.md` covers **the cctts chrome** — tab bar, status bar, dialogs, settings window, overlays. That's a global look-and-feel for the application shell, applied via CSS design tokens and a `data-theme` attribute on `<html>`.

**This feature** is **per-tab visual identity** — the xterm.js terminal palette and the background image behind the terminal text. The two layers are independent and ship independently. The word "theme" appears in both but operates on different surfaces. A user can have:

- A Modern Dark UI chrome (UI modernization) **and**
- A Solarized Light terminal palette in their Claude tab (this feature)
- A different palette in their aider tab (this feature, per-tab override)

…simultaneously, without conflict. The chrome stays consistent across tabs; the per-tab settings give each tab its own identity inside that chrome.

## Items in this group

1. **Terminal color themes** — bundled palette + custom 16-color editor, applied to xterm.js `theme` option.
2. **Terminal background image** — image beneath terminal text, with opacity/blur/size controls. Forces xterm.js DOM renderer (perf trade-off).

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
```

Resolvers live alongside the consumer that needs them — themes and background near `terminals.createForTab` in `src/lib/terminals.ts`.

### Override travels with the tab

Critical: overrides are properties of the *tab*, not the *pane*. A tab dragged from pane A to pane B keeps its override. This falls out naturally because the tab object lives in `settings.tabs[]`, indexed by tab id; pane membership is a separate concern (`pane.tab_ids[]`). No extra work needed.

### Schema migration

Adding override fields is idempotent and additive. For each existing tab in the migration step (`src-tauri/src/settings/migration.rs`):

```
tab.theme_override      = null
tab.background_override = null
```

And add the global groups (`terminal.theme`, `terminal.background`) with default values. No data is lost; existing behavior is preserved (everything inherits global, which equals "old behavior" before the new global field has a value).

Each item bumps the settings version. Backups (`config.json.v1.X.bak`) follow the v1.2/v1.3 pattern in `migration.rs`.

### UI surface — shared shape

- **Global**: Settings → Appearance section. One picker per property. For Custom variants (theme, background), expand a sub-panel.
- **Per-tab**: `src/lib/dialog/ConfigureTabDialog.svelte` gains an Appearance section with one row per overridable property. Each row has the same shape:
  - First option: "**Use global default** (current: <human-readable summary>)" — sets override to `null`.
  - Subsequent options: pick a specific value, or `"disabled"` (background image), or open a Custom editor.
  - Helper text under each row clarifies which file the override is stored in (relevant once Configuration Scope ships and project-local settings are possible).

Reuse a shared `<OverrideRow>` Svelte component if the rows look similar enough; otherwise, bespoke rows with consistent visual structure are fine. Don't over-abstract.

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

## Open questions

- **Theme import** (`.itermcolors`, Windows Terminal JSON, etc.): out of scope for the initial themes ship. Bundled set + custom editor covers ~95% of need. If shipped later, importer feeds the same `ThemeColors` schema; orthogonal to the override question.
- **Per-tab background recreation cost**: if a user has 8 tabs all with a background image and edits the global image, all 8 recreate. Snappy on a fast machine but visibly stutters. Acceptable for a rare action; document if it bites.
- **Migration when item 2 ships before item 1**: the items are independent; ship in any order. The resolver helpers don't depend on each other.

## Status

Both items in this group have shipped:

- **Item 1 — Terminal color themes**: `MILESTONE-V1.4-01-terminal-themes.md` (shipped).
- **Item 2 — Terminal background image**: `MILESTONE-V1.4-02/-03/-04-terminal-background.md` (shipped across three milestones — skeleton, per-tab UI, then polish/presets/cross-restart scrollback).

The shared structure (schema → resolver → wiring → migration → UI) was established by V1.4-01 and reused by V1.4-02/03/04. Per-tab avatar and per-tab TTS were originally planned as items 3 and 4 in this group; both were cancelled as a scope decision (avatar and TTS stay global-only).

## Files most likely touched

- `src-tauri/src/settings/{schema,migration,persistence}.rs`
- `src/lib/themes/index.ts` (new, item 1)
- `src/lib/terminals.ts` (theme + background wiring)
- `src/lib/dialog/ConfigureTabDialog.svelte` (per-tab UI)
- `src/lib/settings/{store,types}.ts` and an "Appearance" Settings tab component

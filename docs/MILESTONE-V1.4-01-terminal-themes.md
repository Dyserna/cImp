# Milestone V1.4-01: Terminal Color Themes (Global + Per-Tab Override)

## Purpose

Ship the first item from `FEATURE-per-tab-overrides.md`: bundled xterm.js color palettes selectable globally, with optional per-tab override that travels with the tab through drag-and-drop. This is the *pattern-setter* for the per-tab-overrides milestones — V1.4-02/03/04 will follow the same shape (schema → resolver → wiring → migration → UI), so the design choices made here govern the other three.

Read `docs/features/FEATURE-per-tab-overrides.md` first — that document establishes the shared override pattern; this milestone executes its item 1.

## What This Milestone Delivers

1. A bundled theme registry at `src/lib/themes/index.ts` exporting ~12 named `ITheme` objects (Default, Dracula, Solarized Dark, Solarized Light, Nord, Tomorrow Night, Gruvbox Dark, Gruvbox Light, One Dark, Monokai, Tokyo Night, GitHub Dark) plus a `ThemeColors` type alias.
2. New `terminal: TerminalSettings` group on the Rust `Settings` struct, with `theme: TerminalThemeSettings { name: String, custom: Option<ThemeColors> }`. `terminal` is the home for the V1.4-02 background config too — adding the group now keeps the migration count to one.
3. `theme_override: Option<TerminalThemeSettings>` field on both `AiToolTabConfig` and `ShellTabConfig`. `None` (the common case) means inherit global; `Some(_)` means override.
4. `effectiveTheme(tab)` resolver (TS) that returns the resolved `ITheme` for a given tab id, used at `terminals.createForTab` and on settings-change subscriptions.
5. Live theme swap: changing the global theme (or a per-tab override) updates the affected `Terminal` instances in place via `term.options.theme = newTheme` — no recreation, no scrollback loss.
6. Settings file migration v1.3 → v1.4: writes `terminal.theme = { name: "Default", custom: null }`, stamps `theme_override: null` on every existing tab, drops the now-dead `display.theme` field. Backup at `config.json.v1.3.bak.<ts>` follows the existing rotation pattern.
7. **Global UI**: Settings → Appearance section gains a "Terminal palette" dropdown with color-swatch preview per entry. Selecting "Custom…" expands a panel with 22 color pickers (foreground, background, cursor, cursorAccent, selectionBackground, selectionForeground, plus 8 ANSI + 8 bright variants).
8. **Per-tab UI**: `ConfigureTabDialog.svelte` gains an "Appearance" section with a single row: a dropdown whose first entry is "**Use global default** (current: <name>)" mapping to `theme_override = null`, followed by every bundled theme name, plus "Custom…" mapping to `theme_override = { name: "Custom", custom: ... }`.
9. The override travels with the tab through drag-and-drop without any extra work — it's a property of the tab object in `settings.tabs[]`, and pane membership lives separately in `settings.layout`.
10. README is updated with a one-paragraph "Terminal palette" section under Configuration, pointing at Settings → Appearance and the Configure Tab → Appearance section.

## What This Milestone Does NOT Do

- No background image. That's V1.4-02. The `terminal` settings group is shaped to host the background config later, but only the `theme` sub-group is wired in V1.4-01.
- No per-tab avatar override. That's V1.4-05. Schema does *not* preemptively add `avatar_override` — V1.4-05 will add it in its own migration step. The "shared override pattern" lives in design, not in a one-shot omnibus schema bump.
- No per-tab TTS override. That's V1.4-06. Same reasoning.
- No theme import (`.itermcolors`, Windows Terminal JSON). Bundled set + custom editor is enough; importer is orthogonal and listed as out-of-scope in `FEATURE-per-tab-overrides.md` Open Questions.
- No project-local settings interaction. `FEATURE-config-scope.md` is independent; per-tab overrides land cleanly inside whichever settings file is active.
- No cleanup of the existing `display.theme: String` field beyond removing it. It is currently dead code (the xterm.js construction in `terminals.ts:204-207` hardcodes `#000000` / `#e0e0e0` and never reads `display.theme`), so dropping it has no behavioral impact. The V5-01 schema comment that calls `display.theme` "the xterm.js terminal palette inside each tab" is aspirational — V1.4-01 is what makes it true, but under the new `terminal.theme.name` field, not `display.theme`.

## Implementation Steps

### 1. Author the bundled theme registry

Create `src/lib/themes/index.ts`:

```ts
import type { ITheme } from '@xterm/xterm';

export type ThemeColors = ITheme; // re-exported for clarity at call sites

export const BUNDLED_THEME_NAMES = [
  'Default',
  'Dracula',
  'Solarized Dark',
  'Solarized Light',
  'Nord',
  'Tomorrow Night',
  'Gruvbox Dark',
  'Gruvbox Light',
  'One Dark',
  'Monokai',
  'Tokyo Night',
  'GitHub Dark',
] as const;

export type BundledThemeName = (typeof BUNDLED_THEME_NAMES)[number];

export const BUNDLED_THEMES: Record<BundledThemeName, ThemeColors> = {
  Default: {
    background: '#000000',
    foreground: '#e0e0e0',
    cursor: '#e0e0e0',
    cursorAccent: '#000000',
    selectionBackground: '#3a3a3a',
    selectionForeground: '#ffffff',
    black: '#000000',
    red: '#cd3131',
    green: '#0dbc79',
    yellow: '#e5e510',
    blue: '#2472c8',
    magenta: '#bc3fbc',
    cyan: '#11a8cd',
    white: '#e5e5e5',
    brightBlack: '#666666',
    brightRed: '#f14c4c',
    brightGreen: '#23d18b',
    brightYellow: '#f5f543',
    brightBlue: '#3b8eea',
    brightMagenta: '#d670d6',
    brightCyan: '#29b8db',
    brightWhite: '#e5e5e5',
  },
  // ... 11 more entries; values sourced from each palette's canonical
  // upstream (Dracula's terminal.html, Nord's Nord-3 spec, etc.).
};

export function resolveBundledTheme(name: string): ThemeColors {
  return (BUNDLED_THEMES as Record<string, ThemeColors>)[name] ?? BUNDLED_THEMES.Default;
}
```

The "Default" entry preserves today's exact appearance — same `#000000`/`#e0e0e0` foreground/background that `terminals.ts:204-207` hardcodes today. That's deliberate: existing users see no visual change after migration.

A small unit test (`src/lib/themes/themes.test.ts`) iterates `BUNDLED_THEME_NAMES`, asserts `BUNDLED_THEMES[name]` is defined and has all 22 fields populated. Catches typos and missing palettes at CI time.

### 2. Settings schema — add `terminal` group and override field

`src-tauri/src/settings/schema.rs`:

```rust
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct Settings {
    pub tts: TtsSettings,
    pub avatar: AvatarSettings,
    pub display: DisplaySettings,
    pub behavior: BehaviorSettings,
    pub compose: ComposeSettings,
    pub shortcuts: ShortcutSettings,
    pub tabs: Vec<TabConfig>,
    pub processing: ProcessingSettings,
    pub session: SessionState,
    pub layout: Option<LayoutPersisted>,
    pub layout_presets: Vec<LayoutPreset>,
    pub ui: UiSettings,
    /// Terminal-pane settings: xterm.js theme (V1.4-01) and background image
    /// (V1.4-02). Distinct from `ui`, which themes the cctts chrome.
    pub terminal: TerminalSettings,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct TerminalSettings {
    pub theme: TerminalThemeSettings,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TerminalThemeSettings {
    /// Bundled theme name, or "Custom" when `custom` is `Some`. Defaults
    /// to "Default" which mirrors today's hardcoded #000/#e0e0e0 palette.
    pub name: String,
    /// Populated only when `name == "Custom"`. The 22-color xterm.js
    /// `ITheme` shape, kept untyped on the Rust side (free-form string
    /// map) since the values are pure data passed through to the
    /// frontend.
    pub custom: Option<HashMap<String, String>>,
}

impl Default for TerminalThemeSettings {
    fn default() -> Self {
        Self { name: "Default".to_string(), custom: None }
    }
}
```

`#[serde(default)]` on `Settings` and `TerminalSettings` means a v1.3 file lacking the `terminal` key still loads — `terminal` defaults populate. The migration step below stamps the values explicitly anyway, both for clarity in the on-disk file and to keep the v1.3 → v1.4 backup contract honest (a versioned bump should have a recoverable backup).

Add `theme_override` to both tab variants:

```rust
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AiToolTabConfig {
    // ... existing fields ...
    pub theme_override: Option<TerminalThemeSettings>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ShellTabConfig {
    // ... existing fields ...
    pub theme_override: Option<TerminalThemeSettings>,
}
```

**Design note: per-variant fields, not a shared struct.** V1.4-02/03/04 will add three more `*_override` fields. We keep them as four sibling fields on each variant rather than lifting into an embedded `TabOverrides` struct. Rationale: (1) zero JSON nesting churn for users who hand-edit settings.json; (2) each override is independently optional and the tab variants don't share enough other fields to justify refactoring out a common base now; (3) if four overrides accumulate and the duplication starts hurting, V1.4-04 (or a later cleanup milestone) can lift them — but designing for that today is premature abstraction. The feature doc doesn't prescribe either shape.

`Default` impl for both variants gains `theme_override: None` so `serde(default)` covers a malformed entry. Update both `default_*_tab()` constructors at `schema.rs:292-`, etc., to set `theme_override: None` explicitly for readability.

Drop `display.theme: String`. The field was vestigial — no code reads it. Removal needs only the schema delete plus the migration step below (which strips it from the file).

### 3. TS settings types — mirror schema

`src/lib/settings/types.ts`:

```ts
export interface ThemeColorsWire {
  // 22 named hex strings, all optional on the wire because Rust serializes
  // a HashMap<String,String> with no schema enforcement. The frontend
  // resolves missing fields from the bundled "Default" theme.
  [key: string]: string;
}

export interface TerminalThemeSettings {
  name: string;
  custom: ThemeColorsWire | null;
}

export interface TerminalSettings {
  theme: TerminalThemeSettings;
}

export interface Settings {
  // ... existing fields ...
  terminal: TerminalSettings;
}

// Per-tab variants:
export interface AiToolTabConfigWire {
  // ... existing fields ...
  theme_override: TerminalThemeSettings | null;
}
export interface ShellTabConfigWire {
  // ... existing fields ...
  theme_override: TerminalThemeSettings | null;
}
```

`defaultSettings()` adds `terminal: { theme: { name: 'Default', custom: null } }`.

### 4. Settings store — expose a `terminal` derived store

`src/lib/settings/store.ts` already exposes `display`, `tts`, etc. as derived stores. Add:

```ts
export const terminal: Readable<TerminalSettings> = derived(settings, (s) => s.terminal);
```

The terminals registry (Step 6) subscribes to `terminal` for global-theme changes; the per-tab dialog reads/writes through the existing settings IPC.

### 5. Theme resolver

`src/lib/themes/resolve.ts` (new):

```ts
import type { ITheme } from '@xterm/xterm';
import { BUNDLED_THEMES, resolveBundledTheme, type ThemeColors } from './index';
import type { TerminalThemeSettings, AiToolTabConfigWire, ShellTabConfigWire }
  from '../settings/types';

export function themeFromSetting(t: TerminalThemeSettings): ThemeColors {
  if (t.name === 'Custom' && t.custom) {
    // Merge custom over Default so omitted keys don't leave xterm.js with
    // undefined ANSI colors. The bundled "Default" is the canonical
    // fill-in.
    return { ...BUNDLED_THEMES.Default, ...(t.custom as ITheme) };
  }
  return resolveBundledTheme(t.name);
}

export function effectiveTheme(
  tab: AiToolTabConfigWire | ShellTabConfigWire,
  globalTheme: TerminalThemeSettings,
): ThemeColors {
  return themeFromSetting(tab.theme_override ?? globalTheme);
}
```

Tests in `src/lib/themes/resolve.test.ts`: tab with `theme_override: null` → returns `themeFromSetting(global)`; tab with `theme_override: { name: 'Dracula', custom: null }` → returns Dracula bundled; tab with `theme_override: { name: 'Custom', custom: {...partial...} }` → returns Default merged with custom.

### 6. Wire into `terminals.ts`

`src/lib/terminals.ts:198-208` currently hardcodes the theme. Change to:

```ts
import { settings as settingsStore, terminal as terminalSettings }
  from './settings/store';
import { effectiveTheme } from './themes/resolve';

// ... inside createTerminal(tabId): ...
const allSettings = get(settingsStore);
const tab = allSettings.tabs.find((t) => t.id === tabId);
const initialTheme = tab
  ? effectiveTheme(tab, allSettings.terminal.theme)
  : themeFromSetting(allSettings.terminal.theme);

const term = new Terminal({
  fontFamily: display.terminal_font_family,
  fontSize: display.terminal_font_size,
  cursorBlink: true,
  allowProposedApi: true,
  theme: initialTheme,
});
```

Live updates: add a settings subscription alongside the existing font subscription:

```ts
let firstTheme = true;
entry.unsubTheme = settingsStore.subscribe((s) => {
  if (firstTheme) { firstTheme = false; return; }
  const tab = s.tabs.find((t) => t.id === tabId);
  const next = tab ? effectiveTheme(tab, s.terminal.theme)
                   : themeFromSetting(s.terminal.theme);
  // Reference equality is fine — the resolver returns a fresh object on
  // each call, so this assignment runs on every settings change. xterm.js
  // diffs internally and only repaints when colors actually changed.
  term.options.theme = next;
});
```

`TerminalEntry` gains `unsubTheme: () => void`. The `destroy(tabId)` path calls `entry.unsubTheme()` alongside `entry.unsubFont()`.

The host element's `style.background = '#000'` at `terminals.ts:195` should become `style.background = initialTheme.background ?? '#000'` so the brief paint between `term.open(host)` and the first xterm frame matches the new theme. Update the host bg in the subscription too.

### 7. Migration v1.3 → v1.4

`src-tauri/src/settings/migration.rs`:

```rust
pub fn migrate_if_needed(
    value: &mut Value,
    path: &Path,
    default_shell: &ShellSpec,
) -> AppResult<bool> {
    let mut changed = false;

    if looks_v1(value) { /* existing */ }
    else if looks_v1_1(value) { /* existing */ }

    if looks_v1_2(value) { /* existing v1.2 → v1.3 */ }

    if looks_v1_3(value) {
        write_backup(path, "v1.3", value)?;
        migrate_v1_3_to_v1_4(value);
        changed = true;
    }

    Ok(changed)
}

fn looks_v1_3(value: &Value) -> bool {
    // Has the v1.3 layout field but lacks the v1.4 terminal field.
    let has_layout = value.as_object()
        .map(|o| o.contains_key("layout"))
        .unwrap_or(false);
    let has_terminal = value.as_object()
        .map(|o| o.contains_key("terminal"))
        .unwrap_or(false);
    has_layout && !has_terminal
}

fn migrate_v1_3_to_v1_4(value: &mut Value) {
    let Some(root) = value.as_object_mut() else { return; };

    // Drop the dead `display.theme` field — V1.4-01 supersedes it with
    // `terminal.theme.name`.
    if let Some(display) = root.get_mut("display").and_then(Value::as_object_mut) {
        display.remove("theme");
    }

    // Add the new `terminal` group with default theme.
    root.insert(
        "terminal".to_string(),
        json!({
            "theme": { "name": "Default", "custom": null }
        }),
    );

    // Stamp `theme_override: null` on every existing tab.
    if let Some(tabs) = root.get_mut("tabs").and_then(Value::as_array_mut) {
        for tab in tabs.iter_mut() {
            if let Some(obj) = tab.as_object_mut() {
                obj.insert("theme_override".to_string(), Value::Null);
            }
        }
    }
}
```

Tests at `src-tauri/src/settings/migration.rs#tests`:
- v1.3 file (no `terminal` key, tabs without `theme_override`) → migrates, backup written, `terminal.theme.name == "Default"`, every tab has `theme_override: null`.
- v1.4 file (already has `terminal`) → no-op, no backup.
- v1.0/v1.1/v1.2 → cascades through prior migrations and lands at v1.4 with one backup per version transition.

### 8. Global UI — Settings → Appearance

The Appearance section already exists (V5-01 placed the UI-Theme dropdown there). Add the "Terminal palette" controls below it.

`src/lib/settings/AppearanceSection.svelte` (or wherever V5-01 placed the section — confirm at impl time):

```svelte
<script lang="ts">
  import { settings as settingsStore } from '../store';
  import { BUNDLED_THEME_NAMES } from '../../themes';
  import ThemeSwatch from './ThemeSwatch.svelte';
  import CustomThemeEditor from './CustomThemeEditor.svelte';
  import { updateSettings } from '../../ipc';

  $: paletteName = $settingsStore.terminal.theme.name;
  $: showCustom = paletteName === 'Custom';

  function setName(name: string) {
    void updateSettings({
      terminal: { theme: { name, custom: $settingsStore.terminal.theme.custom } }
    });
  }
</script>

<div class="row">
  <label for="terminal-palette">Terminal palette</label>
  <select id="terminal-palette" bind:value={paletteName} on:change={(e) => setName(e.currentTarget.value)}>
    {#each BUNDLED_THEME_NAMES as name}
      <option value={name}>{name}</option>
    {/each}
    <option value="Custom">Custom…</option>
  </select>
  <ThemeSwatch name={paletteName} />
</div>

{#if showCustom}
  <CustomThemeEditor />
{/if}
```

`ThemeSwatch.svelte` renders five small color squares (background, foreground, red, green, blue) inline next to the dropdown — fast visual scan.

`CustomThemeEditor.svelte` lays out 22 `<input type="color">` pickers in a grid, bound to `$settingsStore.terminal.theme.custom`. Saves on blur. The grid is grouped: top row = base (foreground, background, cursor, cursorAccent, selectionBackground, selectionForeground); middle row = ANSI 8 (black, red, green, yellow, blue, magenta, cyan, white); bottom row = bright 8.

### 9. Per-tab UI — ConfigureTabDialog

`src/lib/dialog/ConfigureTabDialog.svelte` gains an Appearance section. Single row for V1.4-01; V1.4-02/03/04 will append rows below it.

```svelte
<section class="appearance">
  <h3>Appearance</h3>
  <label for="palette-override">Terminal palette</label>
  <select id="palette-override" bind:value={selectedPalette} on:change={apply}>
    <option value="__inherit">Use global default (current: {globalThemeName})</option>
    {#each BUNDLED_THEME_NAMES as name}
      <option value={name}>{name}</option>
    {/each}
    <option value="Custom">Custom…</option>
  </select>
  {#if selectedPalette === 'Custom'}
    <CustomThemeEditor bind:value={customOverride} />
  {/if}
</section>
```

`apply()` translates the selection into a `theme_override` write:
- `"__inherit"` → `theme_override: null`
- bundled name → `theme_override: { name, custom: null }`
- `"Custom"` → `theme_override: { name: "Custom", custom: customOverride }`

The "current: <name>" hint resolves the global theme name from the live store; users see "current: Dracula" when global is set to Dracula and they're toggling a per-tab override.

Apply on dialog Save (the existing Configure Tab dialog already submits the whole tab config in one IPC call — V1.4-01 just adds the override field to that payload).

### 10. README and DESIGN.md updates

README — add a paragraph under the existing Configuration section:

> **Terminal palette.** Choose from 12 bundled color themes (Dracula, Solarized, Nord, Tomorrow Night, Gruvbox, One Dark, Monokai, Tokyo Night, GitHub Dark, plus Default) in Settings → Appearance, or define a custom 22-color palette. Each tab can override the global palette via Configure Tab → Appearance — useful for color-coding tabs by purpose (e.g., a green palette on the aider tab, default on Claude).

DESIGN.md — extend the Settings section with a brief mention of the `terminal.theme` group and the per-tab `theme_override` field, pointing at `FEATURE-per-tab-overrides.md` for the full design rationale.

## Test Plan

- **Unit tests (Rust)**: migration v1.3 → v1.4 idempotency; backup written exactly once; existing tabs stamped with `theme_override: null`; `display.theme` removed.
- **Unit tests (TS)**: bundled registry completeness; `themeFromSetting(Custom)` merges over Default; `effectiveTheme` returns global when override is null and override otherwise.
- **Manual**: launch v1.3.1 settings file → migration runs, backup at `config.json.v1.3.bak.<ts>`, terminal looks identical to before (Default ≡ today's hardcoded colors).
- **Manual**: change global to Dracula → all tabs without override repaint instantly. No flicker, no scrollback loss.
- **Manual**: set per-tab override on Claude to Solarized Light → only Claude repaints. Drag Claude into a different pane → palette stays Solarized Light.
- **Manual**: select Custom on global, set foreground to magenta → all non-overridden tabs go magenta.
- **Manual**: set Configure Tab → "Use global default" on a previously-overridden tab → it picks up the global Dracula again, override row reflects "Use global default (current: Dracula)".

## Files Most Likely Touched

- `src-tauri/src/settings/schema.rs` — `TerminalSettings`, `TerminalThemeSettings`, `theme_override` on tabs, drop `display.theme`
- `src-tauri/src/settings/migration.rs` — v1.3 → v1.4 transform + backup, tests
- `src/lib/themes/index.ts` (new) — bundled registry
- `src/lib/themes/resolve.ts` (new) — `themeFromSetting`, `effectiveTheme`
- `src/lib/terminals.ts:198-208` — read effective theme, subscribe for live updates
- `src/lib/settings/types.ts`, `src/lib/settings/store.ts` — TS mirror + derived store
- `src/lib/settings/AppearanceSection.svelte` — global palette dropdown + custom editor (path subject to confirmation against V5-01's exact layout)
- `src/lib/settings/ThemeSwatch.svelte` (new), `CustomThemeEditor.svelte` (new)
- `src/lib/dialog/ConfigureTabDialog.svelte` — Appearance section, override row
- `README.md`, `docs/DESIGN.md` — palette docs

## Risks and Open Questions

- **Custom palette serialization**: storing as `HashMap<String,String>` on the Rust side is loose-typed. Validation lives in the frontend (the `ThemeColors` interface). If a malformed custom block lands (e.g., `"red": "not-a-color"`), xterm.js will silently ignore it and fall back to the previous valid value for that slot. Acceptable — settings.json is power-user territory; the global UI's color pickers always emit valid hex.
- **Migration vs. `serde(default)` redundancy**: with `#[serde(default)]` on `Settings`, the `terminal` field would default-populate even without a migration. The migration is doing two things the default can't: (a) writing the values back to disk so the file is self-describing, and (b) writing a v1.3 backup so a rollback path exists. Both are worth the small extra code.
- **Future `theme_override` shape evolution**: if V1.4-02/03/04 reveal that "shared override base struct" is the right shape after all, V1.4-04 can refactor — but each milestone should ship its own override field as a sibling first and let the duplication speak for itself before lifting. See the design note in Step 2.
- **`ITheme` upstream churn**: xterm.js could add or rename theme fields across versions. Pin the xterm.js version in `package.json` and treat any field-shape change as a separate maintenance task. The bundled registry gracefully ignores unknown fields, so partial-coverage palettes still work.

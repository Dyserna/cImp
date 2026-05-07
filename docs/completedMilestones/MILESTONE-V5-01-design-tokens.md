# Milestone V5-01: Design Tokens and Mechanical Component Refactor

## Purpose

Stand up the centralized design-token system that the V5 UI modernization rests on, and convert all 31 component-local hex literals to token references. After this milestone, every styling decision is funnelled through a single `theme.css` file and components no longer carry their own color values.

This milestone is deliberately invisible to end users: the existing visual language stays substantially the same. The work is the *substrate* that V5-02 modernizes the visual language on top of. Splitting it out keeps the design-iteration phase (V5-02) cleanly separated from the mechanical sweep, so a regression in either is easy to localize.

Read `docs/features/FEATURE-ui-modernization.md` first — that document establishes the design intent and token shape; this milestone executes its Stages 1-2.

## What This Milestone Delivers

1. A new `src/theme.css` file containing the full token surface — surfaces, text, accents, semantics, borders, radii, shadows, spacing, motion, and typography — with the V5 design values (cool slate-blue surface palette, mint/teal accent, coral danger).
2. Both webview entry points (`src/main.ts` and `src/settings_main.ts`) load `theme.css` and synchronously set `<html data-theme="modern-dark">` before Svelte mounts, so there is no flash-of-unstyled-content in either window.
3. Both HTML files (`index.html`, `settings.html`) carry the static `data-theme="modern-dark"` attribute as a defense-in-depth fallback.
4. A `prefers-reduced-motion` media query block in `theme.css` zeros out `--motion-fast` and `--motion-base` so users with reduced-motion preferences get instant transitions.
5. New `ui.theme: String` field in `Settings`, defaulted to `"modern-dark"`, on both the Rust backend and the TypeScript frontend. The field round-trips through `applySettings` and persists across launches. No explicit migration is required (`#[serde(default)]` covers it).
6. New "Appearance" section in the Settings window with a UI-Theme dropdown. For V5-01 the dropdown has a single option ("Modern Dark"); the entry exists so V5-02's design iteration can land additional themes without UI plumbing churn.
7. All 31 components with `<style>` blocks have their hex literals replaced with `var(--*)` references using the mapping table below. Visual look is *substantially equivalent* to v1.3.1 — same colors, same radii, same spacing, just driven by tokens.
8. `grep -rE '#[0-9a-fA-F]{3,6}' src/` returns matches only inside `theme.css`. Every other component is token-driven.

## What This Milestone Does NOT Do

- No visual modernization. Active-tab indicator stays as a bottom-border accent stripe (today's pattern). Pill-shaped buttons, mint accents, coral semantics, larger radii, and shadow elevations all land in V5-02.
- No new component primitives (`Pill.svelte` arrives in V5-02).
- No removal of today's surface scheme. The token *names* are V5-shaped (surface-0 through surface-4), but the *values* during V5-01 mirror today's neutral grays so components don't suddenly look different. V5-02 swaps the values to slate-blue.
- No accessibility / contrast audit. Deferred to V5-02 where the new color values will need WCAG verification.
- No DESIGN.md or README updates. V5-02 owns docs.
- No second theme. The picker has one option. A light theme or high-contrast variant is a future feature.

## Implementation Steps

### 1. Author `src/theme.css`

Create `src/theme.css` with the full token block. Two-phase strategy: V5-01 ships *V5-shaped names* with *v1.3-equivalent values* so the mechanical sweep doesn't cause visual change. V5-02 will rewrite the values in place.

```css
:root,
[data-theme="modern-dark"] {
  /* Surfaces — V5-01 values mirror today's neutral palette.
     V5-02 will retint these to cool slate-blue. */
  --surface-0: #000000;        /* app background (today's body bg) */
  --surface-1: #1f1f1f;        /* panes, status bar (today's #1f1f1f) */
  --surface-2: #1a1a1a;        /* tab bar (today's tab-bar bg) */
  --surface-3: #2a2a2a;        /* dialogs, popovers */
  --surface-4: #303030;        /* hover on elevated surfaces (today's #303030) */

  /* Text */
  --text-primary:   #e0e0e0;
  --text-secondary: #c0c0c0;
  --text-tertiary:  #888888;
  --text-disabled:  #555555;
  --text-on-accent: #ffffff;

  /* Accent — V5-01 keeps today's blue. V5-02 swaps to mint/teal. */
  --accent:         #4a90e2;
  --accent-hover:   #5fa3f0;
  --accent-fg:      #ffffff;
  --accent-muted:   rgba(74, 144, 226, 0.15);

  /* Semantics */
  --success:        #4caf50;
  --warning:        #f0a020;
  --danger:         #e74c3c;
  --awaiting:       #f0a020;

  /* Borders */
  --border-subtle:  #2a2a2a;
  --border-default: #3a3a3a;
  --border-strong:  #4a4a4a;
  --border-focus:   var(--accent);

  /* Radii — V5-01 keeps today's 3px. V5-02 expands the scale. */
  --radius-sm:   3px;
  --radius-md:   3px;
  --radius-lg:   3px;
  --radius-pill: 999px;

  /* Elevation — V5-01 defines but does not apply.
     V5-02 wires shadows into popovers, dialogs, sheets. */
  --shadow-sm: 0 1px 2px rgba(0, 0, 0, 0.3);
  --shadow-md: 0 4px 12px rgba(0, 0, 0, 0.4);
  --shadow-lg: 0 8px 24px rgba(0, 0, 0, 0.5);

  /* Spacing scale */
  --space-1: 4px;
  --space-2: 8px;
  --space-3: 12px;
  --space-4: 16px;
  --space-5: 24px;
  --space-6: 32px;

  /* Motion */
  --motion-fast: 120ms;
  --motion-base: 180ms;
  --easing-standard: cubic-bezier(0.4, 0.0, 0.2, 1);

  /* Typography */
  --font-size-xs: 11px;
  --font-size-sm: 12px;
  --font-size-md: 13px;
  --font-size-lg: 15px;
  --font-weight-regular: 400;
  --font-weight-medium: 500;
  --font-weight-semibold: 600;
  --line-height-tight: 1.3;
  --line-height-normal: 1.5;
}

@media (prefers-reduced-motion: reduce) {
  :root {
    --motion-fast: 0ms;
    --motion-base: 0ms;
  }
}
```

The two-phase split is the key insight that keeps this milestone safe. Reviewers can verify "no visual change" against v1.3.1 by inspection; V5-02 then becomes a single-file diff in `theme.css` plus per-component polish.

### 2. Wire entry points

`src/main.ts`:

```ts
import { mount } from 'svelte';
import App from './App.svelte';
import { ttsTest } from './lib/ipc';
import './theme.css';   // must come before app.css so components can override
import './app.css';

document.documentElement.dataset.theme = 'modern-dark';

// existing mount logic unchanged
```

`src/settings_main.ts`: same import order, same `dataset.theme` assignment, before `mount()`.

`index.html` and `settings.html`: change `<html lang="en">` to `<html lang="en" data-theme="modern-dark">`. The runtime assignment in the entry-point overrides this if a future theme is selected; the static attribute prevents FOUC.

`src/app.css`: replace the hardcoded body styles with token references:

```css
html,
body {
  margin: 0;
  padding: 0;
  height: 100%;
  background: var(--surface-0);
  color: var(--text-primary);
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
  overflow: hidden;
}
```

### 3. Settings schema — add `ui` section

`src-tauri/src/settings/schema.rs`:

```rust
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct Settings {
    // ... existing fields ...
    pub ui: UiSettings,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct UiSettings {
    pub theme: String,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: "modern-dark".to_string(),
        }
    }
}
```

`#[serde(default)]` on the top-level struct means existing v1.3 settings files load with `ui.theme = "modern-dark"` automatically — no migration code needed.

`src/lib/settings/types.ts`:

```ts
export interface UiSettings {
  theme: string;
}

export interface Settings {
  // ... existing fields ...
  ui: UiSettings;
}

// Inside defaultSettings():
ui: { theme: 'modern-dark' },
```

### 4. Settings UI — new Appearance section

`src/SettingsApp.svelte`: add a section header "Appearance" with a single labelled select bound to `snapshot.ui.theme`. Use the existing pattern that other sections follow. The `<option>` list has one entry (`<option value="modern-dark">Modern Dark</option>`); a comment notes that V5-02's iteration may add variants.

Persistence flows through the existing `patch()` helper. Verify the field round-trips: open Settings → change selection (no-op since one option, but the bind:value path runs) → check `~/.config/cctts/settings.json` (or platform equivalent) shows `"ui": { "theme": "modern-dark" }`.

### 5. Mechanical token sweep — replacement table

For each component file with hex colors, apply the substitution table. Order of operations matters: do high-visibility components first so any token-binding bugs surface against the components most likely to expose them.

**Substitution table:**

| Current literal | Token |
|---|---|
| `#000` / `#000000` | `var(--surface-0)` |
| `#1f1f1f` | `var(--surface-1)` |
| `#1a1a1a` | `var(--surface-2)` |
| `#2a2a2a` (any) | `var(--surface-3)` or `var(--border-subtle)` (depends on use — bg vs. border) |
| `#303030` | `var(--surface-4)` |
| `#3a3a3a` | `var(--border-default)` |
| `#e0e0e0` | `var(--text-primary)` |
| `#ffffff` (text) | `var(--text-primary)` (or `var(--text-on-accent)` if on accent fill) |
| `#c0c0c0` | `var(--text-secondary)` |
| `#888` / `#888888` | `var(--text-tertiary)` |
| `#555` / `#555555` | `var(--text-disabled)` |
| `#4a90e2` (any context) | `var(--accent)` (or `var(--border-focus)` for focus rings) |
| `rgba(74, 144, 226, ...)` | use `--accent-muted` if alpha matches; otherwise leave the literal with a TODO for V5-02 |
| `#4caf50` | `var(--success)` |
| `#f0a020` | `var(--awaiting)` (preferred for status) or `var(--warning)` |
| `#e74c3c` | `var(--danger)` |

**Components, in order:**

1. `src/lib/Tab.svelte` — most-visible chrome. ~26 hex occurrences. Verify hover/active/focus paths.
2. `src/lib/TabBar.svelte` — ~12 occurrences. Edge-fade gradients reference surface bg; ensure they use `var(--surface-2)`.
3. `src/lib/StatusBar.svelte` and `src/lib/status/{AnnouncementsButton, LayoutsPopover, MuteButton, VolumeSlider}.svelte` — pills, popovers, sliders.
4. `src/lib/Pane.svelte` and `src/lib/Split.svelte` — pane chrome, focused-pane indicator, splitter handle.
5. `src/lib/dialog/{ConfigureTabDialog, ManagePresetsDialog, NewShellTabDialog, SaveLayoutDialog, ShellTabFields}.svelte` — dialog surfaces, inputs, primary/secondary buttons.
6. `src/lib/settings/{ArrayEditor, NotificationEditor, ShortcutCapture, TabSettingsSection, TextAreaWithReset}.svelte` — settings inputs, capture flows.
7. `src/lib/{Toast, ErrorBanner, ComposeOverlay, ClosedShellOverlay, TabErrorOverlay, AiderFirstLaunchNotice}.svelte` — overlays / banners.
8. `src/lib/dnd/{DragGhost, DropZoneOverlay}.svelte` — drag visuals.
9. `src/lib/{TabContextMenu, LayoutNodeRenderer, AvatarOverlay, WaveformOverlay}.svelte` — context menu, layout renderer, avatar/waveform chrome (only chrome around the assets, not the assets themselves — those are out of scope per the feature doc).
10. `src/SettingsApp.svelte` — section headers, layout, the new Appearance entry. ~48 hex occurrences.

For each file, after substitution: visually compare against v1.3.1 (open the app on `develop` in one window and the V5-01 branch in another). The two should be pixel-identical or near-identical.

### 6. Padding / margin scale (light pass)

Replace obvious literal spacing values with spacing tokens *only where the literal exactly matches a scale value*:

- `4px` → `var(--space-1)`
- `8px` → `var(--space-2)`
- `12px` → `var(--space-3)`
- `16px` → `var(--space-4)`
- `24px` → `var(--space-5)`
- `32px` → `var(--space-6)`

Off-scale values (`6px`, `10px`, `14px`, `20px`) stay as literals for now — the feature doc explicitly notes "today's per-component padding values are arbitrary" and a real spacing rationalization is V5-02 polish work, not V5-01 mechanics.

### 7. Border-radius literals

Replace `border-radius: 3px` with `var(--radius-md)` everywhere (V5-01's `--radius-md` is itself `3px`, so no visual change). The substitution sets up V5-02's radius bump.

`border-radius: 50%` (the indicator dots in `Tab.svelte`) stays literal — it's a circle, not on the radius scale.

`border-radius: 999px` → `var(--radius-pill)`.

### 8. Defer-list — what NOT to touch in V5-01

Leave these alone; V5-02 owns them:

- The active-tab `border-bottom: 2px solid var(--accent)` stays. V5-02 replaces this with the elevated-pill treatment.
- `box-shadow` declarations on dialogs / popovers — V5-02 wires `--shadow-md` / `--shadow-lg` here. V5-01 leaves any existing shadows literal.
- `transition:` declarations — leave alone. V5-02 standardizes on `--motion-fast` / `--motion-base`.
- `font-size:` literals — V5-02 normalizes against the font-size token scale.
- Anything in the avatar overlay / waveform that styles the *content* (not the chrome around it).

## Files Touched / Added

**Added:**
- `src/theme.css` — the token surface.

**Modified:**
- `src/main.ts` — import theme.css, set `data-theme`.
- `src/settings_main.ts` — same.
- `src/app.css` — body styles use tokens.
- `index.html` — `data-theme` attribute.
- `settings.html` — same.
- `src-tauri/src/settings/schema.rs` — new `UiSettings` + `ui` field.
- `src/lib/settings/types.ts` — mirror `UiSettings`, update `defaultSettings()`.
- `src/SettingsApp.svelte` — Appearance section + mechanical token sweep.
- All 30 other component files in `src/lib/**/*.svelte` listed in step 5 — mechanical token sweep.

**Not modified:**
- `src-tauri/src/settings/migration.rs` — no migration needed; `#[serde(default)]` covers it.
- `docs/DESIGN.md`, `README.md`, `CHANGELOG.md` — V5-02 owns docs.

## Edge Cases and Gotchas

- **Settings window FOUC.** If `data-theme` is set after Svelte mounts, the user briefly sees an unstyled flash. Set the attribute synchronously at module top of `main.ts` and `settings_main.ts`, before `mount()` is called. The static `data-theme` in the HTML files is a fallback for any code path that races.
- **Portal-mounted panes (V4-01).** Panes are rendered through a DOM portal but stay in the same document, so `:root` tokens inherit naturally. No special handling needed — but verify by opening DevTools on a portaled pane, picking the tab button, and confirming computed styles resolve to token values.
- **The debug status indicator.** Per project memory, the bottom-right debug status overlay must remain in place across milestones. Token-ize its colors but do not remove it.
- **Existing inline `style=` attributes.** A few components (waveform, avatar overlay) use `style="..."` for dynamic values like opacity. These are dynamic — leave alone. Only static `<style>` block hex literals are in scope for the sweep.
- **CSS `currentColor` references.** `Tab.svelte`'s `.indicator-working` uses `background: currentColor`. This is intentional — keep it. The "current color" resolves through the token-driven `color:` cascade.
- **Settings file shape change.** Adding `ui` to `Settings` causes the next save to include the new field. Old v1.3.1 files load without it (defaults applied) and the next save persists it. Verify by launching against a v1.3.1 settings file: load → change something unrelated → check the saved JSON has `"ui": { "theme": "modern-dark" }`.
- **Tauri build.** Adding a new struct field is a clean rebuild. `cargo check` should pass with no other changes; if not, the missing piece is likely the `#[serde(default)]` on the new struct.

## Manual Verification Checklist

Run on Windows (primary target). Linux validation is deferred per project convention.

Visual parity (the load-bearing check for V5-01):

- [ ] Launch the app on the V5-01 branch alongside v1.3.1. Tab bar, status bar, panes, dialogs, settings — all look pixel-identical or near-identical.
- [ ] Active tab still has the blue bottom-border accent.
- [ ] Hover on a tab still highlights with `#303030` (now `var(--surface-4)`).
- [ ] All status-bar buttons render as before.
- [ ] All dialogs render as before — Configure Tab, New Shell Tab, Manage Presets, Save Layout.
- [ ] Settings window shows the new Appearance section with the single Modern Dark option.

Plumbing:

- [ ] Inspect main window's `<html>` in DevTools — `data-theme="modern-dark"` is present.
- [ ] Open Settings — its `<html>` also carries `data-theme="modern-dark"`.
- [ ] No FOUC on either window during launch (start the app several times to be sure).
- [ ] DevTools Computed-Styles panel: pick `body` — `background-color` resolves to `--surface-0`'s value.
- [ ] Pick a tab button — `color` resolves to `--text-secondary`'s value.

Settings round-trip:

- [ ] Launch with a fresh settings file (delete settings.json) — file is created with `"ui": { "theme": "modern-dark" }`.
- [ ] Launch against an older v1.3.1 settings file — loads without error; `ui.theme` is implicitly `"modern-dark"`; next save adds the field.
- [ ] Hand-edit settings.json to remove the `ui` block — relaunch — file reloads, defaults applied, no crash.
- [ ] Hand-edit settings.json to set `"ui": { "theme": "garbage-value" }` — relaunch — value loads as-is (string field, no validation in V5-01); the `data-theme` attribute is still set to `modern-dark` because the theme picker only knows that one value. (Validation lands in V5-02 when more themes exist.)

Token sweep:

- [ ] `grep -rE '#[0-9a-fA-F]{3,6}' src/` returns hits only inside `src/theme.css`. (Caveat: `currentColor`, `transparent`, and named colors are fine if any remain.)
- [ ] No file in `src/lib/**/*.svelte` has a hardcoded color literal in a `<style>` block.
- [ ] `npm run check` passes.
- [ ] `cargo check` passes.
- [ ] `cargo test` passes (if there are settings-shape tests, they should still pass with the new field defaulted).

Reduced motion:

- [ ] Enable "Reduce motion" in Windows Settings → Accessibility → Visual effects.
- [ ] Relaunch app. Hover over a tab — no transition (instant color change).

## Done Criteria

- All 8 "What This Milestone Delivers" items are in place.
- All "Manual Verification Checklist" items pass on Windows.
- No visual regression vs. v1.3.1 in side-by-side comparison.
- No regression in any v1.3 feature (multi-tab, multi-pane, drag-and-drop, splitters, layouts).
- Token sweep is complete (`grep` shows zero hex literals outside `theme.css`).
- The new `ui.theme` field round-trips through settings save/load.
- V5-02 can begin: the only thing it needs to do for the visual change is rewrite values in `theme.css` and apply per-component polish — no plumbing work.

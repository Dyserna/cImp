# Feature: UI Modernization (Modern Dark Theme + Visual Polish)

## Purpose

Introduce a centralized design-token system, ship a single "Modern Dark" theme using those tokens, and refresh the visual language across the cctts chrome — tab bar, status bar, dialogs, overlays, settings window — with larger rounded corners, refined spacing, subtle elevation, and polished hover/focus states.

Today, styling is scoped to each Svelte component, hex colors are duplicated across 31+ files, border radii are tiny (3px) and inconsistent, and there's no shared system for elevation, motion, or spacing. The cctts UI looks dated next to modern desktop apps — flat panels with sharp corners and a single accent. This feature pulls the visual language up to par.

## Scope clarification: this is NOT the per-tab terminal theme feature

`FEATURE-per-tab-overrides.md` § "Terminal color themes" covers **xterm.js terminal palettes** — the colors *inside* a terminal pane (foreground, background, ANSI 16, cursor, selection). That's per-tab, palette-driven, lives in xterm.js's `ITheme` shape.

**This feature** is the **cctts UI chrome** — the tab bar, status bar, dialogs, settings window, overlays, dropdowns, buttons, menus. The two are independent and ship independently. They share the word "theme" but operate on different surfaces. A user can have:

- A Modern Dark UI chrome (this feature) **and**
- A Solarized Light terminal palette in their Claude tab (the per-tab feature)
…simultaneously, without conflict.

## What "modern" means for this project

Concrete design intent, not aesthetic preference:

- **Larger, consistent rounded corners.** Move from 3px to a token-driven scale (e.g., 6px small, 10px medium, 14px large). Tab buttons, dialogs, popovers, status-bar pills — each picks the right scale.
- **Subtle elevation.** Soft shadows for popovers, dialogs, dropdowns, the status bar. Not glassmorphism / blur — that conflicts with xterm.js's renderer. Just `box-shadow` layers in the dark-on-darker idiom.
- **Refined spacing scale.** A 4px/8px/12px/16px/24px scale, applied consistently. Today's per-component padding values are arbitrary.
- **Color hierarchy with depth.** Move from "two grays" (#1f1f1f panel, #303030 hover) to a layered surface scale: surface-0 (app background, near-black), surface-1 (status bar, panes), surface-2 (tab bar, sidebar), surface-3 (dialogs, popovers — slightly elevated lift). Each layer ~3-5% lighter than the one beneath.
- **Polished interaction states.** Hover, active, focus-visible, and disabled all defined per interactive primitive. Today some components have hover-only, some have active-only, some have neither.
- **Motion.** Short, small-amplitude transitions on hover/focus (120-180ms). No heavy entry/exit animations on dialogs (keep them snappy); just edge-smoothing on color/transform changes.
- **Typography refinements.** A consistent type scale, slightly tighter line-height on UI chrome, and font-feature settings for tabular numerics where numbers appear (volume slider, status counts).

The avatar overlay, waveform visualizer, and xterm.js terminal interior are **out of scope** — the avatar and waveform are user-supplied assets and live by their own visual logic; the xterm.js interior is governed by the per-tab terminal theme.

## Architecture

### Design tokens

Today, `src/app.css` is 16 lines and sets only the body background and font. No tokens. After this feature, `src/app.css` (or a new `src/theme.css`) defines the full token surface as CSS variables on `:root`, and components reference them.

```css
:root {
  /* Surfaces — layered from darkest to lightest */
  --surface-0: #0d0d0f;        /* app background */
  --surface-1: #16161a;        /* panes, status bar */
  --surface-2: #1f1f24;        /* tab bar */
  --surface-3: #2a2a31;        /* dialogs, popovers */
  --surface-4: #353540;        /* hover on surface-3 */

  /* Text */
  --text-primary:   #e8e8ec;
  --text-secondary: #a8a8b0;
  --text-tertiary:  #6a6a72;
  --text-disabled:  #4a4a52;
  --text-on-accent: #ffffff;

  /* Accents and semantics */
  --accent:         #6aa6ff;   /* primary accent — focus rings, active tab indicator */
  --accent-hover:   #8ab8ff;
  --accent-muted:   rgba(106, 166, 255, 0.15);  /* subtle bg fills */
  --success:        #4ec9b0;
  --warning:        #f0a020;
  --danger:         #e74c3c;
  --awaiting:       #f0a020;   /* same as warning — semantic alias */

  /* Borders */
  --border-subtle:  #2a2a31;
  --border-default: #3a3a42;
  --border-strong:  #4a4a52;
  --border-focus:   var(--accent);

  /* Radii */
  --radius-sm: 6px;             /* small chips, badges */
  --radius-md: 10px;            /* buttons, tabs, inputs */
  --radius-lg: 14px;            /* dialogs, popovers, cards */
  --radius-pill: 999px;         /* status pills, toggles */

  /* Elevation */
  --shadow-sm: 0 1px 2px rgba(0,0,0,0.3);
  --shadow-md: 0 4px 12px rgba(0,0,0,0.4);
  --shadow-lg: 0 8px 24px rgba(0,0,0,0.5);

  /* Spacing scale (use these in padding/margin/gap) */
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
```

The exact values land at implementation time after a design pass with real components. The shape (token names, layered surfaces, motion/easing pair) is the load-bearing part — components reference the tokens, so swapping values in one place updates everything.

### Theme as a `data-theme` attribute (forward compatibility)

Set the active theme via an attribute on `<html>` or `<body>`:

```html
<html data-theme="modern-dark">
```

A second theme (e.g., light, or "Modern Dark High Contrast") would be a sibling token block:

```css
[data-theme="modern-light"] {
  --surface-0: #ffffff;
  /* ... */
}
```

Initial ship is "modern-dark" only. Don't add a light theme prematurely; the user explicitly asked for dark. The `data-theme` attribute exists from day one so adding more themes later is an additive change with no refactor needed.

### Settings → Appearance → UI Theme

Add a UI Theme picker to the Settings window. Initial dropdown: only "Modern Dark." When future themes ship, they appear here. Persist as `ui.theme: string` in settings, default `"modern-dark"`.

For v1, the picker has one option and is essentially a placeholder. Keep it — it signals theming as a first-class concept, makes the migration entry obvious, and costs almost nothing.

### Optional: a "Classic" toggle for nostalgia / regression escape hatch

If shipping the new theme as a default risks user surprise, expose "Classic" as a second option that mimics today's hex-coded look. **Recommend skipping** — the user explicitly asked for the modernization. Old screenshots in docs are bigger problem than user surprise. If skipped, the cctts visual language is the modern dark from the day this lands.

### Settings window has its own webview / root

`src/SettingsApp.svelte` and `src/settings_main.ts` mount the Settings window — separate root from `App.svelte`. Both must load the same token CSS. Easiest: both `src/main.ts` and `src/settings_main.ts` import the shared theme CSS at the top (Vite handles the bundling). Verify both windows render identically.

### `settings.html` and `index.html`

Both HTML entry points need the `data-theme="modern-dark"` attribute (or the app-level code sets it on mount, after reading the user's saved preference from settings). If the user sets `ui.theme` in their settings, applying it on mount avoids a flash of unstyled content — set the attribute synchronously at the top of `main.ts` / `settings_main.ts` before Svelte mounts, by reading from a small synchronous bootstrap (or just default to the saved value being `modern-dark` until proven otherwise).

## Implementation outline

### 1. Token surface

- Author `src/theme.css` (or expand `app.css`) with the full token block above. Iterate values against real component renderings, not in isolation.
- Decide accent color. The user didn't specify; a calm blue (#6aa6ff) is a safe default that doesn't fight common terminal palettes. Discuss at implementation time — could also be a soft purple, teal, or muted green.
- Establish the radius scale, spacing scale, motion timings, and elevation layers before component refactor begins. Refactoring without these locked in causes thrash.

### 2. Component refactor (mechanical, ~31 files)

For each Svelte component with a `<style>` block, replace hex literals with tokens. The audit list (from current `grep`):

`AvatarOverlay.svelte`, `ClosedShellOverlay.svelte`, `ErrorBanner.svelte`, `ComposeOverlay.svelte`, `Pane.svelte`, `Split.svelte`, `Tab.svelte`, `TabBar.svelte`, `TabContextMenu.svelte`, `TabErrorOverlay.svelte`, `Toast.svelte`, `StatusBar.svelte`, `WaveformOverlay.svelte`, `AiderFirstLaunchNotice.svelte`, `LayoutNodeRenderer.svelte`, `dialog/{ConfigureTabDialog,ManagePresetsDialog,NewShellTabDialog,SaveLayoutDialog,ShellTabFields}.svelte`, `dnd/{DragGhost,DropZoneOverlay}.svelte`, `settings/{ArrayEditor,NotificationEditor,ShortcutCapture,TabSettingsSection,TextAreaWithReset}.svelte`, `status/{AnnouncementsButton,LayoutsPopover,MuteButton,VolumeSlider}.svelte`.

Mechanical part:
- `#1f1f1f` → `var(--surface-1)` (or surface-2/3 depending on visual depth)
- `#303030` → `var(--surface-3)` (hover state on surfaces)
- `#e0e0e0` → `var(--text-primary)`
- `#c0c0c0` → `var(--text-secondary)`
- `#888` → `var(--text-tertiary)`
- `#4a90e2` → `var(--accent)` (or `--border-focus`)
- `#f0a020` → `var(--awaiting)` or `--warning`
- `#4caf50` → `var(--success)`
- `#e74c3c` → `var(--danger)`
- `border-radius: 3px` → `var(--radius-md)` (or `--radius-sm` for small chips)
- Padding/margin literals → spacing tokens

Non-mechanical part (per-component design decisions, takes longer):
- Choose which surface layer each component sits on.
- Choose which radius scale fits.
- Add elevation tokens to dialogs, popovers, dropdowns.
- Add hover/focus-visible/active states where missing.
- Add motion transitions on color/transform.

### 3. Visual polish pass

After the mechanical refactor renders correctly, a polish pass tunes:

- **Tab bar**: rounded tab buttons (--radius-md), active-tab indicator as a top accent line *or* subtle accent fill (decide; prefer fill — cleaner with rounded corners). Subtle separator between tab bar and pane.
- **Status bar**: pill-shaped status buttons (--radius-pill for binary toggles like mute), spacing breath, subtle border-top.
- **Dialogs**: --radius-lg corners, --shadow-lg elevation, surface-3 background, surface-4 on inputs/selects within. Focus rings using --accent at 2px outline-offset 2px.
- **Popovers / dropdowns** (LayoutsPopover, TabContextMenu): --radius-md, --shadow-md, surface-3 background. Items get --radius-sm on hover-fill.
- **Inputs / textareas** (settings, shell-tab dialog): --radius-md, surface-3 background, accent border on focus, --motion-base transition.
- **Buttons**: a primary/secondary/danger variant set, each with hover/active/focus-visible states using the accent and danger tokens.
- **Compose overlay**: re-evaluate the slide-up styling. Target: feels like a sheet anchored to the bottom edge with --radius-lg on top corners, --shadow-lg, surface-2.
- **Toast**: --radius-md, --shadow-md, surface-3.
- **Drop-zone overlay** (dnd): glow / dashed border using --accent at low alpha. Currently likely a flat fill.
- **Drag ghost**: --radius-sm, slight tilt or opacity, --shadow-md.
- **Tab error overlay / closed shell overlay**: surface-3 panel with --radius-lg, the existing iconography retained.

### 4. Cross-window verification

- Mount the main app and Settings window. Both should render with the same tokens.
- Inspect the Settings window's `<html>` for the `data-theme` attribute.
- Verify focus rings, hover states, dialog elevation in both contexts.

### 5. Cross-platform verification

- WebView2 (Windows): CSS variables and `box-shadow` are well-supported. Verify `backdrop-filter` is **not** relied on (it has spotty support across WebView2 versions); the design intent above avoids it.
- WebKitGTK (Linux): same. WebKit's font rendering differs from WebView2's; the typography scale may need Linux-specific tweaks (slightly larger line-height) — verify on real Linux.

### 6. Documentation

- README screenshot updates (if README has UI screenshots, they're now stale).
- `DESIGN.md` gains a small "Visual language" section pointing at `src/theme.css` as the source of truth and naming the layered-surface convention.
- A short note in `CHANGELOG.md`.

## Open questions

- **Accent color**: blue (default desktop convention), purple (common in modern AI-assistant aesthetics — Claude's brand colors lean orange/peach but the cctts UI doesn't have to match), teal, soft green. Decide at implementation time. Reasonable to expose accent as a token the user can tune in settings later — defer that to a follow-on if asked.
- **Custom font**: today the project uses `-apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif` (system stack). Modern apps often ship a UI font (Inter, system-ui, IBM Plex Sans). System stack is fine and avoids licensing/bundling — recommend keeping it. If a custom font is wanted, bundle it as a self-hosted woff2 in `public/fonts/`. Defer until specifically asked.
- **High-contrast or accessibility variant**: the modernization may slightly *reduce* contrast vs. today's stark black/white. Verify text/background pairs hit WCAG AA at minimum, AAA for primary text. Define a `data-theme="modern-dark-high-contrast"` block as a follow-on if accessibility need arises.
- **Animation reduce-motion respect**: prefer-reduced-motion media query suppresses transitions. Add to the token surface from day one — cheap.
- **xterm.js terminal background contrast**: terminals run with their own theme (per-tab). The cctts chrome around them shouldn't fight visually with the most common terminal palettes (Default, Solarized Dark, Dracula). Surface-1 and surface-2 should contrast clearly with typical terminal backgrounds. Verify visually with a few popular terminal palettes installed.
- **Dialog "header" pattern**: should the dialogs gain a structured header (icon + title + close), or stay header-light? Recommend header-light — current dialogs are simple and don't need over-structuring.

## Milestone recommendation

**One milestone needed**, picked up at implementation time:

- `MILESTONE-V1.X-XX-ui-modernization.md` — the work splits into stages within a single milestone: (1) author tokens, (2) refactor components mechanically, (3) per-component polish, (4) cross-window + cross-platform validation, (5) documentation. All five fit within one milestone because the work is mechanical at its core and the design decisions made up front (the token surface) carry the rest.

The milestone is bigger than typical V1.x polish milestones (~31 files touched) but architecturally simple — no new dependencies, no new IPC, no new persistence beyond the small `ui.theme` setting. Compare to V4-05 (polish + cross-platform validation) for size reference; this is somewhat larger but in the same shape.

**When implementation starts, write the milestone in detail then.** This doc captures the intent and the token shape; the milestone captures the per-component checklist and the design-decision log (accent color, radii values, motion timings) once those are settled.

**Trigger to act**: any time. The user has explicitly requested this. No upstream blockers, no other features blocked on it. Pair-friendly with `FEATURE-per-tab-overrides.md` themes work since both touch styling — but they're independent and either can ship first.

## Non-goals

- **Glassmorphism / heavy blur effects.** Conflicts with xterm.js's renderer choices and adds GPU cost.
- **Full redesign of the avatar overlay / waveform visualizer.** Out of scope; those are user-supplied assets and have their own visual logic.
- **Animated transitions on layout changes.** v1.3 explicitly chose snap-no-animation for layout ops (per V4-05); preserve that decision.
- **Theming the xterm.js terminal interior.** That's `FEATURE-per-tab-overrides.md` § "Terminal color themes."
- **A light theme.** Could ship later as an additional `data-theme` block; out of scope here.

## Files most likely touched

- `src/app.css` (or new `src/theme.css`) — token surface.
- `src/main.ts`, `src/settings_main.ts` — import shared theme CSS, set initial `data-theme`.
- `index.html`, `settings.html` — `data-theme` attribute on `<html>`.
- All components under `src/lib/` with `<style>` blocks (~31 files; full list above in §2).
- `src/lib/settings/{store,types}.ts` — new `ui.theme` field.
- `src-tauri/src/settings/{schema,migration}.rs` — `ui.theme` field with default `"modern-dark"`, migration adds it idempotently.
- `src/SettingsApp.svelte` — new "Appearance" → UI Theme picker entry (just a dropdown with one option for now).
- `docs/DESIGN.md` — short "Visual language" section.
- `CHANGELOG.md` — entry.

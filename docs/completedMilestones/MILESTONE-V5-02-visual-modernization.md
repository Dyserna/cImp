# Milestone V5-02: Visual Modernization, Polish, and Verification

## Purpose

Turn cctts's visual language from the v1.3.1 dated dark-on-dark look into a modern desktop UI matching the visual reference (Atom-style cool slate-blue surfaces, mint/teal accent, coral semantics, pill-shaped active tabs, generous radii). This is the user-visible payoff of the V5 modernization.

V5-01 stood up the token system and converted every component to reference tokens. V5-02 changes the *values* of those tokens in `theme.css`, introduces a small set of new UI primitives, and runs a per-component polish pass. Because the substrate is in place, the visual rewrite is mostly a single-file diff plus targeted component changes — not a 31-file sweep.

Read `docs/features/FEATURE-ui-modernization.md` and `docs/MILESTONE-V5-01-design-tokens.md` first.

## What This Milestone Delivers

1. New token values in `src/theme.css` matching the V5 design intent: cool slate-blue surface palette, mint/teal accent (`#3eddb6` family), coral danger (`#f06080`), expanded radius scale (12 / 16 / 999), elevation shadows applied.
2. A new reusable `Pill.svelte` primitive for tag/badge use sites (status indicators, kind labels, severity tags).
3. `Tab.svelte` redesign: filled elevated-pill active state replacing the bottom-border accent stripe. Tabs no longer flush with the pane below; they read as pills floating on the bar.
4. Two-tier active-state pattern applied across the chrome: section selection uses surface elevation, filter/toggle selection uses solid mint accent fill, CTAs use solid mint accent fill.
5. Per-component polish pass against the design intent: dialogs gain `--shadow-lg` and `--radius-lg`; popovers gain `--shadow-md` and `--radius-md`; status-bar toggles become pill-shaped; the compose overlay reads as a sheet with rounded top corners; the drop-zone overlay uses a mint dashed border with low-alpha glow.
6. Standardized `:focus-visible` treatment using `--accent` outline + 2px offset on every interactive primitive.
7. Standardized motion: hover/focus transitions on color and transform use `--motion-fast`; surface/elevation changes use `--motion-base` with `--easing-standard`.
8. Tabular numerics (`font-feature-settings: "tnum"`) applied to numeric displays — volume slider value, status counts.
9. Cross-window verification: main app and Settings window render with the same token values and have visually identical chrome.
10. WCAG AA contrast verification for primary text/background pairs and accent fills.
11. `docs/DESIGN.md` gains a "Visual language" section. `CHANGELOG.md` gets a V5 entry. README screenshots flagged as stale (or refreshed if the user provides them).

## What This Milestone Does NOT Do

- No second theme. The picker still has one option ("Modern Dark"). A light theme or high-contrast variant is a future feature.
- No theming the xterm.js terminal interior. That's `FEATURE-per-tab-overrides.md` § "Terminal color themes" — a separate feature, ships independently.
- No glassmorphism / blur effects. They conflict with xterm.js and add GPU cost; explicitly out of scope per the feature doc.
- No animated transitions on layout changes (pane split, pane close, drag drops). v1.3 chose snap-no-animation; preserve that decision.
- No redesign of the avatar overlay or waveform visualizer. Those are user-supplied assets with their own visual logic.
- No custom UI font. System stack stays. Bundling a font is deferred until specifically asked.
- No user-tunable accent color. The single mint accent is fixed in V5; per-user accent is a future feature.
- No structured dialog headers (icon + title + close button). Today's dialogs are header-light; keep them that way.

## Implementation Steps

### 1. Token values — the single-file diff

Rewrite `src/theme.css` values in place. Token *names* are unchanged from V5-01, so component references continue to resolve.

```css
:root,
[data-theme="modern-dark"] {
  /* Surfaces — cool slate-blue. Tight contrast between layers; depth
     comes from accent + shadow, not extreme bg differences. */
  --surface-0: #1a1d24;        /* app background */
  --surface-1: #21252e;        /* panes, status bar */
  --surface-2: #2a2f3a;        /* tab bar */
  --surface-3: #353b48;        /* dialogs, popovers, active tab */
  --surface-4: #404757;        /* hover on surface-3 */

  /* Text */
  --text-primary:   #f1f3f5;
  --text-secondary: #b0b6c0;
  --text-tertiary:  #6a707c;
  --text-disabled:  #444a55;
  --text-on-accent: #0d1117;   /* dark text on mint fills */

  /* Accent — mint/teal */
  --accent:         #3eddb6;
  --accent-hover:   #5fe7c5;
  --accent-fg:      var(--text-on-accent);
  --accent-muted:   rgba(62, 221, 182, 0.15);

  /* Semantics */
  --success:        var(--accent);   /* mint doubles as success */
  --warning:        #f0a020;
  --danger:         #f06080;          /* coral, not red */
  --awaiting:       var(--warning);

  /* Borders */
  --border-subtle:  #2a2f3a;
  --border-default: #404757;
  --border-strong:  #565d6e;
  --border-focus:   var(--accent);

  /* Radii */
  --radius-sm:    8px;
  --radius-md:   12px;
  --radius-lg:   16px;
  --radius-pill: 999px;

  /* Elevation */
  --shadow-sm: 0 1px 2px rgba(0, 0, 0, 0.35);
  --shadow-md: 0 4px 12px rgba(0, 0, 0, 0.45);
  --shadow-lg: 0 12px 32px rgba(0, 0, 0, 0.55);
}
```

Spacing, motion, and typography tokens are unchanged from V5-01. Eyeball the slate-blue values against `Tab.svelte` and `StatusBar.svelte` first; tune by 1-2% steps if they read too purple, too cold, or too neutral. The values above are starting points.

### 2. `Pill.svelte` primitive

The reference design uses tag pills for kind labels and status badges. Add a small reusable primitive. Path: `src/lib/Pill.svelte`.

```svelte
<script lang="ts">
  type Variant = 'default' | 'mint' | 'coral' | 'orange' | 'accent-fill';
  type Size = 'xs' | 'sm' | 'md';

  let {
    variant = 'default',
    size = 'sm',
    children,
  }: {
    variant?: Variant;
    size?: Size;
    children: import('svelte').Snippet;
  } = $props();
</script>

<span class="pill pill-{variant} pill-{size}">{@render children()}</span>

<style>
  .pill {
    display: inline-flex;
    align-items: center;
    border-radius: var(--radius-pill);
    border: 1px solid transparent;
    font-weight: var(--font-weight-medium);
    line-height: 1;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .pill-xs { font-size: 10px; padding: 2px var(--space-2); }
  .pill-sm { font-size: 11px; padding: 3px var(--space-2); }
  .pill-md { font-size: 12px; padding: var(--space-1) var(--space-3); }

  .pill-default {
    background: var(--surface-3);
    color: var(--text-secondary);
    border-color: var(--border-default);
  }
  .pill-mint {
    background: var(--accent-muted);
    color: var(--accent);
    border-color: var(--accent);
  }
  .pill-coral {
    background: rgba(240, 96, 128, 0.15);
    color: var(--danger);
    border-color: var(--danger);
  }
  .pill-orange {
    background: rgba(240, 160, 32, 0.15);
    color: var(--warning);
    border-color: var(--warning);
  }
  .pill-accent-fill {
    background: var(--accent);
    color: var(--accent-fg);
    border-color: var(--accent);
  }
</style>
```

Use sites (any may be deferred if they don't earn their keep — the primitive is the deliverable, not a mandate to retrofit every status indicator):

- A "RESTART" pill in `TabSettingsSection.svelte` when settings drift from baseline.
- Severity badge in `Toast.svelte` when severity is set.
- Optional: kind badges in `Tab.svelte` ("AI" mint / "SH" default). Decide at polish time — could clutter narrow tabs.

The existing `.indicator` dots in `Tab.svelte` (working / awaiting / done / error) stay as colored dots — pills would be too visually heavy in a tab strip. They retint to the new palette but keep the dot shape.

### 3. `Tab.svelte` — pill-shaped active state

The reference establishes the active tab as an elevated rounded pill with white text, not a flush rectangle with a colored bottom border. Rewrite the active-state styling:

```css
/* Replace today's pattern:
   .tab.active { color: #fff; background: #1f1f1f; border-bottom-color: #4a90e2; }
   With: */
.tab {
  border-right: none;          /* remove the per-tab right divider */
  border-bottom: none;         /* remove the bottom-border accent strip */
  border-radius: var(--radius-md);
  margin: var(--space-1) 2px;  /* small gap so tabs read as separate pills */
  padding: 0 var(--space-3);
  height: calc(100% - var(--space-2));
  transition: background var(--motion-fast) var(--easing-standard),
              color var(--motion-fast) var(--easing-standard);
}
.tab:hover {
  background: var(--surface-3);
  color: var(--text-primary);
}
.tab.active {
  background: var(--surface-3);
  color: var(--text-primary);
  /* No bottom border. Surface elevation IS the indicator. */
}
.tab:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: -2px;
}
```

`TabBar.svelte` consequences:

- The bar's bottom border can become subtler or disappear entirely. The surface contrast between `--surface-2` (bar) and `--surface-1` (pane below) carries the visual separation.
- The per-pane edge-fade gradients (added in V4-05) reference the bar's background. Update those to fade to `--surface-2`.
- The `+` button styling matches tab dimensions but uses `--text-tertiary` color until hovered.

Verify the active-tab read against the reference: the active tab should look like a pill *floating on* the bar, not a flush rectangle.

### 4. Per-component polish

Order components by user visibility. Per-file changes are small but numerous; here's the per-area target.

**Tab bar and tabs:** covered in step 3.

**Status bar (`StatusBar.svelte` + `status/*`):**

- Bar background: `--surface-1`. Subtle top border using `--border-subtle`.
- Toggles (`MuteButton`, `AnnouncementsButton`): pill-shaped (`--radius-pill`), `--surface-2` resting, `--accent` fill when on (mute on, announcements on). White-on-accent or dark-on-accent depending on legibility — verify.
- `VolumeSlider`: track uses `--border-default`, fill uses `--accent`, thumb uses `--text-primary`. Numeric value display gets `font-feature-settings: "tnum"`.
- `LayoutsPopover`: `--surface-3` background, `--shadow-md`, `--radius-md`, list items get `--radius-sm` corners on hover-fill of `--surface-4`.

**Dialogs (`dialog/*`):**

- `--surface-3` background, `--radius-lg` corners, `--shadow-lg` elevation.
- Inputs and selects within dialogs: `--surface-2` background (sit *into* the dialog), `--border-default`, `--accent` border on focus, `--motion-base` transition, `--radius-md`.
- Primary CTAs (e.g., "Save", "Create"): solid `--accent` fill, `--accent-fg` text, `--radius-md`.
- Secondary buttons (e.g., "Cancel"): `--surface-4` background, `--text-primary`, `--radius-md`.
- Danger buttons (e.g., "Delete preset"): `--danger` border + `--danger` text, transparent background; on hover, `--danger` fill + `--text-on-accent`.
- Focus rings: `2px solid var(--accent)` with `outline-offset: 2px` everywhere.

**Settings window:**

- Section headers gain breath: `--space-5` margin top, `--space-3` margin bottom.
- Inputs follow the dialog pattern.
- The new Appearance dropdown (added in V5-01) inherits the input styling.
- `ShortcutCapture`: capturing-state highlight uses `--accent-muted` background + `--accent` border.
- `ArrayEditor`, `NotificationEditor`, `TextAreaWithReset`: same input pattern.

**Overlays:**

- `Toast`: `--surface-3` bg, `--shadow-md`, `--radius-md`. Severity colors come from semantic tokens (`--accent` info, `--warning` warning, `--danger` error). Optional `Pill` for severity.
- `ComposeOverlay`: sheet treatment — `--surface-2` bg, `--radius-lg` *on top corners only* (`border-radius: var(--radius-lg) var(--radius-lg) 0 0`), `--shadow-lg`. The slide-up motion uses `--motion-base` + `--easing-standard`.
- `ErrorBanner`: `--danger` border-left at 4px, `--surface-3` bg, `--text-primary` content, `--radius-md`.
- `TabErrorOverlay`, `ClosedShellOverlay`: `--surface-3` panel, `--radius-lg`, `--shadow-md`. Existing iconography retained.
- `AiderFirstLaunchNotice`: `--surface-3` bg, `--accent-muted` left border, `--radius-lg`.

**DnD visuals (`dnd/*`):**

- `DropZoneOverlay`: today likely a flat fill. Replace with a 2px dashed `--accent` border + `--accent-muted` low-alpha fill + `--shadow-md` inner glow. The center icon uses `--accent`.
- `DragGhost`: `--surface-3` bg, `--radius-sm`, `--shadow-md`, `opacity: 0.85`.

**Layout chrome:**

- `Pane.svelte`: focused-pane indicator. V4-05 used a subtle border. Switch to a 2px top border at `--accent` on the focused pane's tab bar, fading out after the bar height. Keep it subtle — distraction is the failure mode V4-05 explicitly warned against.
- `Split.svelte`: splitter handle `--border-default` at rest, `--accent` on hover.
- `LayoutNodeRenderer`: pure structural; no styling change beyond inherited tokens.

**Avatar / waveform chrome:**

- `AvatarOverlay`: any chrome around the avatar (the container, debug status indicator wrapper) uses the new tokens. The avatar image and animation logic is unchanged.
- `WaveformOverlay`: same treatment for chrome only. The waveform color comes from `settings.avatar.waveform.color` (user-tunable, separate from the chrome theme).

### 5. Focus-visible standardization

A short audit pass: every interactive element gets `:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }`. Override `outline-offset: -2px` for elements where a positive offset would clip (e.g., tab buttons with negative margin).

Elements to check:

- Tab buttons, the tab `+` button, the close-tab `×` button.
- Status-bar toggles, popover trigger buttons.
- Dialog primary/secondary/danger buttons, dialog inputs.
- Settings inputs, ShortcutCapture's capture button.
- LayoutsPopover items, TabContextMenu items.
- Compose overlay's submit button, the `Esc` close affordance.

### 6. Motion standardization

Replace ad-hoc `transition:` values with token references. Where a transition does not exist but should (a hover state changes color but with a hard edge), add `transition: <prop> var(--motion-fast) var(--easing-standard);`.

Standard recipes:

- Color/background changes on hover: `--motion-fast`.
- Transform / position changes: `--motion-base`.
- Both directions (hover-on and hover-off) use the same timing — no asymmetric ease.

Sheet-style entry (compose overlay slide-up): keep existing motion code; only swap the duration to `--motion-base` and the easing to `--easing-standard`.

### 7. Tabular numerics

Apply `font-feature-settings: "tnum"` (and `font-variant-numeric: tabular-nums` as the modern alias) at `:root` for any numeric display. The cleanest approach is a utility class:

```css
.tnum { font-variant-numeric: tabular-nums; font-feature-settings: "tnum"; }
```

Use sites: `VolumeSlider` value, status counts in `LayoutsPopover` ("3 panes"), notification badge counts if present.

### 8. Cross-window verification

- Open the main app and the Settings window simultaneously.
- DevTools: pick a tab in the main window, pick a settings input. Computed colors should resolve to the same token values.
- Open a dialog from the main app (e.g., New Shell Tab). Open a dialog flow in Settings (e.g., editing a notification). Both dialogs should look identical in surface, radius, shadow, focus ring.
- Verify the Settings window's `<html>` carries `data-theme="modern-dark"` (set by V5-01).
- Verify there is no FOUC on either window during launch.

### 9. WCAG AA contrast verification

Test pairs (use any contrast checker — DevTools's accessibility panel works):

- `--text-primary` on `--surface-0`: must hit AAA (7:1) for primary body text.
- `--text-primary` on `--surface-1`, `--surface-2`, `--surface-3`: must hit AA (4.5:1).
- `--text-secondary` on `--surface-1`: must hit AA.
- `--text-on-accent` on `--accent`: must hit AA for button text.
- `--danger` text on `--surface-3` (error states in dialogs): must hit AA.
- `--accent` border at 1px is visible against `--surface-3` (focus-ring sanity check).

If any pair fails, nudge the failing token by 5-10% lightness toward whichever side closes the gap. The values in step 1 are picked to clear AA but verify in DevTools rather than trusting eyeball judgment.

### 10. Terminal palette compatibility

Open a Claude tab and a Shell tab side-by-side in a split layout. Try the default xterm theme, Solarized Dark, and Dracula. The cctts chrome (`--surface-1` panes, `--surface-2` tab bar) should contrast clearly with each terminal background — the tab bar shouldn't blend into the terminal interior.

If the slate-blue chrome reads too close to a particular terminal palette, adjust the surface values in step 1 by 3-5% steps. The user's terminal is more important than the chrome's exact tint, so chrome adjusts to make terminal palettes pop.

### 11. Documentation

`docs/DESIGN.md`: add a short "Visual language" section under the existing structure. Reference `src/theme.css` as the source of truth. Name the layered-surface convention (surface-0 darkest, surface-4 lightest hover). Note the two-tier active-state pattern (elevation for sections, accent fill for filters/CTAs). One paragraph; this is a reference pointer, not a manual.

`CHANGELOG.md`: add a V5 / v1.4 entry. Bullets:

- New Modern Dark theme — refreshed visual language with cool slate surfaces, mint accent, coral semantics.
- Centralized design tokens in `src/theme.css`; all components reference tokens.
- Settings → Appearance → UI Theme picker (one option for now; future themes plug in here).
- Pill-shaped tab indicators replacing the bottom-border accent.
- Larger, more consistent rounded corners across dialogs, popovers, sheets.
- Motion respects `prefers-reduced-motion`.
- v1.3 → v1.4 settings: `ui.theme` field added; existing files continue to load unchanged.

README screenshots: if README has UI screenshots, mark them stale or replace. Screenshot updates require a clean window state and matching DPI to look right; the user may prefer to do these themselves. If screenshots are out of scope, add a one-line note in the milestone retrospective and move on.

## Files Touched / Added

**Added:**
- `src/lib/Pill.svelte` — reusable pill/badge primitive.

**Modified:**
- `src/theme.css` — token *values* rewritten. Names unchanged from V5-01.
- `src/lib/Tab.svelte` — pill-shaped active state, no more bottom-border indicator.
- `src/lib/TabBar.svelte` — bar background, edge-fade gradient color, tab gap.
- `src/lib/StatusBar.svelte`, `src/lib/status/*.svelte` — pill toggles, popover styling.
- `src/lib/Pane.svelte`, `src/lib/Split.svelte` — focused-pane indicator, splitter hover.
- `src/lib/dialog/*.svelte` — surface, shadow, radius, button variants.
- `src/lib/settings/*.svelte` — input styling, ShortcutCapture state.
- `src/lib/{Toast, ErrorBanner, ComposeOverlay, ClosedShellOverlay, TabErrorOverlay, AiderFirstLaunchNotice}.svelte` — overlay surface treatment.
- `src/lib/dnd/*.svelte` — drop-zone glow, drag-ghost shadow.
- `src/lib/{TabContextMenu, LayoutNodeRenderer, AvatarOverlay, WaveformOverlay}.svelte` — chrome polish.
- `src/SettingsApp.svelte` — section spacing, picker styling.
- `docs/DESIGN.md` — Visual language section.
- `CHANGELOG.md` — V5 entry.

**Not modified:**
- `src-tauri/src/settings/*.rs` — V5-01 added the `ui.theme` field; no further backend changes.
- Component logic (script blocks). Polish is `<style>` block only.

## Edge Cases and Gotchas

- **Active-tab geometry change.** Today the tab is flush with the pane below; the bottom-border accent fits this geometry. The pill treatment requires a small gap above and below the tab. Verify the pane viewport doesn't overlap the tab bar after the geometry change — `Pane.svelte`'s top edge may need a small adjustment.
- **xterm.js scrollback area.** The terminal renders into its own canvas/DOM; the chrome around it changes color. Verify there's no visible seam between the chrome's `--surface-1` and the xterm.js container's background. xterm.js's background comes from its own theme, not cctts's tokens — keep them visually compatible.
- **Per-pane edge-fade gradients.** V4-05's tab-bar overflow uses CSS gradients that fade to the bar background. Update these to fade to the new `--surface-2`. A wrong fade color reads as a hard line.
- **Focused-pane indicator visibility.** V4-05 explicitly tuned this to "clearly present but not visually noisy." With slate-blue surfaces and a mint accent, a 2px top accent on the focused pane's tab bar may be more visible than v1.3's version. If it dominates, fall back to a 1px accent or reduce to `--accent-muted` (low alpha).
- **Compose overlay's slide-up clipping.** Top corners get `--radius-lg` but bottom corners stay 0. Some browsers / WebView2 versions render `border-radius: 16px 16px 0 0` with subpixel artifacts at the corners. Add `overflow: hidden` on the overlay container to mask any.
- **Dialog backdrop.** Today's dialog likely uses a semi-transparent black overlay behind the panel. Update to `rgba(0, 0, 0, 0.55)` or use `--surface-0` at ~85% alpha for consistency with the slate palette. Verify the dialog panel still reads as elevated against the backdrop.
- **WebView2 `box-shadow` performance.** Large blur radii (32px+) on dialogs can be expensive. The values in step 1 stay under 32px. If a perf issue surfaces, drop `--shadow-lg` from `0 12px 32px` to `0 8px 24px`.
- **`prefers-reduced-motion`.** V5-01 added the suppression; verify it still applies after V5-02's transitions are added. Re-test with Windows Settings → Accessibility → Visual effects → Animation off.
- **The debug status indicator.** Per project memory, retain it. Token-ize its colors but don't remove or hide it.
- **Color-picker fields (waveform color).** `settings.avatar.waveform.color` is a user-tunable hex value, persisted as a string. It is NOT a chrome token — leave the color picker rendering literal hex.

## Manual Verification Checklist

Run on Windows. Linux validation deferred per project convention.

Visual fidelity vs. reference:

- [ ] Tab bar: tabs read as elevated pills floating on a slate-blue bar. Active tab is filled, white text, no bottom border.
- [ ] Status bar: pill-shaped toggles, mint fill when on.
- [ ] Dialogs: rounded 16px corners, soft shadow, slate-3 surface.
- [ ] Compose overlay: rounded top corners, sheet-like presence at the bottom.
- [ ] Popovers (Layouts, TabContextMenu): rounded 12px corners, soft shadow, slate-3 surface, hover-highlight on items.
- [ ] Drop-zone overlay: dashed mint border with low-alpha fill (vs. today's flat fill).
- [ ] Drag ghost: rounded with shadow, slight transparency.

Active-state two-tier pattern:

- [ ] Section selection (e.g., active tab, active settings section): elevated surface + white text. Calm.
- [ ] Filter selection (e.g., status-bar mute on, announcements on): solid mint fill. Bright.
- [ ] CTAs (dialog Save / Create buttons): solid mint fill.
- [ ] Cancel / secondary buttons: subdued surface fill, no accent.

Focus rings:

- [ ] Tab through every interactive element: tabs, buttons, inputs, popover items, ShortcutCapture, status pills.
- [ ] Each shows a 2px mint outline on focus.
- [ ] Outline doesn't clip on tab buttons (margin-aware offset).

Motion:

- [ ] Hover on tab → smooth color transition (~120ms), no hard snap.
- [ ] Hover on status toggle → smooth.
- [ ] Compose overlay open → slide-up over `--motion-base`.
- [ ] Enable Reduced Motion in Windows → all transitions become instant.

Cross-window parity:

- [ ] Main app and Settings window open side by side. Identical chrome treatment.
- [ ] Open a dialog in main app (New Shell Tab). Open a dialog flow in Settings (notification edit). Identical look.
- [ ] No FOUC on launch of either window.

Contrast (WCAG):

- [ ] DevTools accessibility audit on the main window: no contrast warnings on text elements.
- [ ] DevTools accessibility audit on the Settings window: same.
- [ ] CTA button text passes AA against accent fill.

Terminal palette compatibility:

- [ ] Default xterm theme: chrome and terminal interior contrast clearly.
- [ ] Solarized Dark: same.
- [ ] Dracula: same.
- [ ] In each: tab bar's `--surface-2` does not blend into the terminal background.

Pill primitive:

- [ ] Pill renders with each variant (default, mint, coral, orange, accent-fill).
- [ ] Each size (xs, sm, md) is visually distinct and readable.
- [ ] At least one use site is wired up (e.g., RESTART pill in TabSettingsSection).

Regressions to check:

- [ ] All v1.3 features still work: multi-tab, multi-pane, drag-and-drop, splitters, layouts, presets, shortcuts.
- [ ] Compose overlay submit/cancel paths unchanged.
- [ ] Notification flow unchanged.
- [ ] Avatar overlay renders with its user-supplied images.
- [ ] Waveform overlay renders at the user-configured color.
- [ ] Debug status indicator still visible bottom-right.

End-to-end sanity:

- [ ] Launch app fresh. Slate-blue chrome, mint accents, coral semantics throughout.
- [ ] Open Settings → Appearance → confirm "Modern Dark" is selected.
- [ ] Drag a tab between panes. Drop-zone glow is mint dashed.
- [ ] Open the New Shell Tab dialog. Surface-3 panel, shadow-lg elevation, accent border on focused inputs, mint Create button.
- [ ] Trigger an error toast. Coral severity treatment.
- [ ] Quit. Relaunch. Theme persists; no FOUC; settings file has `ui.theme: "modern-dark"`.

## Done Criteria

- All 11 "What This Milestone Delivers" items are in place.
- All "Manual Verification Checklist" items pass on Windows.
- Visual language matches the reference design intent: cool slate surfaces, mint accent, coral semantics, pill-shaped active tabs, generous radii, soft shadows, polished hover/focus states.
- WCAG AA achieved for primary text/background pairs and CTA contrast.
- No regression in any v1.3 feature.
- `docs/DESIGN.md` documents the visual language.
- `CHANGELOG.md` has the V5 entry.
- v1.4 ships from this milestone.

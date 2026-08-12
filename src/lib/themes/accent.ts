// Accent color for the built-in `tui` theme.
//
// The TUI theme is compiled into the backend binary with its accent family
// keyed off a single CSS variable, `--tui-accent`. This module owns the
// frontend half: validating the persisted `ui.tui_accent` setting and
// injecting it (plus a luminance-derived `--tui-text-on-accent`) onto
// <html>, where the theme CSS picks it up and derives the whole family
// (hover/bright/soft/muted) via color-mix. Only the `tui` theme's CSS reads
// these variables, so applying them is a no-op under any other theme — the
// accent setting is TUI-only by construction.

/// Id of the built-in TUI theme. Mirrors `TUI_THEME_ID` in
/// `src-tauri/src/theming/mod.rs`.
export const TUI_THEME_ID = 'tui';

/// Default accent — the blue the pre-v28 `tui-blue` default theme used.
/// Mirrors `DEFAULT_TUI_ACCENT` in `src-tauri/src/settings/schema.rs`.
export const DEFAULT_TUI_ACCENT = '#7aa2f7';

/// The four accents the legacy tui-* theme variants shipped with, offered
/// as one-click presets next to the free color picker in
/// Settings → Appearance → Theme.
export const TUI_ACCENT_PRESETS: readonly { name: string; color: string }[] = [
  { name: 'Orange', color: '#d77757' },
  { name: 'Blue', color: '#7aa2f7' },
  { name: 'Green', color: '#98c379' },
  { name: 'Grey', color: '#c8ccd0' },
];

/// Validate any persisted `#rrggbb` color setting against a caller-supplied
/// fallback — the same rule `normalizeTuiAccent` applies, factored out for
/// the other color settings (the containment colors below): full hex only
/// (the only shape `<input type="color">` produces), so a hand-edited
/// settings.json can't break the chrome.
export function normalizeHexColor(value: string | null | undefined, fallback: string): string {
  const v = (value ?? '').trim();
  return /^#[0-9a-fA-F]{6}$/.test(v) ? v.toLowerCase() : fallback;
}

/// Validate a persisted accent value. Anything but a full `#rrggbb` hex
/// falls back to the default.
export function normalizeTuiAccent(value: string | null | undefined): string {
  return normalizeHexColor(value, DEFAULT_TUI_ACCENT);
}

// ── V32 containment colors ──────────────────────────────────────────────
// Worn by a tab's taint badge and the pane frame around its content while
// containment applies (`ui.latched_color` / `ui.contaminated_color`,
// resolved through `latch.ts::taintColor`). Defaults mirror the TUI theme's
// `--warning` / `--danger`, which is what the badge wore before the colors
// became configurable; the Rust defaults in `settings/schema.rs` must match.

export const DEFAULT_LATCHED_COLOR = '#fabd2f';
export const DEFAULT_CONTAMINATED_COLOR = '#fb4934';

/// Chrome text color for content painted on the accent fill (selection,
/// filled CTAs). WCAG relative luminance with the standard ~0.179 flip
/// point: dark Gruvbox ink on light accents, light cream on dark ones —
/// so a user picking a dark accent keeps readable selections.
export function tuiTextOnAccent(value: string | null | undefined): string {
  const hex = normalizeTuiAccent(value);
  const lin = (c: number) => {
    const s = c / 255;
    return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  const luminance =
    0.2126 * lin(parseInt(hex.slice(1, 3), 16)) +
    0.7152 * lin(parseInt(hex.slice(3, 5), 16)) +
    0.0722 * lin(parseInt(hex.slice(5, 7), 16));
  return luminance > 0.179 ? '#1d2021' : '#fbf1c7';
}

/// Push the accent onto <html> as inline CSS variables. Called from each
/// window's settings subscription; idempotent and cheap. The theme CSS
/// carries matching fallbacks, so the pre-settings first paint is already
/// the default blue.
export function applyTuiAccent(value: string | null | undefined): void {
  const accent = normalizeTuiAccent(value);
  const style = document.documentElement.style;
  style.setProperty('--tui-accent', accent);
  style.setProperty('--tui-text-on-accent', tuiTextOnAccent(accent));
}

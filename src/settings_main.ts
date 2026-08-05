import { mount } from 'svelte';
import SettingsApp from './SettingsApp.svelte';
import { initSettings, settings } from './lib/settings/store';
import { themeFromSetting, applyTerminalPaletteVars } from './lib/themes/resolve';
import { initThemeRegistry, themeMeta } from './lib/themes/registry';
import { applyTuiAccent, TUI_THEME_ID } from './lib/themes/accent';
import { installReloadBlocker } from './lib/shortcuts/blockReload';
import { getCurrentWindow } from '@tauri-apps/api/window';
import './theme.css';
import './app.css';

document.documentElement.dataset.theme = TUI_THEME_ID;

// Match the main window: no user-triggered reload in the settings window either.
installReloadBlocker();

// The settings window is created hidden (`.visible(false)` in
// `windows.rs::open_or_focus_settings`) to avoid the white WebView flash
// while the bundle loads, plus the title-bar blink as the TUI themes drop
// the OS chrome. Reveal exactly once, after the first decorations pass —
// same pattern as `main.ts`.
let hasShownWindow = false;
let showInFlight = false;
function showWindowOnce() {
  if (hasShownWindow || showInFlight) return;
  showInFlight = true;
  const win = getCurrentWindow();
  void win
    .show()
    .then(() => {
      // Latch only once the window is actually up. Latching before the IPC
      // resolves would make a transient `show()` failure permanent — the
      // failsafe below and every later chrome pass would short-circuit and
      // the settings window would stay invisible for the rest of its life.
      hasShownWindow = true;
      return win.setFocus().catch((e) => {
        // Non-fatal: the window is visible, it just didn't take focus.
        console.error('settings window setFocus failed:', e);
      });
    })
    .catch((e) => {
      console.error('settings window show failed:', e);
    })
    .finally(() => {
      showInFlight = false;
    });
}
// Safety net: if the decorations IPC round-trip never resolves, reveal anyway
// so a backend hiccup can't leave the user staring at nothing.
setTimeout(showWindowOnce, 3000);

let currentThemeId: string = TUI_THEME_ID;
let lastDecorations: boolean | null = null;
/// Both prerequisites for a *correct* first chrome pass are in:
///   1. the theme registry has loaded (real `themeMeta().decorations` +
///      the injected theme CSS), and
///   2. the settings store holds the backend snapshot rather than the
///      `defaultSettings()` value it is constructed with — i.e. we know
///      which theme is actually selected.
/// Until then `applyChrome` does nothing: acting on fallback metadata used
/// to reveal the window before the theme CSS landed (and, for a user theme
/// with `decorations: true`, with the wrong chrome). Only the 3 s failsafe
/// above can reveal the window while this is false.
let chromeInputsReady = false;

function applyChrome() {
  if (!chromeInputsReady) return;
  const wantDecorations = themeMeta(currentThemeId).decorations;
  if (wantDecorations !== lastDecorations) {
    lastDecorations = wantDecorations;
    void getCurrentWindow()
      .setDecorations(wantDecorations)
      // First decorations pass is done — the chrome is in its final state,
      // safe to reveal.
      .finally(showWindowOnce);
  } else {
    showWindowOnce();
  }
}

// Load the verified theme/palette registry (injects theme CSS) AND the real
// settings snapshot, then run the first chrome pass with both in hand.
// `initSettings` is idempotent and awaited by `SettingsApp` too; calling it
// here just means the reveal doesn't depend on component mount timing. Both
// helpers swallow their own failures and fall back to defaults, so the
// `.finally` always runs — but keep the catch so a future throw can't leave
// the window on the failsafe path silently.
void Promise.all([initThemeRegistry(), initSettings()])
  .catch((e) => console.error('settings window init failed:', e))
  .finally(() => {
    chromeInputsReady = true;
    applyChrome();
  });

settings.subscribe((s) => {
  currentThemeId = s.ui?.theme || TUI_THEME_ID;
  if (document.documentElement.dataset.theme !== currentThemeId) {
    document.documentElement.dataset.theme = currentThemeId;
  }
  applyTuiAccent(s.ui?.tui_accent);
  if (s.terminal?.theme) applyTerminalPaletteVars(themeFromSetting(s.terminal.theme));
  applyChrome();
});

const target = document.getElementById('app');
if (!target) {
  throw new Error('#app root element not found');
}

const app = mount(SettingsApp, { target });

export default app;

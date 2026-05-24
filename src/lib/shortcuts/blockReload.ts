// Suppresses the webview's built-in reload accelerators (F5, Ctrl+R,
// Ctrl+Shift+R, Ctrl+F5, and the macOS Cmd+R variants).
//
// A user-triggered reload tears the whole webview down and back up: every
// tab's PTY is killed and respawned, scrollback is lost, and the Claude tab
// in particular can get wedged on an error popup while the session restarts
// underneath it. There is no situation where we want that, so we disable it
// outright rather than expose it as a setting.
//
// We only call `preventDefault()` — never `stopPropagation()`. preventDefault
// cancels the webview's reload (its default action) while letting the keydown
// continue to xterm.js, so the terminal still sees the key. That matters for
// Ctrl+R, which is readline's reverse-history-search in a shell, and for F5,
// which TUIs may bind: the reload dies, the keystroke survives.
//
// Installed once per window (main + settings) at startup, on the capture
// phase so it runs regardless of focus.

let installed = false;

function isReloadKey(e: KeyboardEvent): boolean {
  // `code` is layout-independent. F5 (and Ctrl+F5) both report code "F5".
  if (e.code === 'F5') return true;
  // Ctrl+R / Ctrl+Shift+R, plus the macOS Cmd+R equivalents.
  if ((e.ctrlKey || e.metaKey) && e.code === 'KeyR') return true;
  return false;
}

/// Install the capture-phase keydown guard exactly once. Subsequent calls are
/// no-ops, so it's safe to call unconditionally from each window's entry point.
export function installReloadBlocker(): void {
  if (installed) return;
  installed = true;
  window.addEventListener(
    'keydown',
    (e) => {
      if (isReloadKey(e)) e.preventDefault();
    },
    true,
  );
}

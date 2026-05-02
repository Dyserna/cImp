// Window-level shortcut dispatcher. Installs a single capture-phase keydown
// listener so configured shortcuts run *before* xterm.js sees the key event;
// when no shortcut matches the event continues to xterm.js as normal.
//
// The dispatcher is also live-reconfigurable: every change to the settings
// `shortcuts` slice re-parses the strings and swaps the predicates without
// disturbing the listener.

import { parseShortcut, matches, type ShortcutPredicate } from './parser';
import type { ShortcutSettings } from '../settings/types';

export type ShortcutAction = 'open_compose' | 'submit_compose' | 'cancel_compose' | 'open_settings';

export type ShortcutHandlers = Partial<Record<ShortcutAction, () => void>>;

let predicates: Partial<Record<ShortcutAction, ShortcutPredicate | null>> = {};
let handlers: ShortcutHandlers = {};
let installed = false;
let suppressed = false;

/// Reconfigure both the parsed predicates and the action handlers. Idempotent
/// — calling repeatedly is the intended pattern (re-runs on every settings
/// change). Does NOT install the global listener; callers must call
/// `installDispatcher()` once at app startup.
export function configureShortcuts(s: ShortcutSettings, h: ShortcutHandlers): void {
  predicates = {
    open_compose: parseShortcut(s.open_compose),
    submit_compose: parseShortcut(s.submit_compose),
    cancel_compose: parseShortcut(s.cancel_compose),
    open_settings: parseShortcut(s.open_settings),
  };
  handlers = h;
}

/// Install the capture-phase keydown listener exactly once. Subsequent
/// calls are no-ops.
export function installDispatcher(): void {
  if (installed) return;
  installed = true;
  window.addEventListener('keydown', onKeyDown, true);
}

/// Temporarily silence the dispatcher. Used by the shortcut-capture UI so
/// the user pressing `Ctrl+Shift+E` to bind a new shortcut doesn't also fire
/// the existing `Ctrl+Shift+E` handler. Always pair `setSuppressed(true)`
/// with a matching `setSuppressed(false)`.
export function setSuppressed(value: boolean): void {
  suppressed = value;
}

function onKeyDown(event: KeyboardEvent): void {
  if (suppressed) return;
  for (const name of Object.keys(predicates) as ShortcutAction[]) {
    const pred = predicates[name];
    if (pred && matches(event, pred)) {
      const handler = handlers[name];
      if (handler) {
        event.preventDefault();
        event.stopPropagation();
        handler();
      }
      return;
    }
  }
}

// Window-level shortcut dispatcher. Installs a single capture-phase keydown
// listener so configured shortcuts run *before* xterm.js sees the key event;
// when no shortcut matches the event continues to xterm.js as normal.
//
// The dispatcher is also live-reconfigurable: every change to the settings
// `shortcuts` slice re-parses the strings and swaps the predicates without
// disturbing the listener.

import { parseShortcut, matches, type ShortcutPredicate } from './parser';
import type { ShortcutSettings } from '../settings/types';

export type ShortcutAction =
  | 'open_compose'
  | 'submit_compose'
  | 'cancel_compose'
  | 'open_settings'
  | 'switch_to_tab_1'
  | 'switch_to_tab_2'
  | 'switch_to_tab_3'
  | 'switch_to_tab_4'
  | 'switch_to_tab_5'
  | 'switch_to_tab_6'
  | 'switch_to_tab_7'
  | 'switch_to_tab_8'
  | 'switch_to_tab_9'
  | 'new_shell_tab'
  | 'close_tab';

/// A shortcut binding can be a bare function (always fires when matched) or
/// an object with an `active` predicate. When `active()` returns false the
/// dispatcher does NOT preventDefault — the keypress flows to xterm.js or
/// the focused element as usual. This is how `submit_compose` (Ctrl+Enter)
/// avoids swallowing the key when focus is in the terminal.
export type ShortcutHandler =
  | (() => void)
  | { handler: () => void; active?: () => boolean };

export type ShortcutHandlers = Partial<Record<ShortcutAction, ShortcutHandler>>;

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
    switch_to_tab_1: parseShortcut(s.switch_to_tab_1),
    switch_to_tab_2: parseShortcut(s.switch_to_tab_2),
    switch_to_tab_3: parseShortcut(s.switch_to_tab_3),
    switch_to_tab_4: parseShortcut(s.switch_to_tab_4),
    switch_to_tab_5: parseShortcut(s.switch_to_tab_5),
    switch_to_tab_6: parseShortcut(s.switch_to_tab_6),
    switch_to_tab_7: parseShortcut(s.switch_to_tab_7),
    switch_to_tab_8: parseShortcut(s.switch_to_tab_8),
    switch_to_tab_9: parseShortcut(s.switch_to_tab_9),
    new_shell_tab: parseShortcut(s.new_shell_tab),
    close_tab: parseShortcut(s.close_tab),
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
    if (!pred || !matches(event, pred)) continue;
    const binding = handlers[name];
    if (!binding) return;
    const fn = typeof binding === 'function' ? binding : binding.handler;
    const active = typeof binding === 'function' ? null : binding.active;
    // If an `active` predicate is provided and returns false we do nothing —
    // not even preventDefault — so the key continues to its normal target.
    if (active && !active()) return;
    event.preventDefault();
    event.stopPropagation();
    fn();
    return;
  }
}

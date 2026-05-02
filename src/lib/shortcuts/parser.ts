// Shortcut string ⇄ key event predicate. The string format is the same one
// the user sees in the settings UI, e.g. "Ctrl+Shift+E" or "Ctrl+,". Parser
// is permissive about modifier names (`Cmd`, `Meta`, `Command` all map to
// `meta`), case-insensitive, and special-cases the keys that browsers
// report with multi-char names (`Enter`, `Escape`, `Space`, `Tab`).

export interface ShortcutPredicate {
  key: string;
  ctrl: boolean;
  shift: boolean;
  alt: boolean;
  meta: boolean;
}

const NAMED_KEYS: Record<string, string> = {
  enter: 'enter',
  return: 'enter',
  esc: 'escape',
  escape: 'escape',
  space: ' ',
  tab: 'tab',
  backspace: 'backspace',
  delete: 'delete',
  del: 'delete',
  up: 'arrowup',
  down: 'arrowdown',
  left: 'arrowleft',
  right: 'arrowright',
  home: 'home',
  end: 'end',
  pageup: 'pageup',
  pagedown: 'pagedown',
};

export function parseShortcut(s: string | null | undefined): ShortcutPredicate | null {
  if (!s) return null;
  const trimmed = s.trim();
  if (!trimmed) return null;
  const parts = trimmed.split('+').map((p) => p.trim()).filter((p) => p.length > 0);
  if (parts.length === 0) return null;
  const lastRaw = parts[parts.length - 1].toLowerCase();
  const key = NAMED_KEYS[lastRaw] ?? lastRaw;
  const modifiers = new Set(parts.slice(0, -1).map((p) => p.toLowerCase()));
  return {
    key,
    ctrl: modifiers.has('ctrl') || modifiers.has('control'),
    shift: modifiers.has('shift'),
    alt: modifiers.has('alt') || modifiers.has('option'),
    meta: modifiers.has('meta') || modifiers.has('cmd') || modifiers.has('command') || modifiers.has('win'),
  };
}

export function matches(event: KeyboardEvent, p: ShortcutPredicate): boolean {
  // event.key is the printed character or named key; lower-case for symmetry
  // with what `parseShortcut` produced. Compare modifiers strictly so
  // `Ctrl+E` does NOT also fire on `Ctrl+Shift+E`.
  return (
    event.key.toLowerCase() === p.key &&
    event.ctrlKey === p.ctrl &&
    event.shiftKey === p.shift &&
    event.altKey === p.alt &&
    event.metaKey === p.meta
  );
}

/// Format a KeyboardEvent into the canonical shortcut string. Used by the
/// capture UI in the settings window. Pure-modifier presses are rejected by
/// the caller before this point.
export function formatShortcut(event: KeyboardEvent): string {
  const parts: string[] = [];
  if (event.ctrlKey) parts.push('Ctrl');
  if (event.shiftKey) parts.push('Shift');
  if (event.altKey) parts.push('Alt');
  if (event.metaKey) parts.push('Meta');
  parts.push(displayKey(event.key));
  return parts.join('+');
}

function displayKey(k: string): string {
  // One-character keys: upper-case so "ctrl+e" displays as "Ctrl+E".
  if (k.length === 1) return k.toUpperCase();
  // Named keys: capitalize the first letter for readability.
  // ArrowUp / ArrowDown stay as-is (the browser already capitalizes).
  return k.charAt(0).toUpperCase() + k.slice(1);
}

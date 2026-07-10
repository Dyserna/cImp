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
  plus: '+',
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
  // The literal plus key collides with the '+' separator: a captured string
  // from an older build looks like "Ctrl++" (or a bare "+"), which a naive
  // split-and-filter collapses into a modifier-only, never-matching
  // predicate. Peel a trailing '+' off as the key before splitting. (The
  // capture UI now emits "Ctrl+Plus", mapped back via NAMED_KEYS, but stored
  // strings must keep working.)
  let keyRaw: string;
  let modifierParts: string[];
  if (trimmed.endsWith('+')) {
    keyRaw = '+';
    const rest = trimmed.slice(0, -1).replace(/\+$/, '');
    modifierParts = rest.split('+').map((p) => p.trim()).filter((p) => p.length > 0);
  } else {
    const parts = trimmed.split('+').map((p) => p.trim()).filter((p) => p.length > 0);
    if (parts.length === 0) return null;
    keyRaw = parts[parts.length - 1];
    modifierParts = parts.slice(0, -1);
  }
  const lastRaw = keyRaw.toLowerCase();
  const key = NAMED_KEYS[lastRaw] ?? lastRaw;
  const modifiers = new Set(modifierParts.map((p) => p.toLowerCase()));
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
  // Two keys whose raw `event.key` is unrepresentable in the string format:
  // a literal ' ' is trimmed/filtered away by `parseShortcut`, and a literal
  // '+' collides with the separator. Emit the parseable names instead
  // (NAMED_KEYS maps them back on the parse side).
  if (k === ' ') return 'Space';
  if (k === '+') return 'Plus';
  // One-character keys: upper-case so "ctrl+e" displays as "Ctrl+E".
  if (k.length === 1) return k.toUpperCase();
  // Named keys: capitalize the first letter for readability.
  // ArrowUp / ArrowDown stay as-is (the browser already capitalizes).
  return k.charAt(0).toUpperCase() + k.slice(1);
}

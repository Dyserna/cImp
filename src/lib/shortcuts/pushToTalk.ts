// Push-to-talk (V6-01). A *hold* gesture on a (usually modifiers-only) chord,
// which the generic dispatcher — keydown-only and fire-once — can't model. The
// default binding is bare `Ctrl+Shift`: hold to record, release to transcribe.
//
// Bare `Ctrl+Shift` is held during many ordinary chords (terminal
// `Ctrl+Shift+C`/`V`, OS shortcuts), so a naive "modifiers held → record"
// would fire constantly. Three guards make it robust:
//   - arm + debounce: the chord must be held ~150 ms before recording starts,
//     so a quick `Ctrl+Shift+<key>` chord (which presses a non-modifier almost
//     immediately) never records.
//   - abort-on-other-key: any non-modifier pressed while armed/recording
//     cancels (discards) the recording and lets the keypress flow to its
//     normal handler — this is what lets PTT coexist with un-remappable
//     terminal/OS chords.
//   - repeat latch: `event.repeat` keydowns are ignored so key auto-repeat
//     never re-triggers start.
//
// The binding may also include an explicit key (e.g. `Ctrl+Shift+Space`), in
// which case it's a plain hold of that exact combo — no debounce/abort needed
// because there's no collision with bare modifiers.

import { getSuppressed } from './dispatcher';

export interface PttCallbacks {
  start: () => void;
  stop: () => void;
  cancel: () => void;
}

interface PttChord {
  ctrl: boolean;
  shift: boolean;
  alt: boolean;
  meta: boolean;
  /// The non-modifier key (lower-case, e.g. " " for Space), or null for a
  /// pure-modifier chord.
  key: string | null;
}

const MOD_TOKENS = new Set([
  'ctrl',
  'control',
  'shift',
  'alt',
  'option',
  'meta',
  'cmd',
  'command',
  'win',
]);

const NAMED_KEYS: Record<string, string> = {
  space: ' ',
  enter: 'enter',
  return: 'enter',
  tab: 'tab',
};

const DEBOUNCE_MS = 150;

let chord: PttChord | null = null;
let cbs: PttCallbacks | null = null;
let state: 'idle' | 'armed' | 'recording' = 'idle';
let timer: ReturnType<typeof setTimeout> | null = null;
let installed = false;

/// Parse a PTT binding. If the last token is itself a modifier the whole chord
/// is modifiers-only (`key: null`); otherwise the last token is the key.
function parsePtt(s: string | null | undefined): PttChord | null {
  if (!s) return null;
  const parts = s
    .split('+')
    .map((p) => p.trim())
    .filter((p) => p.length > 0);
  if (parts.length === 0) return null;

  const last = parts[parts.length - 1].toLowerCase();
  let key: string | null = null;
  let mods = parts;
  if (!MOD_TOKENS.has(last)) {
    key = NAMED_KEYS[last] ?? last;
    mods = parts.slice(0, -1);
  }
  const set = new Set(mods.map((p) => p.toLowerCase()));
  return {
    ctrl: set.has('ctrl') || set.has('control'),
    shift: set.has('shift'),
    alt: set.has('alt') || set.has('option'),
    meta: set.has('meta') || set.has('cmd') || set.has('command') || set.has('win'),
    key,
  };
}

function clearTimer() {
  if (timer !== null) {
    clearTimeout(timer);
    timer = null;
  }
}

function reset() {
  clearTimer();
  state = 'idle';
}

/// (Re)configure push-to-talk. Called on every settings change. When STT is
/// disabled or the binding is empty the controller is inert. Re-binding mid-
/// hold cancels any in-flight recording so it can't get stuck.
export function configurePushToTalk(
  enabled: boolean,
  binding: string | null,
  callbacks: PttCallbacks,
): void {
  if (state === 'recording' && cbs) cbs.cancel();
  reset();
  cbs = callbacks;
  chord = enabled ? parsePtt(binding) : null;
}

/// Install the capture-phase keydown + keyup listeners once.
export function installPushToTalk(): void {
  if (installed) return;
  installed = true;
  window.addEventListener('keydown', onKeyDown, true);
  window.addEventListener('keyup', onKeyUp, true);
}

function modifiersSatisfied(e: KeyboardEvent): boolean {
  if (!chord) return false;
  return (
    e.ctrlKey === chord.ctrl &&
    e.shiftKey === chord.shift &&
    e.altKey === chord.alt &&
    e.metaKey === chord.meta
  );
}

function isModifierKey(k: string): boolean {
  return k === 'control' || k === 'shift' || k === 'alt' || k === 'meta';
}

/// True when `k` (lower-case `event.key`) is a modifier the chord requires.
function requiredModifierReleased(k: string): boolean {
  if (!chord) return false;
  return (
    (k === 'control' && chord.ctrl) ||
    (k === 'shift' && chord.shift) ||
    (k === 'alt' && chord.alt) ||
    (k === 'meta' && chord.meta)
  );
}

function onKeyDown(event: KeyboardEvent): void {
  if (!chord || !cbs || getSuppressed()) return;
  const k = event.key.toLowerCase();

  if (chord.key === null) {
    // Pure-modifier chord.
    if (!isModifierKey(k)) {
      // Abort-on-other-key: discard an in-flight recording and let the key
      // through to its normal handler (no preventDefault).
      if (state === 'recording') cbs.cancel();
      reset();
      return;
    }
    if (event.repeat) return;
    if (state === 'idle' && modifiersSatisfied(event)) {
      state = 'armed';
      clearTimer();
      timer = setTimeout(() => {
        timer = null;
        if (state === 'armed') {
          state = 'recording';
          cbs?.start();
        }
      }, DEBOUNCE_MS);
    }
    return;
  }

  // Explicit-key chord: a plain hold of the exact combo.
  if (k === chord.key && modifiersSatisfied(event)) {
    if (event.repeat) {
      event.preventDefault();
      return;
    }
    if (state === 'idle') {
      state = 'recording';
      cbs.start();
      event.preventDefault();
      event.stopPropagation();
    }
  }
}

function onKeyUp(event: KeyboardEvent): void {
  if (!chord || !cbs) return;
  const k = event.key.toLowerCase();

  if (chord.key === null) {
    if (!requiredModifierReleased(k)) return;
    if (state === 'recording') {
      reset();
      cbs.stop();
    } else if (state === 'armed') {
      // Released before the debounce elapsed → a quick chord, not dictation.
      reset();
    }
    return;
  }

  // Explicit-key chord: releasing the key or a required modifier stops.
  if (state === 'recording' && (k === chord.key || requiredModifierReleased(k))) {
    reset();
    cbs.stop();
  }
}

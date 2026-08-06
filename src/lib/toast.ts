// Minimal transient-toast store. Mount `Toast.svelte` once at the app
// root; call `showToast(...)` from anywhere to enqueue a message.
// Toasts auto-dismiss after `durationMs` (default 2.5s).

import { writable, type Writable } from 'svelte/store';

export interface Toast {
  id: number;
  message: string;
}

let nextId = 0;
export const toasts: Writable<Toast[]> = writable([]);

const ESC = '\x1b';
const BEL = '\x07';
const ST8 = '\u009c'; // 8-bit string terminator
/** 8-bit introducers: DCS, SOS, OSC, PM, APC. */
const C1_STRING_INTRO = '\u0090\u0098\u009d\u009e\u009f';

/** A control character with no meaning in composed copy: C0 except `\n`/`\t`,
 * DEL, and the whole C1 block. `\r` is included deliberately — a carriage
 * return overwrites the current line in a terminal, a spoofing primitive on its
 * own, and `\r\n` still leaves its `\n`. */
function isBareControl(c: string): boolean {
  const n = c.charCodeAt(0);
  return (n < 0x20 && c !== '\n' && c !== '\t') || (n >= 0x7f && n <= 0x9f);
}

/**
 * V32 Phase D — terminal-escape hygiene at the toast chokepoint.
 *
 * Every toast today is composed from static app copy (audited 2026-08-06: the
 * only non-literal call site is the `ai-tab-restart-hint` listener in
 * App.svelte, which interpolates a fixed consumer-name mapping), so this is
 * defence in depth, not a fix for a live hole. It exists because the *next*
 * toast is the risk: surfacing a backend error string, an MCP tool failure or
 * an offload result would put external, model-influenced text on this path, and
 * toast text is copyable — a pasted `ESC ] 52 ; c ; …` clipboard write or a
 * cursor-motion run is a real primitive once it reaches a terminal. Svelte's
 * auto-escaping covers HTML in the rendered toast; it does nothing about
 * control sequences, which are not markup.
 *
 * Mirrors the Rust `processing::strip_terminal_escapes`
 * (`src-tauri/src/processing/sanitize.rs`, where the threat model is written
 * out): ESC-initiated sequences are removed WHOLE — introducer *and* body, so a
 * payload cannot survive as visible text once its introducer is gone — along
 * with the 8-bit C1 forms and bare C0/DEL controls. `\n` and `\t` are kept.
 * A scanner rather than a regex chain: the string-body rule needs one character
 * of lookahead it may decline to consume (an escape opening inside a body is
 * re-processed as a fresh sequence, never spilled as text).
 */
export function stripTerminalEscapes(text: string): string {
  let out = '';
  let i = 0;
  // Skip a CSI body: params 0x30–0x3f, intermediates 0x20–0x2f, final 0x40–0x7e.
  const skipCsi = (): void => {
    while (i < text.length) {
      const n = text.charCodeAt(i);
      if (n >= 0x20 && n <= 0x3f) i += 1;
      else if (n >= 0x40 && n <= 0x7e) {
        i += 1;
        return;
      } else return;
    }
  };
  // Skip an OSC/DCS/SOS/PM/APC body up to and including BEL / `ESC \` / U+009C.
  // Unterminated runs to end of input, as a terminal parses it.
  const skipString = (): void => {
    while (i < text.length) {
      const c = text[i];
      if (c === BEL || c === ST8) {
        i += 1;
        return;
      }
      if (c === ESC) {
        if (text[i + 1] === '\\') i += 2; // proper 7-bit ST
        // Otherwise leave the ESC for the main loop: a non-ST escape aborts the
        // string, and the nested sequence must be stripped in turn.
        return;
      }
      i += 1;
    }
  };
  while (i < text.length) {
    const c = text[i];
    if (c === ESC) {
      i += 1;
      if (i >= text.length) break; // trailing lone ESC
      const next = text[i];
      if (next === '[') {
        i += 1;
        skipCsi();
      } else if (next === ']' || 'PX^_'.includes(next)) {
        i += 1;
        skipString();
      } else if ('()*+-./#%'.includes(next)) {
        i += 2; // charset designation, DEC line size, charset select
      } else {
        i += 1; // any other single-character escape (`ESC c`, `ESC 7`, …)
      }
    } else if (c === '\u009b') {
      i += 1; // 8-bit CSI
      skipCsi();
    } else if (C1_STRING_INTRO.includes(c)) {
      i += 1;
      skipString();
    } else if (c === '\n' || c === '\t') {
      out += c;
      i += 1;
    } else if (isBareControl(c)) {
      i += 1;
    } else {
      out += c;
      i += 1;
    }
  }
  return out;
}

export function showToast(message: string, durationMs = 2500): void {
  const id = nextId++;
  const clean = stripTerminalEscapes(message);
  toasts.update((list) => [...list, { id, message: clean }]);
  setTimeout(() => {
    toasts.update((list) => list.filter((t) => t.id !== id));
  }, durationMs);
}

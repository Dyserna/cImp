// V32 Phase D — the toast chokepoint's terminal-escape hygiene.
//
// These pin the SAME behaviour as the Rust unit tests in
// `src-tauri/src/processing/sanitize.rs`. The two implementations are
// deliberate duplicates (one per runtime, no shared module across the Tauri
// boundary), so they are tested against the same cases: if one is edited
// without the other, the pair of suites disagrees.

import { describe, it, expect } from 'vitest';
import { get } from 'svelte/store';
import { stripTerminalEscapes, showToast, toasts } from './toast';

describe('stripTerminalEscapes', () => {
  it('strips OSC 52 clipboard writes with every terminator form', () => {
    // BEL-terminated (the common form).
    expect(stripTerminalEscapes('before\x1b]52;c;bWFsaWNpb3Vz\x07after')).toBe('beforeafter');
    // 7-bit ST (`ESC \\`).
    expect(stripTerminalEscapes('before\x1b]52;c;bWFsaWNpb3Vz\x1b\\after')).toBe('beforeafter');
    // 8-bit ST.
    expect(stripTerminalEscapes('before\x1b]52;c;bWFsaWNpb3Vz\u009cafter')).toBe('beforeafter');
    // 8-bit OSC introducer.
    expect(stripTerminalEscapes('before\u009d52;c;bWFsaWNpb3Vz\x07after')).toBe('beforeafter');
  });

  it('strips CSI colour and cursor sequences, 7-bit and 8-bit', () => {
    expect(stripTerminalEscapes('\x1b[31mred\x1b[0m plain')).toBe('red plain');
    expect(stripTerminalEscapes('a\x1b[2Jb\x1b[1;2Hc\x1b[?25ld\x1b[1;38;5;196me')).toBe('abcde');
    expect(stripTerminalEscapes('a\u009b31mb')).toBe('ab');
  });

  it('leaves plain multi-line text with tabs untouched', () => {
    const text = 'Line one.\n\tIndented [not a CSI] value\n\nLast line — 100% fine.\n';
    expect(stripTerminalEscapes(text)).toBe(text);
    expect(stripTerminalEscapes('')).toBe('');
  });

  it('strips DCS/SOS/PM/APC string sequences', () => {
    for (const intro of ['P', 'X', '^', '_']) {
      expect(stripTerminalEscapes(`a\x1b${intro}payload;1;2\x1b\\b`)).toBe('ab');
    }
    expect(stripTerminalEscapes('a\u009fpayload\u009cb')).toBe('ab');
    expect(stripTerminalEscapes('a\u009epayload\x07b')).toBe('ab');
  });

  it('consumes an unterminated string sequence to the end of input', () => {
    expect(stripTerminalEscapes('keep\x1b]52;c;dGFpbA==')).toBe('keep');
  });

  it('drops lone, single-character and two-character escapes', () => {
    expect(stripTerminalEscapes('text\x1b')).toBe('text');
    expect(stripTerminalEscapes('a\x1bcb')).toBe('ab');
    expect(stripTerminalEscapes('a\x1b(Bb')).toBe('ab');
    expect(stripTerminalEscapes('a\x1b#8b')).toBe('ab');
  });

  it('strips bare controls but keeps newline and tab', () => {
    expect(stripTerminalEscapes('ok\r\nnext\b\v\f\x7f\u0085\ttail')).toBe('ok\nnext\ttail');
    expect(stripTerminalEscapes('nul\0byte')).toBe('nulbyte');
  });

  it('does not let a nested introducer escape the strip', () => {
    const hostile = 'x\x1b]0;\x1b]52;c;cHduZWQ=\x07\x07y';
    expect(stripTerminalEscapes(hostile)).toBe('xy');
  });

  it('is idempotent', () => {
    for (const input of ['\x1b[31mred\x1b[0m', 'a\x1b]52;c;eA==\x07b', 'plain\ttext\nhere']) {
      const once = stripTerminalEscapes(input);
      expect(stripTerminalEscapes(once)).toBe(once);
    }
  });
});

describe('showToast', () => {
  it('sanitizes the message it enqueues', () => {
    showToast('done \x1b]52;c;cHduZWQ=\x07ok', 10_000);
    const list = get(toasts);
    expect(list[list.length - 1].message).toBe('done ok');
  });

  it('sanitizes an action label too — it is rendered and copyable text as well', () => {
    let ran = 0;
    showToast('restart?', 10_000, {
      label: 'Restart \x1b]52;c;cHduZWQ=\x07now',
      run: () => (ran += 1),
    });
    const entry = get(toasts).at(-1);
    expect(entry?.action?.label).toBe('Restart now');
    entry?.action?.run();
    expect(ran).toBe(1);
  });

  it('carries no action when none was given', () => {
    showToast('plain', 10_000);
    expect(get(toasts).at(-1)?.action).toBeUndefined();
  });
});

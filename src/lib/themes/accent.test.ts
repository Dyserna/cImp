import { describe, expect, it } from 'vitest';
import {
  DEFAULT_TUI_ACCENT,
  TUI_ACCENT_PRESETS,
  normalizeTuiAccent,
  tuiTextOnAccent,
} from './accent';

describe('normalizeTuiAccent', () => {
  it('accepts full #rrggbb hex and lowercases it', () => {
    expect(normalizeTuiAccent('#D77757')).toBe('#d77757');
    expect(normalizeTuiAccent('#7aa2f7')).toBe('#7aa2f7');
  });

  it('falls back to the default for anything else', () => {
    for (const bad of ['', '  ', '#fff', '#12345', 'd77757', '#gghhii', null, undefined]) {
      expect(normalizeTuiAccent(bad)).toBe(DEFAULT_TUI_ACCENT);
    }
  });
});

describe('tuiTextOnAccent', () => {
  it('uses dark ink on all four legacy preset accents (the historical look)', () => {
    for (const p of TUI_ACCENT_PRESETS) {
      expect(tuiTextOnAccent(p.color), p.name).toBe('#1d2021');
    }
  });

  it('flips to light text on dark accents so selections stay readable', () => {
    expect(tuiTextOnAccent('#7a2020')).toBe('#fbf1c7');
    expect(tuiTextOnAccent('#20205a')).toBe('#fbf1c7');
    expect(tuiTextOnAccent('#000000')).toBe('#fbf1c7');
  });

  it('treats an invalid value as the default accent (dark ink)', () => {
    expect(tuiTextOnAccent('nonsense')).toBe('#1d2021');
  });
});

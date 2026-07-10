import { describe, expect, test, vi, beforeEach } from 'vitest';

// `substituteTemplate` reads the clipboard through the Tauri plugin (jsdom
// has no real Tauri runtime) and the focused pane's terminal through two
// small app modules — mock all three so the pure substitution logic is
// testable in isolation, matching the pattern in `terminal/background.test.ts`.
// `vi.mock` factories are hoisted above imports (even above other top-level
// `const`s), so the mock state they close over is built via `vi.hoisted`
// with no dependency on any real import — a hand-rolled single-value store
// is enough here, so `svelte/store` doesn't need to be available yet.
const { clipboardReadText, focusedTabId, getTerminalMock } = vi.hoisted(() => {
  let current: string | null = null;
  const subscribers = new Set<(v: string | null) => void>();
  return {
    clipboardReadText: vi.fn<() => Promise<string>>(),
    getTerminalMock: vi.fn<(tabId: string) => { getSelection: () => string } | undefined>(),
    focusedTabId: {
      set(v: string | null) {
        current = v;
        for (const fn of subscribers) fn(current);
      },
      subscribe(fn: (v: string | null) => void) {
        subscribers.add(fn);
        fn(current);
        return () => subscribers.delete(fn);
      },
    },
  };
});

vi.mock('@tauri-apps/plugin-clipboard-manager', () => ({
  readText: () => clipboardReadText(),
}));

vi.mock('../layout/store', () => ({
  focusedActiveTabId: focusedTabId,
}));

vi.mock('../terminals', () => ({
  getTerminal: (tabId: string) => getTerminalMock(tabId),
}));

// Invoke isn't called by anything under test here, but the module imports
// it at top level — stub it so importing doesn't require a Tauri runtime.
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import {
  fuzzyMatch,
  filterTemplates,
  hasPlaceholder,
  nextPlaceholderRange,
  substituteVariables,
  substituteTemplate,
  type ResolvedTemplate,
} from './templates';

describe('fuzzyMatch (subsequence)', () => {
  test('empty query matches everything', () => {
    expect(fuzzyMatch('', 'anything')).toBe(true);
  });

  test('exact match', () => {
    expect(fuzzyMatch('review', 'review-this-diff')).toBe(true);
  });

  test('non-contiguous subsequence matches', () => {
    // "rvw" -> r-e-v-i-e-w: r, v, w in order.
    expect(fuzzyMatch('rvw', 'review')).toBe(true);
  });

  test('is case-insensitive', () => {
    expect(fuzzyMatch('REV', 'review-this-diff')).toBe(true);
  });

  test('out-of-order characters do not match', () => {
    expect(fuzzyMatch('wvr', 'review')).toBe(false);
  });

  test('characters not present at all do not match', () => {
    expect(fuzzyMatch('xyz', 'review')).toBe(false);
  });
});

describe('filterTemplates', () => {
  const templates: ResolvedTemplate[] = [
    { name: 'review-this-diff', body: 'a', scope: 'global' },
    { name: 'write-tests-for', body: 'b', scope: 'global' },
    { name: 'explain-selection', body: 'c', scope: 'project' },
  ];

  test('empty query returns every template, order preserved', () => {
    expect(filterTemplates(templates, '')).toEqual(templates);
  });

  test('filters to subsequence matches only', () => {
    const out = filterTemplates(templates, 'rev');
    expect(out.map((t) => t.name)).toEqual(['review-this-diff']);
  });

  test('no matches returns an empty array', () => {
    expect(filterTemplates(templates, 'zzz')).toEqual([]);
  });
});

describe('hasPlaceholder', () => {
  test('true when a {name} token is present', () => {
    expect(hasPlaceholder('do {thing} now')).toBe(true);
  });

  test('false for plain text', () => {
    expect(hasPlaceholder('nothing to see here')).toBe(false);
  });

  test('false once placeholders have all been overtyped away', () => {
    expect(hasPlaceholder('do the thing now')).toBe(false);
  });

  test('${name} interpolation syntax from substituted code is NOT a placeholder', () => {
    // Regression (2026-07 review): a {selection} substitution splicing in
    // code like `Hello ${name}!` used to leave the Tab-jump handler active
    // forever — the tab-stop scan re-reads the live draft and cannot tell a
    // template placeholder from the user's own pasted interpolation.
    expect(hasPlaceholder('console.log(`Hello ${name}!`)')).toBe(false);
    expect(hasPlaceholder('echo "${HOME}"')).toBe(false);
  });
});

describe('nextPlaceholderRange', () => {
  test('finds the first placeholder from index 0', () => {
    const text = 'Review {file} and write {tests}.';
    const ph = nextPlaceholderRange(text, 0);
    expect(ph).toEqual({ start: 7, end: 13 });
    expect(text.slice(ph!.start, ph!.end)).toBe('{file}');
  });

  test('finds the NEXT placeholder after the current one', () => {
    const text = 'Review {file} and write {tests}.';
    const first = nextPlaceholderRange(text, 0)!;
    const second = nextPlaceholderRange(text, first.end);
    expect(text.slice(second!.start, second!.end)).toBe('{tests}');
  });

  test('wraps around to the first placeholder past the last one', () => {
    const text = '{a} middle {b}';
    const b = nextPlaceholderRange(text, 4)!;
    expect(text.slice(b.start, b.end)).toBe('{b}');
    const wrapped = nextPlaceholderRange(text, b.end);
    expect(text.slice(wrapped!.start, wrapped!.end)).toBe('{a}');
  });

  test('returns null when no placeholder remains', () => {
    expect(nextPlaceholderRange('nothing here', 0)).toBeNull();
  });

  test('skips ${name} spans inside substituted code, jumping to the real placeholder', () => {
    // Regression (2026-07 review): after `{selection}` splices in a template
    // literal, Tab used to select `{name}` inside the user's pasted code —
    // overtyping it silently deleted their own code.
    const text = 'Explain:\nHello ${name}!\n\nContext:\n{context}';
    const ph = nextPlaceholderRange(text, 0)!;
    expect(text.slice(ph.start, ph.end)).toBe('{context}');
  });

  test('self-heals once a placeholder has been overtyped: re-scanning the edited text skips it', () => {
    const original = 'Review {file} and write {tests}.';
    const firstPh = nextPlaceholderRange(original, 0)!;
    // Simulate the user overtyping the first placeholder with real text.
    const edited =
      original.slice(0, firstPh.start) + 'app.rs' + original.slice(firstPh.end);
    // Only {tests} remains — a single scan from 0 finds it directly, with
    // no memory of the original {file} offset needed.
    const remaining = nextPlaceholderRange(edited, 0)!;
    expect(edited.slice(remaining.start, remaining.end)).toBe('{tests}');
  });
});

describe('substituteVariables', () => {
  test('replaces {selection} and {clipboard}', () => {
    const out = substituteVariables('Explain {selection}, then check {clipboard}.', 'SEL', 'CLIP');
    expect(out).toBe('Explain SEL, then check CLIP.');
  });

  test('leaves unknown {name} placeholders literal', () => {
    const out = substituteVariables('Fix {file} using {selection}', 'SEL', 'CLIP');
    expect(out).toBe('Fix {file} using SEL');
  });

  test('substitutes with empty strings when selection/clipboard are empty', () => {
    const out = substituteVariables('[{selection}][{clipboard}]', '', '');
    expect(out).toBe('[][]');
  });

  test('repeated variables are all substituted', () => {
    const out = substituteVariables('{selection} and {selection} again', 'X', '');
    expect(out).toBe('X and X again');
  });

  test('a substituted value containing $-patterns is inserted verbatim (function replacement)', () => {
    const out = substituteVariables('[{selection}]', 'cost $& and $1', '');
    expect(out).toBe('[cost $& and $1]');
  });

  test('${selection} is interpolation syntax, not a substitutable placeholder', () => {
    const out = substituteVariables('run ${selection} and {selection}', 'SEL', '');
    expect(out).toBe('run ${selection} and SEL');
  });
});

describe('substituteTemplate (integration of the two variable sources)', () => {
  beforeEach(() => {
    clipboardReadText.mockReset();
    getTerminalMock.mockReset();
    focusedTabId.set(null);
  });

  test('pulls {selection} from the focused pane terminal and {clipboard} from the plugin', async () => {
    focusedTabId.set('claude');
    getTerminalMock.mockReturnValue({ getSelection: () => 'const x = 1;' });
    clipboardReadText.mockResolvedValue('clipboard text');

    const out = await substituteTemplate('Review: {selection}\nContext: {clipboard}\nFile: {file}');
    expect(out).toBe('Review: const x = 1;\nContext: clipboard text\nFile: {file}');
    expect(getTerminalMock).toHaveBeenCalledWith('claude');
  });

  test('empty selection when there is no focused pane', async () => {
    focusedTabId.set(null);
    clipboardReadText.mockResolvedValue('');

    const out = await substituteTemplate('[{selection}]');
    expect(out).toBe('[]');
    expect(getTerminalMock).not.toHaveBeenCalled();
  });

  test('empty selection when the focused terminal has none selected', async () => {
    focusedTabId.set('claude');
    getTerminalMock.mockReturnValue({ getSelection: () => '' });
    clipboardReadText.mockResolvedValue('');

    const out = await substituteTemplate('[{selection}]');
    expect(out).toBe('[]');
  });

  test('a clipboard read failure degrades to an empty substitution rather than throwing', async () => {
    focusedTabId.set(null);
    clipboardReadText.mockRejectedValue(new Error('denied'));

    await expect(substituteTemplate('[{clipboard}]')).resolves.toBe('[]');
  });
});

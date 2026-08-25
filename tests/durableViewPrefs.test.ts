// Guard: every per-view pref a view actually writes is CLASSIFIED — durable
// (per project, in `ui_state.json`) or ephemeral (per machine, in
// `localStorage`) — and every name in the durable set is one a view writes.
//
// Why this exists (V42 review DEF-2, closed in the Phase-F pass as F-6).
// `DURABLE_VIEW_PREFS` in `src/lib/uiState.ts` is a hand-kept allowlist that
// nothing checked. `saveViewString(view, key, …)` with a name that is not in it
// does not fail, warn, or degrade: it routes to `localStorage` and the value
// persists — in the wrong store and the wrong scope. The only signal was a
// reviewer noticing. DEF-2 named the fix as "the same build-time name-join the
// `cssTokens` guard does for CSS tokens", and this is it.
//
// THE JOIN, both directions:
//
//   view writes `x.y` → `x.y` must be classified (durable set, or the
//                        ephemeral roster below)
//   durable holds `x.y` → some view must actually read or write `x.y`
//
// The second direction matters because `IMPORTED_KEYS` is derived from the
// durable set: a name that no view uses puts a dead key in the one-time
// `localStorage` import forever.
//
// WHAT IT CANNOT DO. It cannot know INTENT — no scan can tell whether a new
// pref was meant to survive a machine change. What it can do is make the
// choice explicit: a new pref name fails this test until someone puts it in
// one of the two rosters, which is the moment the question gets asked. That is
// the same bargain `keepAlivePolls` strikes.
//
// The ephemeral roster lives HERE rather than in `uiState.ts` so there is
// exactly one list per class in the repo: the durable set ships (the routing
// reads it), the ephemeral one is a test fixture, and `uiState.ts`'s module
// docs point at this file instead of restating the names.
//
// The walk, the CRLF-stripping read and the repo root come from `./repoFiles`.

import { join } from 'node:path';
import { describe, expect, test } from 'vitest';

import { DURABLE_VIEW_PREFS } from '../src/lib/uiState';
import { read, rel, REPO_ROOT, walk } from './repoFiles';

const SRC = join(REPO_ROOT, 'src');

/**
 * `<view>.<key>` names that are deliberately NOT durable, with the reason.
 *
 * Per-keystroke or unbounded-growth values whose staleness is by design: they
 * stay in `localStorage`, never reach the backend, and are not worth carrying
 * between machines. `no stale rows` below fails if one stops being written, so
 * a row cannot outlive the code it describes.
 */
const EPHEMERAL_VIEW_PREFS = new Map<string, string>([
  ['diff.expanded', 'which file rows are open in the Diff pane — a working-set detail of one sitting'],
  ['diff.full-view', 'which files are showing the whole file rather than hunks — same sitting-scoped set'],
  ['worktrees.expanded-diff', "the Worktrees pane's open rows — the same thing for worktrees"],
  ['worktrees.full-diff', 'whole-file toggles in the Worktrees pane'],
  ['session-commits.expanded', 'which commit rows are unfolded'],
  ['git-graph.selected', 'the selected commit — points at a hash that may not exist on another machine'],
  ['timeline.open-diff', 'which checkpoint diff is open — a checkpoint id, local to this project copy'],
  ['code-audit.text', 'the free-text filter box, written on every keystroke'],
  ['code-quality.text', 'the same filter box on the quality panel'],
]);

/** `saveViewString('diff', 'view-mode', …)` / `loadViewSet(view, 'expanded')`. */
const CALL = /\b(?:save|load)View(?:String|Set)\s*\(\s*([^,]+?)\s*,\s*'([^']+)'/g;

/** `view="code-audit"` on a `<Component …>` mount. */
function mountedViewNames(component: string, all: readonly string[]): string[] {
  const tag = new RegExp(`<${component}\\b[^>]*\\bview="([^"]+)"`, 'g');
  const out = new Set<string>();
  for (const file of all) {
    for (const m of read(file).matchAll(tag)) out.add(m[1]);
  }
  return [...out];
}

const FILES = walk(SRC, ['.svelte', '.ts']).filter(
  (f) => !f.endsWith('.test.ts') && !f.endsWith('viewSection.ts') && !f.endsWith('uiState.ts'),
);

interface Use {
  name: string;
  where: string;
}

const USES: Use[] = [];
/** Call sites whose `view` argument is an identifier that resolved to nothing. */
const UNRESOLVED: string[] = [];

for (const file of FILES) {
  const src = read(file);
  const where = rel(file);
  const component = where.split('/').pop()!.replace(/\.(svelte|ts)$/, '');
  for (const m of src.matchAll(CALL)) {
    const [, viewArg, key] = m;
    const literal = /^'([^']+)'$/.exec(viewArg.trim());
    if (literal) {
      USES.push({ name: `${literal[1]}.${key}`, where });
      continue;
    }
    // A component that takes its `view` as a prop (`AuditPanel`): resolve it
    // to every value it is actually mounted with, so both panels' prefs are
    // checked rather than neither.
    const names = mountedViewNames(component, FILES);
    if (names.length === 0) {
      UNRESOLVED.push(`${where}: ${viewArg.trim()}, ${key}`);
      continue;
    }
    for (const v of names) USES.push({ name: `${v}.${key}`, where });
  }
}

const USED = new Set(USES.map((u) => u.name));

describe('durable view prefs', () => {
  test('the scan actually sees the call sites (vacuity guard)', () => {
    // Twenty-odd at the time of writing across eight files. A scan that found
    // none would report a clean join over an empty set.
    expect(USES.length, 'no view-pref call site found — the scan is broken').toBeGreaterThan(12);
    expect(
      new Set(USES.map((u) => u.where)).size,
      'the call sites all came from one file — the walk is not walking',
    ).toBeGreaterThan(4);
    // The prop-driven case is the one a naive scan drops silently.
    expect(
      USED.has('code-audit.severity') && USED.has('code-quality.severity'),
      "AuditPanel's `view` prop did not resolve to both panels",
    ).toBe(true);
  });

  test('every `view` argument resolves', () => {
    expect(
      UNRESOLVED,
      'These call sites pass a `view` this scan could not resolve to a name, so their prefs ' +
        'are unchecked. Pass a literal, or mount the component with `view="…"`:\n' +
        UNRESOLVED.join('\n'),
    ).toEqual([]);
  });

  test('every pref a view writes is classified durable or ephemeral', () => {
    const unclassified = [...new Set(USES.filter((u) => !DURABLE_VIEW_PREFS.has(u.name)))]
      .filter((u) => !EPHEMERAL_VIEW_PREFS.has(u.name))
      .map((u) => `${u.name} (${u.where})`);
    expect(
      unclassified,
      'These per-view prefs are in neither roster. A name missing from DURABLE_VIEW_PREFS ' +
        'routes to `localStorage` silently — the value persists, per machine, in the wrong ' +
        'store — so the choice has to be made rather than defaulted into. Add it to ' +
        '`DURABLE_VIEW_PREFS` in src/lib/uiState.ts if it should follow the project, or to ' +
        'EPHEMERAL_VIEW_PREFS here with the reason if it should not:\n' + unclassified.join('\n'),
    ).toEqual([]);
  });

  test('every durable pref is one a view actually uses', () => {
    const orphans = [...DURABLE_VIEW_PREFS].filter((name) => !USED.has(name));
    expect(
      orphans,
      'These are in DURABLE_VIEW_PREFS but no view reads or writes them. `IMPORTED_KEYS` is ' +
        'derived from that set, so an orphan is a dead key in the one-time `localStorage` ' +
        'import forever — drop the row:\n' + orphans.join('\n'),
    ).toEqual([]);
  });

  test('the ephemeral roster has no stale rows', () => {
    const stale = [...EPHEMERAL_VIEW_PREFS.keys()].filter((name) => !USED.has(name));
    expect(
      stale,
      'These are listed as deliberately ephemeral but nothing uses them any more — drop the ' +
        'row, so it stops excusing whatever lands on that name next:\n' + stale.join('\n'),
    ).toEqual([]);
    for (const [name, reason] of EPHEMERAL_VIEW_PREFS) {
      expect(reason, `${name} is excused with no reason given`).not.toBe('');
    }
  });
});

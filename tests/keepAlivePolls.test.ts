// Guard: a periodic job in the frontend must say how it stops.
//
// THE RULE (standing, and the reason `pollWhileVisible` exists). An app view
// stays MOUNTED for the app's lifetime once created (appViews.ts) — hiding its
// tab detaches the host, it does not destroy the component. So a bare
// `setInterval` in one keeps firing forever after the tab has been opened
// once, burning IPC against a view nobody is looking at. `EventsView` calls it
// "a known bug class here" in its own comment, because it was one.
//
// WHAT THIS CHECKS, and what it cannot. Every bare `setInterval` call site has
// to be ACCOUNTED FOR: by the file naming `isAppViewVisible` (it gates the
// interval itself), or by a row here saying why the rule does not bind. It
// cannot tell whether a gate is CORRECT — only that the author met the rule or
// wrote down why it does not apply. That is the failure mode worth catching:
// the poll nobody thought about, rather than the poll thought about wrongly.
//
// TWO HOLES THIS USED TO HAVE (V42 Phase-F review, F-7):
//
//  1. **The excuse was per FILE, not per call site.** `src.includes(...)` — so
//     one `pollWhileVisible` call, or one mention of `isAppViewVisible`,
//     excused every OTHER interval in the same file. A component that polls
//     correctly through the helper and then starts a second, bare interval
//     three functions down was green. `pollWhileVisible` now buys NOTHING for
//     a bare call site: the helper accounts for the interval it starts inside
//     itself, not for one the component starts on its own. `isAppViewVisible`
//     accounts for at most ONE — the one it plausibly gates.
//  2. **It walked `.svelte` only.** A poll started in an imported `.ts` module
//     runs on exactly the same schedule and lives exactly as long; there was
//     nothing to notice it. Five `.ts` files start one today, and each now
//     carries its reason.
//
// `.test.ts` files are out of scope: an interval there runs under vitest, on
// fake timers as often as not, and never in the app.
//
// The walk, the CRLF-stripping read and the repo root come from `./repoFiles`
// — see the note there.

import { join } from 'node:path';
import { describe, expect, test } from 'vitest';

import { read, rel, REPO_ROOT, walk } from './repoFiles';

const SRC = join(REPO_ROOT, 'src');

/**
 * Files whose interval is not an app-view poll, each with the reason.
 *
 * `no stale rows` below fails if one of these stops starting an interval, so a
 * row cannot outlive the code it excuses. A new row is the wrong first move:
 * for a view under `appViews.ts` the answer is `pollWhileVisible`.
 */
const NOT_AN_APP_VIEW_POLL = new Map<string, string>([
  [
    'src/lib/GraphView.svelte',
    'gates on its OWN visibility signal instead: it is a SECTION inside the Tool Activity tab, and it already pauses on an IntersectionObserver + document.visibilityState (`visible`), which is stricter than the tab-level one — it also stops while the section is scrolled out or the window is hidden',
  ],
  [
    'src/lib/NoteView.svelte',
    'per-tab component, destroyed with its tab — the interval is the note autosave flush, and there is no app view to be detached from',
  ],
  [
    'src/lib/status/UsageMeter.svelte',
    'status-bar clock: it re-reads `Date.now()` to age a label, touches no IPC, and lives in the always-visible bottom bar',
  ],
  [
    'src/SettingsApp.svelte',
    'the Settings WINDOW, not an app view — it has no keep-alive host, and closing it destroys the component',
  ],
  [
    'src/lib/appViewVisibility.ts',
    'IS the helper. This interval is the one inside `pollWhileVisible`, the gate every other row here is measured against',
  ],
  [
    'src/lib/delegationState.ts',
    "a Svelte `readable`'s start function: the interval exists only while something is subscribed and is torn down on the last unsubscribe. Its only subscriber is the delegation banner, which renders for a pane's ACTIVE tab — unmounted rather than detached, so there is no app view to gate on",
  ],
  [
    'src/lib/latch.ts',
    'app-wide on purpose: `startLatchPolling` is started once by `App.svelte` and stops with the window. What it feeds — the per-tab taint badge and the injection-status chip — lives in the tab bar and the status bar, which are on screen whatever view is',
  ],
  [
    'src/lib/spritePlayer.ts',
    "the avatar's sprite-rotation timer, cleared on every animation switch and in `destroy()`. It advances a canvas in the always-present avatar overlay and touches no IPC",
  ],
  [
    'src/lib/workbenchDiff.ts',
    'reference-counted watcher: the interval runs only while something is watching (the Diff view, or the status-bar badge) and is cleared when the last watcher leaves — and it self-skips each tick while the graph watcher is running, which is the case it exists as a fallback for',
  ],
]);

/** Bare `setInterval(` call sites. `ReturnType<typeof setInterval>` is a type. */
function bareIntervalCount(src: string): number {
  return (src.match(/(?<!typeof )\bsetInterval\s*\(/g) ?? []).length;
}

/** `pollWhileVisible(` call sites — the gated way to start one. */
function helperCallCount(src: string): number {
  return (src.match(/\bpollWhileVisible\s*\(/g) ?? []).length;
}

interface Scanned {
  path: string;
  bare: number;
  helper: number;
  /** The file gates an interval itself, rather than through the helper. */
  selfGates: boolean;
}

const SCANNED: Scanned[] = walk(SRC, ['.svelte', '.ts'])
  .filter((f) => !f.endsWith('.test.ts'))
  .map((f) => {
    const src = read(f);
    return {
      path: rel(f),
      bare: bareIntervalCount(src),
      helper: helperCallCount(src),
      selfGates: src.includes('isAppViewVisible'),
    };
  })
  .filter((s) => s.bare > 0 || s.helper > 0);

describe('keep-alive polls', () => {
  test('the scan actually sees the components (vacuity guard)', () => {
    // Fourteen at the time of writing: five calling `pollWhileVisible` and nine
    // excused below. The floor's job is to make "the walk found nothing" fail
    // here rather than read as a clean bill of health.
    expect(SCANNED.length, 'nothing starts a poll — the scan is broken').toBeGreaterThan(10);
    expect(
      SCANNED.filter((s) => s.helper > 0).length,
      'nothing uses the helper — the scan is looking at the wrong tree',
    ).toBeGreaterThan(3);
    // The `.ts` half is the half that was missing; a walk that quietly stopped
    // reading it would take five excused polls out of scope with it.
    expect(
      SCANNED.filter((s) => s.path.endsWith('.ts')).length,
      'no `.ts` file is in the scan — the extension list has regressed to `.svelte` only',
    ).toBeGreaterThan(2);
  });

  test('every interval call site is gated, or excused with a reason', () => {
    const unaccounted = SCANNED.filter(({ path, bare, selfGates }) => {
      if (bare === 0) return false;
      if (NOT_AN_APP_VIEW_POLL.has(path)) return false;
      // `isAppViewVisible` accounts for ONE call site — the one it gates.
      // `pollWhileVisible` accounts for none: it gates the interval it starts
      // inside itself, and says nothing about one the component starts.
      return bare > (selfGates ? 1 : 0);
    }).map(({ path, bare, helper, selfGates }) =>
      helper > 0 && !selfGates
        ? `${path} — ${bare} bare setInterval alongside ${helper} pollWhileVisible call(s): the helper does not cover them`
        : `${path} — ${bare} bare setInterval, ${selfGates ? 'and only one is accounted for by `isAppViewVisible`' : 'no visibility gate'}`,
    );
    expect(
      unaccounted,
      'These start an interval with nothing accounting for it. An app view stays mounted ' +
        'when its tab is hidden, so it keeps running forever once the tab has been opened: ' +
        'use `pollWhileVisible(tabId, tick, ms)` from `lib/appViewVisibility.ts`. If the ' +
        'file is not an app view, add a row to NOT_AN_APP_VIEW_POLL with the reason:\n' +
        unaccounted.join('\n'),
    ).toEqual([]);
  });

  test('the excuse list has no stale rows', () => {
    const starting = new Set(SCANNED.filter((s) => s.bare > 0).map((s) => s.path));
    const stale = [...NOT_AN_APP_VIEW_POLL.keys()].filter((p) => !starting.has(p));
    expect(
      stale,
      'These are excused from the visibility gate but no longer start a bare interval — ' +
        'either the file is gone or the poll is (drop the row, so the exemption stops ' +
        'covering whatever lands there next):\n' + stale.join('\n'),
    ).toEqual([]);
    for (const [path, reason] of NOT_AN_APP_VIEW_POLL) {
      expect(reason, `${path} is excused with no reason given`).not.toBe('');
    }
  });
});

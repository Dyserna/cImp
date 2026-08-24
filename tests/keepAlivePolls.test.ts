// Guard: a periodic job in a frontend component must say how it stops.
//
// THE RULE (standing, and the reason `pollWhileVisible` exists). An app view
// stays MOUNTED for the app's lifetime once created (appViews.ts) — hiding its
// tab detaches the host, it does not destroy the component. So a bare
// `setInterval` in one keeps firing forever after the tab has been opened
// once, burning IPC against a view nobody is looking at. `EventsView` calls it
// "a known bug class here" in its own comment, because it was one.
//
// What this checks is deliberately shallow: every `.svelte` file that starts an
// interval must either route it through `pollWhileVisible`, or name
// `isAppViewVisible` itself, or carry a row here saying why neither applies.
// It cannot tell whether a gate is CORRECT — only that the author met the rule
// or wrote down why it does not bind. That is the failure mode worth catching:
// the poll nobody thought about, rather than the poll thought about wrongly.

import { join } from 'node:path';
import { describe, expect, test } from 'vitest';

import { read, rel, REPO_ROOT, walk } from './repoFiles';

const SRC = join(REPO_ROOT, 'src');

/**
 * Components whose interval is not an app-view poll, each with the reason.
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
]);

/// A component with a periodic job — started directly, or through the helper.
/// `let x: ReturnType<typeof setInterval>` is a type, not a call.
function startsAPoll(src: string): boolean {
  return /(?<!typeof )\bsetInterval\s*\(/.test(src) || /\bpollWhileVisible\s*\(/.test(src);
}

describe('keep-alive polls', () => {
  const files = walk(SRC, ['.svelte']).filter((f) => startsAPoll(read(f)));

  test('the scan actually sees the components (vacuity guard)', () => {
    // Nine at the time of writing: five that call `pollWhileVisible`, and the
    // four excused below. The floor's job is to make "the walk found nothing"
    // fail here rather than read as a clean bill of health.
    expect(files.length, 'no component starts a poll — the scan is broken').toBeGreaterThan(6);
    expect(
      files.filter((f) => read(f).includes('pollWhileVisible')).length,
      'no component uses the helper — the scan is looking at the wrong tree',
    ).toBeGreaterThan(3);
  });

  test('every interval is gated, or excused with a reason', () => {
    const ungated = files
      .map((f) => [rel(f), read(f)] as const)
      .filter(([, src]) => !src.includes('pollWhileVisible') && !src.includes('isAppViewVisible'))
      .filter(([path]) => !NOT_AN_APP_VIEW_POLL.has(path))
      .map(([path]) => path);
    expect(
      ungated,
      'These components start an interval with no visibility gate. An app view stays mounted ' +
        'when its tab is hidden, so this keeps running forever once the tab has been opened: ' +
        'use `pollWhileVisible(tabId, tick, ms)` from `lib/appViewVisibility.ts`. If the ' +
        'component is not an app view, add a row to NOT_AN_APP_VIEW_POLL with the reason:\n' +
        ungated.join('\n'),
    ).toEqual([]);
  });

  test('the excuse list has no stale rows', () => {
    const starting = new Set(files.map(rel));
    const stale = [...NOT_AN_APP_VIEW_POLL.keys()].filter((p) => !starting.has(p));
    expect(
      stale,
      'These are excused from the visibility gate but no longer start a poll — either ' +
        'the file is gone or the poll is (drop the row, so the exemption stops covering ' +
        'whatever lands there next):\n' + stale.join('\n'),
    ).toEqual([]);
    for (const [path, reason] of NOT_AN_APP_VIEW_POLL) {
      expect(reason, `${path} is excused with no reason given`).not.toBe('');
    }
  });
});

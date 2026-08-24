import { describe, expect, test } from 'vitest';

import { createDraftSync } from './draftSync';

// A stand-in for `Settings`: three independent fields, one per edit in the
// burst, so "which edit survived" is readable off the final object. The shape
// does not matter — the module is generic and the race is about ORDERING.
type Draft = { a: number; b: number; c: number; external: number };

const ZERO: Draft = { a: 0, b: 0, c: 0, external: 0 };

/**
 * The Settings window's two moving parts, wired exactly as `SettingsApp.svelte`
 * wires them, with every asynchronous step under the test's control:
 *
 * - `patch()` clones the draft, mutates, assigns, and pushes the WHOLE struct
 *   (`settings_update` is a wholesale replace — that is why a regressed draft
 *   destroys a previous edit in the backend too);
 * - `settle(i)` resolves push `i` (the backend applies it at that moment);
 * - `echo(i)` delivers the `settings-changed` broadcast carrying push `i`'s
 *   state — deliberately decoupled from `settle`, because in the live failure
 *   the broadcast of an OLDER push arrived after a newer patch;
 * - `external(state)` delivers a broadcast that is not an echo of ours (a main
 *   window edit).
 *
 * `gated: false` reproduces the pre-fix component: the subscription replaces
 * the draft unconditionally.
 */
function settingsWindow(gated: boolean) {
  let draft: Draft = structuredClone(ZERO);
  let backend: Draft = structuredClone(ZERO);
  const pushed: Draft[] = [];
  const settlers: Array<() => void> = [];

  const sync = createDraftSync<Draft>((s) => {
    draft = structuredClone(s);
  });

  return {
    get draft(): Draft {
      return draft;
    },
    get backend(): Draft {
      return backend;
    },
    get outstanding(): number {
      return sync.outstanding;
    },
    patch(updater: (s: Draft) => void): void {
      const next = structuredClone(draft);
      updater(next);
      draft = next;
      pushed.push(structuredClone(next));
      const done = sync.beginPush();
      settlers.push(() => {
        // `settings_update` replaces the whole struct.
        backend = structuredClone(next);
        done();
      });
    },
    settle(i: number): void {
      settlers[i]();
    },
    settleAll(): void {
      for (const s of settlers) s();
    },
    echo(i: number): void {
      this.receive(pushed[i]);
    },
    receive(state: Draft): void {
      if (gated) {
        sync.broadcast(state);
      } else {
        draft = structuredClone(state); // the pre-fix subscription
      }
    },
  };
}

/** The exact interleaving observed live, in both orderings that matter. */
function runBurst(w: ReturnType<typeof settingsWindow>): void {
  w.patch((s) => (s.a = 1));
  w.patch((s) => (s.b = 1));
  w.echo(0); // the echo of A lands while B is still in flight
  w.patch((s) => (s.c = 1));
  w.echo(1);
  w.echo(2);
  w.settleAll();
}

describe('settings draft sync', () => {
  // The regression guard. Every one of A, B and C must survive in BOTH the
  // draft the user is looking at and the state the backend ends up holding —
  // the second half is the part that made the live loss permanent.
  test('a burst of edits keeps every edit in the draft and in the backend', () => {
    const w = settingsWindow(true);
    runBurst(w);
    expect(w.draft).toEqual({ a: 1, b: 1, c: 1, external: 0 });
    expect(w.backend).toEqual({ a: 1, b: 1, c: 1, external: 0 });
    expect(w.outstanding).toBe(0);
  });

  // The same sequence against the old semantics, so the test above is known to
  // discriminate rather than to pass vacuously: the echo of A regresses the
  // draft, `patch(C)` clones the regressed draft, and the wholesale push wipes
  // B out of the backend as well.
  test('the pre-fix unconditional replace loses the middle edit', () => {
    const w = settingsWindow(false);
    runBurst(w);
    expect(w.draft.b).toBe(0);
    expect(w.backend.b).toBe(0);
  });

  // Load-bearing for cross-window sync: with nothing in flight the broadcast
  // still replaces the draft, exactly as before the fix.
  test('an external broadcast replaces the draft while idle', () => {
    const w = settingsWindow(true);
    w.patch((s) => (s.a = 1));
    w.settle(0);
    w.receive({ a: 1, b: 0, c: 0, external: 7 });
    expect(w.draft.external).toBe(7);
    expect(w.draft.a).toBe(1);
  });

  // An external change that arrives mid-burst is not dropped — it is buffered
  // and adopted when the last push settles, so the draft converges with the
  // store instead of diverging from it.
  test('an external broadcast during a burst is applied once the burst ends', () => {
    const w = settingsWindow(true);
    w.patch((s) => (s.a = 1));
    w.patch((s) => (s.b = 1));
    w.receive({ a: 1, b: 1, c: 0, external: 9 });
    expect(w.draft.external).toBe(0); // suppressed while in flight
    w.settleAll();
    expect(w.draft.external).toBe(9);
  });

  // Only the LAST suppressed broadcast is adopted: an older echo buffered
  // earlier in the burst must not resurface on the idle edge.
  test('only the latest suppressed broadcast is adopted on becoming idle', () => {
    const w = settingsWindow(true);
    w.patch((s) => (s.a = 1));
    w.patch((s) => (s.b = 1));
    w.echo(0); // stale
    w.echo(1); // current
    w.settleAll();
    expect(w.draft).toEqual({ a: 1, b: 1, c: 0, external: 0 });
  });

  // The gate must never wedge shut. A push whose promise settles twice (wired
  // through both `.then` and `.catch`, say) must not over-count acks, and a
  // failed push still closes its window.
  test('the gate reopens exactly once per push', () => {
    let adopted = 0;
    const sync = createDraftSync<Draft>(() => {
      adopted += 1;
    });
    const first = sync.beginPush();
    const second = sync.beginPush();
    expect(sync.outstanding).toBe(2);
    first();
    first(); // duplicate settle — ignored
    expect(sync.outstanding).toBe(1);
    expect(sync.broadcast(ZERO)).toBe(false);
    expect(adopted).toBe(0);
    second();
    expect(sync.outstanding).toBe(0);
    expect(adopted).toBe(1); // the buffered broadcast, once
    expect(sync.broadcast(ZERO)).toBe(true);
    expect(adopted).toBe(2);
  });
});

/// ── The gate's OTHER half: every push of the draft must register ─────────
///
/// The tests above pin the mechanism. This one pins the WIRING, because the
/// mechanism protects nothing that does not call it — and #129's extraction
/// audit found exactly that hole: `commitGraphIgnore` pushed the whole draft
/// through `applySettings` without `beginPush()`, so a broadcast landing
/// mid-push could replace the draft and take the ignore rows the user had just
/// typed with it (they are edited IN PLACE through `ArrayEditor`'s bind, so
/// there is no second copy to restore them from).
///
/// A comment saying "register your push" is the kind of contract this codebase
/// has repeatedly re-learned gets violated by someone who never read it, so the
/// rule is enforced here instead: in `SettingsApp.svelte`, every top-level
/// function that calls `applySettings` must also call `draftSync.beginPush()`,
/// with one named exemption below.
///
/// Read through Vite's own glob rather than `node:fs` — the app's tsconfig has
/// no node types, and the emptiness assertions below fail loudly if this ever
/// resolves to the wrong tree.
const WINDOW_SOURCES = import.meta.glob(['/src/SettingsApp.svelte'], {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

/**
 * The one sanctioned unregistered push, with the reason it is one.
 *
 * `resetSettingsToDefaults` does not push the DRAFT — it pushes
 * `defaultSettings()` and never assigns `snapshot`, deliberately letting the
 * echo bring the draft to defaults. Registering it would not help and would
 * mislead: the gate suppresses broadcasts so the draft can win, and here there
 * is no draft that should win. Making the reset gate-safe means having it
 * assign `snapshot` as `patch()` does, which is a behaviour change to the
 * reset, not a wiring fix — filed rather than smuggled in.
 *
 * Adding a row here is a review decision, not a formality.
 */
const UNREGISTERED_PUSHES = new Map<string, string>([
  [
    'resetSettingsToDefaults',
    'pushes defaultSettings(), not the draft, and never assigns snapshot — the echo is meant to replace the draft',
  ],
]);

/** Every top-level `function name(…) { … }` block in the component's script. */
function topLevelFunctions(src: string): Map<string, string> {
  const out = new Map<string, string>();
  // Top-level declarations sit at two-space indent and close on a line that is
  // exactly `  }`; every nested closer is indented further.
  const re = /^ {2}(?:async )?function (\w+)[\s\S]*?^ {2}\}$/gm;
  for (const m of src.matchAll(re)) out.set(m[1], m[0]);
  return out;
}

describe('every settings push registers with the draft-sync gate', () => {
  const path = '/src/SettingsApp.svelte';
  const src = WINDOW_SOURCES[path];

  test('the settings window source is actually in the scan', () => {
    expect(src, `${path} did not resolve — the scan is looking at the wrong tree`).toBeTypeOf(
      'string',
    );
    expect(src).toContain('draftSync.beginPush()');
  });

  test('every function that calls applySettings also opens a push window', () => {
    const fns = topLevelFunctions(src);
    // Under-parse guard: a regex that matched nothing would make this test
    // pass vacuously, which is the failure mode of every source scan.
    expect(fns.has('patch'), `parsed ${fns.size} top-level functions: ${[...fns.keys()]}`).toBe(
      true,
    );

    const pushers = [...fns].filter(([, body]) => /\bapplySettings\(/.test(body));
    // `patch`, `applyMcpRegistry`, `commitGraphIgnore`, `resetSettingsToDefaults`.
    expect(pushers.length).toBeGreaterThanOrEqual(4);

    const unregistered = pushers
      .filter(([, body]) => !body.includes('draftSync.beginPush()'))
      .map(([name]) => name);
    expect(unregistered.filter((n) => !UNREGISTERED_PUSHES.has(n))).toEqual([]);
  });

  test('commitGraphIgnore in particular takes the gate (#129)', () => {
    const body = topLevelFunctions(src).get('commitGraphIgnore');
    expect(body, 'commitGraphIgnore is no longer a top-level function of the window').toBeTypeOf(
      'string',
    );
    expect(body).toContain('draftSync.beginPush()');
    // …and settles it in either direction, or the gate wedges shut.
    expect(body).toMatch(/\.finally\(settled\)/);
  });

  test('the exemption list has no stale rows', () => {
    const fns = topLevelFunctions(src);
    for (const [name] of UNREGISTERED_PUSHES) {
      const body = fns.get(name);
      expect(body, `${name} is exempted but no longer exists`).toBeTypeOf('string');
      expect(body).toMatch(/\bapplySettings\(/);
      expect(body).not.toContain('draftSync.beginPush()');
    }
  });
});

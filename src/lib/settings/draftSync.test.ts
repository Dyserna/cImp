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
    /**
     * `resetSettingsToDefaults` — the hard reset. It pushes the DEFAULTS
     * rather than a mutation of the draft, but is otherwise shaped like
     * `patch()`: assign the draft, then push it under the gate.
     *
     * `likePatch: false` reproduces the pre-fix reset, which pushed the
     * defaults, left the draft alone (relying on the echo to bring it down)
     * and — precisely because it never assigned the draft — did not register
     * the push either. The two halves went together, and so does the fix.
     */
    reset(likePatch = true): void {
      const next = structuredClone(ZERO); // `defaultSettings()`, a fresh clone per call
      if (likePatch) draft = next;
      pushed.push(structuredClone(next));
      const done = likePatch ? sync.beginPush() : () => {};
      settlers.push(() => {
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

  // ── The reset (#129) ────────────────────────────────────────────────────
  //
  // The reset pushes the defaults. Until it assigned the draft too, there was
  // a window between its push and its echo in which the draft still held the
  // PRE-reset settings — and an edit made in that window clones the draft and
  // pushes it wholesale, so every setting the user had just reset comes back,
  // in the backend as well as on screen. Same lost-update shape as the burst,
  // with the reset playing the part of the edit that gets undone.
  test('an edit right after a reset does not resurrect the pre-reset settings', () => {
    const w = settingsWindow(true);
    w.patch((s) => (s.a = 1)); // the setting the user is about to reset away
    w.settleAll();
    w.echo(0);
    expect(w.draft.a).toBe(1);

    w.reset();
    expect(w.draft).toEqual(ZERO); // the draft is at defaults IMMEDIATELY
    w.patch((s) => (s.b = 1)); // …so an edit before the reset's echo builds on defaults
    w.echo(1); // the reset's echo
    w.echo(2);
    w.settleAll();

    expect(w.draft).toEqual({ a: 0, b: 1, c: 0, external: 0 });
    expect(w.backend).toEqual({ a: 0, b: 1, c: 0, external: 0 });
  });

  // The discriminating control: the pre-fix reset, on the same interleaving.
  // `a` is back — in the draft the user is looking at and, because the push is
  // wholesale, in the backend for good.
  test('the pre-fix reset lets the following edit undo the reset', () => {
    const w = settingsWindow(true);
    w.patch((s) => (s.a = 1));
    w.settleAll();
    w.echo(0);

    w.reset(false); // pushes defaults, leaves the draft alone
    expect(w.draft.a).toBe(1); // still pre-reset — this is the hole
    w.patch((s) => (s.b = 1)); // clones the stale draft…
    w.echo(1);
    w.echo(2);
    w.settleAll();

    // …and the wholesale push puts `a` back, permanently.
    expect(w.draft.a).toBe(1);
    expect(w.backend.a).toBe(1);
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
/// rule was enforced here instead: every top-level function of
/// `SettingsApp.svelte` that called `applySettings` had to call
/// `draftSync.beginPush()` too.
///
/// **The V42 tranche-2 review (T2-6) found the scope of that wrong**, and the
/// rule below is its replacement. `applySettings` is importable by all
/// twenty-one section children, while the scan read exactly ONE file — so the
/// hole it was built to catch could be reopened in any of the other twenty-one
/// and the guard would have gone on passing. The fix is to make the pair
/// inseparable rather than to scan more files for it: `pushDraft` is the gate
/// and the push in one call, and nothing under `src/lib/settings/` — nor the
/// window itself — may import `applySettings` to assemble its own.
///
/// So there are two checks now, and the second is the load-bearing one:
///
/// 1. every top-level function of the window that pushes the draft does it
///    through `pushDraft` (and none of them touches `applySettings`);
/// 2. no source under `src/lib/settings/`, and not `SettingsApp.svelte`,
///    IMPORTS `applySettings` at all — `store.ts` defines it and `draftSync.ts`
///    is the gate that wraps it, and those two are the whole allowlist.
///
/// Read through Vite's own glob rather than `node:fs` — the app's tsconfig has
/// no node types, and the emptiness assertions below fail loudly if this ever
/// resolves to the wrong tree.
const WINDOW_SOURCES = import.meta.glob(['/src/SettingsApp.svelte'], {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

/// Every settings source the import ban covers: the twenty-one sections, the
/// editors they share, this directory's own modules — and the window, which
/// owns the draft and is therefore the one file that used to need it.
const SETTINGS_SOURCES = import.meta.glob(
  ['/src/lib/settings/**/*.ts', '/src/lib/settings/**/*.svelte', '/src/SettingsApp.svelte'],
  { query: '?raw', import: 'default', eager: true },
) as Record<string, string>;

/// The two files that may name `applySettings` in an import: the module that
/// DEFINES it, and the gate that is the only sanctioned way to call it.
const APPLY_SETTINGS_ALLOWED = new Set([
  '/src/lib/settings/store.ts',
  '/src/lib/settings/draftSync.ts',
]);

/**
 * Sanctioned RAW pushes — a function that calls `applySettings` itself instead
 * of `pushDraft` — each with the reason it is one. Currently NONE, and a row is
 * a review decision rather than a formality.
 *
 * It held exactly one row for a while: `resetSettingsToDefaults`, which pushed
 * `defaultSettings()` and never assigned `snapshot`, letting the echo bring the
 * draft to defaults. That was not gate-safe, it was gate-SHAPED — the reset
 * left the pre-reset draft standing until the echo landed, and an edit made in
 * that window cloned it and pushed it wholesale, undoing the reset. The fix was
 * the behaviour change the row said it would take: the reset now assigns the
 * draft and pushes it through the gate exactly as `patch()` does, so the row is
 * gone and `the reset assigns the draft and takes the gate (#129)` below pins
 * the new shape.
 */
const UNGATED_PUSHES = new Map<string, string>();

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
    expect(src).toContain('pushDraft(draftSync');
  });

  test('every function that pushes the draft pushes it through pushDraft', () => {
    const fns = topLevelFunctions(src);
    // Under-parse guard: a regex that matched nothing would make this test
    // pass vacuously, which is the failure mode of every source scan.
    expect(fns.has('patch'), `parsed ${fns.size} top-level functions: ${[...fns.keys()]}`).toBe(
      true,
    );

    // `patch`, `applyMcpRegistry`, `commitGraphIgnore`, `resetSettingsToDefaults`.
    const gated = [...fns].filter(([, body]) => /\bpushDraft\(/.test(body));
    expect(
      gated.length,
      `only ${gated.length} function(s) push the draft — the window has four, so either the ` +
        'parse has drifted or a push has found another way out',
    ).toBeGreaterThanOrEqual(4);

    const raw = [...fns]
      .filter(([, body]) => /\bapplySettings\(/.test(body))
      .map(([name]) => name);
    expect(
      raw.filter((n) => !UNGATED_PUSHES.has(n)),
      'these functions push the draft with a raw `applySettings` instead of `pushDraft`, so ' +
        'the lost-update gate is theirs to remember and one day one of them will not',
    ).toEqual([]);
  });

  test('commitGraphIgnore in particular takes the gate (#129)', () => {
    const body = topLevelFunctions(src).get('commitGraphIgnore');
    expect(body, 'commitGraphIgnore is no longer a top-level function of the window').toBeTypeOf(
      'string',
    );
    // One call now carries both halves — registering the push and settling it
    // in either direction (`pushDraft`'s `.finally`), which is what stops the
    // gate wedging shut on a rejected push.
    expect(body).toMatch(/\bpushDraft\(draftSync,/);
  });

  test('the reset assigns the draft and takes the gate (#129)', () => {
    const body = topLevelFunctions(src).get('resetSettingsToDefaults');
    expect(body, 'resetSettingsToDefaults is no longer a top-level function of the window').toBeTypeOf(
      'string',
    );
    // The two halves of the fix, and both are load-bearing: pushing through the
    // gate without assigning the draft would leave the pre-reset state standing
    // (the gate would then actively DEFEND it against the reset's own echo),
    // and assigning without the gate re-opens the burst race.
    expect(body, 'the reset must assign the draft, not wait for its own echo').toMatch(
      /\bsnapshot = \w+/,
    );
    expect(body).toMatch(/\bpushDraft\(draftSync,/);
    // …and it is still the DEFAULTS that get assigned and pushed, not the draft.
    expect(body).toMatch(/=\s*defaultSettings\(\)/);
  });

  test('the exemption list has no stale rows', () => {
    const fns = topLevelFunctions(src);
    for (const [name, reason] of UNGATED_PUSHES) {
      const body = fns.get(name);
      expect(body, `${name} is exempted but no longer exists`).toBeTypeOf('string');
      expect(body).toMatch(/\bapplySettings\(/);
      expect(body).not.toMatch(/\bpushDraft\(/);
      expect(reason, `${name} is exempted with no reason given`).not.toBe('');
    }
    // The list is empty today, so the loop above asserts nothing — and a test
    // that asserts nothing is not a test. Adding a row is meant to be a
    // deliberate act with a reviewer behind it, so it has to come through this
    // line: state the roster, and let a silent new exemption fail here.
    expect(
      [...UNGATED_PUSHES.keys()],
      'a push was exempted from the draft-sync gate — that is a review decision: ' +
        'name it here with the reason it cannot go through `pushDraft`',
    ).toEqual([]);
  });

  // ── The check that made the one above stop being enough (T2-6) ───────────

  test('the settings tree is actually in the import scan', () => {
    const files = Object.keys(SETTINGS_SOURCES);
    // ~30 files: 21 sections, the shared editors, this directory's modules and
    // the window. A glob that resolved nothing would make the ban vacuous.
    expect(
      files.length,
      `the settings glob resolved ${files.length} file(s) — it is looking at the wrong tree`,
    ).toBeGreaterThan(20);
    for (const allowed of APPLY_SETTINGS_ALLOWED) {
      expect(files, `${allowed} is allowlisted but not in the scan`).toContain(allowed);
    }
    // …and the allowlisted files must actually be the ones that name it, or the
    // allowlist has outlived its reason.
    expect(SETTINGS_SOURCES['/src/lib/settings/store.ts']).toContain(
      'export async function applySettings',
    );
    expect(SETTINGS_SOURCES['/src/lib/settings/draftSync.ts']).toContain(
      "import { applySettings } from './store'",
    );
  });

  test('no settings source imports applySettings except the store and the gate', () => {
    // Import statements only: the identifier appears in plenty of PROSE here
    // and in the components, and a comment explaining the rule must not break
    // it. Both the named form and a namespace import of the store count — the
    // latter would reach `applySettings` through the namespace.
    const offenders: string[] = [];
    for (const [file, source] of Object.entries(SETTINGS_SOURCES)) {
      if (APPLY_SETTINGS_ALLOWED.has(file)) continue;
      for (const m of source.matchAll(/import\s+([\s\S]*?)\s+from\s+['"]([^'"]+)['"]/g)) {
        const [, bindings, specifier] = m;
        const named = /\{[\s\S]*\bapplySettings\b[\s\S]*\}/.test(bindings);
        const namespaced = /\*\s+as\s+\w+/.test(bindings) && /(^|\/)store$/.test(specifier);
        if (named || namespaced) offenders.push(`${file}  (from '${specifier}')`);
      }
    }
    expect(
      offenders,
      'these files import `applySettings` directly, which lets them push the settings draft ' +
        'without the lost-update gate. Push through `pushDraft(draftSync, next)` instead — or, ' +
        'in a section child, through the callback the window passes down:\n' +
        offenders.join('\n'),
    ).toEqual([]);
  });
});

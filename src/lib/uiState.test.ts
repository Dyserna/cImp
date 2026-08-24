import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';

/// V42 Phase C — the per-project UI-state cache and the one-time
/// `localStorage` import.
///
/// This whole path had ZERO test coverage before the move: not one test
/// referenced any of the 20 durable keys. The properties asserted here are
/// the ones the old `localStorage` implementation got for free and the new one
/// has to earn — synchronous reads before first paint, a write that cannot
/// outrun its own cache, an import that never deletes the only copy of a
/// user's state, and a failure posture where losing view state never surfaces
/// as an error.

const invoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke: (...a: unknown[]) => invoke(...a) }));

/// A minimal `localStorage` stand-in — vitest runs in the node environment,
/// so there is no DOM one.
class FakeStorage {
  map = new Map<string, string>();
  throwOn: 'none' | 'get' = 'none';
  getItem(k: string): string | null {
    if (this.throwOn === 'get') throw new Error('storage unavailable');
    return this.map.has(k) ? (this.map.get(k) as string) : null;
  }
  setItem(k: string, v: string): void {
    this.map.set(k, v);
  }
  /// Present because the durable keys are still read through `viewSection.ts`'s
  /// ephemeral path in these tests. The import itself must never call it — see
  /// `the import never calls removeItem at all`.
  removeItem(k: string): void {
    this.map.delete(k);
  }
}

let store: FakeStorage;

/// Each test gets a pristine copy of the module: the cache and the
/// hydrated/dirty flags are module-level state by design (they must be
/// readable from a `$state(...)` initialiser with no context to thread), so a
/// fresh import is the honest reset.
async function freshModule() {
  vi.resetModules();
  return import('./uiState');
}

const MARKER = 'cimp.ui-state.imported-from-local-storage.v1';

/// The patch object from the Nth `ui_state_set` call.
function patchOf(call: number): Record<string, string | null> {
  const [cmd, args] = invoke.mock.calls[call] as [string, { patch: Record<string, string | null> }];
  expect(cmd).toBe('ui_state_set');
  return args.patch;
}

/// `ui_state_get`'s answer for a project that has already been imported.
function file(values: Record<string, unknown> = {}) {
  return { version: 1, values: { [MARKER]: '1', ...values } };
}

beforeEach(() => {
  invoke.mockReset();
  store = new FakeStorage();
  vi.stubGlobal('localStorage', store);
  vi.spyOn(console, 'error').mockImplementation(() => {});
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe('hydrateUiState', () => {
  test('a hydrated window reads the file synchronously afterwards', async () => {
    invoke.mockResolvedValue(file({ 'cimp.view-section.v1.workbench': 'diff' }));
    const m = await freshModule();
    await m.hydrateUiState();

    // The point of the whole design: no await at the read site.
    expect(m.getUiValue('cimp.view-section.v1.workbench')).toBe('diff');
    expect(m.getUiValue('cimp.view-section.v1.nothing-saved')).toBeNull();
  });

  test('values that are not strings read as absent', async () => {
    // A hand-edited file can hold anything. Outside the string value domain
    // means "no saved value", not a value every call site must defend against.
    invoke.mockResolvedValue(file({ a: 42, b: null, c: { nested: true }, d: 'ok' }));
    const m = await freshModule();
    await m.hydrateUiState();

    expect(m.getUiValue('a')).toBeNull();
    expect(m.getUiValue('b')).toBeNull();
    expect(m.getUiValue('c')).toBeNull();
    expect(m.getUiValue('d')).toBe('ok');
  });

  test('a backend that cannot answer leaves defaults and never rejects', async () => {
    invoke.mockRejectedValue(new Error('no backend'));
    const m = await freshModule();

    await expect(m.hydrateUiState()).resolves.toBeUndefined();
    expect(m.getUiValue('anything')).toBeNull();
  });

  test('an unhydrated window is WRITE-INERT', async () => {
    // The settings window transitively bundles the modules that own this
    // state but mounts none of the views. If it ever did call a setter, it
    // must not be able to patch the file with its own defaults.
    invoke.mockRejectedValue(new Error('no backend'));
    const m = await freshModule();
    await m.hydrateUiState();

    m.setUiValue('cimp.view-section.v1.workbench', 'diff');
    await m.flushUiState();

    expect(invoke.mock.calls.filter(([c]) => c === 'ui_state_set')).toHaveLength(0);
  });

  test('a backend that never answers is given up on, not waited for', async () => {
    // V42 review RV-3. `mount(App)` awaits this, so an unbounded read meant a
    // revealed-but-never-mounted window (`showMainWindowOnce`'s 3 s net fires
    // regardless of what this does).
    vi.useFakeTimers();
    vi.spyOn(console, 'warn').mockImplementation(() => {});
    invoke.mockReturnValue(new Promise(() => {})); // never settles
    const m = await freshModule();

    const hydrating = m.hydrateUiState();
    await vi.advanceTimersByTimeAsync(2000);
    await expect(hydrating).resolves.toBeUndefined();

    expect(console.warn).toHaveBeenCalledTimes(1);
    expect(m.getUiValue('anything')).toBeNull();
  });

  test('a window that timed out is WRITE-INERT, and a late answer cannot arm it', async () => {
    // The cache may be missing everything the file holds, so a patch from
    // this window would persist defaults over good state. The abandoned read
    // must also not land afterwards and quietly re-arm writes under an app
    // that has already painted.
    vi.useFakeTimers();
    vi.spyOn(console, 'warn').mockImplementation(() => {});
    let land: (v: unknown) => void = () => {};
    invoke.mockImplementation((cmd: string) =>
      cmd === 'ui_state_get'
        ? new Promise((resolve) => {
            land = resolve;
          })
        : Promise.resolve(),
    );
    const m = await freshModule();

    const hydrating = m.hydrateUiState();
    await vi.advanceTimersByTimeAsync(2000);
    await hydrating;

    // The read finally answers, well after the app mounted.
    land(file({ 'cimp.view-section.v1.workbench': 'diff' }));
    await vi.advanceTimersByTimeAsync(0);

    expect(m.getUiValue('cimp.view-section.v1.workbench')).toBeNull();
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);
    m.setUiValue('k', 'v');
    await m.flushUiState();
    expect(invoke).not.toHaveBeenCalled();
  });

  test('a read that lands inside the budget is a normal hydrate', async () => {
    // The guard must not cost anything on the path everyone actually takes.
    vi.useFakeTimers();
    const m = await freshModule();
    invoke.mockResolvedValue(file({ 'cimp.view-section.v1.workbench': 'diff' }));

    await m.hydrateUiState();
    expect(m.getUiValue('cimp.view-section.v1.workbench')).toBe('diff');
    // …and the timer it armed is cleared, so a fake-timer suite cannot be
    // left with a pending handle.
    expect(vi.getTimerCount()).toBe(0);
  });

  test('a garbage file shape degrades to empty rather than throwing', async () => {
    invoke.mockResolvedValue({ version: 1 });
    const m = await freshModule();
    await expect(m.hydrateUiState()).resolves.toBeUndefined();
    expect(m.getUiValue('anything')).toBeNull();
  });
});

describe('setUiValue', () => {
  beforeEach(() => {
    invoke.mockResolvedValue(file());
  });

  test('writes through to the cache before the flush lands', async () => {
    const m = await freshModule();
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);

    m.setUiValue('k', 'v');
    // Read back before the microtask has run — an `$effect` writer followed by
    // a remount must see the new value, not the old one.
    expect(m.getUiValue('k')).toBe('v');

    await m.flushUiState();
    expect(patchOf(0)).toEqual({ k: 'v' });
  });

  test('a write goes out on the NEXT MICROTASK, with no timer to lose', async () => {
    // V42 review RV-2. The 250 ms debounce this replaces meant a toggle
    // followed by a close inside that window was lost unless `pagehide` won —
    // the closing race the synchronous `localStorage.setItem` did not have.
    // Fake timers here so a re-introduced `setTimeout` cannot be papered over
    // by real time passing while the assertions are awaited.
    vi.useFakeTimers();
    const m = await freshModule();
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);

    m.setUiValue('k', 'v');
    // Not synchronous: one handler touching three keys still costs one IPC.
    expect(invoke).not.toHaveBeenCalled();

    // No timer is advanced — draining the microtask queue is enough.
    await Promise.resolve();
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(patchOf(0)).toEqual({ k: 'v' });
  });

  test('a burst inside ONE tick coalesces into a single patch', async () => {
    vi.useFakeTimers();
    const m = await freshModule();
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);

    // One handler writing the same key repeatedly, plus a neighbour.
    for (const w of ['100', '110', '120', '130']) m.setUiValue('cols', w);
    m.setUiValue('other', 'x');
    expect(invoke).not.toHaveBeenCalled();

    await Promise.resolve();
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(patchOf(0)).toEqual({ cols: '130', other: 'x' });
  });

  test('writes in SEPARATE ticks are separate writes, not one deferred one', async () => {
    // The other half of RV-2: a per-`pointermove` splitter drag emits one
    // write per task, and each must reach the backend rather than waiting
    // behind a window a close can end.
    const m = await freshModule();
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);

    m.setUiValue('cols', '100');
    await Promise.resolve();
    m.setUiValue('cols', '110');
    await Promise.resolve();

    expect(invoke).toHaveBeenCalledTimes(2);
    expect(patchOf(0)).toEqual({ cols: '100' });
    expect(patchOf(1)).toEqual({ cols: '110' });
  });

  test('the patch carries ONLY touched keys, never the whole cache', async () => {
    // What makes a second window safe: an untouched key can never be
    // clobbered by another window's flush.
    invoke.mockResolvedValue(file({ untouched: 'keep-me' }));
    const m = await freshModule();
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);

    m.setUiValue('touched', 'new');
    await m.flushUiState();

    expect(patchOf(0)).toEqual({ touched: 'new' });
    expect(m.getUiValue('untouched')).toBe('keep-me');
  });

  test('re-writing the SAME value is not a write at all', async () => {
    // Every `$effect` writer fires once on mount, re-writing what it just
    // read. Under localStorage that was free; here it would be an IPC call
    // per view per mount.
    invoke.mockResolvedValue(file({ k: 'same' }));
    const m = await freshModule();
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);

    m.setUiValue('k', 'same');
    await m.flushUiState();
    expect(invoke).not.toHaveBeenCalled();
  });

  test('null removes the key and sends null so the backend removes it too', async () => {
    invoke.mockResolvedValue(file({ k: 'v' }));
    const m = await freshModule();
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);

    m.setUiValue('k', null);
    expect(m.getUiValue('k')).toBeNull();
    await m.flushUiState();
    expect(patchOf(0)).toEqual({ k: null });

    // Removing an already-absent key is a no-op, not an empty write.
    invoke.mockReset();
    m.setUiValue('k', null);
    await m.flushUiState();
    expect(invoke).not.toHaveBeenCalled();
  });

  test('a failed write loses persistence and nothing else', async () => {
    const m = await freshModule();
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockRejectedValue(new Error('disk full'));

    m.setUiValue('k', 'v');
    await expect(m.flushUiState()).resolves.toBeUndefined();
    // The cache still answers — the UI keeps working for this session.
    expect(m.getUiValue('k')).toBe('v');
  });

  test('a refusal that repeats is logged ONCE and never retried in a loop', async () => {
    // V42 review RV-10: `ui_state_set` refuses outright when the file on disk
    // is a version this build does not understand, and that refusal repeats
    // for the whole session. A console line behind every `<details>` toggle
    // is noise; a retry timer would be worse. The cache stays live.
    const m = await freshModule();
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockRejectedValue(
      new Error('ui_state.json is version 999, newer than this build understands (1)'),
    );

    for (const v of ['a', 'b', 'c']) {
      m.setUiValue('k', v);
      await m.flushUiState();
    }

    expect(console.error).toHaveBeenCalledTimes(1);
    // One IPC per mutation, none of them a retry of an earlier patch.
    expect(invoke).toHaveBeenCalledTimes(3);
    expect(patchOf(2)).toEqual({ k: 'c' });
    expect(m.getUiValue('k')).toBe('c');
  });

  test('values are stored verbatim, including JSON-inside-a-string', async () => {
    // `events.col-widths` is double-encoded. Nothing in this path may parse
    // or re-encode it.
    const m = await freshModule();
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);

    const doubled = '{"ts":120,"tool":240}';
    m.setUiValue('cimp.view-pref.v1.events.col-widths', doubled);
    await m.flushUiState();
    expect(patchOf(0)['cimp.view-pref.v1.events.col-widths']).toBe(doubled);
    expect(m.getUiValue('cimp.view-pref.v1.events.col-widths')).toBe(doubled);
  });
});

describe('the durable/ephemeral split', () => {
  test('only the nine named prefs are durable', async () => {
    const m = await freshModule();
    for (const [view, key] of [
      ['diff', 'view-mode'],
      ['events', 'col-widths'],
      ['events', 'cols-hidden'],
      ['code-intelligence', 'usage-cost-mode'],
      ['code-intelligence', 'dash-model-colors'],
      ['code-audit', 'severity'],
      ['code-audit', 'hidden-tools'],
      ['code-quality', 'severity'],
      ['code-quality', 'hidden-tools'],
    ]) {
      expect(m.isDurablePref(view, key), `${view}.${key}`).toBe(true);
    }
    // The by-design-stale ones stay in localStorage forever.
    for (const [view, key] of [
      ['diff', 'expanded'],
      ['diff', 'full-view'],
      ['worktrees', 'expanded-diff'],
      ['worktrees', 'full-diff'],
      ['session-commits', 'expanded'],
      ['git-graph', 'selected'],
      ['timeline', 'open-diff'],
      ['code-audit', 'text'],
      ['code-quality', 'text'],
    ]) {
      expect(m.isDurablePref(view, key), `${view}.${key}`).toBe(false);
    }
  });

  test('the import list is exactly the 20 durable keys', async () => {
    const m = await freshModule();
    expect([...m.IMPORTED_KEYS].sort()).toEqual(
      [
        'cimp.hidden-tabs.v1',
        'cimp.view-card-open.v1.code-intelligence.usage-advisor',
        'cimp.view-card-open.v1.code-intelligence.usage-cost',
        'cimp.view-card-open.v1.code-intelligence.usage-dashboard',
        'cimp.view-card-open.v1.code-intelligence.usage-effectiveness',
        'cimp.view-card-open.v1.code-intelligence.usage-sessions',
        'cimp.view-card-open.v1.code-intelligence.usage-this-session',
        'cimp.view-pref.v1.code-audit.hidden-tools',
        'cimp.view-pref.v1.code-audit.severity',
        'cimp.view-pref.v1.code-intelligence.dash-model-colors',
        'cimp.view-pref.v1.code-intelligence.usage-cost-mode',
        'cimp.view-pref.v1.code-quality.hidden-tools',
        'cimp.view-pref.v1.code-quality.severity',
        'cimp.view-pref.v1.diff.view-mode',
        'cimp.view-pref.v1.events.col-widths',
        'cimp.view-pref.v1.events.cols-hidden',
        'cimp.view-section.v1.code-audit',
        'cimp.view-section.v1.code-intelligence',
        'cimp.view-section.v1.tool-activity',
        'cimp.view-section.v1.workbench',
      ].sort(),
    );
  });
});

describe('the one-time localStorage import', () => {
  test('copies the durable values, marks the project, and LEAVES the originals', async () => {
    store.map.set('cimp.view-section.v1.workbench', 'diff');
    store.map.set('cimp.hidden-tabs.v1', '["tab-9"]');
    store.map.set('cimp.view-pref.v1.events.col-widths', '{"ts":120}');
    // Ephemeral neighbours must survive untouched.
    store.map.set('cimp.view-pref.v1.diff.expanded', '["a.rs"]');
    store.map.set('cimp.view-pref.v1.code-audit.text', 'unwrap');

    invoke.mockImplementation((cmd: string) =>
      cmd === 'ui_state_get' ? Promise.resolve({ version: 1, values: {} }) : Promise.resolve(),
    );
    const m = await freshModule();
    await m.hydrateUiState();

    const patch = patchOf(1);
    expect(patch['cimp.view-section.v1.workbench']).toBe('diff');
    expect(patch['cimp.hidden-tabs.v1']).toBe('["tab-9"]');
    expect(patch['cimp.view-pref.v1.events.col-widths']).toBe('{"ts":120}');
    expect(patch[MARKER]).toBe('1');
    // Not carried across.
    expect(patch).not.toHaveProperty('cimp.view-pref.v1.diff.expanded');
    expect(patch).not.toHaveProperty('cimp.view-pref.v1.code-audit.text');

    // Readable in the very same session, without a second hydrate.
    expect(m.getUiValue('cimp.view-section.v1.workbench')).toBe('diff');

    // V42 review RV-1: nothing is deleted. `localStorage` is per-MACHINE and
    // the marker that stops the import is per-PROJECT, so removing the
    // originals here would hand the first checkout launched after the upgrade
    // the machine's only copy and leave every other checkout importing
    // nothing.
    expect(store.map.get('cimp.view-section.v1.workbench')).toBe('diff');
    expect(store.map.get('cimp.hidden-tabs.v1')).toBe('["tab-9"]');
    expect(store.map.get('cimp.view-pref.v1.events.col-widths')).toBe('{"ts":120}');
    expect(store.map.get('cimp.view-pref.v1.diff.expanded')).toBe('["a.rs"]');
    expect(store.map.get('cimp.view-pref.v1.code-audit.text')).toBe('unwrap');
  });

  test('a SECOND project imports the same seeds losslessly', async () => {
    // The property RV-1 exists for, end to end: two checkouts, one machine,
    // one `localStorage`. Each has its own `ui_state.json` and therefore its
    // own marker, so each must get the full set.
    store.map.set('cimp.view-section.v1.workbench', 'diff');
    store.map.set('cimp.hidden-tabs.v1', '["tab-9"]');
    invoke.mockImplementation((cmd: string) =>
      cmd === 'ui_state_get' ? Promise.resolve({ version: 1, values: {} }) : Promise.resolve(),
    );

    const first = await freshModule();
    await first.hydrateUiState();
    const firstPatch = patchOf(1);

    invoke.mockReset();
    invoke.mockImplementation((cmd: string) =>
      cmd === 'ui_state_get' ? Promise.resolve({ version: 1, values: {} }) : Promise.resolve(),
    );
    const second = await freshModule();
    await second.hydrateUiState();

    expect(patchOf(1)).toEqual(firstPatch);
    expect(second.getUiValue('cimp.hidden-tabs.v1')).toBe('["tab-9"]');
  });

  test('a second boot does not re-import', async () => {
    // The marker is in the file, so a value a user has since deleted must not
    // come back from a stale localStorage key.
    store.map.set('cimp.view-section.v1.workbench', 'stale');
    invoke.mockResolvedValue(file());
    const m = await freshModule();
    await m.hydrateUiState();

    expect(invoke.mock.calls.filter(([c]) => c === 'ui_state_set')).toHaveLength(0);
    expect(m.getUiValue('cimp.view-section.v1.workbench')).toBeNull();
    expect(store.map.get('cimp.view-section.v1.workbench')).toBe('stale');
  });

  test('a project with nothing to import is still marked, so it runs once', async () => {
    invoke.mockImplementation((cmd: string) =>
      cmd === 'ui_state_get' ? Promise.resolve({ version: 1, values: {} }) : Promise.resolve(),
    );
    const m = await freshModule();
    await m.hydrateUiState();

    expect(patchOf(1)).toEqual({ [MARKER]: '1' });
    expect(m.getUiValue(MARKER)).toBe('1');
  });

  test('a corrupt localStorage value is carried across VERBATIM', async () => {
    // The import is a move, not a repair: a truncated JSON string or a
    // section id that no longer exists must reach the same call-site
    // validation that rejects it today.
    store.map.set('cimp.hidden-tabs.v1', '["tab-9"'); // truncated
    store.map.set('cimp.view-section.v1.workbench', 'a-section-that-was-renamed');
    invoke.mockImplementation((cmd: string) =>
      cmd === 'ui_state_get' ? Promise.resolve({ version: 1, values: {} }) : Promise.resolve(),
    );
    const m = await freshModule();
    await m.hydrateUiState();

    expect(patchOf(1)['cimp.hidden-tabs.v1']).toBe('["tab-9"');
    expect(patchOf(1)['cimp.view-section.v1.workbench']).toBe('a-section-that-was-renamed');
  });

  test('a FAILED import leaves the project unmarked and the window WRITE-INERT', async () => {
    // The one thing that must never happen: dropping the only copy of a
    // user's state because the file could not be written.
    //
    // V42 review RV-4: `hydrated` used to be set before the import ran, so a
    // failed import left the window write-LIVE over a cache the import had
    // not finished filling — the next toggle would persist that half-story
    // and the un-imported values would never be tried again.
    store.map.set('cimp.view-section.v1.workbench', 'diff');
    invoke.mockImplementation((cmd: string) =>
      cmd === 'ui_state_get'
        ? Promise.resolve({ version: 1, values: { survivor: 'from-the-file' } })
        : Promise.reject(new Error('read-only volume')),
    );
    const m = await freshModule();
    await expect(m.hydrateUiState()).resolves.toBeUndefined();

    expect(store.map.get('cimp.view-section.v1.workbench')).toBe('diff');
    expect(m.getUiValue(MARKER)).toBeNull();

    // Reads still work — the file WAS read, so the session is usable.
    expect(m.getUiValue('survivor')).toBe('from-the-file');

    // Writes do not. `ui_state_set` was called once (the failed import) and
    // must not be called again.
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);
    m.setUiValue('cimp.view-section.v1.workbench', 'status');
    await m.flushUiState();
    expect(invoke).not.toHaveBeenCalled();
  });

  test('a project that needs no import is write-live immediately', async () => {
    // The other side of RV-4: the marker is present, there is nothing to
    // settle, and the window must not be left read-only by the new ordering.
    invoke.mockResolvedValue(file());
    const m = await freshModule();
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);

    m.setUiValue('k', 'v');
    await m.flushUiState();
    expect(patchOf(0)).toEqual({ k: 'v' });
  });

  test('a SUCCESSFUL import leaves the window write-live', async () => {
    invoke.mockImplementation((cmd: string) =>
      cmd === 'ui_state_get' ? Promise.resolve({ version: 1, values: {} }) : Promise.resolve(),
    );
    const m = await freshModule();
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);

    m.setUiValue('k', 'v');
    await m.flushUiState();
    expect(patchOf(0)).toEqual({ k: 'v' });
  });

  test('an unusable localStorage still marks the project instead of retrying forever', async () => {
    store.throwOn = 'get';
    invoke.mockImplementation((cmd: string) =>
      cmd === 'ui_state_get' ? Promise.resolve({ version: 1, values: {} }) : Promise.resolve(),
    );
    const m = await freshModule();
    await expect(m.hydrateUiState()).resolves.toBeUndefined();
    expect(patchOf(1)).toEqual({ [MARKER]: '1' });
  });

  test('the import never calls removeItem at all', async () => {
    // The structural half of RV-1, stated where a future edit would trip over
    // it: any deletion here is per-machine and destroys the seed every OTHER
    // checkout on this machine still needs.
    store.map.set('cimp.view-section.v1.workbench', 'diff');
    const removed: string[] = [];
    store.removeItem = (k: string) => {
      removed.push(k);
    };
    invoke.mockImplementation((cmd: string) =>
      cmd === 'ui_state_get' ? Promise.resolve({ version: 1, values: {} }) : Promise.resolve(),
    );
    const m = await freshModule();
    await expect(m.hydrateUiState()).resolves.toBeUndefined();

    expect(removed).toEqual([]);
    expect(patchOf(1)['cimp.view-section.v1.workbench']).toBe('diff');
    expect(m.getUiValue(MARKER)).toBe('1');
  });
});

describe('viewSection helpers over the cache', () => {
  test('sections and cards round-trip, and validation stays at the call site', async () => {
    vi.resetModules();
    invoke.mockResolvedValue(
      file({
        'cimp.view-section.v1.workbench': 'no-longer-a-section',
        'cimp.view-card-open.v1.code-intelligence.usage-cost': '1',
      }),
    );
    const m = await import('./uiState');
    const vs = await import('./viewSection');
    await m.hydrateUiState();

    // Rejected by the caller's `valid` list, exactly as before the move.
    expect(vs.loadViewSection('workbench', ['diff', 'status'] as const, 'diff')).toBe('diff');
    expect(vs.loadCardOpen('code-intelligence', 'usage-cost')).toBe(true);
    // Per-site fallbacks are unchanged for an absent key.
    expect(vs.loadCardOpen('code-intelligence', 'usage-dashboard', true)).toBe(true);
    expect(vs.loadCardOpen('code-intelligence', 'usage-advisor')).toBe(false);

    invoke.mockReset();
    invoke.mockResolvedValue(undefined);
    vs.saveViewSection('workbench', 'status');
    await m.flushUiState();
    expect(patchOf(0)).toEqual({ 'cimp.view-section.v1.workbench': 'status' });
  });

  test('a durable pref goes to the cache and an ephemeral one to localStorage', async () => {
    vi.resetModules();
    invoke.mockResolvedValue(file());
    const m = await import('./uiState');
    const vs = await import('./viewSection');
    await m.hydrateUiState();
    invoke.mockReset();
    invoke.mockResolvedValue(undefined);

    vs.saveViewString('diff', 'view-mode', 'side-by-side');
    vs.saveViewSet('diff', 'expanded', ['a.rs', 'b.rs']);
    await m.flushUiState();

    expect(patchOf(0)).toEqual({ 'cimp.view-pref.v1.diff.view-mode': 'side-by-side' });
    expect(store.map.get('cimp.view-pref.v1.diff.expanded')).toBe('["a.rs","b.rs"]');
    // ...and both read back from where they were written.
    expect(vs.loadViewString('diff', 'view-mode')).toBe('side-by-side');
    expect(vs.loadViewSet('diff', 'expanded')).toEqual(['a.rs', 'b.rs']);
  });

  test('the audit panels keep their legacy namespaces', async () => {
    // `code-audit` / `code-quality` are the `view` prop CodeAuditView passes;
    // renaming either would silently drop users' saved filters.
    vi.resetModules();
    invoke.mockResolvedValue(
      file({
        'cimp.view-pref.v1.code-audit.severity': 'high',
        'cimp.view-pref.v1.code-quality.hidden-tools': '["clippy"]',
      }),
    );
    const m = await import('./uiState');
    const vs = await import('./viewSection');
    await m.hydrateUiState();

    expect(vs.loadViewString('code-audit', 'severity')).toBe('high');
    expect(vs.loadViewSet('code-quality', 'hidden-tools')).toEqual(['clippy']);
  });

  test('a corrupt durable set reads as empty, never as a throw', async () => {
    vi.resetModules();
    invoke.mockResolvedValue(file({ 'cimp.view-pref.v1.events.cols-hidden': '["ts"' }));
    const m = await import('./uiState');
    const vs = await import('./viewSection');
    await m.hydrateUiState();

    expect(vs.loadViewSet('events', 'cols-hidden')).toEqual([]);
  });
});

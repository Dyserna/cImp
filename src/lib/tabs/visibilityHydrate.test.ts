import { beforeEach, describe, expect, test, vi } from 'vitest';

/// V42 Phase C — the hidden-tab set now comes from the per-project UI-state
/// cache instead of `localStorage`, which cost it the one thing localStorage
/// gave it for free: an answer at module-import time.
///
/// `hiddenTabs` therefore starts EMPTY and is filled by `hydrateHiddenTabs()`,
/// which `main.ts` calls after `hydrateUiState()` and before `mount(App)`.
/// App's `onMount` reads it on the fresh-install path, where there is no
/// persisted tree for the backend to have repaired and the layout has to be
/// built hidden-aware here. (V42 Phase B: on every other launch the backend's
/// `settings::layout` reads the same file and hands over a tree the hidden tabs
/// were never in.) These tests pin the two halves of that seam: an unhydrated
/// store is empty (so nothing accidentally works by reading it too early), and
/// a hydrated one holds exactly the saved set.

const invoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke: (...a: unknown[]) => invoke(...a) }));

const MARKER = 'cimp.ui-state.imported-from-local-storage.v1';

function snapshot(store: { subscribe: (r: (v: ReadonlySet<string>) => void) => () => void }) {
  let out: ReadonlySet<string> = new Set();
  store.subscribe((v) => (out = v))();
  return [...out];
}

beforeEach(() => {
  invoke.mockReset();
  vi.stubGlobal('localStorage', {
    getItem: () => null,
    setItem: () => {},
    removeItem: () => {},
  });
});

describe('hydrateHiddenTabs', () => {
  test('the set is empty until hydration, then holds the saved ids', async () => {
    vi.resetModules();
    invoke.mockResolvedValue({
      version: 1,
      values: { [MARKER]: '1', 'cimp.hidden-tabs.v1': '["events","note"]' },
    });
    const { hydrateUiState } = await import('../uiState');
    const { hiddenTabs, hydrateHiddenTabs, isTabHidden } = await import('./visibility');

    expect(snapshot(hiddenTabs)).toEqual([]);

    await hydrateUiState();
    hydrateHiddenTabs();

    expect(snapshot(hiddenTabs).sort()).toEqual(['events', 'note']);
    expect(isTabHidden('events')).toBe(true);
    expect(isTabHidden('workbench')).toBe(false);
  });

  test('a corrupt saved set hydrates as empty rather than throwing', async () => {
    vi.resetModules();
    invoke.mockResolvedValue({
      version: 1,
      values: { [MARKER]: '1', 'cimp.hidden-tabs.v1': '["events"' },
    });
    const { hydrateUiState } = await import('../uiState');
    const { hiddenTabs, hydrateHiddenTabs } = await import('./visibility');

    await hydrateUiState();
    expect(() => hydrateHiddenTabs()).not.toThrow();
    expect(snapshot(hiddenTabs)).toEqual([]);
  });

  test('hydrating is idempotent — it is a pure read of the cache', async () => {
    vi.resetModules();
    invoke.mockResolvedValue({
      version: 1,
      values: { [MARKER]: '1', 'cimp.hidden-tabs.v1': '["events"]' },
    });
    const { hydrateUiState, flushUiState } = await import('../uiState');
    const { hiddenTabs, hydrateHiddenTabs } = await import('./visibility');

    await hydrateUiState();
    hydrateHiddenTabs();
    hydrateHiddenTabs();

    expect(snapshot(hiddenTabs)).toEqual(['events']);
    // Hydration must never write: no patch was queued.
    invoke.mockReset();
    await flushUiState();
    expect(invoke).not.toHaveBeenCalled();
  });
});

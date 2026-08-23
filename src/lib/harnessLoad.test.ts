import { beforeEach, describe, expect, test, vi } from 'vitest';

/// V40 review findings F-3 (frontend H-2) and L-18 — **how the roster LOADS**,
/// as opposed to what it contains once it has.
///
/// The registry parity suite (`harness.test.ts`) is about the answer. This one
/// is about the window between mount and the answer, and about the failure that
/// used to be indistinguishable from it: `loadHarnesses` caught, logged, left
/// the store `[]` forever and told nobody, so a window whose `harness_list` had
/// failed rendered without its per-harness settings form, without the per-tab
/// *Use local provider* checkbox, without the MCP access columns and without
/// the usage widget — with nothing anywhere saying why.

const invoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke: (...a: unknown[]) => invoke(...a) }));

const { harnesses, harnessLoadState, loadHarnesses } = await import('./harness');
const { FIXTURE_HARNESSES } = await import('./harness.fixture');

beforeEach(() => {
  invoke.mockReset();
  harnesses.set([]);
  harnessLoadState.set('loading');
});

function state(): string {
  let out = '';
  harnessLoadState.subscribe((v) => (out = v))();
  return out;
}

function roster(): unknown[] {
  let out: unknown[] = [];
  harnesses.subscribe((v) => (out = v))();
  return out;
}

describe('loadHarnesses', () => {
  test('a first-try answer lands the roster and reports ready', async () => {
    invoke.mockResolvedValue(FIXTURE_HARNESSES);
    await loadHarnesses();
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(roster()).toHaveLength(FIXTURE_HARNESSES.length);
    expect(state()).toBe('ready');
  });

  test('a transient failure is retried, and the retry is what fills the store', async () => {
    invoke.mockRejectedValueOnce(new Error('not ready')).mockResolvedValue(FIXTURE_HARNESSES);
    await loadHarnesses();
    expect(invoke).toHaveBeenCalledTimes(2);
    expect(state()).toBe('ready');
  });

  test('an EMPTY roster is a failure, not a successful load', async () => {
    // The registry always has at least one entry, so `[]` means the call
    // answered something this build cannot use. Treated as success it would
    // render a window with every harness section silently gone.
    invoke.mockResolvedValue([]);
    await loadHarnesses();
    expect(invoke.mock.calls.length).toBeGreaterThan(1);
    expect(state()).toBe('failed');
    expect(roster()).toEqual([]);
  });

  test('a persistent failure ends at `failed`, which is NOT `loading`', async () => {
    invoke.mockRejectedValue(new Error('backend down'));
    await loadHarnesses();
    expect(state()).toBe('failed');
    // The whole point of the finding: a consumer can tell "not yet" from
    // "never", so the Settings window can show a banner and a retry instead of
    // a permanently half-rendered pane.
    expect(state()).not.toBe('loading');
  });

  test('a retry after a failure recovers without a reload', async () => {
    invoke.mockRejectedValue(new Error('backend down'));
    await loadHarnesses();
    expect(state()).toBe('failed');
    invoke.mockReset();
    invoke.mockResolvedValue(FIXTURE_HARNESSES);
    await loadHarnesses();
    expect(state()).toBe('ready');
    expect(roster()).toHaveLength(FIXTURE_HARNESSES.length);
  });

  test('a later failure never empties a roster that already landed', async () => {
    invoke.mockResolvedValue(FIXTURE_HARNESSES);
    await loadHarnesses();
    invoke.mockReset();
    invoke.mockRejectedValue(new Error('backend down'));
    await loadHarnesses();
    expect(roster()).toHaveLength(FIXTURE_HARNESSES.length);
    expect(state()).toBe('ready');
  });
});

describe('the active-tab placeholder (L-18)', () => {
  test('the registry corrects the placeholder, and never walks back a real value', async () => {
    // Fresh module instances, because `tabs/state.ts` subscribes at import
    // time and its one-shot re-seed has already fired in this process.
    vi.resetModules();
    invoke.mockResolvedValue(FIXTURE_HARNESSES);
    const h = await import('./harness');
    h.harnesses.set([]);
    const { activeTab } = await import('./tabs/state');

    // The bootstrap placeholder, before any roster.
    let seen = '';
    const stop = activeTab.subscribe((v) => (seen = v));
    expect(seen).not.toBe('');

    // Something authoritative speaks first — a restored `session.active_tab_id`
    // or the backend's `ActiveTabChanged`.
    activeTab.set('shell-default-1');
    // …and THEN the roster arrives. It must not yank the user back to the
    // first harness's tab. The old guard (`cur === ''`) could never fire at all
    // because `defaultTabId` has a bootstrap fallback, so this was dead code
    // sitting one deletion away from being a live regression.
    h.harnesses.set(FIXTURE_HARNESSES);
    expect(seen).toBe('shell-default-1');
    stop();
  });
});

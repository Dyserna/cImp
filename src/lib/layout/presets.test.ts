// Tests for the preset actions' seam with the backend.
//
// V42 Phase B moved the integrity walk that adapts a preset's tree to the live
// tab list into Rust (`settings::layout`, exercised there — see
// `restore_layout_preset` and the ported rule cases). What is left on this side
// is a three-step orchestration, and each step has a way to go wrong that a
// type checker will not catch: ask the backend, cancel any drag in flight, swap
// the store. These pin the order and the failure path.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { get } from 'svelte/store';

const invoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke: (...a: unknown[]) => invoke(...a) }));

const cancelDrag = vi.fn();
vi.mock('../dnd/drag', () => ({ cancelDrag: () => cancelDrag() }));

import { restoreLayoutPreset, saveCurrentLayoutAsPreset } from './presets';
import { layout } from './store';
import type { LayoutNode } from './types';

function pane(id: string, tab_ids: string[], active: string | null): LayoutNode {
  return { type: 'pane', id, tab_ids, active_tab_id: active };
}

beforeEach(() => {
  invoke.mockReset();
  cancelDrag.mockReset();
  layout.set({ tree: pane('live', ['tabA'], 'tabA'), focused_pane_id: 'live' });
});

describe('restoreLayoutPreset', () => {
  test('takes the backend-repaired tree verbatim and cancels any drag', async () => {
    const repaired = {
      tree: pane('preset-pane', ['tabA', 'tabNew'], 'tabA'),
      focused_pane_id: 'preset-pane',
    };
    invoke.mockResolvedValue(repaired);

    await restoreLayoutPreset('two-up');

    expect(invoke).toHaveBeenCalledWith('restore_layout_preset', { name: 'two-up' });
    // Verbatim: no second sieve on this side. A repair here would be a copy of
    // rules that have to exist exactly once.
    expect(get(layout)).toEqual(repaired);
    // A drag in flight references the OLD tree's pane ids; replacing the tree
    // under it would strand sourcePaneId.
    expect(cancelDrag).toHaveBeenCalledTimes(1);
  });

  test('a rejected restore leaves the live layout and any drag alone', async () => {
    const before = get(layout);
    invoke.mockRejectedValue(new Error("no preset named 'gone'"));

    await expect(restoreLayoutPreset('gone')).resolves.toBeUndefined();

    expect(get(layout)).toBe(before);
    // Cancelling a drag the user is still making and THEN failing the restore
    // would be a change with no result — hence cancel AFTER the await.
    expect(cancelDrag).not.toHaveBeenCalled();
  });
});

describe('saveCurrentLayoutAsPreset', () => {
  test('sends the live tree, without focus', async () => {
    invoke.mockResolvedValue(undefined);
    await saveCurrentLayoutAsPreset('mine');
    expect(invoke).toHaveBeenCalledWith('save_layout_preset', {
      name: 'mine',
      tree: get(layout).tree,
    });
  });
});

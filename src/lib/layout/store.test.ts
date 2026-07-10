// Tests for the layout store's lifecycle layer: placement-queue
// routing for guided tab creation, the drag-commit branches, and the
// merge/collapse helpers. The store is a module-level Svelte writable,
// so each test seeds it explicitly via `layout.set` and resets the
// placement queue.

import { beforeEach, describe, expect, test } from 'vitest';
import { get } from 'svelte/store';
import {
  _resetPlacementQueueForTests,
  applyTabClosedFromLayout,
  applyTabCreatedToLayout,
  cancelPlacement,
  closeFocusedPane,
  commitDrop,
  layout,
  moveAllTabsToPane,
  requestTabIntoPane,
  requestTabIntoSplit,
  resetLayoutToSinglePane,
} from './store';
import { eachPane, findPane, findPaneContainingTab } from './tree';
import {
  _resetIdCounterForTests,
  type LayoutNode,
  type PaneNode,
  type SplitNode,
} from './types';

function pane(id: string, tab_ids: string[], active: string | null = tab_ids[0] ?? null): PaneNode {
  return { type: 'pane', id, tab_ids, active_tab_id: active };
}

function split(id: string, first: LayoutNode, second: LayoutNode): SplitNode {
  return { type: 'split', id, direction: 'horizontal', ratio: 0.5, first, second };
}

function allTabIds(root: LayoutNode): string[] {
  const out: string[] = [];
  for (const p of eachPane(root)) out.push(...p.tab_ids);
  return out;
}

beforeEach(() => {
  _resetIdCounterForTests();
  _resetPlacementQueueForTests();
  layout.set({ tree: pane('root', ['t1'], 't1'), focused_pane_id: 'root' });
});

describe('applyTabCreatedToLayout', () => {
  test('default routing appends to the focused pane and activates', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a']), pane('p2', ['b'])),
      focused_pane_id: 'p2',
    });
    applyTabCreatedToLayout('new');
    const state = get(layout);
    const p2 = findPane(state.tree, 'p2')!;
    expect(p2.tab_ids).toEqual(['b', 'new']);
    expect(p2.active_tab_id).toBe('new');
    expect(state.focused_pane_id).toBe('p2');
  });

  test('pane placement routes into the requested pane, not the focused one', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a']), pane('p2', ['b'])),
      focused_pane_id: 'p2',
    });
    requestTabIntoPane('p1');
    applyTabCreatedToLayout('new');
    const state = get(layout);
    expect(findPane(state.tree, 'p1')!.tab_ids).toEqual(['a', 'new']);
    expect(state.focused_pane_id).toBe('p1');
  });

  test('split placement creates a sibling pane and preserves the source verbatim', () => {
    layout.set({
      tree: pane('root', ['a', 'b'], 'a'),
      focused_pane_id: 'root',
    });
    requestTabIntoSplit('root', 'vertical', 'second');
    applyTabCreatedToLayout('new');
    const state = get(layout);
    expect(state.tree.type).toBe('split');
    const root = state.tree as SplitNode;
    expect(root.direction).toBe('vertical');
    const kept = root.first as PaneNode;
    const fresh = root.second as PaneNode;
    expect(kept.id).toBe('root');
    expect(kept.tab_ids).toEqual(['a', 'b']);
    expect(kept.active_tab_id).toBe('a');
    expect(fresh.tab_ids).toEqual(['new']);
    expect(state.focused_pane_id).toBe(fresh.id);
  });

  test('placements are consumed FIFO', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a']), pane('p2', ['b'])),
      focused_pane_id: 'p1',
    });
    requestTabIntoPane('p1');
    requestTabIntoPane('p2');
    applyTabCreatedToLayout('n1');
    applyTabCreatedToLayout('n2');
    const state = get(layout);
    expect(findPane(state.tree, 'p1')!.tab_ids).toEqual(['a', 'n1']);
    expect(findPane(state.tree, 'p2')!.tab_ids).toEqual(['b', 'n2']);
  });

  test('cancelPlacement removes the identified placement, not the newest one', () => {
    // Regression for the LIFO-pop bug: with queue [A, B], cancelling A
    // (its IPC failed) must leave B to route its own tab correctly.
    layout.set({
      tree: split('s1', pane('p1', ['a']), pane('p2', ['b'])),
      focused_pane_id: 'p1',
    });
    const a = requestTabIntoPane('p1');
    requestTabIntoPane('p2');
    cancelPlacement(a);
    applyTabCreatedToLayout('n1');
    const state = get(layout);
    expect(findPane(state.tree, 'p1')!.tab_ids).toEqual(['a']);
    expect(findPane(state.tree, 'p2')!.tab_ids).toEqual(['b', 'n1']);
  });

  test('placement for a vanished pane falls back to the focused pane', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a']), pane('p2', ['b'])),
      focused_pane_id: 'p2',
    });
    requestTabIntoPane('gone');
    applyTabCreatedToLayout('new');
    const state = get(layout);
    expect(findPane(state.tree, 'p2')!.tab_ids).toEqual(['b', 'new']);
  });
});

describe('applyTabClosedFromLayout', () => {
  test('collapses an emptied non-root pane and refocuses the survivor', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a']), pane('p2', ['b'])),
      focused_pane_id: 'p2',
    });
    applyTabClosedFromLayout('b');
    const state = get(layout);
    expect(state.tree.type).toBe('pane');
    expect((state.tree as PaneNode).id).toBe('p1');
    expect(state.focused_pane_id).toBe('p1');
  });

  test('keeps focus when a different pane loses a (non-last) tab', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a', 'x'], 'a'), pane('p2', ['b'])),
      focused_pane_id: 'p2',
    });
    applyTabClosedFromLayout('x');
    const state = get(layout);
    expect(state.focused_pane_id).toBe('p2');
    expect(findPane(state.tree, 'p1')!.tab_ids).toEqual(['a']);
  });
});

describe('commitDrop', () => {
  test('reorder: dropping on the original spot is a no-op (same state reference)', () => {
    layout.set({
      tree: pane('root', ['a', 'b', 'c'], 'a'),
      focused_pane_id: 'root',
    });
    const before = get(layout);
    // insertIndex 1 with oldIndex 0 → adjusted to 0 → no-op.
    commitDrop('a', 'root', { kind: 'reorder', paneId: 'root', insertIndex: 1 });
    expect(get(layout)).toBe(before);
  });

  test('reorder: drag right lands after the hovered tab', () => {
    layout.set({
      tree: pane('root', ['a', 'b', 'c'], 'a'),
      focused_pane_id: 'root',
    });
    commitDrop('a', 'root', { kind: 'reorder', paneId: 'root', insertIndex: 2 });
    const p = get(layout).tree as PaneNode;
    expect(p.tab_ids).toEqual(['b', 'a', 'c']);
    expect(p.active_tab_id).toBe('a');
  });

  test('moveToPane: collapses the emptied source pane', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a']), pane('p2', ['b'])),
      focused_pane_id: 'p1',
    });
    commitDrop('a', 'p1', { kind: 'moveToPane', paneId: 'p2' });
    const state = get(layout);
    expect(state.tree.type).toBe('pane');
    const p = state.tree as PaneNode;
    expect(p.id).toBe('p2');
    expect(p.tab_ids).toEqual(['b', 'a']);
    expect(p.active_tab_id).toBe('a');
    expect(state.focused_pane_id).toBe('p2');
  });

  test('moveToPane: stale target pane → state unchanged, no tab loss', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a']), pane('p2', ['b'])),
      focused_pane_id: 'p1',
    });
    const before = get(layout);
    commitDrop('a', 'p1', { kind: 'moveToPane', paneId: 'gone' });
    expect(get(layout)).toBe(before);
    expect(allTabIds(get(layout).tree).sort()).toEqual(['a', 'b']);
  });

  test('same-pane split of a multi-tab pane moves the dragged tab into the new sibling', () => {
    layout.set({
      tree: pane('root', ['a', 'b'], 'a'),
      focused_pane_id: 'root',
    });
    commitDrop('b', 'root', { kind: 'split', paneId: 'root', direction: 'left' });
    const state = get(layout);
    expect(state.tree.type).toBe('split');
    const root = state.tree as SplitNode;
    // direction 'left' → horizontal split, new pane on the first side.
    expect(root.direction).toBe('horizontal');
    expect((root.first as PaneNode).tab_ids).toEqual(['b']);
    expect((root.second as PaneNode).tab_ids).toEqual(['a']);
    expect(state.focused_pane_id).toBe((root.first as PaneNode).id);
  });

  test('same-pane split of a single-tab pane dissolves back to one pane (no tab loss)', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a']), pane('p2', ['b'])),
      focused_pane_id: 'p1',
    });
    commitDrop('a', 'p1', { kind: 'split', paneId: 'p1', direction: 'bottom' });
    const state = get(layout);
    expect(allTabIds(state.tree).sort()).toEqual(['a', 'b']);
    // The kept (empty) side collapsed, so the tree still has two panes.
    expect([...eachPane(state.tree)].length).toBe(2);
    expect(findPaneContainingTab(state.tree, 'a')).not.toBeNull();
  });

  test('cross-pane split: stale target → state unchanged, no tab loss', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a', 'x'], 'a'), pane('p2', ['b'])),
      focused_pane_id: 'p1',
    });
    const before = get(layout);
    commitDrop('x', 'p1', { kind: 'split', paneId: 'gone', direction: 'right' });
    expect(get(layout)).toBe(before);
    expect(allTabIds(get(layout).tree).sort()).toEqual(['a', 'b', 'x']);
  });

  test('cross-pane split: moves the tab and collapses an emptied source', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a']), pane('p2', ['b'])),
      focused_pane_id: 'p1',
    });
    commitDrop('a', 'p1', { kind: 'split', paneId: 'p2', direction: 'top' });
    const state = get(layout);
    expect(allTabIds(state.tree).sort()).toEqual(['a', 'b']);
    // p1 collapsed; root is now the vertical split of the new pane over p2.
    expect(state.tree.type).toBe('split');
    const root = state.tree as SplitNode;
    expect(root.direction).toBe('vertical');
    expect((root.first as PaneNode).tab_ids).toEqual(['a']);
    expect((root.second as PaneNode).id).toBe('p2');
  });
});

describe('resetLayoutToSinglePane', () => {
  test("keeps the FOCUSED pane's active tab active in the merged pane", () => {
    // Regression: the merged pane used to adopt the first active tab in
    // document order, silently switching what the user was looking at.
    layout.set({
      tree: split('s1', pane('p1', ['a', 'b'], 'a'), pane('p2', ['c', 'd'], 'd')),
      focused_pane_id: 'p2',
    });
    resetLayoutToSinglePane();
    const state = get(layout);
    expect(state.tree.type).toBe('pane');
    const p = state.tree as PaneNode;
    expect(p.tab_ids).toEqual(['a', 'b', 'c', 'd']);
    expect(p.active_tab_id).toBe('d');
    expect(state.focused_pane_id).toBe(p.id);
  });
});

describe('closeFocusedPane', () => {
  test('merges the focused pane into the sibling leftmost leaf and collapses', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a', 'b'], 'b'), pane('p2', ['c'], 'c')),
      focused_pane_id: 'p1',
    });
    closeFocusedPane();
    const state = get(layout);
    expect(state.tree.type).toBe('pane');
    const p = state.tree as PaneNode;
    expect(p.id).toBe('p2');
    expect(p.tab_ids).toEqual(['c', 'a', 'b']);
    expect(p.active_tab_id).toBe('b');
    expect(state.focused_pane_id).toBe('p2');
  });

  test('no-op when the focused pane is the root', () => {
    layout.set({ tree: pane('root', ['a'], 'a'), focused_pane_id: 'root' });
    const before = get(layout);
    closeFocusedPane();
    expect(get(layout)).toBe(before);
  });
});

describe('moveAllTabsToPane', () => {
  test('moves every tab, carries the active tab, collapses the source', () => {
    const inner = split('s2', pane('target', ['x'], 'x'), pane('p3', ['y'], 'y'));
    layout.set({
      tree: split('s1', pane('source', ['a', 'b'], 'b'), inner),
      focused_pane_id: 'source',
    });
    moveAllTabsToPane('source', 'target');
    const state = get(layout);
    expect(state.tree.type).toBe('split');
    const target = findPane(state.tree, 'target')!;
    expect(target.tab_ids).toEqual(['x', 'a', 'b']);
    expect(target.active_tab_id).toBe('b');
    expect(findPane(state.tree, 'source')).toBeNull();
    expect(state.focused_pane_id).toBe('target');
  });

  test('no-op when source and target are the same pane', () => {
    layout.set({
      tree: split('s1', pane('p1', ['a']), pane('p2', ['b'])),
      focused_pane_id: 'p1',
    });
    const before = get(layout);
    moveAllTabsToPane('p1', 'p1');
    expect(get(layout)).toBe(before);
  });
});

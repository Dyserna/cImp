// Pure-data tests for the layout-tree operations. Vitest as runner; tests
// exercise structural invariants (root protection, parent-replacement
// rebalancing, immutability of unrelated subtrees) more than just leaf
// behavior, because the M2 drag-and-drop and M3 close paths will lean on
// these ops in places that are awkward to test end-to-end.

import { beforeEach, describe, expect, test } from 'vitest';
import {
  closePane,
  findPane,
  findPaneContainingTab,
  findSplitContaining,
  firstPane,
  insertTabIntoPane,
  moveTab,
  removeTab,
  setActiveTabId,
  setSplitRatio,
  splitPane,
} from './tree';
import {
  _resetIdCounterForTests,
  type LayoutNode,
  type PaneNode,
  type SplitNode,
} from './types';

beforeEach(() => {
  _resetIdCounterForTests();
});

function pane(id: string, tab_ids: string[], active_tab_id: string | null = tab_ids[0] ?? null): PaneNode {
  return { type: 'pane', id, tab_ids, active_tab_id };
}

function split(
  id: string,
  direction: 'horizontal' | 'vertical',
  ratio: number,
  first: LayoutNode,
  second: LayoutNode,
): SplitNode {
  return { type: 'split', id, direction, ratio, first, second };
}

describe('findPane', () => {
  test('returns the pane when present at root', () => {
    const root = pane('p1', ['t1']);
    expect(findPane(root, 'p1')).toBe(root);
  });

  test('walks into splits to find a deep pane', () => {
    const left = pane('p1', ['t1']);
    const right = pane('p2', ['t2']);
    const root = split('s1', 'horizontal', 0.5, left, right);
    expect(findPane(root, 'p2')).toBe(right);
  });

  test('returns null when the id is unknown', () => {
    const root = pane('p1', ['t1']);
    expect(findPane(root, 'nope')).toBeNull();
  });
});

describe('findSplitContaining', () => {
  test('returns the parent split for a child pane', () => {
    const left = pane('p1', ['t1']);
    const right = pane('p2', ['t2']);
    const root = split('s1', 'horizontal', 0.5, left, right);
    expect(findSplitContaining(root, 'p1')).toBe(root);
    expect(findSplitContaining(root, 'p2')).toBe(root);
  });

  test('returns null for the root pane', () => {
    const root = pane('p1', ['t1']);
    expect(findSplitContaining(root, 'p1')).toBeNull();
  });

  test('returns the immediate parent split, not a more distant ancestor', () => {
    const inner = split('s2', 'vertical', 0.5, pane('p2', ['t2']), pane('p3', ['t3']));
    const root = split('s1', 'horizontal', 0.5, pane('p1', ['t1']), inner);
    expect(findSplitContaining(root, 'p2')).toBe(inner);
  });
});

describe('insertTabIntoPane', () => {
  test('inserts at the requested position and clamps out-of-range', () => {
    const root = pane('p1', ['a', 'b', 'c']);
    const next = insertTabIntoPane(root, 'p1', 'x', 1) as PaneNode;
    expect(next.tab_ids).toEqual(['a', 'x', 'b', 'c']);
    const head = insertTabIntoPane(root, 'p1', 'x', -5) as PaneNode;
    expect(head.tab_ids).toEqual(['x', 'a', 'b', 'c']);
    const tail = insertTabIntoPane(root, 'p1', 'x', 99) as PaneNode;
    expect(tail.tab_ids).toEqual(['a', 'b', 'c', 'x']);
  });

  test('activates the inserted tab when the pane was empty', () => {
    const root = pane('p1', [], null);
    const next = insertTabIntoPane(root, 'p1', 'x', 0) as PaneNode;
    expect(next.active_tab_id).toBe('x');
  });

  test('preserves the existing active tab on insert by default', () => {
    const root = pane('p1', ['a', 'b'], 'b');
    const next = insertTabIntoPane(root, 'p1', 'x', 0) as PaneNode;
    expect(next.active_tab_id).toBe('b');
  });

  test('activates the inserted tab when activate: true', () => {
    const root = pane('p1', ['a', 'b'], 'a');
    const next = insertTabIntoPane(root, 'p1', 'x', 1, { activate: true }) as PaneNode;
    expect(next.active_tab_id).toBe('x');
  });

  test('no-ops when the tab is already in the pane', () => {
    const root = pane('p1', ['a', 'b']);
    const next = insertTabIntoPane(root, 'p1', 'a', 0) as PaneNode;
    expect(next.tab_ids).toEqual(['a', 'b']);
  });
});

describe('removeTab', () => {
  test('removes the tab and reports the holding pane', () => {
    const root = pane('p1', ['a', 'b', 'c'], 'b');
    const { tree, paneId } = removeTab(root, 'b');
    expect(paneId).toBe('p1');
    expect((tree as PaneNode).tab_ids).toEqual(['a', 'c']);
  });

  test('promotes the left neighbor as the new active tab', () => {
    const root = pane('p1', ['a', 'b', 'c'], 'b');
    const { tree } = removeTab(root, 'b');
    expect((tree as PaneNode).active_tab_id).toBe('a');
  });

  test('promotes the right neighbor when the leftmost active tab is removed', () => {
    const root = pane('p1', ['a', 'b', 'c'], 'a');
    const { tree } = removeTab(root, 'a');
    expect((tree as PaneNode).active_tab_id).toBe('b');
  });

  test('clears active_tab_id when the pane becomes empty', () => {
    const root = pane('p1', ['a'], 'a');
    const { tree } = removeTab(root, 'a');
    expect((tree as PaneNode).tab_ids).toEqual([]);
    expect((tree as PaneNode).active_tab_id).toBeNull();
  });

  test('returns paneId null when the tab is not in any pane', () => {
    const root = pane('p1', ['a']);
    const { paneId } = removeTab(root, 'missing');
    expect(paneId).toBeNull();
  });

  test('finds the holding pane in a deep tree', () => {
    const inner = split('s2', 'vertical', 0.5, pane('p2', ['x', 'y'], 'x'), pane('p3', ['z']));
    const root = split('s1', 'horizontal', 0.5, pane('p1', ['a']), inner);
    const { paneId } = removeTab(root, 'y');
    expect(paneId).toBe('p2');
  });
});

describe('moveTab', () => {
  test('moves a tab to a different pane and activates it there', () => {
    const root = split(
      's1',
      'horizontal',
      0.5,
      pane('p1', ['a', 'b'], 'a'),
      pane('p2', ['c'], 'c'),
    );
    const next = moveTab(root, 'b', 'p2', 0);
    const left = findPane(next, 'p1')!;
    const right = findPane(next, 'p2')!;
    expect(left.tab_ids).toEqual(['a']);
    expect(right.tab_ids).toEqual(['b', 'c']);
    expect(right.active_tab_id).toBe('b');
  });

  test('reorders within the same pane', () => {
    const root = pane('p1', ['a', 'b', 'c'], 'b');
    const next = moveTab(root, 'a', 'p1', 2);
    const p = next as PaneNode;
    expect(p.tab_ids).toEqual(['b', 'c', 'a']);
  });
});

describe('setActiveTabId', () => {
  test('sets active tab when present in the pane', () => {
    const root = pane('p1', ['a', 'b'], 'a');
    const next = setActiveTabId(root, 'p1', 'b') as PaneNode;
    expect(next.active_tab_id).toBe('b');
  });

  test('no-ops when the tab is not in the pane', () => {
    const root = pane('p1', ['a', 'b'], 'a');
    const next = setActiveTabId(root, 'p1', 'missing') as PaneNode;
    expect(next).toBe(root);
  });
});

describe('splitPane', () => {
  test('wraps a single root pane in a split with the dragged tab on the right', () => {
    const root = pane('p1', ['a', 'b', 'c'], 'a');
    const result = splitPane(root, 'p1', 'horizontal', 'b');
    expect(result).not.toBeNull();
    const tree = result!.tree as SplitNode;
    expect(tree.type).toBe('split');
    expect(tree.direction).toBe('horizontal');
    const left = tree.first as PaneNode;
    const right = tree.second as PaneNode;
    expect(left.id).toBe('p1');
    expect(left.tab_ids).toEqual(['a', 'c']);
    expect(left.active_tab_id).toBe('a');
    expect(right.tab_ids).toEqual(['b']);
    expect(right.active_tab_id).toBe('b');
    expect(right.id).toBe(result!.newPaneId);
  });

  test('splits a deeper pane without disturbing siblings', () => {
    const left = pane('p1', ['a']);
    const right = pane('p2', ['b', 'c'], 'b');
    const root = split('s1', 'horizontal', 0.5, left, right);
    const result = splitPane(root, 'p2', 'vertical', 'c');
    const tree = result!.tree as SplitNode;
    expect(tree.id).toBe('s1');
    expect(tree.first).toBe(left);
    const newSplit = tree.second as SplitNode;
    expect(newSplit.type).toBe('split');
    expect(newSplit.direction).toBe('vertical');
    expect((newSplit.first as PaneNode).tab_ids).toEqual(['b']);
    expect((newSplit.second as PaneNode).tab_ids).toEqual(['c']);
  });

  test('returns null when the dragged tab is not in the target pane', () => {
    const root = pane('p1', ['a']);
    expect(splitPane(root, 'p1', 'horizontal', 'missing')).toBeNull();
  });

  test('returns null when the target pane does not exist', () => {
    const root = pane('p1', ['a']);
    expect(splitPane(root, 'pX', 'horizontal', 'a')).toBeNull();
  });

  test('promotes the next available tab when dragging the active tab out', () => {
    const root = pane('p1', ['a', 'b'], 'b');
    const result = splitPane(root, 'p1', 'horizontal', 'b');
    const left = (result!.tree as SplitNode).first as PaneNode;
    expect(left.active_tab_id).toBe('a');
  });
});

describe('closePane', () => {
  test('returns tree unchanged when paneId is the root', () => {
    const root = pane('p1', ['a']);
    expect(closePane(root, 'p1')).toBe(root);
  });

  test('replaces parent split with the surviving sibling', () => {
    const left = pane('p1', ['a']);
    const right = pane('p2', ['b']);
    const root = split('s1', 'horizontal', 0.5, left, right);
    expect(closePane(root, 'p1')).toBe(right);
    expect(closePane(root, 'p2')).toBe(left);
  });

  test('rebalances a deeper subtree, preserving sibling layout', () => {
    const innerLeft = pane('p2', ['x']);
    const innerRight = pane('p3', ['y']);
    const inner = split('s2', 'vertical', 0.4, innerLeft, innerRight);
    const root = split('s1', 'horizontal', 0.5, pane('p1', ['a']), inner);
    const next = closePane(root, 'p2') as SplitNode;
    expect(next.id).toBe('s1');
    expect(next.first).toEqual(pane('p1', ['a']));
    expect(next.second).toBe(innerRight);
  });

  test('returns tree unchanged when paneId is not in the tree', () => {
    const root = split('s1', 'horizontal', 0.5, pane('p1', ['a']), pane('p2', ['b']));
    expect(closePane(root, 'pX')).toBe(root);
  });
});

describe('setSplitRatio', () => {
  test('updates the ratio on the named split', () => {
    const root = split('s1', 'horizontal', 0.5, pane('p1', ['a']), pane('p2', ['b']));
    const next = setSplitRatio(root, 's1', 0.3) as SplitNode;
    expect(next.ratio).toBeCloseTo(0.3);
  });

  test('clamps very small or very large ratios', () => {
    const root = split('s1', 'horizontal', 0.5, pane('p1', ['a']), pane('p2', ['b']));
    expect((setSplitRatio(root, 's1', 0.001) as SplitNode).ratio).toBeCloseTo(0.05);
    expect((setSplitRatio(root, 's1', 0.999) as SplitNode).ratio).toBeCloseTo(0.95);
  });

  test('descends into nested splits', () => {
    const inner = split('s2', 'vertical', 0.5, pane('p2', ['x']), pane('p3', ['y']));
    const root = split('s1', 'horizontal', 0.5, pane('p1', ['a']), inner);
    const next = setSplitRatio(root, 's2', 0.7) as SplitNode;
    expect((next.second as SplitNode).ratio).toBeCloseTo(0.7);
    expect(next.ratio).toBeCloseTo(0.5);
  });
});

describe('eachPane / firstPane / findPaneContainingTab', () => {
  test('eachPane yields panes in document order', () => {
    const root = split(
      's1',
      'horizontal',
      0.5,
      pane('p1', ['a']),
      split('s2', 'vertical', 0.5, pane('p2', ['b']), pane('p3', ['c'])),
    );
    expect(firstPane(root).id).toBe('p1');
    expect(findPaneContainingTab(root, 'b')!.id).toBe('p2');
    expect(findPaneContainingTab(root, 'c')!.id).toBe('p3');
    expect(findPaneContainingTab(root, 'missing')).toBeNull();
  });
});

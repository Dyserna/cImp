// Tests for the splitter-drag math (layout/resize.ts). The core contract:
// dragging a divider changes the size of ONLY the two panes directly
// touching it — every other pane keeps its absolute pixel size. Sizes are
// verified by re-deriving each pane's px from the returned ratios with the
// same flex model Split.svelte renders (child px = ratio * (container -
// splitter)), so the assertions catch ratio math that "looks right" but
// renders wrong.

import { describe, expect, test } from 'vitest';
import { planSplitterDrag, ratiosForOffset } from './resize';
import { SPLITTER_THICKNESS_PX } from './constants';
import type {
  LayoutNode,
  PaneNode,
  SplitDirection,
  SplitNode,
} from './types';

function pane(id: string): PaneNode {
  return { type: 'pane', id, tab_ids: [], active_tab_id: null };
}

function split(
  id: string,
  direction: SplitDirection,
  ratio: number,
  first: LayoutNode,
  second: LayoutNode,
): SplitNode {
  return { type: 'split', id, direction, ratio, first, second };
}

/// Each pane's pixel extent along `axis`, given the tree and a set of
/// ratio overrides (the drag's updates). Mirrors the renderer: a split in
/// the measured axis divides (container - splitter) by ratio; a
/// perpendicular split passes the full extent to both children.
function paneSizes(
  node: LayoutNode,
  sizePx: number,
  axis: SplitDirection,
  ratios: Map<string, number>,
): Record<string, number> {
  if (node.type === 'pane') return { [node.id]: sizePx };
  const r = ratios.get(node.id) ?? node.ratio;
  if (node.direction !== axis) {
    return {
      ...paneSizes(node.first, sizePx, axis, ratios),
      ...paneSizes(node.second, sizePx, axis, ratios),
    };
  }
  const inner = sizePx - SPLITTER_THICKNESS_PX;
  return {
    ...paneSizes(node.first, r * inner, axis, ratios),
    ...paneSizes(node.second, (1 - r) * inner, axis, ratios),
  };
}

function toMap(updates: ReadonlyArray<{ id: string; ratio: number }>): Map<string, number> {
  return new Map(updates.map((u) => [u.id, u.ratio]));
}

const MIN = 200;

describe('balanced 4-column layout: split(split(a,b), split(c,d))', () => {
  const tree = split(
    'root',
    'horizontal',
    0.5,
    split('s1', 'horizontal', 0.5, pane('a'), pane('b')),
    split('s2', 'horizontal', 0.5, pane('c'), pane('d')),
  );
  const total = 2404; // inner 2400 → subtrees 1200 → panes 598 each

  test('dragging the middle divider resizes only b and c', () => {
    const before = paneSizes(tree, total, 'horizontal', new Map());
    expect(before.a).toBeCloseTo(598);

    const plan = planSplitterDrag(tree, total, 0.5, MIN)!;
    // Divider sits at firstPx0; drag it 100px right.
    const after = paneSizes(
      tree,
      total,
      'horizontal',
      toMap(ratiosForOffset(plan, plan.firstPx0 + 100)),
    );
    expect(after.a).toBeCloseTo(before.a); // outer panes untouched
    expect(after.d).toBeCloseTo(before.d);
    expect(after.b).toBeCloseTo(before.b + 100); // adjacent panes absorb it
    expect(after.c).toBeCloseTo(before.c - 100);
  });

  test('drag clamps so the shrinking adjacent pane stops at min size', () => {
    const plan = planSplitterDrag(tree, total, 0.5, MIN)!;
    const after = paneSizes(
      tree,
      total,
      'horizontal',
      toMap(ratiosForOffset(plan, plan.firstPx0 + 1_000_000)),
    );
    expect(after.c).toBeCloseTo(MIN); // c hit the min, not 0
    expect(after.d).toBeCloseTo(598); // d still untouched at the clamp
    expect(after.a).toBeCloseTo(598);
  });

  test('dragging left mirrors: only b shrinks, a and d fixed', () => {
    const before = paneSizes(tree, total, 'horizontal', new Map());
    const plan = planSplitterDrag(tree, total, 0.5, MIN)!;
    const after = paneSizes(
      tree,
      total,
      'horizontal',
      toMap(ratiosForOffset(plan, plan.firstPx0 - 150)),
    );
    expect(after.a).toBeCloseTo(before.a);
    expect(after.b).toBeCloseTo(before.b - 150);
    expect(after.c).toBeCloseTo(before.c + 150);
    expect(after.d).toBeCloseTo(before.d);
  });
});

describe('right-leaning 4-column layout: split(a, split(b, split(c,d)))', () => {
  const s3 = split('s3', 'horizontal', 0.5, pane('c'), pane('d'));
  const s2 = split('s2', 'horizontal', 0.5, pane('b'), s3);
  const tree = split('root', 'horizontal', 1 / 3, pane('a'), s2);
  const total = 1804; // inner 1800 → a 600, s2 1200 → b 598, s3 598 → c,d 297

  test('dragging the b|c divider (s2) leaves a and d fixed', () => {
    const before = paneSizes(tree, total, 'horizontal', new Map());
    expect(before.b).toBeCloseTo(598);
    expect(before.c).toBeCloseTo(297);

    // s2's own container is the root's second child: 1200px.
    const s2Total = (total - SPLITTER_THICKNESS_PX) * (1 - 1 / 3);
    const plan = planSplitterDrag(s2, s2Total, 0.5, MIN)!;
    const after = paneSizes(
      tree,
      total,
      'horizontal',
      toMap(ratiosForOffset(plan, plan.firstPx0 + 50)),
    );
    expect(after.a).toBeCloseTo(before.a);
    expect(after.b).toBeCloseTo(before.b + 50);
    expect(after.c).toBeCloseTo(before.c - 50);
    expect(after.d).toBeCloseTo(before.d);
  });

  test('dragging the innermost divider (s3) touches only c and d', () => {
    const before = paneSizes(tree, total, 'horizontal', new Map());
    const s3Total = ((total - SPLITTER_THICKNESS_PX) * (2 / 3) - SPLITTER_THICKNESS_PX) * 0.5;
    const plan = planSplitterDrag(s3, s3Total, 0.5, MIN)!;
    const after = paneSizes(
      tree,
      total,
      'horizontal',
      toMap(ratiosForOffset(plan, plan.firstPx0 + 40)),
    );
    expect(after.a).toBeCloseTo(before.a);
    expect(after.b).toBeCloseTo(before.b);
    expect(after.c).toBeCloseTo(before.c + 40);
    expect(after.d).toBeCloseTo(before.d - 40);
  });
});

describe('perpendicular subtree at the divider', () => {
  test('a vertical stack next to the divider resizes as one unit', () => {
    // split(vertical(a,b), c): a and b BOTH touch the divider, so both
    // widths must follow it; c is the other adjacent unit.
    const tree = split(
      'root',
      'horizontal',
      0.5,
      split('v1', 'vertical', 0.5, pane('a'), pane('b')),
      pane('c'),
    );
    const total = 1204; // inner 1200 → each side 600
    const plan = planSplitterDrag(tree, total, 0.5, MIN)!;
    expect(plan.chainFirst).toHaveLength(0); // perpendicular split ends the chain
    const after = paneSizes(
      tree,
      total,
      'horizontal',
      toMap(ratiosForOffset(plan, plan.firstPx0 + 100)),
    );
    expect(after.a).toBeCloseTo(700);
    expect(after.b).toBeCloseTo(700);
    expect(after.c).toBeCloseTo(500);
  });
});

describe('clamping edge cases', () => {
  test('an adjacent pane already below min can grow but not shrink', () => {
    // b is 100px — under the 200px min (window was squeezed). The divider
    // may only move left (grow b), never right.
    const total = 904; // inner 900
    const ratio = 800 / 900; // a=800, b=100
    const tree = split('root', 'horizontal', ratio, pane('a'), pane('b'));
    const plan = planSplitterDrag(tree, total, ratio, MIN)!;
    expect(plan.maxDelta).toBe(0); // cannot squeeze b further
    expect(plan.minDelta).toBeLessThan(0); // but b may grow
  });

  test('degraded container below 2*min keeps the divider draggable', () => {
    const tree = split('root', 'horizontal', 0.5, pane('a'), pane('b'));
    const plan = planSplitterDrag(tree, 300, 0.5, MIN)!;
    // 300 < 2*200: min collapses to 0 so the user isn't frozen.
    expect(plan.minDelta).toBeLessThan(0);
    expect(plan.maxDelta).toBeGreaterThan(0);
  });

  test('returns null when the container has no draggable space', () => {
    const tree = split('root', 'horizontal', 0.5, pane('a'), pane('b'));
    expect(planSplitterDrag(tree, 0, 0.5, MIN)).toBeNull();
  });
});

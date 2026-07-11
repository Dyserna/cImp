// Pure math for the splitter drag (Split.svelte). Dragging a divider must
// resize ONLY the two panes directly touching it. The layout is a binary
// split tree, so a 4-column layout is nested splits — and naively changing
// one split's ratio rescales every pane inside both of its subtrees
// proportionally (the original bug: dragging the divider between columns
// 2 and 3 also resized columns 1 and 4). To keep non-adjacent panes at
// their absolute size, the drag also rewrites the ratios of the chain of
// same-direction splits that touch the divider on each side, so the whole
// pixel delta rides down each chain to the single divider-adjacent pane.
//
// A perpendicular split ends a chain: both of its children physically
// touch the divider, so that whole subtree is the adjacent unit and
// resizes as one (unavoidable — its children share the drag axis).
//
// Factored out of Split.svelte so the arithmetic is unit-testable without
// mounting the component (same pattern as usageMath.ts).

import { SPLITTER_THICKNESS_PX } from './constants';
import type { LayoutNode, SplitDirection, SplitId, SplitNode } from './types';

export interface RatioUpdate {
  id: SplitId;
  ratio: number;
}

interface ChainEntry {
  id: SplitId;
  /// The divider-far child's pixel size at drag start — held constant.
  fixedPx: number;
  /// The split's inner size (its container minus its own splitter bar)
  /// at drag start.
  innerPx: number;
}

export interface DragPlan {
  /// The dragged split itself.
  splitId: SplitId;
  /// Its inner size (container minus its splitter bar).
  innerPx: number;
  /// Its first child's pixel size at drag start.
  firstPx0: number;
  /// Clamp bounds for the drag delta, keeping the two divider-adjacent
  /// panes at or above the min pane size.
  minDelta: number;
  maxDelta: number;
  /// Same-direction splits between the divider and the adjacent pane,
  /// inside the first / second subtree respectively.
  chainFirst: ChainEntry[];
  chainSecond: ChainEntry[];
}

/// Walk from `node` toward the dragged divider, collecting every
/// same-direction split on the way. `adjacentChild` names which child of
/// each chain split touches the divider: 'second' inside the dragged
/// split's first subtree, 'first' inside its second subtree. Stops at a
/// pane or a perpendicular split; the remaining `adjacentPx` is the
/// divider-adjacent unit's size — the thing that absorbs the delta.
///
/// Sizes derive from stored ratios, not measured DOM rects. They can
/// diverge from the rendered pixels only while a nested Split's visual
/// min-size clamp is active (window squeezed below the min-size
/// invariant) — a degraded state the drag clamp already tolerates.
function collectChain(
  node: LayoutNode,
  sizePx: number,
  adjacentChild: 'first' | 'second',
  direction: SplitDirection,
): { entries: ChainEntry[]; adjacentPx: number } {
  const entries: ChainEntry[] = [];
  let cur = node;
  let size = sizePx;
  while (cur.type === 'split' && cur.direction === direction) {
    const inner = size - SPLITTER_THICKNESS_PX;
    if (inner <= 0) break;
    const fixedPx = adjacentChild === 'second' ? cur.ratio * inner : (1 - cur.ratio) * inner;
    entries.push({ id: cur.id, fixedPx, innerPx: inner });
    size = inner - fixedPx;
    cur = adjacentChild === 'second' ? cur.second : cur.first;
  }
  return { entries, adjacentPx: size };
}

/// Snapshot everything a drag needs, once, at mousedown. `renderedRatio`
/// is the split's on-screen ratio (Split.svelte's render-clamped value),
/// which anchors the plan to where the divider actually sits; `totalPx`
/// is the split container's size along the drag axis. Returns `null` when
/// the container is too small to have any draggable space.
export function planSplitterDrag(
  split: SplitNode,
  totalPx: number,
  renderedRatio: number,
  minPanePx: number,
): DragPlan | null {
  const innerPx = totalPx - SPLITTER_THICKNESS_PX;
  if (innerPx <= 0) return null;
  const firstPx0 = renderedRatio * innerPx;
  const first = collectChain(split.first, firstPx0, 'second', split.direction);
  const second = collectChain(split.second, innerPx - firstPx0, 'first', split.direction);
  // Degraded container (can't fit two min-size panes): let the divider
  // move freely across the adjacent panes instead of freezing — matches
  // the pre-existing top-level behavior for too-small windows.
  const minPx = totalPx < 2 * minPanePx ? 0 : minPanePx;
  return {
    splitId: split.id,
    innerPx,
    firstPx0,
    // When an adjacent pane is ALREADY below min (squeezed window), the
    // min/max guards collapse toward 0 on that side — the divider can
    // only move away from it, never squeeze it further.
    minDelta: Math.min(0, minPx - first.adjacentPx),
    maxDelta: Math.max(0, second.adjacentPx - minPx),
    chainFirst: first.entries,
    chainSecond: second.entries,
  };
}

/// Ratio updates for the divider dragged to `offsetPx` from the split
/// container's leading edge. The dragged split's ratio follows the
/// cursor; each chain split's ratio is recomputed so its divider-far
/// child keeps its drag-start pixel size, i.e. the delta passes through
/// untouched to the divider-adjacent pane. Pure function of the plan and
/// the live cursor offset, so errors can't accumulate across mousemoves.
export function ratiosForOffset(plan: DragPlan, offsetPx: number): RatioUpdate[] {
  const delta = Math.max(plan.minDelta, Math.min(plan.maxDelta, offsetPx - plan.firstPx0));
  const updates: RatioUpdate[] = [
    { id: plan.splitId, ratio: (plan.firstPx0 + delta) / plan.innerPx },
  ];
  for (const c of plan.chainFirst) {
    const inner = c.innerPx + delta;
    if (inner > 0) updates.push({ id: c.id, ratio: c.fixedPx / inner });
  }
  for (const c of plan.chainSecond) {
    const inner = c.innerPx - delta;
    if (inner > 0) updates.push({ id: c.id, ratio: (inner - c.fixedPx) / inner });
  }
  return updates;
}

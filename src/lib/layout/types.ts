// Layout-tree node types. The content area renders a binary tree where
// internal nodes are splits (with a direction and ratio) and leaves are
// panes (each with its own ordered tab list and active tab). See
// docs/DESIGN-V4.md "Layout Tree" for the contract this enforces.
//
// Direction convention: `horizontal` arranges children side-by-side (a
// vertical splitter between them, matching CSS `flex-direction: row`).
// `vertical` stacks children top-to-bottom. This is the opposite of
// tmux's `split-window -h` naming; we picked the CSS-flexbox convention.

import type { TabId } from '../tabs/types';

export type SplitId = string;
export type PaneId = string;
export type SplitDirection = 'horizontal' | 'vertical';

export interface SplitNode {
  type: 'split';
  id: SplitId;
  direction: SplitDirection;
  /// First child's share of the available space, in `0.0..1.0`. The
  /// second child gets `1 - ratio`. Bounds are not clamped here; callers
  /// (`setSplitRatio`, the splitter drag handler in M3) are responsible
  /// for keeping ratios sensible.
  ratio: number;
  first: LayoutNode;
  second: LayoutNode;
}

export interface PaneNode {
  type: 'pane';
  id: PaneId;
  tab_ids: TabId[];
  /// `null` only when `tab_ids` is empty. Empty panes are transient
  /// during move operations; the lifecycle layer is responsible for
  /// collapsing non-root empty panes promptly.
  active_tab_id: TabId | null;
}

export type LayoutNode = SplitNode | PaneNode;

export interface LayoutState {
  tree: LayoutNode;
  focused_pane_id: PaneId;
}

let idCounter = 0;

/// Generate a unique pane id. Collisions are not a concern across launches —
/// ids are not persisted in M1, and M4's persistence layer will round-trip
/// whatever ids are in memory.
export function newPaneId(): PaneId {
  idCounter += 1;
  return `pane-${Date.now().toString(36)}-${idCounter.toString(36)}`;
}

export function newSplitId(): SplitId {
  idCounter += 1;
  return `split-${Date.now().toString(36)}-${idCounter.toString(36)}`;
}

/// Test-only id reset so deterministic ids land in `tree.test.ts`. Never
/// call this from production code.
export function _resetIdCounterForTests(): void {
  idCounter = 0;
}

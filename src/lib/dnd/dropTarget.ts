// Drop-target hit-testing. Given a cursor position and the source
// pane id of the in-flight drag, returns the drop zone the cursor is
// currently over (or `null` if outside any pane).
//
// The pure `computeZone` function takes geometry as plain inputs so
// it's unit-testable without DOM. The exported `computeDropTarget`
// wrapper pulls live geometry from `paneRegistry` (which reads
// `getBoundingClientRect` on demand) and delegates.
//
// Zone math (matches DESIGN-V4 / VS Code):
//   - Tab bar takes priority over content-area edges. The tab-bar zone
//     means "reorder" if the source pane is the same; otherwise
//     "moveToPane" — the user is dragging onto a different pane's tab
//     bar, which inserts at the end.
//   - Content area uses 25% edge fractions: left/right/top/bottom 25%
//     are split zones; center 50% × 50% is move-to-pane.
//   - The split-top zone is anchored to the *content area* (below the
//     tab bar), not the pane's full top edge — otherwise dropping near
//     the top would ambiguously hit both "tab bar" and "split-top".

import { paneRegistry } from '../layout/registry';
import type { PaneId } from '../layout/types';
import type { TabId } from '../tabs/types';
import type { DropTarget } from './types';

const EDGE_FRACTION = 0.25;

interface SimpleRect {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

interface TabRect {
  tabId: TabId;
  left: number;
  right: number;
}

export interface ZoneGeometry {
  paneId: PaneId;
  paneRect: SimpleRect;
  /// `null` if the pane has no registered tab bar (transient during
  /// mount). Treated as "no tab-bar zone, the entire pane is the
  /// content area for split/center math."
  tabBarRect: SimpleRect | null;
  /// In DOM order. Empty when the tab bar is absent.
  tabRects: TabRect[];
}

/// Pure zone resolver. Returns `null` when (x, y) falls outside the
/// pane's bounding rect — the caller is expected to have already
/// verified the cursor is over this pane via `findUnderCursor`, so
/// `null` here is the rare race-condition case (rect changed between
/// the hit-test and the call).
export function computeZone(
  x: number,
  y: number,
  sourcePaneId: PaneId,
  geom: ZoneGeometry,
): DropTarget | null {
  const { paneId, paneRect, tabBarRect, tabRects } = geom;
  if (
    x < paneRect.left ||
    x > paneRect.right ||
    y < paneRect.top ||
    y > paneRect.bottom
  ) {
    return null;
  }

  if (tabBarRect && y >= tabBarRect.top && y <= tabBarRect.bottom) {
    if (paneId === sourcePaneId) {
      return { kind: 'reorder', paneId, insertIndex: insertIndexAt(x, tabRects) };
    }
    return { kind: 'moveToPane', paneId };
  }

  // Anchor the content-area math below the tab bar so split-top can't
  // overlap the tab-bar zone. If there's no tab bar, use the pane top.
  const contentTop = tabBarRect ? tabBarRect.bottom : paneRect.top;
  const contentLeft = paneRect.left;
  const contentWidth = paneRect.right - paneRect.left;
  const contentHeight = paneRect.bottom - contentTop;
  if (contentHeight <= 0 || contentWidth <= 0) return null;

  const rx = (x - contentLeft) / contentWidth;
  const ry = (y - contentTop) / contentHeight;

  if (rx < EDGE_FRACTION) return { kind: 'split', paneId, direction: 'left' };
  if (rx > 1 - EDGE_FRACTION) return { kind: 'split', paneId, direction: 'right' };
  if (ry < EDGE_FRACTION) return { kind: 'split', paneId, direction: 'top' };
  if (ry > 1 - EDGE_FRACTION) return { kind: 'split', paneId, direction: 'bottom' };
  return { kind: 'moveToPane', paneId };
}

/// First tab whose horizontal center is to the right of `x`. If past
/// every tab, returns `tabRects.length` (insert at end).
function insertIndexAt(x: number, tabRects: readonly TabRect[]): number {
  for (let i = 0; i < tabRects.length; i++) {
    const center = (tabRects[i].left + tabRects[i].right) / 2;
    if (x < center) return i;
  }
  return tabRects.length;
}

/// Live wrapper. Pulls geometry from the DOM registry and runs the
/// pure zone resolver. Returns `null` when the cursor isn't over any
/// registered pane.
export function computeDropTarget(
  x: number,
  y: number,
  sourcePaneId: PaneId,
): DropTarget | null {
  const paneId = paneRegistry.findUnderCursor(x, y);
  if (!paneId) return null;
  const paneRect = paneRegistry.getPaneRect(paneId);
  if (!paneRect) return null;
  const tabBarRect = paneRegistry.getTabBarRect(paneId);
  const tabBarEl = paneRegistry.getTabBarElement(paneId);
  const tabRects: TabRect[] = [];
  if (tabBarEl) {
    const tabs = tabBarEl.querySelectorAll<HTMLElement>('[data-tab-id]');
    for (const t of tabs) {
      const r = t.getBoundingClientRect();
      const id = t.dataset.tabId;
      if (id) tabRects.push({ tabId: id as TabId, left: r.left, right: r.right });
    }
  }
  return computeZone(x, y, sourcePaneId, {
    paneId,
    paneRect,
    tabBarRect,
    tabRects,
  });
}

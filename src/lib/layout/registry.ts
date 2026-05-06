// DOM registry for live pane and tab-bar elements. The drag-and-drop
// layer needs three kinds of geometric queries on every mousemove:
//
//   1. Which pane is under the cursor? (5+ panes worst case; trivially
//      fast even at 60fps mousemove.)
//   2. What is a given pane's bounding rect? (For zone math: which 25%
//      edge or 50% center the cursor falls into.)
//   3. What is a given pane's tab-bar bounding rect? (Tab-bar drops
//      reorder/move; non-tab-bar drops split. The split-top zone has
//      to live entirely *below* the tab bar to disambiguate.)
//
// Rather than thread element refs through every drag store update,
// each Pane and TabBar registers itself on mount and unregisters on
// destroy. The drag layer reads from this module directly. Rects come
// from `getBoundingClientRect` at query time so window resizes and
// mid-drag splitter moves stay correct without re-registering.

import type { PaneId } from './types';

class PaneRegistry {
  private panes = new Map<PaneId, HTMLElement>();
  private tabBars = new Map<PaneId, HTMLElement>();

  /// Pane.svelte calls this with its root element on mount and with
  /// `null` on destroy. Re-registration with the same id is treated as
  /// an update — useful if a pane's element ref ever changes (it
  /// shouldn't, but the API is robust to it).
  setPaneElement(id: PaneId, el: HTMLElement | null): void {
    if (el) this.panes.set(id, el);
    else this.panes.delete(id);
  }

  /// TabBar.svelte calls this with its root .tab-bar element on mount
  /// and with `null` on destroy.
  setTabBarElement(id: PaneId, el: HTMLElement | null): void {
    if (el) this.tabBars.set(id, el);
    else this.tabBars.delete(id);
  }

  /// First pane whose bounding rect contains the cursor. Iteration
  /// order is insertion order — this matters when nested splits cause
  /// any geometric overlap (which they shouldn't given the tree's flex
  /// containment, but the deterministic walk avoids races during
  /// transient mid-drag rebalances). Returns null when the cursor is
  /// over an inter-pane gutter (splitter) or outside the content area.
  findUnderCursor(x: number, y: number): PaneId | null {
    for (const [id, el] of this.panes) {
      const r = el.getBoundingClientRect();
      if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) {
        return id;
      }
    }
    return null;
  }

  /// Live bounding rect of a registered pane. Null when the pane was
  /// unregistered between the cursor hit-test and the rect query (race
  /// during a split-induced rerender, for instance) — caller should
  /// treat null as "drop target gone."
  getPaneRect(id: PaneId): DOMRect | null {
    const el = this.panes.get(id);
    return el ? el.getBoundingClientRect() : null;
  }

  /// Live bounding rect of a registered tab bar. Null when the pane is
  /// transient (its TabBar hasn't mounted yet) or when only the pane
  /// element is registered. The drop-target code falls back to "no
  /// tab-bar zone, full content area is split-or-center" in that case.
  getTabBarRect(id: PaneId): DOMRect | null {
    const el = this.tabBars.get(id);
    return el ? el.getBoundingClientRect() : null;
  }

  /// The tab-bar element itself, for callers that need to walk its
  /// child tabs (e.g. reorder-index resolution). Null when not
  /// registered.
  getTabBarElement(id: PaneId): HTMLElement | null {
    return this.tabBars.get(id) ?? null;
  }
}

export const paneRegistry = new PaneRegistry();

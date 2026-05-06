// Pure-data tests for the zone resolver. The wrapper that pulls live
// geometry from `paneRegistry` is tested implicitly by the manual
// verification checklist; the zone math is what actually carries
// risk and benefits from per-region coverage.
//
// Geometry convention used throughout: pane is 1000×600 anchored at
// (100, 100). Tab bar is the top 32px of the pane (matching the live
// CSS height: 32px). Content area is the rest (568px tall).

import { describe, expect, test } from 'vitest';
import { computeZone, type ZoneGeometry } from './dropTarget';

const PANE: ZoneGeometry['paneRect'] = {
  left: 100,
  top: 100,
  right: 1100,
  bottom: 700,
};

const TABBAR: NonNullable<ZoneGeometry['tabBarRect']> = {
  left: 100,
  top: 100,
  right: 1100,
  bottom: 132,
};

function geom(overrides: Partial<ZoneGeometry> = {}): ZoneGeometry {
  return {
    paneId: 'p1',
    paneRect: PANE,
    tabBarRect: TABBAR,
    tabRects: [],
    ...overrides,
  };
}

describe('computeZone', () => {
  test('returns null when cursor is outside the pane rect', () => {
    expect(computeZone(50, 50, 'p1', geom())).toBeNull();
    expect(computeZone(1500, 400, 'p1', geom())).toBeNull();
  });

  describe('tab-bar zone', () => {
    test('reorder when source pane equals target pane', () => {
      const tabs = [
        { tabId: 'a', left: 100, right: 180 },
        { tabId: 'b', left: 180, right: 260 },
        { tabId: 'c', left: 260, right: 340 },
      ];
      // Cursor at 200 (between a/b centers): inserts at index 1
      const r = computeZone(200, 116, 'p1', geom({ tabRects: tabs }));
      expect(r).toEqual({ kind: 'reorder', paneId: 'p1', insertIndex: 1 });
    });

    test('reorder past last tab inserts at the end', () => {
      const tabs = [
        { tabId: 'a', left: 100, right: 180 },
        { tabId: 'b', left: 180, right: 260 },
      ];
      const r = computeZone(500, 116, 'p1', geom({ tabRects: tabs }));
      expect(r).toEqual({ kind: 'reorder', paneId: 'p1', insertIndex: 2 });
    });

    test('reorder before the first tab inserts at index 0', () => {
      const tabs = [
        { tabId: 'a', left: 100, right: 180 },
        { tabId: 'b', left: 180, right: 260 },
      ];
      // Cursor at 110 (left of a's center 140): index 0
      const r = computeZone(110, 116, 'p1', geom({ tabRects: tabs }));
      expect(r).toEqual({ kind: 'reorder', paneId: 'p1', insertIndex: 0 });
    });

    test('moveToPane when source differs from target', () => {
      const r = computeZone(500, 116, 'pSource', geom());
      expect(r).toEqual({ kind: 'moveToPane', paneId: 'p1' });
    });

    test('tab-bar zone takes priority over split-top', () => {
      // y=116 is inside tab bar AND would be in the top 25% of pane —
      // the tab-bar zone wins.
      const r = computeZone(600, 116, 'pSource', geom());
      expect(r).toEqual({ kind: 'moveToPane', paneId: 'p1' });
    });
  });

  describe('content-area splits', () => {
    test('left edge → split-left', () => {
      // contentLeft=100, contentWidth=1000. Left 25% is 100..350.
      const r = computeZone(200, 400, 'pSource', geom());
      expect(r).toEqual({ kind: 'split', paneId: 'p1', direction: 'left' });
    });

    test('right edge → split-right', () => {
      // Right 25% starts at 850.
      const r = computeZone(1000, 400, 'pSource', geom());
      expect(r).toEqual({ kind: 'split', paneId: 'p1', direction: 'right' });
    });

    test('top of content area → split-top (below tab bar)', () => {
      // contentTop=132, contentHeight=568. Top 25% is 132..274.
      const r = computeZone(600, 200, 'pSource', geom());
      expect(r).toEqual({ kind: 'split', paneId: 'p1', direction: 'top' });
    });

    test('bottom edge → split-bottom', () => {
      // Bottom 25% is 558..700.
      const r = computeZone(600, 650, 'pSource', geom());
      expect(r).toEqual({ kind: 'split', paneId: 'p1', direction: 'bottom' });
    });

    test('center of content area → moveToPane', () => {
      // (600, 400) — well inside the 50% × 50% center.
      const r = computeZone(600, 400, 'pSource', geom());
      expect(r).toEqual({ kind: 'moveToPane', paneId: 'p1' });
    });

    test('left-edge precedence over top-edge in the corner', () => {
      // (200, 200): rx=0.1 (left), ry=(200-132)/568≈0.12 (top).
      // Order in the resolver: left checked first → split-left.
      const r = computeZone(200, 200, 'pSource', geom());
      expect(r).toEqual({ kind: 'split', paneId: 'p1', direction: 'left' });
    });
  });

  describe('without a tab bar', () => {
    test('full pane treated as content area', () => {
      // Pane top is 100; without tab bar, top 25% is 100..250.
      const r = computeZone(600, 150, 'pSource', geom({ tabBarRect: null }));
      expect(r).toEqual({ kind: 'split', paneId: 'p1', direction: 'top' });
    });
  });
});

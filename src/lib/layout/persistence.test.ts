// Tests for `validateAndRepairLayout` — the load-time integrity sieve
// that adapts a persisted layout to whatever the live tab list happens
// to be after the user added/removed tabs since the last save.

import { beforeEach, describe, expect, test } from 'vitest';
import { defaultLayoutForTabs, leftmostLeafPaneId, validateAndRepairLayout } from './persistence';
import { _resetIdCounterForTests, type LayoutNode } from './types';
import type { LayoutPersisted, TabConfig } from '../settings/types';

beforeEach(() => {
  _resetIdCounterForTests();
});

function shellTab(id: string): TabConfig {
  return {
    kind: 'shell',
    id,
    builtin: false,
    name: id,
    command: '/bin/sh',
    args: [],
    cwd: null,
    env: {},
    notifications: {
      error: { enabled: false, text: '' },
      exited: { enabled: false, text: '' },
    },
    theme_override: null,
    background_override: null,
  };
}

function pane(id: string, tab_ids: string[], active: string | null): LayoutNode {
  return { type: 'pane', id, tab_ids, active_tab_id: active };
}

function split(id: string, first: LayoutNode, second: LayoutNode): LayoutNode {
  return { type: 'split', id, direction: 'horizontal', ratio: 0.5, first, second };
}

describe('validateAndRepairLayout', () => {
  test('drops tab ids no longer in settings.tabs', () => {
    const persisted: LayoutPersisted = {
      tree: pane('p1', ['tabA', 'tabB', 'tabC'], 'tabB'),
      focused_pane_id: 'p1',
    };
    const tabs = [shellTab('tabA'), shellTab('tabC')];
    const out = validateAndRepairLayout(persisted, tabs);
    expect(out.tree.type).toBe('pane');
    if (out.tree.type === 'pane') {
      expect(out.tree.tab_ids).toEqual(['tabA', 'tabC']);
      // tabB was active and got dropped → falls back to first remaining.
      expect(out.tree.active_tab_id).toBe('tabA');
    }
  });

  test('places orphan tabs at the end of the focused pane', () => {
    const persisted: LayoutPersisted = {
      tree: split(
        's1',
        pane('p1', ['tabA'], 'tabA'),
        pane('p2', ['tabB'], 'tabB'),
      ),
      focused_pane_id: 'p2',
    };
    // tabC was created after the preset save; it's an orphan.
    const tabs = [shellTab('tabA'), shellTab('tabB'), shellTab('tabC')];
    const out = validateAndRepairLayout(persisted, tabs);
    expect(out.focused_pane_id).toBe('p2');
    expect(out.tree.type).toBe('split');
    if (out.tree.type === 'split' && out.tree.second.type === 'pane') {
      expect(out.tree.second.tab_ids).toEqual(['tabB', 'tabC']);
    }
  });

  test('falls back to leftmost leaf when focused_pane_id is invalid', () => {
    const persisted: LayoutPersisted = {
      tree: split(
        's1',
        pane('p1', ['tabA'], 'tabA'),
        pane('p2', ['tabB'], 'tabB'),
      ),
      focused_pane_id: 'p-does-not-exist',
    };
    const tabs = [shellTab('tabA'), shellTab('tabB')];
    const out = validateAndRepairLayout(persisted, tabs);
    expect(out.focused_pane_id).toBe('p1');
  });

  test('collapses non-root empty panes', () => {
    const persisted: LayoutPersisted = {
      tree: split(
        's1',
        pane('p1', [], null), // empty after a tab deletion
        pane('p2', ['tabA'], 'tabA'),
      ),
      focused_pane_id: 'p2',
    };
    const tabs = [shellTab('tabA')];
    const out = validateAndRepairLayout(persisted, tabs);
    // The split should be gone — only the surviving sibling remains.
    expect(out.tree.type).toBe('pane');
    if (out.tree.type === 'pane') {
      expect(out.tree.id).toBe('p2');
      expect(out.tree.tab_ids).toEqual(['tabA']);
    }
    expect(out.focused_pane_id).toBe('p2');
  });

  test('drop-then-orphan-then-collapse interplay', () => {
    // p1 had tabX (now deleted) and tabY (still around). p2 had tabZ
    // (deleted). New tabs tabN (orphan) was added since the save.
    const persisted: LayoutPersisted = {
      tree: split(
        's1',
        pane('p1', ['tabX', 'tabY'], 'tabX'),
        pane('p2', ['tabZ'], 'tabZ'),
      ),
      focused_pane_id: 'p1',
    };
    const tabs = [shellTab('tabY'), shellTab('tabN')];
    const out = validateAndRepairLayout(persisted, tabs);
    // p2 was emptied by the drop step → collapsed; p1 absorbs the
    // surviving sibling subtree, but here p1 *is* the survivor since
    // p2 was the empty one. Result: just p1 with tabY + the orphan
    // tabN appended.
    expect(out.tree.type).toBe('pane');
    if (out.tree.type === 'pane') {
      expect(out.tree.id).toBe('p1');
      expect(out.tree.tab_ids).toEqual(['tabY', 'tabN']);
      expect(out.tree.active_tab_id).toBe('tabY');
    }
    expect(out.focused_pane_id).toBe('p1');
  });

  test('completely empty root pane rebuilds from defaults', () => {
    const persisted: LayoutPersisted = {
      tree: pane('p1', ['stale'], 'stale'),
      focused_pane_id: 'p1',
    };
    // The persisted "stale" tab is gone; the live tab list has new
    // entries the persisted tree doesn't know about. The integrity
    // walk drops "stale" → empty pane → orphan-placement repopulates
    // it from the live tab list. (This test exercises the 'root
    // pane was emptied but tabs do exist' recovery path.)
    const tabs = [shellTab('tabA'), shellTab('tabB')];
    const out = validateAndRepairLayout(persisted, tabs);
    expect(out.tree.type).toBe('pane');
    if (out.tree.type === 'pane') {
      expect(out.tree.tab_ids).toEqual(['tabA', 'tabB']);
      expect(out.tree.active_tab_id).toBe('tabA');
    }
  });

  test('completely empty layout with empty tabs leaves an empty pane', () => {
    const persisted: LayoutPersisted = {
      tree: pane('p1', [], null),
      focused_pane_id: 'p1',
    };
    const out = validateAndRepairLayout(persisted, []);
    expect(out.tree.type).toBe('pane');
    if (out.tree.type === 'pane') {
      expect(out.tree.tab_ids).toEqual([]);
      expect(out.tree.active_tab_id).toBeNull();
    }
  });

  test('preserves a healthy multi-pane layout untouched', () => {
    const persisted: LayoutPersisted = {
      tree: split(
        's1',
        pane('p1', ['tabA', 'tabB'], 'tabA'),
        split(
          's2',
          pane('p2', ['tabC'], 'tabC'),
          pane('p3', ['tabD'], 'tabD'),
        ),
      ),
      focused_pane_id: 'p2',
    };
    const tabs = ['tabA', 'tabB', 'tabC', 'tabD'].map(shellTab);
    const out = validateAndRepairLayout(persisted, tabs);
    expect(out).toEqual(persisted);
  });

  test('dedupes a tab id repeated within one pane', () => {
    // A duplicate key inside a pane's tab_ids breaks the keyed {#each}
    // in TabBar.svelte — the sieve must keep only the first occurrence.
    const persisted: LayoutPersisted = {
      tree: pane('p1', ['tabA', 'tabA', 'tabB'], 'tabA'),
      focused_pane_id: 'p1',
    };
    const tabs = [shellTab('tabA'), shellTab('tabB')];
    const out = validateAndRepairLayout(persisted, tabs);
    expect(out.tree.type).toBe('pane');
    if (out.tree.type === 'pane') {
      expect(out.tree.tab_ids).toEqual(['tabA', 'tabB']);
      expect(out.tree.active_tab_id).toBe('tabA');
    }
  });

  test('dedupes a tab id present in two panes — first occurrence in document order wins', () => {
    // Cross-pane duplicates make two panes fight over the single
    // terminal-host element; the later occurrence is dropped and the
    // emptied pane collapses.
    const persisted: LayoutPersisted = {
      tree: split(
        's1',
        pane('p1', ['tabA'], 'tabA'),
        pane('p2', ['tabA'], 'tabA'),
      ),
      focused_pane_id: 'p1',
    };
    const tabs = [shellTab('tabA')];
    const out = validateAndRepairLayout(persisted, tabs);
    expect(out.tree.type).toBe('pane');
    if (out.tree.type === 'pane') {
      expect(out.tree.id).toBe('p1');
      expect(out.tree.tab_ids).toEqual(['tabA']);
    }
    expect(out.focused_pane_id).toBe('p1');
  });

  test('clamps out-of-range split ratios and leaves in-range ones alone', () => {
    const tree = split('s1', pane('p1', ['tabA'], 'tabA'), pane('p2', ['tabB'], 'tabB'));
    (tree as { ratio: number }).ratio = 5.0;
    const inner: LayoutNode = {
      type: 'split',
      id: 's2',
      direction: 'horizontal',
      ratio: -3.0,
      first: pane('p3', ['tabC'], 'tabC'),
      second: pane('p4', ['tabD'], 'tabD'),
    };
    const outer: LayoutNode = {
      type: 'split',
      id: 's0',
      direction: 'horizontal',
      ratio: 0.42,
      first: tree,
      second: inner,
    };
    const tabs = ['tabA', 'tabB', 'tabC', 'tabD'].map(shellTab);
    const out = validateAndRepairLayout({ tree: outer, focused_pane_id: 'p1' }, tabs);
    expect(out.tree.type).toBe('split');
    if (out.tree.type === 'split') {
      expect(out.tree.ratio).toBeCloseTo(0.42); // untouched
      if (out.tree.first.type === 'split') {
        expect(out.tree.first.ratio).toBeCloseTo(0.95); // clamped down
      }
      if (out.tree.second.type === 'split') {
        expect(out.tree.second.ratio).toBeCloseTo(0.05); // clamped up
      }
    }
  });

  test('non-finite split ratio resets to 0.5', () => {
    const tree = split('s1', pane('p1', ['tabA'], 'tabA'), pane('p2', ['tabB'], 'tabB'));
    (tree as { ratio: number }).ratio = Number.NaN;
    const out = validateAndRepairLayout(
      { tree, focused_pane_id: 'p1' },
      [shellTab('tabA'), shellTab('tabB')],
    );
    if (out.tree.type === 'split') {
      expect(out.tree.ratio).toBe(0.5);
    } else {
      throw new Error('expected split root');
    }
  });

  test('orphans are placed once even when two panes share the focused id', () => {
    // Corrupt duplicate-pane-id file: appending the orphans to every
    // matching pane would manufacture cross-pane duplicate tab ids.
    const persisted: LayoutPersisted = {
      tree: split(
        's1',
        pane('dup', ['tabA'], 'tabA'),
        pane('dup', ['tabB'], 'tabB'),
      ),
      focused_pane_id: 'dup',
    };
    const tabs = [shellTab('tabA'), shellTab('tabB'), shellTab('tabN')];
    const out = validateAndRepairLayout(persisted, tabs);
    let count = 0;
    const stack: LayoutNode[] = [out.tree];
    while (stack.length > 0) {
      const node = stack.pop()!;
      if (node.type === 'pane') {
        count += node.tab_ids.filter((id) => id === 'tabN').length;
      } else {
        stack.push(node.first, node.second);
      }
    }
    expect(count).toBe(1);
  });
});

describe('defaultLayoutForTabs', () => {
  test('builds a single pane with all tabs in order', () => {
    const tabs = [shellTab('a'), shellTab('b'), shellTab('c')];
    const state = defaultLayoutForTabs(tabs);
    expect(state.tree.type).toBe('pane');
    if (state.tree.type === 'pane') {
      expect(state.tree.tab_ids).toEqual(['a', 'b', 'c']);
      expect(state.tree.active_tab_id).toBe('a');
      expect(state.focused_pane_id).toBe(state.tree.id);
    }
  });

  test('handles an empty tab list', () => {
    const state = defaultLayoutForTabs([]);
    expect(state.tree.type).toBe('pane');
    if (state.tree.type === 'pane') {
      expect(state.tree.tab_ids).toEqual([]);
      expect(state.tree.active_tab_id).toBeNull();
    }
  });
});

describe('leftmostLeafPaneId', () => {
  test('returns the deepest-leftmost pane id', () => {
    const tree = split(
      's1',
      split('s2', pane('deepest-left', ['x'], 'x'), pane('mid-right', ['y'], 'y')),
      pane('right', ['z'], 'z'),
    );
    expect(leftmostLeafPaneId(tree)).toBe('deepest-left');
  });

  test('returns root id for a single-pane tree', () => {
    expect(leftmostLeafPaneId(pane('p1', ['x'], 'x'))).toBe('p1');
  });
});

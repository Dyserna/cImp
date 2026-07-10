// Tests for the drag state machine. The suite runs in the default node
// environment (no jsdom in this repo), so it installs minimal stand-ins
// for the DOM pieces drag.ts touches at call time: `window` (a real
// EventTarget — the move/up/cancel/keydown handlers live there),
// `document.body.style`, `Element`, and `PointerEvent`. Node's built-in
// Event/EventTarget provide dispatch semantics (target/currentTarget)
// so `beginDrag` can be driven exactly like Svelte drives it: a
// pointerdown listener on the source element.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { get } from 'svelte/store';

class FakeElement extends EventTarget {
  captured: number[] = [];
  released: number[] = [];
  closest(_selector: string): Element | null {
    return null;
  }
  setPointerCapture(pointerId: number): void {
    this.captured.push(pointerId);
  }
  releasePointerCapture(pointerId: number): void {
    this.released.push(pointerId);
  }
}

class FakePointerEvent extends Event {
  clientX = 0;
  clientY = 0;
  pointerId = 0;
  button = 0;
  constructor(type: string, props: Partial<FakePointerEvent> = {}) {
    super(type, { bubbles: true });
    Object.assign(this, props);
  }
}

class FakeKeyboardEvent extends Event {
  key = '';
  constructor(key: string) {
    super('keydown');
    this.key = key;
  }
}

const g = globalThis as Record<string, unknown>;
let savedGlobals: Record<string, unknown> = {};

function installDomStubs(): void {
  savedGlobals = {
    window: g.window,
    document: g.document,
    Element: g.Element,
    PointerEvent: g.PointerEvent,
  };
  g.window = new EventTarget();
  g.document = { body: { style: { cursor: '' } } };
  g.Element = FakeElement;
  g.PointerEvent = FakePointerEvent;
}

function restoreGlobals(): void {
  for (const [key, value] of Object.entries(savedGlobals)) {
    if (value === undefined) delete g[key];
    else g[key] = value;
  }
}

// Import AFTER the stub classes exist (drag.ts only touches the DOM at
// call time, but keeping the import here makes the dependency obvious).
import { beginDrag, cancelDrag, dragState } from './drag';
import { layout, _resetPlacementQueueForTests } from '../layout/store';
import { paneRegistry } from '../layout/registry';
import type { LayoutNode, PaneNode, SplitNode } from '../layout/types';

function pane(id: string, tab_ids: string[], active: string | null = tab_ids[0] ?? null): PaneNode {
  return { type: 'pane', id, tab_ids, active_tab_id: active };
}

function split(id: string, first: LayoutNode, second: LayoutNode): SplitNode {
  return { type: 'split', id, direction: 'horizontal', ratio: 0.5, first, second };
}

/// Dispatch a pointerdown on `el` wired to beginDrag, mirroring
/// Tab.svelte's `onpointerdowndrag={(e) => beginDrag(id, paneId, e)}`.
function pressOn(
  el: FakeElement,
  tabId: string,
  paneId: string,
  props: Partial<FakePointerEvent>,
): void {
  const listener = (e: Event) => beginDrag(tabId, paneId, e as unknown as PointerEvent);
  el.addEventListener('pointerdown', listener, { once: true });
  el.dispatchEvent(new FakePointerEvent('pointerdown', props));
}

function windowTarget(): EventTarget {
  return g.window as EventTarget;
}

function bodyCursor(): string {
  return (g.document as { body: { style: { cursor: string } } }).body.style.cursor;
}

beforeEach(() => {
  // Fake timers: cleanup(true) schedules a 50ms click-swallow removal
  // that dereferences `window` — it must run while the stubs are still
  // installed, not after restoreGlobals().
  vi.useFakeTimers();
  installDomStubs();
  _resetPlacementQueueForTests();
  layout.set({ tree: pane('root', ['t1'], 't1'), focused_pane_id: 'root' });
});

afterEach(() => {
  cancelDrag();
  vi.runAllTimers();
  vi.useRealTimers();
  restoreGlobals();
});

describe('drag lifecycle', () => {
  test('pointerdown → small move stays pending; pointerup ends as a plain click', () => {
    const el = new FakeElement();
    pressOn(el, 't1', 'root', { pointerId: 1, clientX: 10, clientY: 10 });
    expect(get(dragState).kind).toBe('pending');
    expect(el.captured).toEqual([1]);

    windowTarget().dispatchEvent(
      new FakePointerEvent('pointermove', { pointerId: 1, clientX: 12, clientY: 11 }),
    );
    expect(get(dragState).kind).toBe('pending');

    windowTarget().dispatchEvent(new FakePointerEvent('pointerup', { pointerId: 1 }));
    expect(get(dragState).kind).toBe('idle');
    expect(bodyCursor()).toBe('');
  });

  test('move past threshold promotes to dragging; pointerup via window cleans up', () => {
    // The move/up handlers live on window — this is exactly the path
    // that used to strand the machine when pointer capture failed and
    // the events stopped targeting the source element.
    const el = new FakeElement();
    pressOn(el, 't1', 'root', { pointerId: 1, clientX: 10, clientY: 10 });

    windowTarget().dispatchEvent(
      new FakePointerEvent('pointermove', { pointerId: 1, clientX: 40, clientY: 40 }),
    );
    const state = get(dragState);
    expect(state.kind).toBe('dragging');
    expect(bodyCursor()).toBe('grabbing');

    windowTarget().dispatchEvent(new FakePointerEvent('pointerup', { pointerId: 1 }));
    expect(get(dragState).kind).toBe('idle');
    expect(bodyCursor()).toBe('');
    expect(el.released).toContain(1);
  });

  test('events from a different pointerId are ignored', () => {
    const el = new FakeElement();
    pressOn(el, 't1', 'root', { pointerId: 1, clientX: 10, clientY: 10 });
    windowTarget().dispatchEvent(
      new FakePointerEvent('pointermove', { pointerId: 2, clientX: 99, clientY: 99 }),
    );
    expect(get(dragState).kind).toBe('pending');
    windowTarget().dispatchEvent(new FakePointerEvent('pointerup', { pointerId: 2 }));
    expect(get(dragState).kind).toBe('pending');
  });

  test('Escape cancels an in-flight drag', () => {
    const el = new FakeElement();
    pressOn(el, 't1', 'root', { pointerId: 1, clientX: 10, clientY: 10 });
    windowTarget().dispatchEvent(
      new FakePointerEvent('pointermove', { pointerId: 1, clientX: 40, clientY: 40 }),
    );
    expect(get(dragState).kind).toBe('dragging');

    windowTarget().dispatchEvent(new FakeKeyboardEvent('Escape'));
    expect(get(dragState).kind).toBe('idle');
    expect(bodyCursor()).toBe('');
  });

  test('cancelDrag is idempotent and releases capture', () => {
    const el = new FakeElement();
    pressOn(el, 't1', 'root', { pointerId: 7, clientX: 0, clientY: 0 });
    cancelDrag();
    expect(get(dragState).kind).toBe('idle');
    expect(el.released).toContain(7);
    cancelDrag(); // second call: no-op, no throw
    expect(get(dragState).kind).toBe('idle');
  });
});

describe('beginDrag re-entrancy', () => {
  test('a second pointerdown force-cancels the stale drag (capture released, cursor reset)', () => {
    // Regression: without the guard, the first drag's window listeners
    // and pointer capture leaked, and `cursor: grabbing` stuck app-wide
    // because the first machine's cleanup never ran.
    const elA = new FakeElement();
    const elB = new FakeElement();

    pressOn(elA, 'tA', 'root', { pointerId: 1, clientX: 10, clientY: 10 });
    windowTarget().dispatchEvent(
      new FakePointerEvent('pointermove', { pointerId: 1, clientX: 50, clientY: 50 }),
    );
    expect(get(dragState).kind).toBe('dragging');
    expect(bodyCursor()).toBe('grabbing');

    // Second pointer goes down on another tab mid-drag.
    pressOn(elB, 'tB', 'root', { pointerId: 2, clientX: 20, clientY: 20 });

    const state = get(dragState);
    expect(state.kind).toBe('pending');
    if (state.kind === 'pending') expect(state.tabId).toBe('tB');
    // The stale drag's capture was released and its cursor override undone.
    expect(elA.released).toContain(1);
    expect(bodyCursor()).toBe('');

    // The new drag is fully functional.
    windowTarget().dispatchEvent(
      new FakePointerEvent('pointermove', { pointerId: 2, clientX: 60, clientY: 60 }),
    );
    expect(get(dragState).kind).toBe('dragging');
    windowTarget().dispatchEvent(new FakePointerEvent('pointerup', { pointerId: 2 }));
    expect(get(dragState).kind).toBe('idle');
  });
});

describe('drop commit end to end', () => {
  test('dragging a tab into another pane commits a moveToPane drop', () => {
    layout.set({
      tree: split('s1', pane('p1', ['t1', 't2'], 't1'), pane('p2', ['t3'], 't3')),
      focused_pane_id: 'p1',
    });
    const paneEl = (rect: { left: number; top: number; right: number; bottom: number }) =>
      ({ getBoundingClientRect: () => rect }) as unknown as HTMLElement;
    paneRegistry.setPaneElement('p1', paneEl({ left: 0, top: 0, right: 100, bottom: 100 }));
    paneRegistry.setPaneElement('p2', paneEl({ left: 100, top: 0, right: 200, bottom: 100 }));

    try {
      const el = new FakeElement();
      pressOn(el, 't2', 'p1', { pointerId: 1, clientX: 10, clientY: 10 });
      // Into the center of p2 → moveToPane target.
      windowTarget().dispatchEvent(
        new FakePointerEvent('pointermove', { pointerId: 1, clientX: 150, clientY: 50 }),
      );
      const state = get(dragState);
      expect(state.kind).toBe('dragging');
      if (state.kind === 'dragging') {
        expect(state.dropTarget).toEqual({ kind: 'moveToPane', paneId: 'p2' });
      }
      windowTarget().dispatchEvent(new FakePointerEvent('pointerup', { pointerId: 1 }));

      const after = get(layout);
      const p1 = after.tree.type === 'split' ? (after.tree.first as PaneNode) : null;
      const p2 = after.tree.type === 'split' ? (after.tree.second as PaneNode) : null;
      expect(p1?.tab_ids).toEqual(['t1']);
      expect(p2?.tab_ids).toEqual(['t3', 't2']);
      expect(p2?.active_tab_id).toBe('t2');
      expect(after.focused_pane_id).toBe('p2');
      expect(get(dragState).kind).toBe('idle');
    } finally {
      paneRegistry.setPaneElement('p1', null);
      paneRegistry.setPaneElement('p2', null);
    }
  });
});

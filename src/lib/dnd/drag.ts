// Drag state machine for tab tearing. Handles the full lifecycle
// from pointerdown on a tab through threshold-crossing into a real
// drag, hit-testing on every move, and either committing the drop
// (via the layout store) or cancelling on Esc / pointercancel.
//
// Pointer events (not mouse events) with `setPointerCapture` on the
// source tab give us reliable behavior when the cursor leaves the
// window: pointermove keeps firing on the captured element, and
// pointerup arrives even if the actual release happens off-screen.
// Window-level mousemove/mouseup tend to drop events under WebView2
// once the pointer crosses the chrome.
//
// Click suppression: a click is synthesized after pointerdown→
// pointerup on the same captured element. After a real drag (state
// reached `dragging`) we install a one-shot capture-phase click
// swallower so the post-drag click doesn't accidentally re-fire the
// tab's onclick handler (which would call setPaneActiveTab on a tab
// the commit may have just moved out of the source pane). Pure clicks
// (no movement past threshold) skip suppression and fire normally.

import { get, writable } from 'svelte/store';
import { commitDrop } from '../layout/store';
import type { PaneId } from '../layout/types';
import type { TabId } from '../tabs/types';
import { computeDropTarget } from './dropTarget';
import type { DragState, DropTarget } from './types';

export type { DragState, DropTarget } from './types';

const DRAG_THRESHOLD_PX = 4;

export const dragState = writable<DragState>({ kind: 'idle' });

/// CSS selector matching tab sub-controls that handle their own
/// clicks (close ×, rename input, confirm Yes/No). Pointerdown on
/// any of them must NOT begin a drag — pointer capture on the parent
/// button would otherwise redirect the synthesized click to the
/// button itself, swallowing the sub-control's click handler. The
/// drag should only begin when the user grabs the tab body proper.
const NON_DRAG_TARGET_SELECTOR = 'input, .close, .confirm-btn, .rename-input';

/// Called from a tab's `onpointerdown` handler. Records the pending
/// drag and attaches pointer-event listeners to the source element
/// (which is also the pointer-capture target). The state stays
/// `pending` until movement crosses the 4px threshold; below that, a
/// pointerup is just a click and the listeners are torn down without
/// a commit.
export function beginDrag(
  tabId: TabId,
  sourcePaneId: PaneId,
  event: PointerEvent,
): void {
  if (event.button !== 0) return; // left button only
  const target = event.target;
  if (target instanceof Element && target.closest(NON_DRAG_TARGET_SELECTOR)) {
    return;
  }
  const sourceEl = event.currentTarget;
  if (!(sourceEl instanceof Element)) return;
  try {
    sourceEl.setPointerCapture(event.pointerId);
  } catch {
    // Some webviews can refuse capture (rare). The handlers fall back
    // to bubbling through the document; clientX/Y stay correct either
    // way.
  }
  dragState.set({
    kind: 'pending',
    tabId,
    sourcePaneId,
    startX: event.clientX,
    startY: event.clientY,
    pointerId: event.pointerId,
    sourceEl,
  });
  sourceEl.addEventListener('pointermove', onPointerMove);
  sourceEl.addEventListener('pointerup', onPointerUp);
  sourceEl.addEventListener('pointercancel', onPointerCancel);
  window.addEventListener('keydown', onKeyDown, true);
}

function onPointerMove(event: Event): void {
  if (!(event instanceof PointerEvent)) return;
  const state = get(dragState);
  if (state.kind === 'idle') return;
  if (event.pointerId !== state.pointerId) return;

  if (state.kind === 'pending') {
    const dx = event.clientX - state.startX;
    const dy = event.clientY - state.startY;
    if (Math.hypot(dx, dy) < DRAG_THRESHOLD_PX) return;
    const dropTarget = computeDropTarget(event.clientX, event.clientY, state.sourcePaneId);
    dragState.set({
      kind: 'dragging',
      tabId: state.tabId,
      sourcePaneId: state.sourcePaneId,
      cursorX: event.clientX,
      cursorY: event.clientY,
      pointerId: state.pointerId,
      sourceEl: state.sourceEl,
      dropTarget,
    });
    document.body.style.cursor = 'grabbing';
    return;
  }

  // dragging
  const dropTarget = computeDropTarget(event.clientX, event.clientY, state.sourcePaneId);
  dragState.set({
    ...state,
    cursorX: event.clientX,
    cursorY: event.clientY,
    dropTarget,
  });
}

function onPointerUp(event: Event): void {
  if (!(event instanceof PointerEvent)) return;
  const state = get(dragState);
  if (state.kind === 'idle') return;
  if (event.pointerId !== state.pointerId) return;

  const wasDragging = state.kind === 'dragging';
  const committed: DropTarget | null = wasDragging ? state.dropTarget : null;
  const { tabId, sourcePaneId } = state;
  cleanup(wasDragging);
  if (committed) commitDrop(tabId, sourcePaneId, committed);
}

function onPointerCancel(event: Event): void {
  if (!(event instanceof PointerEvent)) return;
  const state = get(dragState);
  if (state.kind === 'idle') return;
  if (event.pointerId !== state.pointerId) return;
  cleanup(state.kind === 'dragging');
}

function onKeyDown(event: KeyboardEvent): void {
  if (event.key !== 'Escape') return;
  const state = get(dragState);
  if (state.kind === 'idle') return;
  event.preventDefault();
  event.stopPropagation();
  cleanup(state.kind === 'dragging');
}

/// Forcibly cancel any in-flight drag (pending or dragging) without
/// committing a drop. Called by external state mutations that yank the
/// ground out from under the drag — e.g. restoring a layout preset
/// while a tab is mid-drag, where the source pane id may no longer
/// exist after `layout.set(...)`. Idempotent: a no-op when state is
/// already idle.
export function cancelDrag(): void {
  const state = get(dragState);
  if (state.kind === 'idle') return;
  cleanup(false);
}

function cleanup(suppressClick: boolean): void {
  const state = get(dragState);
  if (state.kind !== 'idle') {
    try {
      state.sourceEl.releasePointerCapture(state.pointerId);
    } catch {
      // Already released (browsers auto-release on pointerup).
    }
    state.sourceEl.removeEventListener('pointermove', onPointerMove);
    state.sourceEl.removeEventListener('pointerup', onPointerUp);
    state.sourceEl.removeEventListener('pointercancel', onPointerCancel);
  }
  window.removeEventListener('keydown', onKeyDown, true);
  document.body.style.cursor = '';
  dragState.set({ kind: 'idle' });

  if (suppressClick) {
    // The click event synthesized by pointerdown→pointerup on the
    // captured element would re-fire the tab's onclick handler — the
    // commit logic has already done the right thing, so we eat it.
    // Capture-phase + once: ensures we win against any later listener
    // and self-removes on the first click.
    //
    // Time-bounded: if the source element was removed by commitDrop
    // before the click could be synthesized (e.g. a moveToPane
    // unmounts the source tab's button), no click ever fires and the
    // listener would otherwise eat the next unrelated click. A short
    // timeout removes it safely.
    const swallow = (e: Event) => {
      e.stopImmediatePropagation();
      e.preventDefault();
    };
    window.addEventListener('click', swallow, { capture: true, once: true });
    setTimeout(() => {
      window.removeEventListener('click', swallow, true);
    }, 50);
  }
}

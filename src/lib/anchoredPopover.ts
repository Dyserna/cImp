// Shared mechanics for the cursor-anchored popovers: `TabContextMenu`,
// `TaintMenu`, `DelegationPopover`.
//
// All three are `position: fixed` panels opened at raw click coordinates and
// dismissed by Escape or a mousedown outside. Before #128 each carried its own
// byte-identical copy of the clamp `$effect` and the mousedown/Escape pair —
// `DelegationPopover` even documents itself as a deliberate copy of
// `TaintMenu`. The prose those copies carried is preserved here, because the
// two non-obvious rules below are the reason each line exists.
//
// This is a plain module, not a component or an action: the clamp has to run
// inside the caller's own `$effect` (it writes the caller's `$state` position),
// and the dismissal has to be torn down by the caller's `onMount` return.

/** The margin, in px, kept between a clamped popover and the viewport edge. */
const MARGIN = 4;

/**
 * The selector matching an anchored popover's PANEL element.
 *
 * All three panels are `<div class="menu" role="menu|dialog">`. Svelte's scoped
 * styles add a hash class and keep the authored one, so `menu` is really in the
 * DOM; the `role` half is what keeps this from matching some future unrelated
 * `.menu`.
 */
const PANEL_SELECTOR = '.menu[role="menu"], .menu[role="dialog"]';

/**
 * Whether `node` is inside an anchored popover panel (#147).
 *
 * **Why anything outside this module asks.** `Pane.svelte` refocuses its
 * terminal on the next frame whenever the pane becomes focused, and a popover
 * opened from the tab bar lives inside a pane: clicking an `<input>` in the
 * `DelegationPopover` ran `onmousedowncapture` → `setFocusedPane` → that
 * refocus, which then took the focus the browser had just given the field. The
 * name/tier/context fields could not be typed into at all, and the radios only
 * worked because a `<label>`'s click-to-focus fires after mouseup and could win
 * the race.
 *
 * The pane-focus bookkeeping itself is NOT the problem and must keep running —
 * pane focus drives audio and avatar routing. Only the terminal refocus is
 * skipped, and only while the focus sits in a panel: a popover is a modal-ish
 * surface the user is deliberately inside, and nothing in one wants the
 * terminal to have the keyboard.
 *
 * Deliberately narrower than "is a form control": the tab strip's tabs are
 * `<button>`s, so a blanket form-control test would suppress exactly the
 * refocus this app wants most — clicking a tab in an unfocused pane.
 *
 * Typed against the `closest` surface rather than `Element` so it can be
 * exercised without a DOM.
 */
export function inAnchoredPopover(
  node: { closest(selector: string): unknown } | null | undefined,
): boolean {
  return !!node && node.closest(PANEL_SELECTOR) !== null;
}

/**
 * The top-left corner at which a popover of `el`'s current size can be placed
 * so it stays fully on screen, given the coordinates the caller wants.
 *
 * Opened at the raw cursor coords a popover overflows off-screen — and
 * therefore out of reach — near the right and bottom edges.
 *
 * `el` is `undefined` on the first run, before `bind:this` has landed: the
 * wanted coordinates are returned unchanged and the caller's effect re-runs
 * with the element once it exists.
 *
 * Call this from inside an `$effect`, passing `x`/`y` positionally so both are
 * read as dependencies and a reposition re-clamps.
 */
export function clampToViewport(
  el: HTMLElement | undefined,
  wantX: number,
  wantY: number,
): { x: number; y: number } {
  if (!el) return { x: wantX, y: wantY };
  const rect = el.getBoundingClientRect();
  return {
    x: Math.max(MARGIN, Math.min(wantX, window.innerWidth - rect.width - MARGIN)),
    y: Math.max(MARGIN, Math.min(wantY, window.innerHeight - rect.height - MARGIN)),
  };
}

/**
 * Wire "Escape or a click outside dismisses this popover", and return the
 * teardown. Intended as the whole body of an `onMount`:
 *
 * ```ts
 * onMount(() => dismissOn(() => menuEl, () => onDismiss()));
 * ```
 *
 * Two rules are load-bearing:
 *
 *  * **The mousedown listener is deferred by a tick.** The click (or
 *    right-click) that opened the popover is still being dispatched when the
 *    popover mounts; subscribing synchronously would let that same event
 *    dismiss it immediately.
 *  * **Dismissal is skipped when the mousedown started inside the panel.**
 *    Without the containment check a mousedown on an entry unmounts the
 *    popover before its `click` fires, so no entry ever runs its action. The
 *    check is `contains`, so anything rendered *inside* the panel element —
 *    `TabContextMenu`'s hover submenus, for instance — is inside it too. A
 *    popover that ever renders part of itself outside this element (a portal,
 *    a second fixed layer) would have to widen the check.
 *
 * Both arguments are getters, deliberately: `el` is bound after mount and can
 * be re-bound, and `dismiss` is a prop the parent may replace, so each is read
 * at event time rather than captured at mount.
 */
export function dismissOn(
  el: () => HTMLElement | undefined,
  dismiss: () => void,
): () => void {
  function onWindowMouseDown(e: MouseEvent): void {
    const target = e.target as Node | null;
    const panel = el();
    if (target && panel && panel.contains(target)) return;
    dismiss();
  }

  function onWindowKeyDown(e: KeyboardEvent): void {
    if (e.key === 'Escape') {
      e.preventDefault();
      dismiss();
    }
  }

  const id = setTimeout(() => {
    window.addEventListener('mousedown', onWindowMouseDown);
  }, 0);
  window.addEventListener('keydown', onWindowKeyDown);

  return () => {
    clearTimeout(id);
    window.removeEventListener('mousedown', onWindowMouseDown);
    window.removeEventListener('keydown', onWindowKeyDown);
  };
}

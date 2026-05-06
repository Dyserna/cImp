# Feature: Touch / Pen Drag-and-Drop

## Purpose

Migrate the v1.3 drag-and-drop implementation from `mouse*` events to `pointer*` events, so a single code path supports mouse, touch, and pen input. Enables tab dragging and splitter resize on touchscreen laptops, tablets running cctts, and accessibility devices that emulate pointer events.

See `FUTURE-FEATURES.md` § "Touch / pen drag-and-drop" for the rationale; this doc captures the implementation strategy.

## Scope

Single-item feature; one PR's worth of work. In this group of feature docs because it's discrete enough to handle separately and doesn't compose with the larger UX features.

The change is mechanical at its core (`mousedown` → `pointerdown`, `mousemove` → `pointermove`, `mouseup` → `pointerup`) but has UX wrinkles:

- Touch has **no hover state**. The v1.3 drag implementation likely uses hover for drop-zone preview before commit. On touch, the user can't preview a drop zone without committing. Either accept this (touch users get less feedback during drag, same as on every other touch app) or implement a "press-and-hold to enter preview mode" — too heavy. Recommend accept.
- Pen has **pressure** and **tilt**. Ignore both; we're using the pen as a pointer, not a stylus.
- Pointer events fire **`pointercancel`** when the OS or browser interrupts the drag (e.g., system gesture, screen rotation). The v1.3 mouse code probably doesn't handle a cancel — there's no `mousecancel`. Add a `pointercancel` handler that aborts the drag and restores state, identical to a `mouseup` outside any drop target.
- Default touch behavior on a draggable element scrolls the page. **Set `touch-action: none`** in CSS on tabs and splitters to suppress this. Without it, the browser will scroll instead of letting your handler see the move.

## Implementation outline

### 1. Audit the v1.3 drag layer

Files to audit (per the open status: `src/lib/dnd/dropTarget.ts` is already showing modifications in git status):

- `src/lib/dnd/dropTarget.ts` — main drag handler.
- `src/lib/Tab.svelte` — drag source wiring (mousedown).
- `src/lib/TabBar.svelte` — drag-over / drop visual states.
- `src/lib/Pane.svelte` — drop zones.
- `src/lib/Split.svelte` — splitter drag (separate code path, also mouse-based).

Find every `mouse*` listener, drag-state-machine transition, and DOM event reference. Catalogue before changing — easy to miss one.

### 2. Translate to pointer events

For each site:

- `mousedown` → `pointerdown`
- `mousemove` → `pointermove`
- `mouseup` → `pointerup`
- Add `pointercancel` handlers wherever `mouseup` is handled. Cancel = abort the drag, restore original state, do not fire the drop logic.
- Pointer events expose `event.pointerType: "mouse" | "touch" | "pen"` if needed to branch behavior. For drag, no branching is required — all three should behave identically. For *threshold* tuning (the 4px threshold mentioned in V4-05's M2 verification), consider relaxing to ~8px on touch since fingers are imprecise. Branch on `pointerType` only if the default threshold causes false drag starts on touch.

### 3. Pointer capture

For drag-during-drag-out-of-source-element scenarios, `setPointerCapture(event.pointerId)` on the source element ensures all subsequent `pointermove` / `pointerup` events fire on the source even if the cursor leaves it. v1.3's mouse implementation may rely on `document`-level listeners for the same effect; either approach works. Pointer capture is cleaner and the recommended pattern.

Release the capture on `pointerup` / `pointercancel` with `releasePointerCapture(pointerId)`.

### 4. CSS

Add to draggable elements (tabs, splitters):

```css
.tab, .splitter {
  touch-action: none;
}
```

Without this, the browser intercepts touch sequences for scrolling and the drag handler never sees the moves.

### 5. Cursor styling on touch

`cursor: grabbing` is a no-op on touch input. Don't try to compensate with a synthetic indicator — the user can see their finger. The cursor styling continues to apply for mouse input, which is the only input type that has a cursor.

### 6. Splitter

The splitter (`src/lib/Split.svelte`) gets the same treatment. Splitter drag with a finger is awkward (fingers are wider than a splitter line) — consider widening the splitter hit area on touch via a pseudo-element (`::before` with negative margins) so users can grab it. Branch on `pointerType === "touch"` if needed. Defer if the default hit area works in practice.

### 7. Cross-platform validation

V4-05 documented WebView2 (Windows) and WebKitGTK (Linux) DnD quirks. With pointer events, re-validate on both:

- WebView2 supports `PointerEvent` natively. Validate `pointercancel` fires reliably on window blur and screen lock — historical wrinkle on older WebView2 versions.
- WebKitGTK (under Wayland and X11) supports `PointerEvent`. Validate touch input under both display servers if possible. Wayland touch may behave subtly differently from X11.

Document any quirks in the source file's top comment, same convention as V4-05.

## Open questions

- **Drag ghost on touch**: the v1.3 drag ghost is a small text label that follows the cursor. Under touch, the user's finger covers it. Consider offsetting the ghost above the touch point (e.g., `transform: translate(0, -40px)`). Branch on `pointerType === "touch"` if so. Easy adjustment; not blocking.
- **Multi-touch**: pointer events fire per touch contact. If the user puts two fingers down, two `pointerdown` events fire with different `pointerId`s. Route only the first `pointerId` through the drag state machine; ignore additional pointers until the active drag ends. (Alternative: cancel the drag if a second pointer joins. Simpler but worse UX.) Recommend the first-pointer-wins approach.
- **Accessibility**: pointer events also reach assistive devices. Verify that screen-reader-driven drag (rare but possible) doesn't crash. Add ARIA labels if not already present (V4-05 mentioned ARIA on the splitter and panes — verify post-migration).

## Milestone recommendation

**No milestone doc needed.** Single PR. Mostly mechanical translation + CSS additions + cross-platform validation. Comparable in size to a typical V1.x polish task.

**Trigger to act**: per `FUTURE-FEATURES.md`, "if you ever run cctts on a touchscreen device, or accessibility need." Don't pre-emptively pick this up — touch isn't part of the primary user's daily driver setup, and the current mouse-only implementation works fine for the primary use case.

## Files most likely touched

- `src/lib/dnd/dropTarget.ts` — main migration.
- `src/lib/Tab.svelte` — drag source.
- `src/lib/TabBar.svelte` — drag-over visual states.
- `src/lib/Pane.svelte` — drop zones.
- `src/lib/Split.svelte` — splitter drag + (optional) widened hit area on touch.
- A small CSS-additions surface across the same files (`touch-action: none`).
- README — optional note about touchscreen support.

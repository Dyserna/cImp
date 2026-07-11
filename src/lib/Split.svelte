<script lang="ts">
  // Internal node of the layout tree: two children with a draggable
  // splitter between them. M3 wires:
  //
  //   * mousedown-driven resize: dragging the splitter line adjusts the
  //     split's `ratio` in the layout store — plus compensating ratios on
  //     the nested same-direction splits touching the divider, so ONLY the
  //     two panes directly adjacent to the divider change size (all other
  //     panes keep their absolute px; math in layout/resize.ts). Min-size
  //     constraints clamp during drag so neither adjacent pane can shrink
  //     below MIN_PANE_*_PX.
  //
  //   * Visual-only render clamp: when the application window resizes
  //     such that a stored ratio would violate min sizes, the ratio is
  //     clamped *on render* but never written back. When the window grows
  //     again, the original user-chosen ratio is honored. Without this,
  //     a transient narrow window would silently rewrite the user's
  //     preference.
  //
  // The drag handler stops mousedown propagation so it doesn't also
  // trigger the focus-on-mousedown handler in Pane.svelte (which would
  // flicker focus to whichever pane the splitter happens to be inside
  // of).

  import { onDestroy } from 'svelte';
  import LayoutNodeRenderer from './LayoutNodeRenderer.svelte';
  import { setSplitRatios } from './layout/store';
  import { planSplitterDrag, ratiosForOffset } from './layout/resize';
  import {
    MIN_PANE_HEIGHT_PX,
    MIN_PANE_WIDTH_PX,
  } from './layout/constants';
  import type { SplitNode } from './layout/types';

  let { split }: { split: SplitNode } = $props();

  // Teardown for an in-progress splitter drag. Held here so it can be invoked
  // both on mouseup AND on component destroy — otherwise unmounting mid-drag
  // (or the window losing focus) leaks the window listeners and leaves the body
  // cursor/user-select overrides stuck on.
  let dragCleanup: (() => void) | null = null;

  let containerEl: HTMLDivElement | undefined = $state();
  let containerSize = $state({ width: 0, height: 0 });

  // Track the container's live size via ResizeObserver. The render
  // clamp below depends on the container's geometry — without a live
  // observer, a window resize would not re-render the clamp and the
  // splits would visually break the min-size invariant until the next
  // unrelated layout-store mutation.
  $effect(() => {
    if (!containerEl) return;
    const el = containerEl;
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const r = entry.contentRect;
        if (r.width !== containerSize.width || r.height !== containerSize.height) {
          containerSize = { width: r.width, height: r.height };
        }
      }
    });
    observer.observe(el);
    // Initial measurement so the first render after mount uses the real
    // size rather than the {0, 0} placeholder.
    const r = el.getBoundingClientRect();
    containerSize = { width: r.width, height: r.height };
    return () => observer.disconnect();
  });

  // Visual-only ratio clamp. When the container is too small for two
  // min-sized children, fall back to 0.5 (each child gets half of an
  // already-too-small space — degraded but consistent). Otherwise clamp
  // to [minRatio, 1 - minRatio]. The original `split.ratio` is preserved
  // in the store so growing the window restores the user's preference.
  const clampedRatio = $derived.by(() => {
    const isHorizontal = split.direction === 'horizontal';
    const total = isHorizontal ? containerSize.width : containerSize.height;
    if (total <= 0) return split.ratio;
    const minPx = isHorizontal ? MIN_PANE_WIDTH_PX : MIN_PANE_HEIGHT_PX;
    if (total < 2 * minPx) return 0.5;
    const minRatio = minPx / total;
    return Math.max(minRatio, Math.min(1 - minRatio, split.ratio));
  });

  function onSplitterMouseDown(event: MouseEvent): void {
    if (event.button !== 0) return;
    if (!containerEl) return;
    event.preventDefault();
    // Stop propagation: without it, the parent Pane.svelte's
    // mousedown-capture focus handler would briefly flicker focus to
    // whichever pane the cursor is currently over (the cursor is on the
    // splitter line, which is between two panes — paneRegistry's
    // findUnderCursor would pick one of them deterministically, but the
    // user didn't ask for that). We're handling the splitter at the
    // capture phase explicitly anyway.
    event.stopPropagation();

    const splitEl = containerEl;
    const startRect = splitEl.getBoundingClientRect();
    const isHorizontal = split.direction === 'horizontal';

    const total = isHorizontal ? startRect.width : startRect.height;
    const start = isHorizontal ? startRect.left : startRect.top;
    if (total <= 0) return;

    const minPx = isHorizontal ? MIN_PANE_WIDTH_PX : MIN_PANE_HEIGHT_PX;
    // Snapshot the drag plan once at mousedown: the divider-adjacent
    // chains and their pixel sizes. Every mousemove recomputes all ratios
    // from this snapshot plus the live cursor offset, so rounding can't
    // accumulate across moves. `clampedRatio` anchors the plan to where
    // the divider is actually rendered.
    const maybePlan = planSplitterDrag(split, total, clampedRatio, minPx);
    if (!maybePlan) return;
    // Re-bind after the null guard so the narrowed type reaches onMove's
    // closure (TS doesn't propagate the guard into nested functions).
    const plan = maybePlan;

    const previousCursor = document.body.style.cursor;
    const previousUserSelect = document.body.style.userSelect;
    document.body.style.cursor = isHorizontal ? 'col-resize' : 'row-resize';
    // Disabling text selection during drag prevents the cursor from
    // grabbing arbitrary tab labels under it as the pointer moves.
    document.body.style.userSelect = 'none';

    function onMove(e: MouseEvent): void {
      const offset = (isHorizontal ? e.clientX : e.clientY) - start;
      setSplitRatios(ratiosForOffset(plan, offset));
    }

    function onUp(): void {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
      // A drag interrupted by the window losing focus never sees a mouseup;
      // 'blur' releases it so listeners and body styles don't stick.
      window.removeEventListener('blur', onUp);
      document.body.style.cursor = previousCursor;
      document.body.style.userSelect = previousUserSelect;
      dragCleanup = null;
    }

    dragCleanup = onUp;
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
    window.addEventListener('blur', onUp);
  }

  // Component destroyed mid-drag: release the window listeners and restore the
  // body styles instead of leaking them.
  onDestroy(() => dragCleanup?.());
</script>

<div
  class="split"
  class:horizontal={split.direction === 'horizontal'}
  class:vertical={split.direction === 'vertical'}
  bind:this={containerEl}
>
  <div class="split-child" style:flex={`${clampedRatio} 1 0%`}>
    <LayoutNodeRenderer node={split.first} />
  </div>
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <div
    class="splitter"
    role="separator"
    aria-orientation={split.direction === 'horizontal' ? 'vertical' : 'horizontal'}
    aria-label="Resize panes"
    tabindex="0"
    onmousedown={onSplitterMouseDown}
  ></div>
  <div class="split-child" style:flex={`${1 - clampedRatio} 1 0%`}>
    <LayoutNodeRenderer node={split.second} />
  </div>
</div>

<style>
  .split {
    display: flex;
    flex: 1 1 auto;
    min-width: 0;
    min-height: 0;
    width: 100%;
    height: 100%;
  }
  .split.horizontal {
    flex-direction: row;
  }
  .split.vertical {
    flex-direction: column;
  }
  .split-child {
    display: flex;
    min-width: 0;
    min-height: 0;
    overflow: hidden;
  }
  .splitter {
    background: var(--surface-2);
    flex-shrink: 0;
    user-select: none;
  }
  .split.horizontal > .splitter {
    width: 4px;
    cursor: col-resize;
  }
  .split.vertical > .splitter {
    height: 4px;
    cursor: row-resize;
  }
  .splitter:hover {
    background: var(--accent);
  }
  .splitter:focus-visible {
    background: var(--accent);
    outline: 2px solid var(--accent-bright);
    outline-offset: -1px;
  }
</style>

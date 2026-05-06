<script lang="ts">
  // Translucent rectangle showing where the dropped tab will land.
  // Reads the drop target's pane geometry from `paneRegistry` on
  // every drag-state change and computes the overlay rect:
  //   * split: half the pane in the indicated direction.
  //   * moveToPane: the tab-bar strip (the tab is appended; the
  //     existing content stays put).
  //   * reorder: a thin vertical line at the insertion gap.
  //
  // The 80ms transition smooths zone changes; pointer events are
  // disabled so the overlay can sit anywhere without intercepting
  // moves/ups.

  import { dragState } from './drag';
  import type { DropTarget } from './types';
  import { paneRegistry } from '../layout/registry';

  interface OverlayRect {
    left: number;
    top: number;
    width: number;
    height: number;
    style: 'fill' | 'line';
  }

  function rectForTarget(target: DropTarget): OverlayRect | null {
    const paneRect = paneRegistry.getPaneRect(target.paneId);
    if (!paneRect) return null;

    if (target.kind === 'split') {
      const half = (target.direction === 'left' || target.direction === 'right')
        ? { width: paneRect.width / 2, height: paneRect.height }
        : { width: paneRect.width, height: paneRect.height / 2 };
      const left = target.direction === 'right' ? paneRect.left + paneRect.width / 2 : paneRect.left;
      const top = target.direction === 'bottom' ? paneRect.top + paneRect.height / 2 : paneRect.top;
      return { left, top, width: half.width, height: half.height, style: 'fill' };
    }

    if (target.kind === 'moveToPane') {
      const barRect = paneRegistry.getTabBarRect(target.paneId);
      const top = barRect ? barRect.top : paneRect.top;
      const height = barRect ? barRect.bottom - barRect.top : 32;
      return {
        left: paneRect.left,
        top,
        width: paneRect.width,
        height,
        style: 'fill',
      };
    }

    // reorder — thin line at the insertion gap.
    const barEl = paneRegistry.getTabBarElement(target.paneId);
    const barRect = paneRegistry.getTabBarRect(target.paneId);
    if (!barEl || !barRect) return null;
    const tabs = barEl.querySelectorAll<HTMLElement>('[data-tab-id]');
    let x: number;
    if (target.insertIndex >= tabs.length) {
      const last = tabs[tabs.length - 1];
      x = last ? last.getBoundingClientRect().right : barRect.left;
    } else {
      x = tabs[target.insertIndex].getBoundingClientRect().left;
    }
    return {
      left: x - 1.5,
      top: barRect.top,
      width: 3,
      height: barRect.bottom - barRect.top,
      style: 'line',
    };
  }

  const overlay = $derived.by((): OverlayRect | null => {
    if ($dragState.kind !== 'dragging') return null;
    if (!$dragState.dropTarget) return null;
    return rectForTarget($dragState.dropTarget);
  });
</script>

{#if overlay}
  <div
    class="drop-zone"
    class:line={overlay.style === 'line'}
    style:left="{overlay.left}px"
    style:top="{overlay.top}px"
    style:width="{overlay.width}px"
    style:height="{overlay.height}px"
  ></div>
{/if}

<style>
  .drop-zone {
    position: fixed;
    z-index: 9999;
    pointer-events: none;
    background: var(--accent-muted);
    border: 2px dashed var(--accent);
    border-radius: var(--radius-md);
    box-shadow: inset 0 0 24px rgba(62, 221, 182, 0.18);
    transition:
      left 80ms ease-out,
      top 80ms ease-out,
      width 80ms ease-out,
      height 80ms ease-out;
  }
  .drop-zone.line {
    background: var(--accent);
    border: none;
    border-radius: 1px;
    box-shadow: 0 0 6px var(--accent);
    /* Reorder lines are thin and shouldn't animate width — animation
       would lag the cursor across rapid tab-by-tab moves. */
    transition: left 80ms ease-out, top 80ms ease-out;
  }
</style>

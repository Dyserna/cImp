<script lang="ts">
  // Movable left cluster of the bottom status bar. Hosts the "session"
  // (UsageMeter) and "cpu" (SystemStats) panels as draggable slots. Each
  // slot carries a leading gap (px): dragging a panel right grows its gap
  // so it "stays where you drop it"; dragging left shrinks the gap and,
  // once it bottoms out, swaps the panel past its neighbour — which resets
  // all gaps to 0 (reordering removes any manual spacing). Order + gaps
  // persist in `ui.status_bar.items`.
  //
  // Both panels are display-only, so grabbing anywhere on a slot is safe.
  // Pointer move/up are bound at the window level (not via pointer capture)
  // so a mid-drag DOM reorder can't drop the gesture. Right-click offers a
  // single "Reset to default" escape hatch (also in Settings → Bottom bar).
  //
  // The persisted list is normalized on read so `usage` and `system_stats`
  // each appear exactly once regardless of what's on disk.
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import UsageMeter from './UsageMeter.svelte';
  import SystemStats from './SystemStats.svelte';
  import { settings, applySettings } from '../settings/store';
  import type { Settings, StatusBarSlot } from '../settings/types';

  const DRAG_THRESHOLD_PX = 4;

  function defaultItems(): StatusBarSlot[] {
    return [
      { component: 'usage', gap: 0 },
      { component: 'system_stats', gap: 0 },
    ];
  }

  function normalize(input: StatusBarSlot[] | undefined): StatusBarSlot[] {
    const out: StatusBarSlot[] = [];
    let hasUsage = false;
    let hasStats = false;
    for (const it of input ?? []) {
      const gap = Math.max(0, Math.round(Number(it?.gap) || 0));
      if (it?.component === 'usage') {
        if (hasUsage) continue;
        hasUsage = true;
        out.push({ component: 'usage', gap });
      } else if (it?.component === 'system_stats') {
        if (hasStats) continue;
        hasStats = true;
        out.push({ component: 'system_stats', gap });
      }
      // unknown components are dropped
    }
    if (!hasUsage) out.push({ component: 'usage', gap: 0 });
    if (!hasStats) out.push({ component: 'system_stats', gap: 0 });
    return out;
  }

  // Persisted (normalized) arrangement. `working` overrides it live during
  // a drag so the per-frame gap/reorder edits stay local until commit.
  const items = $derived(normalize($settings.ui.status_bar.items));
  let working = $state<StatusBarSlot[] | null>(null);
  const view = $derived(working ?? items);

  let rowEl: HTMLDivElement | undefined = $state();

  // Drag state.
  let activePointer: number | null = null;
  let pointerStartX = 0;
  let startIndex = -1;
  let grabDX = 0; // pointer x minus dragged slot's left edge, at drag start
  let moved = false;
  let dragIndex = $state<number | null>(null);

  function commit(next: StatusBarSlot[]): void {
    const cur: Settings = get(settings);
    void applySettings({
      ...cur,
      ui: { ...cur.ui, status_bar: { items: next } },
    });
  }

  function onPointerDown(e: PointerEvent, index: number): void {
    if (e.button !== 0) return; // left button only; right-click opens the menu
    activePointer = e.pointerId;
    pointerStartX = e.clientX;
    startIndex = index;
    moved = false;
    window.addEventListener('pointermove', onPointerMove);
    window.addEventListener('pointerup', onPointerUp);
    // Also handle pointercancel: the OS can cancel a gesture (touch
    // interruption, focus loss) without ever firing pointerup, which would
    // otherwise leave `activePointer` set and the move/up listeners attached —
    // locking out all further dragging.
    window.addEventListener('pointercancel', onPointerCancel);
  }

  function detachDragListeners(): void {
    window.removeEventListener('pointermove', onPointerMove);
    window.removeEventListener('pointerup', onPointerUp);
    window.removeEventListener('pointercancel', onPointerCancel);
  }

  function onPointerCancel(e: PointerEvent): void {
    if (activePointer === null || e.pointerId !== activePointer) return;
    detachDragListeners();
    activePointer = null;
    // Cancelled gesture: discard the in-progress reorder rather than commit it.
    working = null;
    dragIndex = null;
    moved = false;
  }

  function onPointerMove(e: PointerEvent): void {
    if (activePointer === null || e.pointerId !== activePointer || !rowEl) return;
    const slots = Array.from(rowEl.children) as HTMLElement[];
    if (!moved) {
      if (Math.abs(e.clientX - pointerStartX) < DRAG_THRESHOLD_PX) return;
      moved = true;
      working = items.map((s) => ({ ...s }));
      dragIndex = startIndex;
      grabDX = pointerStartX - slots[startIndex].getBoundingClientRect().left;
    }
    if (dragIndex === null || !working) return;

    const draggedRect = slots[dragIndex].getBoundingClientRect();
    const desiredLeft = e.clientX - grabDX;
    const desiredCenter = desiredLeft + draggedRect.width / 2;

    // Reorder: where does the dragged panel's centre sit among the others?
    let target = 0;
    for (let j = 0; j < slots.length; j++) {
      if (j === dragIndex) continue;
      const r = slots[j].getBoundingClientRect();
      if (desiredCenter > r.left + r.width / 2) target++;
    }

    if (target !== dragIndex) {
      // Order changed — drop all manual spacing and repack.
      const dragged = working[dragIndex];
      const arr = working
        .filter((_, i) => i !== dragIndex)
        .map((s) => ({ ...s, gap: 0 }));
      arr.splice(target, 0, { ...dragged, gap: 0 });
      working = arr;
      dragIndex = target;
    } else {
      // Same order — adjust this panel's leading gap so it tracks the
      // pointer. `prevRight` is a slot before the dragged one (or the zone
      // edge), so it's unaffected by the gap we're about to set.
      const prevRight =
        dragIndex === 0
          ? rowEl.getBoundingClientRect().left
          : slots[dragIndex - 1].getBoundingClientRect().right;
      const gap = Math.max(
        0,
        Math.min(Math.round(desiredLeft - prevRight), rowEl.clientWidth),
      );
      working = working.map((s, i) => (i === dragIndex ? { ...s, gap } : s));
    }
  }

  function onPointerUp(e: PointerEvent): void {
    if (activePointer === null || e.pointerId !== activePointer) return;
    detachDragListeners();
    activePointer = null;
    if (moved && working) commit(working);
    working = null;
    dragIndex = null;
    moved = false;
  }

  // Right-click → single "Reset to default" entry.
  let menu = $state<{ x: number; y: number } | null>(null);

  function onContext(e: MouseEvent): void {
    e.preventDefault();
    e.stopPropagation();
    menu = { x: e.clientX, y: e.clientY };
  }

  function resetArrangement(): void {
    commit(defaultItems());
    menu = null;
  }

  onMount(() => {
    function onDocMouseDown(): void {
      if (menu) menu = null;
    }
    function onKeyDown(e: KeyboardEvent): void {
      if (menu && e.key === 'Escape') {
        e.preventDefault();
        menu = null;
      }
    }
    // Capture phase so the dismiss runs before a fresh contextmenu opens.
    document.addEventListener('mousedown', onDocMouseDown, true);
    window.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('mousedown', onDocMouseDown, true);
      window.removeEventListener('keydown', onKeyDown);
      detachDragListeners();
    };
  });
</script>

<div
  class="arrangement"
  bind:this={rowEl}
  oncontextmenu={onContext}
  role="toolbar"
  tabindex="-1"
  aria-label="Status bar panels"
>
  {#each view as slot, i (slot.component)}
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div
      class="slot"
      class:dragging={working !== null && dragIndex === i}
      style="margin-left: {slot.gap}px;"
      onpointerdown={(e) => onPointerDown(e, i)}
    >
      {#if slot.component === 'usage'}
        <UsageMeter />
      {:else}
        <SystemStats />
      {/if}
    </div>
  {/each}
</div>

{#if menu}
  <div class="ctx-menu" style="left: {menu.x}px; top: {menu.y}px;" role="menu">
    <button type="button" class="ctx-entry" role="menuitem" onclick={resetArrangement}>
      Reset to default
    </button>
  </div>
{/if}

<style>
  .arrangement {
    display: flex;
    flex-direction: row;
    align-items: center;
    flex: 1 1 auto;
    min-width: 0;
    height: 100%;
    /* Clip rather than spill over the right (settings) cluster on narrow
       windows; the right cluster stays fully visible. */
    overflow: hidden;
  }
  .slot {
    display: inline-flex;
    align-items: center;
    height: 100%;
    box-sizing: border-box;
    padding: 0 var(--space-3);
    /* Frame each panel on both sides. Adjacent packed panels show both
       seams; a dragged-in gap separates them cleanly. */
    border-left: 1px solid var(--border-subtle);
    border-right: 1px solid var(--border-subtle);
    cursor: grab;
    user-select: none;
    touch-action: none;
    flex: 0 0 auto;
  }
  .slot.dragging {
    opacity: 0.5;
    cursor: grabbing;
  }

  .ctx-menu {
    position: fixed;
    background: var(--surface-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-md);
    padding: var(--space-1);
    min-width: 160px;
    z-index: 200;
  }
  .ctx-entry {
    appearance: none;
    border: none;
    background: transparent;
    color: var(--text-primary);
    text-align: left;
    width: 100%;
    padding: 6px var(--space-3);
    font-size: var(--font-size-md);
    font-family: inherit;
    cursor: pointer;
    border-radius: var(--radius-sm);
    transition: background var(--motion-fast) var(--easing-standard);
  }
  .ctx-entry:hover {
    background: var(--surface-4);
  }
  .ctx-entry:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
</style>

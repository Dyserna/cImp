<script lang="ts">
  // A leaf of the layout tree. Renders its own tab bar (scoped to this
  // pane's tabs only) plus a content slot that portals in the active
  // tab's xterm host element from the registry. Each pane has its own
  // active tab; tabs are physically owned by the registry, not by the
  // pane component, so moving a tab between panes is just a slot
  // attach/detach pair — xterm state, scrollback, and the running PTY
  // are preserved.
  //
  // Click-to-focus uses mousedown so xterm can't steal it first; the
  // tab bar's tab clicks bubble through and end up focusing the pane
  // before they update its active tab.

  import { layout, setFocusedPane } from './layout/store';
  import { paneRegistry } from './layout/registry';
  import {
    attachTerminal,
    detachTerminal,
    focusTerminalFor,
    hasTerminal,
    retryTerminal,
  } from './terminals';
  import TabBar from './TabBar.svelte';
  import TabErrorOverlay from './TabErrorOverlay.svelte';
  import ClosedShellOverlay from './ClosedShellOverlay.svelte';
  import OffloadServerView from './OffloadServerView.svelte';
  import GraphMonitorView from './GraphMonitorView.svelte';
  import NoteView from './NoteView.svelte';
  import { isShellTab, isOffloadTab, isGraphMonitorTab, isNoteTab, type TabId } from './tabs/types';
  import { tabs } from './tabs/store';
  import type { LayoutNode, PaneNode } from './layout/types';

  let { pane }: { pane: PaneNode } = $props();

  let paneEl: HTMLDivElement | undefined = $state();
  let slotEl: HTMLDivElement | undefined = $state();
  let mountedTab: TabId | null = $state(null);

  // Register this pane's root element with the DOM registry so the
  // M2 drag-and-drop hit-tester can find it. Cleanup runs on unmount
  // and (defensively) when the pane id changes — in practice id is
  // stable for a pane's lifetime, but the cleanup captures the prior
  // id from the closure so an id change wouldn't strand a stale entry.
  $effect(() => {
    if (!paneEl) return;
    const id = pane.id;
    paneRegistry.setPaneElement(id, paneEl);
    return () => {
      paneRegistry.setPaneElement(id, null);
    };
  });

  // Track the currently mounted tab so we can detach it on the next
  // active-tab change (or on unmount). Both detach calls pass `slotEl`
  // so that during a tree rearrangement (e.g. split-pane), an
  // unmounting pane won't pull a host back to offscreen if a sibling
  // pane has already attached it — the host's current parent is no
  // longer this slot in that case, so detach skips.
  //
  // The attach passes `focus: focused` so only the focused pane's
  // active-tab change moves keyboard focus to xterm. Without this
  // gate, an active-tab swap in any pane (context menu, drag commit,
  // layout-store mutation) would steal focus from whichever pane the
  // user is currently typing in. The dedicated focus-on-pane-focus
  // effect below still fires `focusTerminalFor` when the focused
  // pane's tab is unchanged.
  $effect(() => {
    if (!slotEl) return;
    const desired = pane.active_tab_id;
    if (mountedTab === desired) return;
    if (mountedTab !== null) {
      detachTerminal(mountedTab, slotEl);
    }
    if (desired !== null && hasTerminal(desired)) {
      attachTerminal(desired, slotEl, { focus: focused });
    }
    mountedTab = desired;
  });

  // Detach on unmount so the tab's host doesn't get torn down with this
  // component — the registry owns the host's lifetime, not us. The
  // slot-conditional detach prevents a tear-down of a freshly-split
  // pane from yanking its tabs back offscreen.
  $effect(() => {
    return () => {
      if (mountedTab !== null && slotEl) {
        detachTerminal(mountedTab, slotEl);
        mountedTab = null;
      }
    };
  });

  const focused = $derived($layout.focused_pane_id === pane.id);

  // Compute "Pane N of M, contains <active tab name> and K more tabs"
  // for screen readers. Walks the layout tree once per render — cheap
  // for the small tree sizes we ever expect (typically <8 panes). The
  // active-tab name comes from the live tabs store so renames are
  // reflected immediately.
  const ariaLabel = $derived.by(() => {
    const panes: PaneNode[] = [];
    const visit = (node: LayoutNode): void => {
      if (node.type === 'pane') {
        panes.push(node);
        return;
      }
      visit(node.first);
      visit(node.second);
    };
    visit($layout.tree);
    const index = panes.findIndex((p) => p.id === pane.id);
    const ordinal = index < 0 ? 1 : index + 1;
    const total = panes.length;
    const activeId = pane.active_tab_id;
    const active = activeId ? $tabs.find((t) => t.id === activeId) : null;
    const others = Math.max(0, pane.tab_ids.length - 1);
    if (!active) {
      return `Pane ${ordinal} of ${total}, empty`;
    }
    if (others === 0) {
      return `Pane ${ordinal} of ${total}, contains ${active.name}`;
    }
    return `Pane ${ordinal} of ${total}, contains ${active.name} and ${others} more tab${others === 1 ? '' : 's'}`;
  });

  // When the pane becomes focused, refocus its terminal. Without this a
  // pane-to-pane focus shift via click on the bar (or the M3 keyboard
  // shortcuts) wouldn't move keyboard focus to the new pane's xterm,
  // because the slot effect short-circuits when the active tab id
  // hasn't changed.
  $effect(() => {
    if (!focused) return;
    const tabId = pane.active_tab_id;
    if (tabId === null) return;
    requestAnimationFrame(() => focusTerminalFor(tabId));
  });

  function handlePaneMouseDown(): void {
    setFocusedPane(pane.id);
  }

  function handleRetry(): void {
    if (pane.active_tab_id === null) return;
    void retryTerminal(pane.active_tab_id);
  }
</script>

<div
  class="pane"
  class:focused
  role="group"
  aria-label={ariaLabel}
  bind:this={paneEl}
  onmousedowncapture={handlePaneMouseDown}
>
  <TabBar {pane} />
  <div class="pane-content">
    <div class="terminal-slot" bind:this={slotEl}></div>
    {#if pane.active_tab_id !== null}
      {#if isOffloadTab(pane.active_tab_id)}
        <OffloadServerView />
      {:else if isGraphMonitorTab(pane.active_tab_id)}
        <GraphMonitorView />
      {:else if isNoteTab(pane.active_tab_id)}
        <NoteView />
      {:else}
        <TabErrorOverlay tabId={pane.active_tab_id} onretry={handleRetry} />
        {#if isShellTab(pane.active_tab_id)}
          <ClosedShellOverlay tabId={pane.active_tab_id} />
        {/if}
      {/if}
    {/if}
  </div>
</div>

<style>
  .pane {
    display: flex;
    flex-direction: column;
    flex: 1 1 0%;
    /* Critical: nested flexbox refuses to shrink below content's
       intrinsic size without these. Without min-*: 0, splits stop
       resizing once a terminal's preferred width exceeds the pane. */
    min-width: 0;
    min-height: 0;
    overflow: hidden;
  }
  .pane-content {
    position: relative;
    flex: 1 1 auto;
    min-width: 0;
    min-height: 0;
    background: var(--surface-0);
  }
  .terminal-slot {
    position: absolute;
    inset: 0;
  }
  /* Focused-pane indicator: a 2px accent line along the top edge of
     the focused pane's tab bar. Top placement (not bottom) so it
     doesn't visually merge with the active-tab underline, which is
     also the accent color — when both rendered at the bottom they
     read as one continuous line and the focused-vs-active distinction
     blurs. Single-pane layouts still render it; with no unfocused
     pane to compare against it just looks like a top accent on the bar. */
  .pane.focused :global(.tab-bar) {
    box-shadow: inset 0 2px 0 0 var(--accent);
  }
  /* Keyboard focus traversal: the pane root receives focus via
     Ctrl+Alt+Arrow (which calls setFocusedPane). Tab key doesn't
     reach the pane root because the role is group, but :focus-visible
     on the inner xterm host is good enough — for ARIA traversal the
     aria-label on the .pane element above is what gets announced. */
  .pane:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
</style>

// Keep-alive registry for the reserved app-rendered views (Code
// Intelligence, Note, Workbench, Graph View, Tool Activity, Code Audit).
// Mirrors
// the terminal registry (terminals.ts): each view is mounted ONCE into a
// registry-owned host element (lazily, on first activation) and panes just
// attach/detach that host — so switching tabs, hiding/un-hiding, or moving
// the tab between panes never destroys the component, and every bit of its
// in-memory state (selections, expansions, scroll, the Graph View's laid-out
// simulation) survives.
//
// Cost control: a detached view keeps running, so anything periodic in a
// view must gate itself on appViewVisibility (appViewVisibility.ts) — the
// registry flips that store on attach/detach. GraphView needs nothing extra:
// its IntersectionObserver already pauses the render loop when detached.
//
// The view is unmounted for real only when its tab is closed (feature toggled
// off, builtin removed) — see the tab-closed hook in avatarState.ts.

import { mount, unmount, type Component } from 'svelte';
import {
  CODE_AUDIT_TAB_ID,
  GRAPH_MONITOR_TAB_ID,
  GRAPH_VIEW_TAB_ID,
  NOTE_TAB_ID,
  TOOL_ACTIVITY_TAB_ID,
  WORKBENCH_TAB_ID,
  type TabId,
} from './tabs/types';
import { setAppViewVisible } from './appViewVisibility';
import CodeIntelligenceView from './CodeIntelligenceView.svelte';
import NoteView from './NoteView.svelte';
import WorkbenchView from './WorkbenchView.svelte';
import GraphView from './GraphView.svelte';
import ToolActivityView from './ToolActivityView.svelte';
import CodeAuditView from './CodeAuditView.svelte';

const COMPONENTS = new Map<TabId, Component>([
  [GRAPH_MONITOR_TAB_ID, CodeIntelligenceView as Component],
  [NOTE_TAB_ID, NoteView as Component],
  [WORKBENCH_TAB_ID, WorkbenchView as Component],
  [GRAPH_VIEW_TAB_ID, GraphView as Component],
  [TOOL_ACTIVITY_TAB_ID, ToolActivityView as Component],
  [CODE_AUDIT_TAB_ID, CodeAuditView as Component],
]);

interface Host {
  el: HTMLDivElement;
  instance: Record<string, unknown>;
}

const hosts = new Map<TabId, Host>();

/// True for tab ids this registry renders. Pane uses it to suppress the
/// error/closed overlays that only make sense for real terminal tabs.
export function isAppViewTab(id: TabId): boolean {
  return COMPONENTS.has(id);
}

/// Put `id`'s view on screen inside `slot`, creating (and mounting) the host
/// on first use. No-op for non-app-view ids. If the host is currently in
/// another pane's slot, appendChild relocates it — the old pane's detach
/// then skips via the parent check, same convention as attachTerminal.
export function attachAppView(id: TabId, slot: HTMLElement): void {
  const component = COMPONENTS.get(id);
  if (!component) return;
  let host = hosts.get(id);
  if (!host) {
    const el = document.createElement('div');
    el.className = 'app-view-host';
    // The host fills the pane content area and re-enables the pointer events
    // its .app-slot parent disables (the slot must stay click-through when
    // empty, or it would swallow every click on the terminal below it).
    el.style.position = 'absolute';
    el.style.inset = '0';
    el.style.pointerEvents = 'auto';
    // Visible BEFORE the first mount: init-time onAppViewShown subscriptions
    // must not see a false→true flip (their onMount covers first-paint work).
    // And in the DOM before the first mount, so component init observes real
    // geometry — the same conditions as the old inline rendering.
    setAppViewVisible(id, true);
    slot.appendChild(el);
    host = { el, instance: mount(component, { target: el }) };
    hosts.set(id, host);
    return;
  }
  if (host.el.parentElement !== slot) slot.appendChild(host.el);
  setAppViewVisible(id, true);
}

/// Take `id`'s view off screen. Skips when `slot` no longer owns the host
/// (a sibling pane already attached it during a layout rearrangement).
export function detachAppView(id: TabId, slot: HTMLElement): void {
  const host = hosts.get(id);
  if (!host || host.el.parentElement !== slot) return;
  slot.removeChild(host.el);
  setAppViewVisible(id, false);
}

/// Really unmount the view and drop its host — tab-closed lifecycle only.
/// The next attach after a tab-id reuse starts a fresh instance.
export function destroyAppView(id: TabId): void {
  const host = hosts.get(id);
  if (!host) return;
  hosts.delete(id);
  setAppViewVisible(id, false);
  void unmount(host.instance);
  host.el.remove();
}

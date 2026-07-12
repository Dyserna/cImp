// Visibility signal for the keep-alive app views (see appViews.ts). An app
// view stays MOUNTED for the app's lifetime once created — this store is how
// a view knows whether it's actually on screen, so background polls can idle
// while it's detached and refresh the moment it returns.
//
// Deliberately a separate module from appViews.ts: views import THIS (and
// appViews.ts imports the view components), so there's no import cycle.

import { get, writable, type Readable, type Writable } from 'svelte/store';
import type { TabId } from './tabs/types';

const stores = new Map<TabId, Writable<boolean>>();

function store(id: TabId): Writable<boolean> {
  let s = stores.get(id);
  if (!s) {
    s = writable(false);
    stores.set(id, s);
  }
  return s;
}

/// Reactive visibility of one app view (true ⇔ its host is attached to a
/// pane). Safe to call for any tab id — non-app-view ids just stay false.
export function appViewVisibility(id: TabId): Readable<boolean> {
  return store(id);
}

export function isAppViewVisible(id: TabId): boolean {
  return get(store(id));
}

/// Registry-side setter — only appViews.ts should call this.
export function setAppViewVisible(id: TabId, visible: boolean): void {
  store(id).set(visible);
}

/// Run `cb` on every hidden→visible transition (NOT on subscribe, and not on
/// the initial attach — the view's own onMount covers first-paint work).
/// Returns the unsubscriber; callers tie it to component teardown.
export function onAppViewShown(id: TabId, cb: () => void): () => void {
  let prev = isAppViewVisible(id);
  return store(id).subscribe((v) => {
    const was = prev;
    prev = v;
    if (v && !was) cb();
  });
}

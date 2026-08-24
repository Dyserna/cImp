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

/// Run `tick` every `everyMs` for as long as the app view `id` is on screen,
/// and stop when the returned teardown is called.
///
/// THE RULE THIS EXISTS TO KEEP (#130). An app view stays mounted for the
/// app's lifetime once created, so a bare `setInterval` in one keeps burning
/// IPC forever after the tab is opened once — even while it is detached.
/// Every keep-alive poll therefore has to gate on `isAppViewVisible`, and
/// five of them wrote that gate out by hand. One spelling, five callers: a
/// sixth poll added tomorrow gets the gate by construction rather than by
/// whether its author had read the comment on one of the others.
///
/// The GATE ONLY. Two things deliberately stay at the call site: the
/// hidden→visible refresh (`onAppViewShown`, which several views want to do
/// something different from a tick) and whatever the tick itself is — the
/// Events feed runs its checkpoint read on every third tick, the Timeline
/// asks for a cheaper refresh than its on-shown one. Those are decisions,
/// not boilerplate.
export function pollWhileVisible(
  id: TabId,
  tick: () => void,
  everyMs: number,
  opts: {
    /// Skip THIS tick without stopping the poll — an action or a fetch
    /// already in flight. Asked after the visibility gate, so a hidden view
    /// never pays for it.
    skipWhen?: () => boolean;
  } = {},
): () => void {
  const timer = setInterval(() => {
    if (!isAppViewVisible(id)) return;
    if (opts.skipWhen?.()) return;
    tick();
  }, everyMs);
  return () => clearInterval(timer);
}

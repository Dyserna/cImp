// V13 Phase B — the shared live-diff summary store. A single consumer that
// mounts for the app's lifetime (`WorkbenchDiffBadge.svelte`, in the status
// bar) owns the underlying listener/poll; the Diff section (`DiffView.svelte`)
// just reads `workbenchDiff` rather than re-fetching the same summary, and
// only calls `workbenchDiffFile` for files the user actually expands. Both
// call `watchWorkbenchDiff()`/release it in `onMount`/`onDestroy` — the
// ref-counting means whichever one is mounted first starts it and whichever
// is mounted last tears it down, with no duplicate listeners in between.
//
// Refresh sources:
//   - the `fs-batch` Tauri event (debounced 500ms), emitted by the graph
//     watcher only while `workbench.enabled` — see `workbench::publish_fs_batch`.
//   - since fs-batch only flows while the graph watcher is running
//     (`graph.enabled` — `graph::service::start_watch` is a no-op otherwise),
//     a 5s poll fallback that skips itself whenever `graph.enabled` is on
//     (fs-batch already covers that case, so the tick is a no-op rather than
//     the poll being torn down/rebuilt on every settings change).
import { writable, get } from 'svelte/store';
import { type UnlistenFn } from '@tauri-apps/api/event';
import { listenEvent, FS_BATCH } from './events';
import { settings } from './settings/store';
import { workbenchDiffSummary, type DiffSummary } from './workbench';

export const workbenchDiff = writable<DiffSummary | null>(null);
export const workbenchDiffError = writable<string | null>(null);

const POLL_MS = 5000;
const DEBOUNCE_MS = 500;

let refCount = 0;
let unlistenFsBatch: UnlistenFn | null = null;
let debounceTimer: ReturnType<typeof setTimeout> | undefined;
let pollTimer: ReturnType<typeof setInterval> | undefined;
// Monotonic token: bumped on every refresh start AND on teardown. A refresh
// only writes the stores if its token is still current — so a slow
// `workbench_diff_summary` that resolves after a newer refresh (or after the
// watcher was torn down) is discarded instead of clobbering fresher state
// with stale data (e.g. a post-revert refresh being overwritten by an
// in-flight fs-batch refresh, showing a phantom un-reverted hunk).
let refreshSeq = 0;
// Monotonic token for the fs-batch listener registration. `listen()` resolves
// asynchronously; if the last consumer releases (or a new session starts)
// before it resolves, the pending registration must unlisten itself rather
// than leak a handler for the app's lifetime.
let listenGen = 0;

async function refresh(): Promise<void> {
  const mine = ++refreshSeq;
  try {
    const summary = await workbenchDiffSummary();
    if (mine !== refreshSeq) return;
    workbenchDiff.set(summary);
    workbenchDiffError.set(null);
  } catch (e) {
    if (mine !== refreshSeq) return;
    workbenchDiffError.set(String(e));
    console.warn('workbench_diff_summary failed:', e);
  }
}

function scheduleRefresh(): void {
  if (debounceTimer) clearTimeout(debounceTimer);
  debounceTimer = setTimeout(() => void refresh(), DEBOUNCE_MS);
}

/// Start (ref-counted) the shared fs-batch listener + fallback poll, and do
/// an immediate refresh. Call from `onMount`; call the returned function
/// from `onDestroy`/when the caller no longer needs live updates (e.g. the
/// badge hiding because `workbench.enabled` flipped off). Safe to call from
/// multiple components concurrently — the underlying listener/poll starts
/// once and tears down only when the last consumer releases it.
export function watchWorkbenchDiff(): () => void {
  refCount += 1;
  if (refCount === 1) {
    const gen = ++listenGen;
    void refresh();
    listenEvent(FS_BATCH, () => scheduleRefresh())
      .then((fn) => {
        // If this session was torn down (or superseded by a newer start)
        // while `listen()` was still resolving, `gen` no longer matches —
        // unlisten immediately so the handler doesn't leak.
        if (gen === listenGen && refCount > 0) {
          unlistenFsBatch = fn;
        } else {
          fn();
        }
      })
      .catch((e) => console.warn('fs-batch listen failed:', e));
    pollTimer = setInterval(() => {
      // fs-batch already covers the "graph watcher running" case; only do
      // the redundant poll-driven refresh when it won't fire on its own.
      if (get(settings).graph.enabled) return;
      void refresh();
    }, POLL_MS);
  }
  let released = false;
  return () => {
    if (released) return;
    released = true;
    refCount = Math.max(0, refCount - 1);
    if (refCount === 0) {
      // Invalidate any in-flight listen() registration and any in-flight
      // refresh so neither lands after teardown (leaked handler / stale store
      // write).
      listenGen += 1;
      refreshSeq += 1;
      unlistenFsBatch?.();
      unlistenFsBatch = null;
      if (pollTimer) {
        clearInterval(pollTimer);
        pollTimer = undefined;
      }
      if (debounceTimer) {
        clearTimeout(debounceTimer);
        debounceTimer = undefined;
      }
      // Drop the last session's summary/error so a later remount doesn't
      // briefly render stale state before its first refresh resolves.
      workbenchDiff.set(null);
      workbenchDiffError.set(null);
    }
  };
}

/// Force an immediate refresh (bypassing the debounce) — used right after a
/// successful hunk revert so the badge/file list update without waiting for
/// the next fs-batch event.
export function refreshWorkbenchDiffNow(): Promise<void> {
  return refresh();
}

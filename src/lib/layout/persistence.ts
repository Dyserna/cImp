// Layout persistence: the save subscription that pushes layout-store
// changes back to the backend, and the serializer it uses.
//
// Hydration is NOT here any more. V42 Phase B moved the integrity walk
// that adapts a persisted tree to the live tab list into the backend
// (`src-tauri/src/settings/layout.rs`), so what arrives in
// `settings.layout` is already correct and App.svelte sets it verbatim.
// A preset restore runs the same walk through the `restore_layout_preset`
// command. There is no frontend copy of those rules, deliberately — two
// copies is how the tree the user sees stops matching the tree on disk.
//
// Save flow at runtime: every layout-store update fires a `save_layout`
// IPC call. The first emission after installLayoutPersistence() (the
// hydration emission, or the initial store value if no hydration
// happened) is intentionally swallowed — we don't want to round-trip
// the store's value back to the backend on launch.
//
// Restore-preset flow: callers swap the layout-store value via
// `layout.set(...)`; the same subscription persists the new tree.

import { type Unsubscriber } from 'svelte/store';
import { layout } from './store';
import { saveLayout } from './ipc';
import { type LayoutState } from './types';
import type { LayoutPersisted } from '../settings/types';

/// Serialize the in-memory `LayoutState` for the backend. The wire
/// shape is identical to the in-memory shape — both use the
/// `'split' | 'pane'`-discriminated tree with the same field names —
/// so this is structural identity. Kept as a named function so future
/// shape divergences have a single conversion point.
export function serializeLayout(state: LayoutState): LayoutPersisted {
  return { tree: state.tree, focused_pane_id: state.focused_pane_id };
}

/// Install the eager save subscription. Subscribes to the layout store
/// and invokes `save_layout` immediately on every mutation.
///
/// V0.6+ change: pre-V0.6 used a 250ms front-end debounce that left a
/// closing-race window where a layout edit in the last 250ms before
/// `beforeunload` was silently dropped (the IPC promise resolved after
/// the WebView had already torn down). The backend already debounces
/// settings persistence by 500ms, so the front-end debounce was double
/// rate-limiting; removing it closes the race without adding disk
/// writes — the backend still coalesces.
///
/// The very first emission is swallowed: Svelte writables fire on
/// subscribe with the current value, and we don't want to round-trip
/// the just-hydrated layout back to the backend.
///
/// Returns an unsubscribe function.
export function installLayoutPersistence(): Unsubscriber {
  let firstEmission = true;
  return layout.subscribe((state) => {
    if (firstEmission) {
      firstEmission = false;
      return;
    }
    void saveLayout(serializeLayout(state)).catch((e) => {
      console.error('save_layout failed', e);
    });
  });
}


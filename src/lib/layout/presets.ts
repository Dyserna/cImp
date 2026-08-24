// Preset actions for the Layouts menu (V4-04). Thin orchestrators on
// top of the IPC wrappers — they snapshot the current state, call the
// backend, and (for restore) drive the local layout store. The
// settings store updates reactively from the backend's broadcast on
// every preset CRUD, so callers don't need to refresh anything by
// hand.

import { get } from 'svelte/store';
import { layout } from './store';
import { cancelDrag } from '../dnd/drag';
import {
  deleteLayoutPreset as deletePresetIpc,
  renameLayoutPreset as renamePresetIpc,
  restoreLayoutPreset as restorePresetIpc,
  saveLayoutPreset as savePresetIpc,
} from './ipc';

/// Snapshot the current layout's tree (without focus — presets are
/// "set up panes this way" and focus is the user's next-click
/// concern) and persist under `name`. Upserts: a same-named preset is
/// replaced.
///
/// The settings store reactively receives the new presets list via the
/// `settings-changed` broadcast, so the popover and the manage dialog
/// pick it up without a refetch. Errors propagate so callers can show
/// them inline (e.g. an empty-name validation failure).
export async function saveCurrentLayoutAsPreset(name: string): Promise<void> {
  const tree = get(layout).tree;
  await savePresetIpc(name, tree);
}

/// Restore a preset into the live layout.
///
/// The backend adapts the preset's tree to the current tab list — orphans
/// (tabs created since the preset was saved) land in the focused pane, missing
/// tabs are dropped, hidden tabs stay hidden, ratios are clamped — and seeds
/// focus from the leftmost leaf, because presets don't store focus and the
/// user's next click moves it from there. V42 Phase B: that is the same
/// integrity walk the launch path runs, and it exists only there; this function
/// asks for the answer rather than recomputing it.
///
/// Async now that the adaptation is an IPC round-trip. Nothing else about the
/// flow changed: the eager layout-save subscription (installed in App.svelte)
/// writes the result back to settings on its own — no extra IPC from here.
///
/// A rejection (only cause: the preset vanished between the menu render and the
/// click) leaves the live layout alone, which is the right outcome — the user
/// keeps what they had.
export async function restoreLayoutPreset(name: string): Promise<void> {
  let repaired;
  try {
    repaired = await restorePresetIpc(name);
  } catch (e) {
    console.warn(`restoreLayoutPreset: ${String(e)}`);
    return;
  }
  // A drag in flight references the live tree's pane ids; replacing the tree
  // wholesale would strand sourcePaneId. Cancelling the drag first is cheaper
  // than threading "tree may have changed" checks through every pointermove
  // handler. After the await, not before: cancelling a drag the user is still
  // making and THEN failing the restore would be a change with no result.
  cancelDrag();
  layout.set(repaired);
}

/// Delete a preset by name. No-op if the name doesn't exist.
export async function deleteLayoutPreset(name: string): Promise<void> {
  await deletePresetIpc(name);
}

/// Rename a preset. Errors when `oldName` doesn't exist or `newName`
/// collides with another preset (caller should surface inline). The
/// `created_at` timestamp is preserved — only the name changes.
export async function renameLayoutPreset(
  oldName: string,
  newName: string,
): Promise<void> {
  await renamePresetIpc(oldName, newName);
}

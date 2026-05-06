// Preset actions for the Layouts menu (V4-04). Thin orchestrators on
// top of the IPC wrappers — they snapshot the current state, call the
// backend, and (for restore) drive the local layout store. The
// settings store updates reactively from the backend's broadcast on
// every preset CRUD, so callers don't need to refresh anything by
// hand.

import { get } from 'svelte/store';
import { layout } from './store';
import { settings } from '../settings/store';
import { tabs as tabsStore } from '../settings/store';
import { cancelDrag } from '../dnd/drag';
import {
  deleteLayoutPreset as deletePresetIpc,
  renameLayoutPreset as renamePresetIpc,
  saveLayoutPreset as savePresetIpc,
} from './ipc';
import {
  leftmostLeafPaneId,
  validateAndRepairLayout,
} from './persistence';
import type { LayoutPersisted, LayoutPreset } from '../settings/types';

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

/// Restore a preset into the live layout. The preset's tree is
/// adapted to the current tab list via the same integrity sieve used
/// at launch — orphans (tabs created since the preset was saved)
/// land in the focused pane, missing tabs are silently dropped.
/// Focus is seeded from the leftmost leaf because presets don't
/// store focus; the user's next click moves it from there.
///
/// The debounced layout-save subscription (installed in App.svelte)
/// writes the new layout back to settings on its own — no extra IPC
/// from this function.
export function restoreLayoutPreset(name: string): void {
  const presets = get(settings).layout_presets;
  const preset = presets.find((p) => p.name === name);
  if (!preset) {
    console.warn(`restoreLayoutPreset: no preset named '${name}'`);
    return;
  }
  // A drag in flight references the live tree's pane ids; replacing
  // the tree wholesale would strand sourcePaneId. Cancelling the drag
  // first is cheaper than threading "tree may have changed" checks
  // through every pointermove handler.
  cancelDrag();
  const persisted: LayoutPersisted = {
    tree: preset.tree,
    focused_pane_id: leftmostLeafPaneId(preset.tree),
  };
  const repaired = validateAndRepairLayout(persisted, get(tabsStore));
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

/// Read-only snapshot of the current presets list, sorted by
/// `created_at` descending (most recent first). Used by the popover
/// to render the "Recent presets" section. Stable equality: re-call
/// after a settings update to get the refreshed order.
export function recentPresets(): LayoutPreset[] {
  const presets = get(settings).layout_presets;
  return [...presets].sort((a, b) => b.created_at.localeCompare(a.created_at));
}

// Layout / preset IPC wrappers (V4-04). Each function targets a single
// Tauri command on the backend; the backend's `SettingsHandle::set`
// after every mutation drives the broadcast that updates the frontend
// settings store, so callers don't need to refresh anything by hand —
// `settings.layout_presets` reactively reflects the new list.

import { invoke } from '@tauri-apps/api/core';
import type { LayoutNode } from './types';
import type { LayoutPersisted } from '../settings/types';

/// Push the full layout state to the backend. Frontend callers should
/// debounce this — see `installLayoutPersistence` in `persistence.ts`.
/// The backend's settings handle does its own 500ms debounce on the
/// disk write, but the broadcast fires synchronously, so spamming this
/// during a splitter drag would re-emit `settings-changed` 60 times a
/// second. The frontend's debounce keeps that quiet.
export async function saveLayout(layout: LayoutPersisted): Promise<void> {
  await invoke('save_layout', { layout });
}

/// Save the current layout tree under `name`. Upserts: a preset with
/// the same name is replaced (preserving its `created_at`). Whitespace
/// is trimmed by the backend; an all-whitespace name returns an error
/// the caller should surface.
export async function saveLayoutPreset(name: string, tree: LayoutNode): Promise<void> {
  await invoke('save_layout_preset', { name, tree });
}

/// Ask the backend for a preset's tree, adapted to the live tab list and ready
/// to drop into the layout store: tabs deleted since the save are gone, tabs
/// created since it land in the focused pane, hidden tabs stay hidden, ratios
/// are clamped, panes emptied by any of that collapse. Same integrity walk the
/// load path runs — the rules live in `settings::layout` and nowhere else.
/// Rejects when no preset has that name.
export async function restoreLayoutPreset(name: string): Promise<LayoutPersisted> {
  return await invoke<LayoutPersisted>('restore_layout_preset', { name });
}

/// Delete a preset by name. No-op when the name doesn't exist.
export async function deleteLayoutPreset(name: string): Promise<void> {
  await invoke('delete_layout_preset', { name });
}

/// Rename a preset. Errors if `oldName` doesn't exist or `newName`
/// collides; the dialog should surface the error inline.
export async function renameLayoutPreset(
  oldName: string,
  newName: string,
): Promise<void> {
  await invoke('rename_layout_preset', { oldName, newName });
}

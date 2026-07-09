// Single global dialog store. Only one dialog is visible at a time;
// opening a new one while another is open replaces the existing dialog
// (the previous dialog's transient state is discarded). Dialog
// components subscribe to `dialogState` and render when the discriminator
// matches their kind.

import { writable, type Writable } from 'svelte/store';
import type { TabId } from '../tabs/types';

export type DialogState =
  | { kind: 'none' }
  | { kind: 'new-shell-tab' }
  | { kind: 'configure-tab'; tab: TabId }
  | { kind: 'save-layout' }
  | { kind: 'manage-presets' }
  | { kind: 'restore-checkpoint'; id: string; root?: string };

export const dialogState: Writable<DialogState> = writable({ kind: 'none' });

export function openNewShellTabDialog(): void {
  dialogState.set({ kind: 'new-shell-tab' });
}

export function openConfigureTabDialog(tab: TabId): void {
  dialogState.set({ kind: 'configure-tab', tab });
}

/// Opened from the Layouts popover's "Save current layout as..." entry.
/// The dialog itself snapshots the live layout on submit, so no payload
/// is needed in the dialog state.
export function openSaveLayoutDialog(): void {
  dialogState.set({ kind: 'save-layout' });
}

/// Opened from the Layouts popover's "Manage presets..." entry. The
/// dialog renders from the live `settings.layout_presets` reactive
/// store; no payload needed.
export function openManagePresetsDialog(): void {
  dialogState.set({ kind: 'manage-presets' });
}

/// V13 Phase C: opened from the Timeline section's "Restore" row action.
/// The dialog itself fetches the dry-run diff (`workbench_checkpoint_diff`)
/// on open to list the affected files — no diff payload needed here, just
/// which checkpoint and (optionally) which project root.
export function openRestoreCheckpointDialog(id: string, root?: string): void {
  dialogState.set({ kind: 'restore-checkpoint', id, root });
}

export function closeDialog(): void {
  dialogState.set({ kind: 'none' });
}

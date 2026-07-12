// Single global dialog store. Only one dialog is visible at a time;
// opening a new one while another is open replaces the existing dialog
// (the previous dialog's transient state is discarded). Dialog
// components subscribe to `dialogState` and render when the discriminator
// matches their kind.

import { writable, type Writable } from 'svelte/store';
import type { TabId } from '../tabs/types';
import type { PaneId } from '../layout/types';

export type DialogState =
  | { kind: 'none' }
  | { kind: 'new-shell-tab'; paneId: PaneId | null }
  | { kind: 'configure-tab'; tab: TabId }
  | { kind: 'save-layout' }
  | { kind: 'manage-presets' }
  | { kind: 'restore-checkpoint'; id: string; root?: string }
  | { kind: 'new-worktree-tab'; template: TabId; paneId: PaneId }
  | { kind: 'offload-start-command'; name: string; command: string };

export const dialogState: Writable<DialogState> = writable({ kind: 'none' });

/// `paneId` is where the new shell tab should land: the pane whose `+`
/// button opened the dialog, or `null` (default — the Ctrl+T shortcut
/// path) for "the focused pane". The dialog enqueues the placement at
/// submit time, not here — pushing it at open time leaked a stale
/// placement whenever the dialog was cancelled.
export function openNewShellTabDialog(paneId: PaneId | null = null): void {
  dialogState.set({ kind: 'new-shell-tab', paneId });
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

/// V13 Phase D D3: opened from a builtin AI tab's context menu — "New
/// <Claude|OpenCode> tab in worktree…". `template` is the AI tab whose
/// config the new tab clones; `paneId` is where the frontend routes the new
/// tab once spawned (mirrors the plain "+" duplicate's `requestTabIntoPane`).
export function openNewWorktreeTabDialog(template: TabId, paneId: PaneId): void {
  dialogState.set({ kind: 'new-worktree-tab', template, paneId });
}

/// Opened by the Offload Server tab's Start button when the Local backend's
/// "Show command on start" setting is on. `command` seeds the editable
/// textarea with the configured `server_command`; the dialog launches with
/// the (possibly edited) command as a one-shot override — never persisted.
export function openOffloadStartCommandDialog(name: string, command: string): void {
  dialogState.set({ kind: 'offload-start-command', name, command });
}

export function closeDialog(): void {
  dialogState.set({ kind: 'none' });
}

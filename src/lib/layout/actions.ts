// Async layout actions: thin glue between IPC (create_shell_tab,
// default_shell_spec) and the layout store. The store itself stays
// IPC-free so its mutations remain synchronous and unit-testable; the
// helpers here orchestrate "create tab on the backend, then route it
// into a fresh sibling pane via pendingTabPlacement."

import { get } from 'svelte/store';
import { createShellTab, defaultShellSpec } from '../ipc';
import { tabs } from '../tabs/store';
import type { SplitDirection } from './types';
import {
  layout,
  requestTabIntoSplit,
  cancelPlacement,
} from './store';

/// Split the focused pane in `direction` and put a fresh Shell tab in
/// the new pane. Called from the `Ctrl+\` / `Ctrl+Shift+\` shortcuts
/// and from the pane context menu.
///
/// Sequence:
///   1. Snapshot the focused pane id.
///   2. Resolve default shell command + args from the backend's
///      auto-detection (Git Bash on Windows, $SHELL elsewhere).
///   3. Set `pendingTabPlacement` to a `split` request so the
///      tab-created event consumer routes the new tab into a sibling
///      pane rather than appending to the focused pane.
///   4. Call `create_shell_tab`. The backend creates the tab,
///      broadcasts `tab-created`, and `applyTabCreatedToLayout`
///      consumes the placement cell to perform the split.
///
/// On IPC failure, the placement cell is cleared so a subsequent
/// unrelated tab-created event can't accidentally use a stale
/// instruction. The caller (settings dispatcher) logs the error; we
/// don't surface a toast here because the failure modes are the same
/// as the New Shell Tab dialog and the user can retry via the dialog
/// for a more informative error.
export async function splitFocusedPaneWithNewShell(
  direction: SplitDirection,
  placeOn: 'first' | 'second' = 'second',
): Promise<void> {
  const sourcePaneId = get(layout).focused_pane_id;

  let command: string;
  let argsString: string;
  let notificationsError: string;
  let notificationsExited: string;
  try {
    const spec = await defaultShellSpec();
    command = spec.command;
    argsString = spec.args;
    notificationsError = spec.notifications_error;
    notificationsExited = spec.notifications_exited;
  } catch (e) {
    console.error('default_shell_spec failed:', e);
    return;
  }

  // Mirror the dialog's name-from-count heuristic so keyboard-created
  // shells get the same "Shell N" sequence the dialog produces.
  const shellCount = get(tabs).filter((m) => !m.builtin).length;
  const name = `Shell ${shellCount + 1}`;

  const placement = requestTabIntoSplit(sourcePaneId, direction, placeOn);
  try {
    await createShellTab({
      name,
      command,
      argsString,
      cwd: null,
      env: {},
      notificationsError,
      notificationsExited,
    });
  } catch (e) {
    cancelPlacement(placement);
    console.error('create_shell_tab failed:', e);
  }
}

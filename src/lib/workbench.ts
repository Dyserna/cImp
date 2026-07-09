// V13 Phase A — frontend IPC wrapper for the Workbench tab. Diff (Phase B),
// checkpoints (Phase C), and worktrees (Phase D) each add their own wrappers
// here as they land; today this only carries the top-of-view banner status
// `WorkbenchView` needs to explain why a section is unavailable.

import { invoke } from '@tauri-apps/api/core';

/// Mirror of Rust `workbench::WorkbenchStatus`. `git_available: false` implies
/// `is_repo: false` (there's no point probing further without `git`).
export interface WorkbenchStatus {
  git_available: boolean;
  is_repo: boolean;
}

/// `root` defaults (backend side) to the app's launch directory.
export function workbenchStatus(root?: string): Promise<WorkbenchStatus> {
  return invoke<WorkbenchStatus>('workbench_status', { root: root ?? null });
}

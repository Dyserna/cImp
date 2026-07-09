// V13 Phase A/B — frontend IPC wrapper for the Workbench tab. Diff (Phase B)
// lives here alongside the Phase A top-of-view status; checkpoints (Phase C)
// and worktrees (Phase D) add their own wrappers as they land.

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

// ── Phase B: live diff pane ────────────────────────────────────────────

/// Mirror of Rust `workbench::diff::FileStatus`. A discriminated union on
/// `kind`, matching the `#[serde(tag = "kind")]` wire shape — `Renamed`
/// carries the source path, every other variant is bare.
export type FileStatus =
  | { kind: 'Modified' }
  | { kind: 'Added' }
  | { kind: 'Deleted' }
  | { kind: 'Renamed'; from: string }
  | { kind: 'Untracked' };

/// Mirror of Rust `workbench::diff::FileDiffMeta` — one row of the file list.
export interface FileDiffMeta {
  path: string;
  status: FileStatus;
  binary: boolean;
  too_large: boolean;
  added: number;
  removed: number;
}

/// Mirror of Rust `workbench::diff::DiffSource`.
export type DiffSource = 'git' | 'shadow';

/// Mirror of Rust `workbench::diff::DiffSummary` — the `workbench_diff_summary`
/// payload. `source: null` means neither git nor a checkpoint snapshot is
/// available (non-git project, checkpoints off) — the frontend renders the
/// requirements banner rather than an empty file list in that case.
export interface DiffSummary {
  files: FileDiffMeta[];
  readonly: boolean;
  source: DiffSource | null;
}

/// Mirror of Rust `workbench::diff::Hunk`. `lines` is `[marker, text]` pairs —
/// `marker` is `' '` (context), `'+'` (added), or `'-'` (removed); `text`
/// excludes the marker and any trailing newline. `hash` is opaque — the
/// frontend never computes or inspects it, only echoes it back verbatim to
/// `workbenchRevertHunk` as the staleness guard.
export interface Hunk {
  header: string;
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  lines: [string, string][];
  hash: string;
}

/// Mirror of Rust `workbench::diff::FileDiff` — the `workbench_diff_file`
/// payload, one file's full parsed diff.
export interface FileDiff {
  path: string;
  status: FileStatus;
  binary: boolean;
  hunks: Hunk[];
  too_large: boolean;
}

/// `root` defaults (backend side) to the app's launch directory.
export function workbenchDiffSummary(root?: string): Promise<DiffSummary> {
  return invoke<DiffSummary>('workbench_diff_summary', { root: root ?? null });
}

export function workbenchDiffFile(path: string, root?: string): Promise<FileDiff> {
  return invoke<FileDiff>('workbench_diff_file', { root: root ?? null, path });
}

/// Revert one hunk. `hunkHash` must be `Hunk.hash` from the last fetched
/// diff for this file (opaque — never computed client-side, only echoed
/// back); a mismatch (the file changed underneath the view) rejects with a
/// typed error rather than applying against stale content. Returns the
/// file's fresh diff on success.
export function workbenchRevertHunk(
  path: string,
  hunkIndex: number,
  hunkHash: string,
  root?: string,
): Promise<FileDiff> {
  return invoke<FileDiff>('workbench_revert_hunk', {
    root: root ?? null,
    path,
    hunkIndex,
    hunkHash,
  });
}

/// Format one hunk as a fenced block + `path:line` header for the compose
/// overlay's "Send to agent" action.
export function workbenchSendHunk(path: string, hunkIndex: number, root?: string): Promise<string> {
  return invoke<string>('workbench_send_hunk', { root: root ?? null, path, hunkIndex });
}

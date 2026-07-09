// V13 Phase A/B/C/D — frontend IPC wrapper for the Workbench tab. Diff
// (Phase B) lives here alongside the Phase A top-of-view status; checkpoints
// (Phase C) and worktrees (Phase D) add their own sections below.

import { invoke } from '@tauri-apps/api/core';
import { writable } from 'svelte/store';

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

// ── Phase C: checkpoints (shadow repo) ──────────────────────────────────

/// Mirror of Rust `workbench::shadow::Trigger`.
export type CheckpointTrigger = 'prompt' | 'burst' | 'manual' | 'pre-restore';

/// Mirror of Rust `workbench::shadow::Checkpoint` — one Timeline row.
export interface Checkpoint {
  id: string;
  seq: number;
  commit: string;
  /// ISO-8601 (from git's own commit date).
  ts: string;
  ts_unix: number;
  label: string;
  trigger: CheckpointTrigger;
  agent: string | null;
  files_changed: number;
}

/// Mirror of Rust `workbench::shadow::RestoreReport` — the
/// `workbench_restore` result, used for the post-restore summary.
export interface RestoreReport {
  pre_restore_id: string;
  changed: string[];
  created_since: string[];
  deleted: string[];
}

/// The Timeline section's row list, oldest first. Empty (not an error) when
/// checkpoints have never run for `root`.
export function workbenchCheckpoints(root?: string): Promise<Checkpoint[]> {
  return invoke<Checkpoint[]>('workbench_checkpoints', { root: root ?? null });
}

/// Checkpoint `id` vs. the CURRENT working tree, parsed the same way
/// `workbenchDiffFile` is — powers both the Timeline's "Diff vs now" viewer
/// and the restore confirmation dialog's dry-run file list.
export function workbenchCheckpointDiff(id: string, root?: string): Promise<FileDiff[]> {
  return invoke<FileDiff[]>('workbench_checkpoint_diff', { root: root ?? null, id });
}

/// The manual "Checkpoint now" action. `label` defaults (backend side) to
/// "manual checkpoint" when omitted.
export function workbenchCheckpointNow(label?: string, root?: string): Promise<string> {
  return invoke<string>('workbench_checkpoint_now', { root: root ?? null, label: label ?? null });
}

/// Restore the working tree to checkpoint `id`. `deleteNew` MUST default to
/// `false` at every call site — the restore confirmation dialog's "delete
/// files created since" checkbox starts unchecked (the dangerous case is
/// silently losing untracked new work, never keeping it).
export function workbenchRestore(id: string, deleteNew: boolean, root?: string): Promise<RestoreReport> {
  return invoke<RestoreReport>('workbench_restore', { root: root ?? null, id, deleteNew });
}

/// Bumped by `RestoreCheckpointDialog` after a successful
/// `workbench_checkpoint_now`/`workbench_restore` — the Timeline section
/// subscribes to trigger a refetch without the dialog needing a direct
/// reference to it (both just depend on this store, not on each other).
export const workbenchCheckpointsVersion = writable<number>(0);
export function bumpWorkbenchCheckpointsVersion(): void {
  workbenchCheckpointsVersion.update((n) => n + 1);
}

// ── Phase D: worktree manager ───────────────────────────────────────────

/// Mirror of Rust `workbench::worktree::WorktreeInfo` — one Worktrees-table
/// row.
export interface WorktreeInfo {
  slug: string;
  path: string;
  branch: string;
  base: string;
  ahead: number;
  behind: number;
  has_live_tab: boolean;
}

/// Mirror of Rust `workbench::worktree::MergeReport`.
export interface MergeReport {
  fast_forward: boolean;
  commit: string;
}

/// Mirror of Rust `checks::Severity`.
export type CheckSeverity = 'error' | 'warning' | 'note';

/// Mirror of Rust `checks::DiagGroup`.
export interface DiagGroup {
  key: string;
  severity: CheckSeverity;
  message: string;
  count: number;
  sites: [string, number][];
}

/// Mirror of Rust `checks::CheckReport`.
export interface CheckReport {
  name: string;
  exit_code: number | null;
  duration_ms: number;
  timed_out: boolean;
  groups: DiagGroup[];
}

/// Mirror of Rust `workbench::WorktreeCheckStatus` — the merge-readiness
/// chip's cached result. `reports.length === 0` with no checks configured at
/// all should render as "no checks configured", not a green chip, even
/// though `pass` is vacuously `true` in that case.
export interface WorktreeCheckStatus {
  pass: boolean;
  checked_at_unix: number;
  reports: CheckReport[];
}

/// Every cImp-managed worktree of `root`'s repo (empty when `root` isn't a
/// git repo, or has none).
export function workbenchWorktrees(root?: string): Promise<WorktreeInfo[]> {
  return invoke<WorktreeInfo[]>('workbench_worktrees', { root: root ?? null });
}

/// The Diff row action: worktree `slug` vs. the base branch it was cut
/// from, parsed the same way `workbenchDiffFile` is. Read-only — there is
/// no revert action on this diff (it's a diff between two commits, not the
/// working tree).
export function workbenchWorktreeDiff(slug: string, root?: string): Promise<FileDiff[]> {
  return invoke<FileDiff[]>('workbench_worktree_diff', { root: root ?? null, slug });
}

/// Create a bare worktree (no tab) for `slug` — used by the Worktrees
/// section's own "create" affordance, distinct from the tab-bar's "New tab
/// in worktree…" flow (`createAiTabInWorktree` in `ipc.ts`), which creates
/// one AND spawns a tab into it in one step.
export function workbenchWorktreeCreate(slug: string, root?: string): Promise<string> {
  return invoke<string>('workbench_worktree_create', { root: root ?? null, slug });
}

/// Merge worktree `slug`'s branch back into the branch it was cut from. Runs
/// entirely in the main working tree; **never** leaves it half-merged — a
/// conflict aborts the merge and rejects with a plain-string error before
/// anything is left in a partial state (see the backend's
/// `workbench::worktree::merge` doc comment).
export function workbenchWorktreeMerge(slug: string, root?: string): Promise<MergeReport> {
  return invoke<MergeReport>('workbench_worktree_merge', { root: root ?? null, slug });
}

/// Remove worktree `slug`'s directory AND delete its branch. Double
/// confirmation is this dialog's job, not the backend's — call only after
/// the user has explicitly confirmed.
export function workbenchWorktreeDiscard(slug: string, root?: string): Promise<void> {
  return invoke('workbench_worktree_discard', { root: root ?? null, slug });
}

/// The merge-readiness chip's "Run checks" action: runs every configured
/// check with `cwd` = the worktree, caches the aggregate pass/fail
/// server-side, and returns it.
export function workbenchWorktreeRunChecks(slug: string, root?: string): Promise<WorktreeCheckStatus> {
  return invoke<WorktreeCheckStatus>('workbench_worktree_run_checks', { root: root ?? null, slug });
}

/// The merge-readiness chip's last cached result, if any — `null` means "not
/// checked yet this session" (render as such, not as a failure).
export function workbenchWorktreeCheckStatus(slug: string, root?: string): Promise<WorktreeCheckStatus | null> {
  return invoke<WorktreeCheckStatus | null>('workbench_worktree_check_status', { root: root ?? null, slug });
}

/// Bumped after a successful create/merge/discard so the Worktrees section
/// refetches without a direct reference to whichever dialog/action triggered
/// it — same pattern as `workbenchCheckpointsVersion`.
export const workbenchWorktreesVersion = writable<number>(0);
export function bumpWorkbenchWorktreesVersion(): void {
  workbenchWorktreesVersion.update((n) => n + 1);
}

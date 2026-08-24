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

/// Mirror of Rust `workbench::worddiff::WordDiffPart` — one span of a
/// word-diffed line. `del` spans only ever appear in a `pair`'s `left`, `add`
/// spans only in its `right`.
export interface WordDiffPart {
  text: string;
  kind: 'same' | 'add' | 'del';
}

/// Mirror of Rust `workbench::worddiff::HunkLineGroup` — the backend's
/// rendering decision for each of a hunk's lines.
///
/// **Lines are named by INDEX into `Hunk.lines`, not by text** (a full-file
/// diff would otherwise ship every line twice): render `lines[g.line][1]`.
/// Only `pair` carries text, and only the word-level spans of the two lines it
/// diffed. V42 Phase D moved this derivation out of `src/lib/diffWords.ts` —
/// nothing in the app computes it client-side any more.
export type HunkLineGroup =
  | { type: 'ctx'; line: number }
  | { type: 'del'; line: number }
  | { type: 'add'; line: number }
  | {
      type: 'pair';
      old_line: number;
      new_line: number;
      left: WordDiffPart[];
      right: WordDiffPart[];
    };

/// Mirror of Rust `workbench::diff::Hunk`. `lines` is `[marker, text]` pairs —
/// `marker` is `' '` (context), `'+'` (added), or `'-'` (removed); `text`
/// excludes the marker and any trailing newline. `hash` is opaque — the
/// frontend never computes or inspects it, only echoes it back verbatim to
/// `workbenchRevertHunk` as the staleness guard. `groups` is the precomputed
/// render plan over `lines` (see `HunkLineGroup`).
export interface Hunk {
  header: string;
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  lines: [string, string][];
  hash: string;
  groups: HunkLineGroup[];
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

/// Unified-context width for the per-file "Full file" view: larger than any
/// real file's line count, so the backend's `git diff --unified=<n>` returns
/// the whole file as one hunk (change highlighting intact). Clamped
/// backend-side (`diff::MAX_CONTEXT`).
export const FULL_FILE_CONTEXT = 999_999;

/// `context` is the unified-context width — omit for git's default (3), pass
/// `FULL_FILE_CONTEXT` for the whole-file view.
export function workbenchDiffFile(path: string, context?: number, root?: string): Promise<FileDiff> {
  return invoke<FileDiff>('workbench_diff_file', { root: root ?? null, path, context: context ?? null });
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
///
/// V33 (contract C8) adds `'tool'`: taken *immediately before* a
/// filesystem-mutating tool call, so restoring to it recovers the tree as it
/// was just before that one call. That is what separates it from `'prompt'`
/// (a whole turn ago) and `'burst'` (after the writes already landed).
export type CheckpointTrigger = 'prompt' | 'burst' | 'manual' | 'pre-restore' | 'tool';

/// Mirror of Rust `workbench::shadow::Checkpoint` — one Timeline row.
///
/// Hand-mirrored: there is no codegen, so a field added on the Rust side must
/// be added here or it is silently invisible to the UI.
export interface Checkpoint {
  id: string;
  seq: number;
  commit: string;
  /// ISO-8601 (from git's own commit date).
  ts: string;
  ts_unix: number;
  label: string;
  trigger: CheckpointTrigger;
  /// The harness NAME (a registry id) — shared by every tab of that
  /// kind, which is why `tab` below exists.
  agent: string | null;
  files_changed: number;
  /// V33: the harness conversation this checkpoint was taken for. `null` for a
  /// burst/manual/pre-restore checkpoint (no conversation behind it) and for
  /// every checkpoint written before this field existed.
  session: string | null;
  /// V33: the cImp tab this checkpoint was taken for — what makes two
  /// same-agent tabs on one project root distinguishable in the Timeline, and
  /// what a contamination row is joined against. `null` on the same terms as
  /// `session`.
  tab: string | null;
  /// V33 (contract C8): which tool call this checkpoint was taken in front of,
  /// as `harness:tool_name` — `<harness-id>:<its tool>`, `offload:run_command`.
  ///
  /// Optional AND nullable, deliberately, and they mean different things at the
  /// same point in a rollout: `undefined` = a backend that predates the field
  /// (the key is simply absent from the payload), `null` = a backend that has
  /// it but this checkpoint has no tool behind it — which is every
  /// prompt/burst/manual/pre-restore checkpoint, i.e. almost all of them.
  /// Neither is an error; every consumer must render "absent" as the normal
  /// case rather than as a gap.
  source?: string | null;
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
export function workbenchCheckpointDiff(id: string, context?: number, root?: string): Promise<FileDiff[]> {
  return invoke<FileDiff[]>('workbench_checkpoint_diff', { root: root ?? null, id, context: context ?? null });
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
export function workbenchWorktreeDiff(slug: string, context?: number, root?: string): Promise<FileDiff[]> {
  return invoke<FileDiff[]>('workbench_worktree_diff', { root: root ?? null, slug, context: context ?? null });
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
  return invoke<void>('workbench_worktree_discard', { root: root ?? null, slug });
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

// ── Session commits + git graph ─────────────────────────────────────────

/// Mirror of Rust `workbench::history::CommitInfo` — one commit, shared by
/// the Session-commits list and the Git-graph node.
export interface CommitInfo {
  hash: string;
  short: string;
  /// Parent hashes, first parent first.
  parents: string[];
  /// Committer timestamp, epoch ms.
  ts_ms: number;
  author: string;
  /// `%D` decorations, e.g. "HEAD -> develop", "tag: v0.40.5", "origin/develop".
  refs: string[];
  subject: string;
  body: string;
  /// True when this commit was caught live from the session's transcript
  /// (exact provenance) rather than merely falling inside the session's
  /// time window. Always false in the git-graph payload.
  tracked: boolean;
}

/// Mirror of Rust `workbench::history::GitGraph`.
export interface GitGraph {
  /// Current branch name; null when detached or on an unborn branch.
  head: string | null;
  /// Topological order — children strictly before parents.
  commits: CommitInfo[];
  truncated: boolean;
}

/// Mirror of Rust `workbench::history::SessionCommits` — the commit list
/// plus whether the backend's log walk hit its cap before reaching the
/// window's start (older commits may be missing).
export interface SessionCommits {
  commits: CommitInfo[];
  truncated: boolean;
}

/// One session's commits: the union of commits caught live from its
/// transcript (`tracked: true`) and commits (from every ref) whose committer
/// time falls inside the session's window (the backend widens
/// `fromMs..=toMs` with its own fresher session bounds). Newest first.
export function workbenchSessionCommits(
  sessionId: string,
  fromMs: number,
  toMs: number,
  root?: string,
): Promise<SessionCommits> {
  return invoke<SessionCommits>('workbench_session_commits', { root: root ?? null, sessionId, fromMs, toMs });
}

/// Per-session commit counts (session_id → count) for the Sessions card's
/// per-row "commits" button — zero disables it. One backend log walk serves
/// every window.
export function workbenchSessionCommitCounts(
  windows: { session_id: string; from_ms: number; to_ms: number }[],
  root?: string,
): Promise<Record<string, number>> {
  return invoke<Record<string, number>>('workbench_session_commit_counts', { root: root ?? null, windows });
}

/// One commit vs. its first parent, in the same `FileDiff` shape the other
/// diff surfaces render. Read-only.
export function workbenchCommitDiff(hash: string, context?: number, root?: string): Promise<FileDiff[]> {
  return invoke<FileDiff[]>('workbench_commit_diff', { root: root ?? null, hash, context: context ?? null });
}

/// The Git-graph section's commit list (topological order) + HEAD branch.
export function workbenchGitGraph(limit?: number, root?: string): Promise<GitGraph> {
  return invoke<GitGraph>('workbench_git_graph', { root: root ?? null, limit: limit ?? null });
}

/// The Sessions card's "commits" button writes the clicked session here and
/// reveals the Workbench tab; `WorkbenchView` switches to the Session-commits
/// section on a nonce change and `SessionCommitsView` renders the request.
/// Deliberately NOT cleared after consumption (unlike `graphReveal`) — the
/// section keeps showing the last-picked session while the user browses
/// other sections.
export interface SessionCommitsRequest {
  sessionId: string;
  agent: string;
  fromMs: number;
  toMs: number;
  nonce: number;
}

let sessionCommitsNonce = 0;

export const sessionCommitsRequest = writable<SessionCommitsRequest | null>(null);

export function openSessionCommits(sessionId: string, agent: string, fromMs: number, toMs: number): void {
  sessionCommitsNonce += 1;
  sessionCommitsRequest.set({ sessionId, agent, fromMs, toMs, nonce: sessionCommitsNonce });
}

/// One-shot routing latch for WorkbenchView's "jump to the Session-commits
/// section" effect. MODULE scope, not component state: WorkbenchView is
/// destroyed and recreated on every tab switch, so a component-local latch
/// would reset and replay the (never-cleared) store's last request, yanking
/// the user back to Session commits after they had navigated away.
let routedSessionCommitsNonce = 0;

export function takeSessionCommitsRoute(nonce: number): boolean {
  if (nonce === routedSessionCommitsNonce) return false;
  routedSessionCommitsNonce = nonce;
  return true;
}

// ── Events-tab → Timeline checkpoint jump (#51) ─────────────────────────────
// Same store-bus + one-shot-latch idiom as the Session-commits jump above,
// with one difference: TWO consumers read one request — WorkbenchView switches
// to the Timeline section, TimelineView highlights (and scrolls to) the
// checkpoint — so each consumes its OWN latch. Sharing one latch would make
// whichever component's effect ran first swallow the other's half of the jump.

export interface TimelineCheckpointRequest {
  /// The checkpoint to land on (the `cp-N` tag id).
  id: string;
  nonce: number;
}

let timelineCheckpointNonce = 0;

export const timelineCheckpointRequest = writable<TimelineCheckpointRequest | null>(null);

export function openTimelineCheckpoint(id: string): void {
  timelineCheckpointNonce += 1;
  timelineCheckpointRequest.set({ id, nonce: timelineCheckpointNonce });
}

let routedTimelineSectionNonce = 0;

/// WorkbenchView's half: switch to the Timeline section, once per request.
export function takeTimelineSectionRoute(nonce: number): boolean {
  if (nonce === routedTimelineSectionNonce) return false;
  routedTimelineSectionNonce = nonce;
  return true;
}

let routedTimelineHighlightNonce = 0;

/// TimelineView's half: highlight the checkpoint, once per request. Module
/// scope for the same reason as `takeSessionCommitsRoute` — the component is
/// destroyed/recreated, and a replay would re-highlight a long-done jump.
export function takeTimelineHighlightRoute(nonce: number): boolean {
  if (nonce === routedTimelineHighlightNonce) return false;
  routedTimelineHighlightNonce = nonce;
  return true;
}

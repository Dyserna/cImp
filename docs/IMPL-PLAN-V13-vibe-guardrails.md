# IMPL-PLAN V13 — Vibe-Coding Guardrails

Companion to `docs/MILESTONE-V13-vibe-guardrails.md`. File-by-file build plan.
Open decisions **assumed at proposed defaults** (one sectioned Workbench tab;
checkpoints default off in V1; spawned `git`, no libgit2; the prompt-tap fires
whenever *any* consumer needs it, with the gating spelled out in C3) —
sections marked ⚠ change if a decision flips.

Phases: **A** (Workbench tab shell) → **B** (live diff pane) →
**C** (checkpoints) → **D** (worktrees) → **E** (docs/tests/release).

Grounding anchors (verified against current `develop`, post-V10):
- Reserved-tab pattern: `lib/tabs/types.ts` — reserved ids + helpers
  (offload tab `:14`, graph-monitor `:24` + `isGraphMonitorTab:30`, Note tab
  `:34`), `TabKind = 'ai-tool' | 'shell'` (`:74`); backend registration in
  `state/manager.rs` / `tabs/registry.rs`; render branch in `lib/Pane.svelte`
  (the `CodeIntelligenceView` branch is the template); settings migration
  precedent = the migration that introduced the graph-monitor tab.
- Watcher: `graph/watcher.rs::start` (`:28`) — notify → debounce thread →
  coalesced batches handed to a callback owned by `graph/service.rs`.
- Prompt taps: Claude `UserPromptSubmit` shim `context_hook.rs` →
  `offload/loopback.rs` `/context/retrieve` (`:346`,
  `handle_context_retrieve:591`); OpenCode plugin `chat.message` +
  `tool.execute.after` writer at `tabs/config.rs:395-460`; hook install
  conditions in `tabs/config.rs:199-218`.
- Compose: `lib/ComposeOverlay.svelte` (submit targets the focused pane's
  active tab); status bar `lib/StatusBar.svelte`.
- Subprocess: the console-suppression helper used for all spawned subprocesses.
- Settings: `settings/schema.rs` (+ timestamped-backup migration system).

---

## 0. Cross-cutting: the `workbench` module + spawned-git harness

**0.1** New backend module `src-tauri/src/workbench/` with `mod.rs`
(service struct, managed state), `git.rs` (spawned-git harness), `diff.rs`
(unified-diff parser + types), `shadow.rs` (Phase C), `worktree.rs` (Phase D).

**0.2 Git harness** (`workbench/git.rs`):
```rust
pub struct GitCtx { pub root: PathBuf, pub git_dir: Option<PathBuf>,
                    pub work_tree: Option<PathBuf>, pub index_file: Option<PathBuf> }
pub async fn run(ctx: &GitCtx, args: &[&str]) -> AppResult<GitOutput>  // {stdout, stderr, code}
```
- Always sets `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` env **explicitly**
  (empty ⇒ removed from env) so shadow-repo commands can never touch the
  user's repo, and vice versa; console-suppressed; per-call timeout (30 s
  default); `git` missing ⇒ one typed `AppError::GitUnavailable` that every
  UI surface renders as guidance.
- Probe helper `is_repo(root)` (`rev-parse --is-inside-work-tree`), cached
  per root with invalidation on demand.

**0.3 FS-batch fan-out:** the graph watcher's coalesced batches currently feed
only the re-index. In `graph/service.rs`, at the point the debounce thread
hands over a batch, additionally emit a Tauri event
`fs-batch { root, paths: Vec<String> }` (bounded: cap the paths list at ~200
+ a `truncated` flag). Workbench (diff refresh, checkpoint burst trigger)
subscribes on the backend via an internal broadcast channel
(`tokio::sync::broadcast` owned by managed state) — the Tauri event is for
the frontend, the broadcast for backend consumers. **Emitted only when the
Workbench feature is enabled** to avoid new idle chatter. When the graph is
disabled entirely, Workbench falls back to a 5 s `git status` poll while the
tab is visible (B4).

**0.4 Settings** (`settings/schema.rs`): new group
```rust
pub struct WorkbenchSettings {
  pub enabled: bool,                 // master: tab exists (default true — the tab is cheap; features inside gate themselves)
  pub checkpoints: bool,             // default false (V1)
  pub checkpoint_max: u32,           // 100
  pub checkpoint_max_age_days: u32,  // 7
  pub checkpoint_burst_files: u32,   // 5
  pub checkpoint_burst_window_s: u32,// 60
  pub checkpoint_min_gap_s: u32,     // 120
}
```
Settings migration (timestamped backup per convention) adds the group +
inserts the reserved tab (A1).

---

## Phase A — Workbench tab shell

**A1. Reserved tab:** `lib/tabs/types.ts` — `WORKBENCH_TAB_ID = "workbench-1"`
+ `isWorkbenchTab(id)` (mirror `:24-30`); shell-kind on the backend like the
Note tab (`:34` precedent). `state/manager.rs`/`tabs/registry.rs`: register
the reserved tab (label **"Workbench"**), gated on `workbench.enabled`;
settings migration inserts it for existing users (graph-monitor migration as
the template). No PTY behind it (app-rendered).

**A2. View:** `lib/WorkbenchView.svelte` — section router
`let section = $state<'diff'|'timeline'|'worktrees'>('diff')`, same segmented
control as `CodeIntelligenceView`. `lib/Pane.svelte`: import + render branch
guarded by `isWorkbenchTab` (copy the CodeIntelligenceView branch). Sections
start as placeholders (filled by B/C/D). A top-of-view banner renders the
`GitUnavailable`/not-a-repo states per section (diff needs git *or*
checkpoints; timeline needs checkpoints on; worktrees needs git).

**A3. Settings UI:** `SettingsApp.svelte` new "Workbench" section — the
toggles/sliders from §0.4 (checkpoint fields disabled until `checkpoints` on).

Tests: none (UI shell). Manual: tab appears/disappears with `workbench.enabled`.

---

## Phase B — Live diff pane

**B1. Diff engine** (`workbench/diff.rs`):
```rust
pub struct FileDiff { pub path: String, pub status: FileStatus,  // Modified|Added|Deleted|Renamed{from}|Untracked
                      pub binary: bool, pub hunks: Vec<Hunk>, pub too_large: bool }
pub struct Hunk { pub header: String, pub old_start: u32, pub old_lines: u32,
                  pub new_start: u32, pub new_lines: u32, pub lines: Vec<(char, String)> }
pub fn parse_unified(diff_text: &str) -> Vec<FileDiff>;
```
- `summary(root)`: `git status --porcelain=v1 -z` → statuses;
  `diff_file(root, path)`: `git diff --no-color --unified=3 -- <path>`
  (staged+unstaged vs HEAD: use `git diff HEAD -- <path>`); untracked ⇒
  synthesize an all-added diff from the file (cap: files > 1 MiB ⇒
  `too_large`). Binary detected from git's `Binary files … differ` marker.
- Non-git root + checkpoints enabled ⇒ diff vs the latest shadow snapshot
  (Phase C exposes `shadow_diff_file`); neither ⇒ section shows the
  requirements banner.
- Special-state guard: `git rev-parse -q --verify MERGE_HEAD` /
  `REBASE_HEAD` ⇒ read-only mode flag in the summary payload.

**B2. Revert hunk** (`workbench/mod.rs`): reconstruct a minimal patch
(headers + the one hunk) → if checkpoints enabled, snapshot first
("pre-revert") → `git apply --reverse --unidiff-zero -` piped via stdin →
re-diff the file. Refuse in read-only (merge/rebase) mode.

**B3. IPC** (`ipc/commands.rs`): `workbench_diff_summary(root?) ->
{ files: Vec<FileDiffMeta>, readonly: bool, source: "git"|"shadow"|null }`,
`workbench_diff_file(path)`, `workbench_revert_hunk(path, hunk_index,
hunk_hash)` (hash guards against reverting a stale hunk after an agent edit
raced the UI), `workbench_send_hunk(path, hunk_index) -> String` (formatted
block; frontend routes it into compose).

**B4. Frontend** (`WorkbenchView` Diff section + new `lib/DiffView.svelte`):
- File list (status chips, per-file collapse, ±line counts); virtualized:
  only expanded files fetch `workbench_diff_file`; hunks render lazily.
- Unified/side-by-side toggle; intra-line word-diff computed per hunk-line
  pair with a small LCS helper in TS (no new dependency); syntax color reuses
  the app's existing highlight approach if one exists — else plain +/-
  coloring in V1 (do not pull a highlighter dependency for this).
- Refresh: subscribe to the `fs-batch` Tauri event → refetch summary +
  invalidate touched files (debounced 500 ms on the frontend too); fallback
  5 s poll while the tab is visible and no watcher (graph off).
- Hunk actions: Revert (confirm dialog when > 20 lines) · Copy · **Send to
  agent** → `ComposeOverlay` store gains
  `openComposeWith(text: string)` (exported action: opens the overlay,
  appends to the draft) — submit path unchanged (focused tab).

**B5. Status-bar badge** (`StatusBar.svelte`): `±N` chip fed by a
`workbenchDiff` store updated from the same summary event; click →
focus/open the Workbench tab (existing tab-focus action). Hidden when 0
changes or feature off.

**B6. Tests:** `parse_unified` fixtures (multi-hunk, rename, binary, no
newline at EOF); untracked synthesis; hunk-hash staleness rejection;
reverse-apply round-trip on a temp repo (integration test creates a repo with
`git init` in a tempdir — skip test when git is absent).

---

## Phase C — Checkpoints (shadow repo)

**C1. Shadow repo** (`workbench/shadow.rs`), all via the §0.2 harness with
`git_dir = <root>/.cimp/shadow.git`, `work_tree = root`,
`index_file = <root>/.cimp/shadow.git/index`:
```rust
pub async fn ensure(root) -> AppResult<()>;            // init once + config
pub async fn snapshot(root, label: &str, trigger: Trigger) -> AppResult<CheckpointId>;
pub async fn list(root) -> AppResult<Vec<Checkpoint>>; // id, ts, label, trigger, files_changed
pub async fn diff_vs_now(root, id) -> AppResult<String>;      // unified text → diff.rs parser
pub async fn restore(root, id, delete_new: bool) -> AppResult<RestoreReport>;
pub async fn gc(root, max, max_age_days) -> AppResult<()>;
```
- `ensure`: `git init` with the explicit git-dir; config `core.autocrlf=false`,
  `core.fileMode=false`, `user.name=cimp`, `user.email=cimp@local`,
  `gc.auto=0` (we gc explicitly); seed `shadow.git/info/exclude` with
  `.cimp/` + the graph's extra ignore globs. `git add -A` honors the
  project's own `.gitignore` automatically (same work-tree).
- `snapshot`: `add -A` → `commit --allow-empty-message -m <label>` on an
  unborn/main shadow branch; skip (return previous id) when
  `status --porcelain` (shadow) is empty. Oversized files excluded via a
  maintained `info/exclude` line list driven by `graph.max_file_bytes`
  (checked during the burst trigger's path scan, appended before `add`).
- `restore`: snapshot current state first (`pre-restore` trigger) →
  `checkout <id> -- .` → compute files present now but absent in `<id>`
  (`diff --name-only --diff-filter=A <id> HEAD-shadow`) → delete them only
  when `delete_new` (default false). Returns the full changed/deleted list
  for the UI report. Never touches `<root>/.git`.
- `gc`: drop refs beyond `checkpoint_max`/age (rewrite the branch with
  `git commit-tree` chain or simpler: keep checkpoints as **tags on orphan
  commits** — each snapshot is `commit-tree` of the current index with no
  parent, tagged `cp-<seq>`; deletion = tag delete + `git gc --prune=now`
  occasionally. ⚠ decide at implementation; orphan-commit-per-checkpoint is
  simpler to reason about and diff still works via commit ids).

**C2. First-snapshot cost:** run `ensure` + first `snapshot` on a background
task with progress surfaced in the Timeline section ("building first
checkpoint — N files"); never block a prompt or the UI thread. Log
`.cimp/shadow.git` size after each gc (feeds the default-on decision later).

**C3. Triggers** (`workbench/mod.rs` service):
- **Prompt tap:** `handle_context_retrieve` (`offload/loopback.rs:591`) and
  the OpenCode `chat.message` plugin already receive every prompt when the
  respective hook is installed. Add a call-out to
  `workbench::on_prompt(root, prompt_head)` from the retrieve handler
  (before the injection gate — checkpointing must fire even when injection
  yields nothing). Hook install condition (`tabs/config.rs:199`) widens to
  `context_injection || workbench.checkpoints` — and the retrieve handler's
  *injection* gate stays on `context_injection` alone. ⚠ This implements
  milestone Decision 4's proposal; if a separate `hooks_enabled` umbrella is
  chosen instead, only the install condition changes.
- **Burst:** subscribe the §0.3 broadcast: ≥ `checkpoint_burst_files` distinct
  paths within `checkpoint_burst_window_s` **and** ≥ `checkpoint_min_gap_s`
  since the last snapshot ⇒ `snapshot("activity")`.
- **Manual:** IPC `workbench_checkpoint_now(label?)` + a rebindable shortcut
  through the existing shortcut dispatcher.
- Debounce all triggers behind a single `Mutex<Instant>` (min-gap applies to
  prompt triggers too — rapid-fire prompts don't spam snapshots).

**C4. IPC + UI (Timeline section):** `workbench_checkpoints(root?)`,
`workbench_checkpoint_diff(id)` (renders in the B4 `DiffView`),
`workbench_restore { id, delete_new }` behind a confirmation dialog that
lists the files (from a dry-run diff) + a "delete files created since"
checkbox (default unchecked); `workbench_checkpoint_now`. Rows: time ·
trigger icon · label (prompt head) · files-changed count.

**C5. Checkpoint health (soft-dep V12-A):** checkpoint metadata (the same
store as label/trigger — the `cp-<seq>` tag message or the meta sidecar,
per the C1 retention decision) gains an optional
`check_summary: {check_name: error_count}` captured from the most recent
`CheckReport` per configured check — the checks module emits a small
broadcast after every run (auto or tool-invoked) that the workbench service
subscribes to. Timeline renders an errors column + a CSS-bar trend; a
regression banner appears when current errors materially exceed a checkpoint
within the retention window (V1 rule: ≥ 2× and ≥ +5), offering "Diff vs
cp-N". No checks configured ⇒ column absent. Tests: summary attach on
snapshot; banner threshold; absence path.

**C6. Tests** (tempdir integration, skip without git): snapshot/restore
round-trip byte-faithful (CRLF file included); restore keeps new files by
default and deletes with the flag; pre-restore snapshot exists; empty
work-tree change ⇒ snapshot dedupes; gc respects pinned retention counts;
user `.git` untouched throughout (assert `git -C root status` unchanged
where a user repo exists).

---

## Phase D — Worktrees

**D1. Backend** (`workbench/worktree.rs`):
```rust
pub async fn create(root, slug) -> AppResult<PathBuf>;  // .cimp/worktrees/<slug>, branch cimp/<slug> from HEAD
pub async fn list(root) -> AppResult<Vec<WorktreeInfo>>; // path, branch, base, ahead, behind, has_live_tab
pub async fn merge(root, slug) -> AppResult<MergeReport>; // ff or merge commit; conflict ⇒ typed error, no state change
pub async fn discard(root, slug) -> AppResult<()>;      // worktree remove --force + branch -D
pub async fn prune(root) -> AppResult<()>;              // on app start
```
- `create`: refuse when root is itself a linked worktree or a submodule
  (detect via `rev-parse --git-common-dir` vs `--git-dir`); record the base
  branch (`rev-parse --abbrev-ref HEAD`) in
  `.cimp/worktrees/<slug>.meta.json`.
- `merge`: run in the **main** work-tree; require it clean
  (`status --porcelain` empty) and on the recorded base branch — else a
  typed error the UI renders with instructions. `git merge --no-edit
  cimp/<slug>`; conflict ⇒ `git merge --abort` + error (V1 never leaves a
  half-merged tree).
- Ensure `.cimp/` is already ignored (it is, by convention) so worktrees
  don't dirty status; add `worktrees/` to the graph's default ignore globs so
  the main project's index doesn't ingest them (each worktree, opened as its
  own cwd, indexes itself — accepted V1 redundancy per the milestone).

**D2. Tab spawn with cwd:** AI-tool tabs today spawn with the app cwd. Add
`pub cwd_override: Option<String>` to the ai-tool tab config (settings schema
+ per-tab Configure dialog read-only display), threaded through the PTY spawn
site (`pty/manager.rs`) exactly as shell tabs' cwd already is. The
tab-creation flow from D3 sets it; users don't hand-edit it. This is the one
tab-schema field this milestone adds (rides the §0.4 migration).

**D3. UI (Worktrees section + tab-bar entry points):**
- Tab-bar `+` menu / AI-tab context menu: "New <Claude|OpenCode> tab in
  worktree…" → name prompt → `create` → duplicate the source tab's config
  with `cwd_override` set → spawn; tab title prefixed `⑂ <slug>`.
- Worktrees section table: slug · branch · ahead/behind · live-tab indicator ·
  actions **Diff** (B4 viewer against the base branch via
  `git diff <base>...cimp/<slug>`) · **Merge** · **Discard**
  (double-confirm) · **Open shell here** (existing shell-tab creation with
  cwd).
- **Merge-readiness chip (soft-dep V12-A):** a per-row "Run checks" action
  executes the configured checks with cwd = the worktree (reuses
  `checks::run`; `changed_only` vs the recorded base branch); the chip caches
  pass/fail + age. When V12 `auto_check` is on, fs-batch paths under
  `.cimp/worktrees/<slug>` refresh it automatically. The Merge button gains a
  green highlight on pass — advisory only, never auto-merge.
- Closing the tab leaves the worktree; the section lists orphans for cleanup;
  `prune` runs at app start.

**D4. Tests** (tempdir, skip without git): create/list/ahead-behind; merge
ff and merge-commit paths; conflict aborts cleanly (tree state restored);
discard removes branch + dir; nested-worktree refusal; meta.json base
recording.

---

## Phase E — Docs, settings polish, tests, release

- README / `docs/FEATURES.md`: Workbench tab (diff / timeline / worktrees),
  checkpoint trust model (shadow repo, never your `.git`), the
  delete-new-files restore semantics, worktree flow.
  `docs/MAINTENANCE.md`: spawned-git dependency note (git required for
  diff/worktrees; checkpoints work without a user repo), shadow-repo size
  logging, the `fs-batch` event.
- Settings UI polish; shortcut registration (`checkpoint now`); CHANGELOG;
  version bump; full `cargo test` + `npm run check`; release per the
  standard workflow. Revisit the checkpoints-default-off decision with the
  C2 size/time logs in hand.

---

## Appendix — consolidated change surface

**New reserved tab:** `workbench-1` (+ settings migration inserting it).

**New settings group:** `WorkbenchSettings` (7 fields, §0.4) + ai-tab
`cwd_override`.

**New IPC:** `workbench_diff_summary`, `workbench_diff_file`,
`workbench_revert_hunk`, `workbench_send_hunk`, `workbench_checkpoints`,
`workbench_checkpoint_diff`, `workbench_checkpoint_now`, `workbench_restore`,
`workbench_worktrees` (list), `workbench_worktree_create/merge/discard`.

**New Rust files:** `workbench/{mod,git,diff,shadow,worktree}.rs`.

**New frontend files:** `lib/WorkbenchView.svelte`, `lib/DiffView.svelte`;
touches: `Pane.svelte`, `tabs/types.ts`, `StatusBar.svelte`,
`ComposeOverlay.svelte` (`openComposeWith`), `SettingsApp.svelte`.

**Backend touches:** `graph/service.rs` (fs-batch broadcast),
`offload/loopback.rs` (`on_prompt` call-out), `tabs/config.rs` (hook install
condition), `pty/manager.rs` (cwd override), `state/manager.rs` /
`tabs/registry.rs` (reserved tab).

**Soft dependencies on V12 Phase A:** checkpoint health (C5) and the
merge-readiness chip (D3) consume `checks::run` + its result broadcast when
present; both ship dark without it.

**No new MCP tools, no graph-schema change, no new C dependencies** —
everything is spawned `git` + existing plumbing.

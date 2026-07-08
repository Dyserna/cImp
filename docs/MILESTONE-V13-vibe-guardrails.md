# V13 — Vibe-Coding Guardrails (checkpoints · live diff · worktrees)

**Status:** SPEC (written 2026-07-08). Not yet coded.
**Builds on:** the reserved app-rendered tab pattern (V9-01
`GraphMonitorView.svelte` / `TabId::GraphMonitor`), the graph FS watcher
(`graph::watcher`), multi-pane layouts (v1.3), AI-tab lifecycle
(`tabs/config.rs` spawn plumbing).

## Why

The Context Engine makes agents smarter; nothing in cImp yet makes letting them
loose *safe*. Three trust features, one theme — **"let it rip, I can always get
back"**:

1. **Checkpoints** — Claude Code has its own rewind, but it's per-tool and
   per-session: OpenCode tabs, multi-tab sessions, and shell-tab side effects
   share nothing. cImp sits above all tabs and already watches the FS — it's
   the right layer for one timeline over the whole working tree.
2. **Live diff pane** — watching the working-tree diff evolve *while the agent
   narrates over TTS* is cImp's natural review loop, and today it requires a
   shell tab and manual `git diff` spam.
3. **Worktree manager** — multi-pane layouts invite running 2–3 agents at once;
   on one working tree they trample each other. Isolated worktrees make
   parallel agents safe.

Everything is git-native and local. cImp never touches the user's index, stash,
or refs except where explicitly stated.

---

## Feature 1 — Cross-agent checkpoints ("undo since prompt N")

### Goal
Automatic snapshots of the working tree at meaningful moments, with a timeline
UI to diff and restore — regardless of *which* agent (or shell command) made
the mess.

### Snapshot mechanism (shadow git, not the user's)
- A separate object store: `git --git-dir=.cimp/shadow.git --work-tree=<project>`
  with its own index. `.cimp/shadow.git` is inside the already-git-ignored
  `.cimp/` dir. The user's `.git` is **never written** — no stash entries, no
  refs, no reflog noise.
- A snapshot = `add -A` (honoring the project's `.gitignore` +
  `graph.ignore` globs) + `commit` in the shadow repo. Incremental object
  reuse makes steady-state snapshots cheap (only changed blobs). `.cimp/`
  itself and `max_file_bytes`-oversized blobs are excluded.
- Retention: ring of `checkpoint_max` snapshots (default 100) +
  age cap (default 7 days); `git gc` the shadow repo opportunistically.

### Triggers
- **Per user prompt** (primary): the V10 injection shim already POSTs every
  Claude prompt to the loopback, and the OpenCode plugin's `chat.message` hook
  does the same — tap those to fire "prompt checkpoint: <first 60 chars>". When
  injection is disabled the hook still runs (it's a no-op for context but still
  reports the prompt event) — this decouples checkpointing from the injection
  *toggle* while reusing its transport.
- **Debounced burst trigger** (fallback, covers shell tabs + non-hooked flows):
  the graph watcher already sees every file event; N files changed within a
  debounce window and no snapshot in the last M minutes ⇒ "activity checkpoint".
- Manual: a "Checkpoint now" button + a rebindable shortcut.

### Restore
- **Restore all** — hard-reset the working tree to snapshot X (via the shadow
  repo's checkout). Confirmation dialog shows the full file list first; the
  current state is itself snapshotted ("pre-restore") so restore is always
  undoable. Restoring **never** touches `.git` — after a restore, the user's
  own `git status` simply reflects the restored tree.
- **Restore single file / hunk** — from the diff view (Feature 2 integration).
- Files deleted since the snapshot are re-created; files created since are
  listed and deleted only with the confirmation's explicit opt-in checkbox
  (untracked-new-work is the dangerous case — default is to keep them).

### UI — new **Timeline** section (in a new reserved tab, see Feature 2)
- Vertical list: timestamp · trigger (prompt text / activity / manual) · files
  changed vs previous · agent tab that was focused.
- Row actions: **Diff vs now** (opens Feature 2's viewer) · **Restore**.

### Settings
`checkpoints: bool` (default off in V1 — flip to on once the shadow-repo cost
is validated on a big repo), `checkpoint_max`, `checkpoint_max_age_days`,
`checkpoint_burst_files` / `_window_s`.

### Edge cases
- Non-git projects: shadow repo works anyway (it's self-contained) — this makes
  checkpoints valuable even *before* the user runs `git init`.
- Huge repos: first snapshot is the expensive one; run it on a background
  thread with progress in the tab, never blocking a prompt.
- Line-ending / filemode churn on Windows: shadow repo pins `core.autocrlf=false`,
  `core.fileMode=false` so restores are byte-faithful.

---

## Feature 2 — Live diff pane

### Goal
A reserved, app-rendered tab showing the working-tree diff **live** as agents
edit — per-hunk revert, and "send hunk to agent" to close the review loop.

### Design
- New reserved tab `TabId::Workbench` (one new app-rendered tab hosts **both**
  V13 UI surfaces as sections — *Diff* and *Timeline* — same left-rail pattern
  as Code Intelligence; avoids two new tab types and a second schema bump).
- **Diff source:** for git projects, `git diff` + `git status` against HEAD
  (spawned `git`, parsed unified diff — no libgit2 dependency for V1); for
  non-git projects, diff against the latest checkpoint (Feature 1) when
  enabled, else the section explains what it needs.
- **Live:** re-diff on graph-watcher events (debounced ~500 ms) — the watcher
  already exists and already debounces; the diff pane is just another consumer.
  Only re-diff changed paths; cache per-file patches.
- **Rendering:** side-by-side or unified toggle, syntax highlighting reusing
  the existing frontend highlighter, per-file collapse, intra-line word diff.
  Large diffs virtualized (only visible hunks rendered).
- **Actions per hunk:** **Revert hunk** (apply the reverse patch via
  `git apply -R` on the working tree — with the same pre-action checkpoint
  guarantee as Feature 1 restores) · **Copy** · **Send to agent** (formats the
  hunk as a fenced block + file:line header into the compose overlay, targeted
  at the focused AI tab — reuses the compose submit path).
- A status-bar badge shows `±N files` changed, click → opens/focuses the
  Workbench tab (same pattern as the usage meter).

### Edge cases
- Binary / oversized files: listed, not rendered.
- Mid-merge / rebase states: show git's own state banner and go read-only
  (no hunk reverts while the index is in a special state).

---

## Feature 3 — Worktree manager (parallel agents, isolated)

### Goal
"New task in isolated worktree" as a first-class action: create a worktree +
branch, spawn an AI tab in it, and merge back (or discard) from the UI.

### Design
- **Create:** tab-bar `+` menu and tab context menu gain "New Claude/OpenCode
  tab in worktree…" → prompts for a task name → runs
  `git worktree add .cimp/worktrees/<slug> -b cimp/<slug>` (location inside
  `.cimp/` keeps it out of the user's way; the branch namespace `cimp/` keeps
  refs tidy) → spawns the AI tab with `cwd` = the worktree. The tab title gets
  a `⑂ slug` marker.
- **Per-worktree services:** the graph/memory layer is per-project keyed by
  cwd today — a worktree *is* a different cwd, so it gets its own `.cimp/`
  graph/memory by default. V1 accepts that (correct, if redundant); a
  shared-read graph is a follow-on optimization. Checkpoints (Feature 1)
  attach to the main tree only in V1.
- **Merge back:** a per-worktree row in the Workbench → *Worktrees* subsection:
  ahead/behind counts, diff vs the base branch (reuses Feature 2's viewer),
  and three actions — **Merge** (fast-forward or merge commit into the branch
  the worktree was cut from, refuses on conflicts with a clear message — V1
  does *not* attempt conflict resolution UI; the user gets a shell one click
  away), **Discard** (remove worktree + delete branch, double-confirm), and
  **Open shell here**.
- **Lifecycle:** closing the tab does **not** remove the worktree (work
  survives); the Worktrees list shows orphaned ones (no live tab) for cleanup.
  `git worktree prune` on app start.

### Edge cases
- Repos that are themselves worktrees / submodules: detect and disable with an
  explanatory tooltip (V1 scope cut).
- Uncommitted changes in the main tree don't block worktree creation (git
  handles it) — but the create dialog states the new worktree starts from HEAD,
  not from uncommitted work.

---

## Phasing

| Phase | Scope | Notes |
|---|---|---|
| **A. Workbench tab shell** | Reserved tab + section nav + settings/schema plumbing (schema bump: one new reserved tab id) | Same pattern as V10 Phase A |
| **B. Diff pane** | git diff parsing + live watcher wiring + viewer + revert/copy/send actions + status-bar badge | The daily-use win; ships first |
| **C. Checkpoints** | Shadow repo + triggers (prompt-tap, burst, manual) + Timeline section + restore flows | Depends on A; prompt-tap reuses V10 shims |
| **D. Worktrees** | Create flow + tab spawn cwd + Worktrees section (merge/discard) + prune | Depends on A; B's viewer reused for its diffs |
| **E. Docs/tests** | README/FEATURES/MAINTENANCE, settings UI, unit+integration | Per repo convention |

Suggested order **A → B → C → D → E**. B alone (live diff) is a complete,
shippable release if C/D slip.

## Decisions — OPEN

1. **One Workbench tab vs. separate Diff/Timeline tabs** — proposed: one tab,
   sectioned (mirrors Code Intelligence; fewer reserved ids).
2. **Checkpoint default** — proposed off in V1, on-by-default once shadow-repo
   cost is validated on a large real repo (define: first-snapshot time and
   `.cimp/shadow.git` size logged and reviewed).
3. **libgit2 (`git2` crate) vs. spawned `git`** — proposed spawned `git` for V1
   (zero new C dependency, git is a hard prerequisite for the features anyway);
   revisit if parse surface grows. C-FFI is allowed when it earns its keep —
   V1 doesn't need it yet.
4. **Prompt-tap when injection is off** — proposed: the hook always fires and
   the loopback route treats "injection disabled" as context-no-op but still
   records the prompt event for checkpoints/memory. Confirm this doesn't
   surprise users who read "injection: off" as "no hooks installed" — may need
   a separate `hooks_enabled` umbrella toggle in Settings.

## Cost note

The diff viewer frontend and git plumbing are mechanical (Sonnet/Haiku
fan-out). Reserve Opus for the checkpoint restore-safety review — restore paths
are the one place in this milestone where a bug destroys user work — per the
standing agent-cost guidance.

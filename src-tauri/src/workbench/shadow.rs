//! Phase C — the shadow checkpoint repo (`ensure`/`snapshot`/`list`/
//! `diff_vs_now`/`restore`/`gc`) at `<root>/.cimp/shadow.git`, built on the
//! §0.2 git harness via [`GitCtx::shadow`](super::git::GitCtx::shadow).
//!
//! **Safety contract (read before touching this file):**
//!   - **Invariant A** — every git invocation in this module goes through
//!     [`shadow_ctx`], the ONE place that builds a shadow `GitCtx`
//!     (`git_dir`/`index_file` pinned under `<root>/.cimp/shadow.git`,
//!     `work_tree = root`). No function here ever constructs `GitCtx`
//!     another way — grep `shadow_ctx(` to audit that claim.
//!   - **Invariant B** — `<root>/.git` (the user's own repo, if any) is never
//!     written. Every git call below sets `GIT_DIR`/`GIT_WORK_TREE`/
//!     `GIT_INDEX_FILE` explicitly via [`shadow_ctx`] (see `git.rs`'s
//!     `env_overrides`), so a shadow command can't inherit or wander into the
//!     user's repo; on top of that, [`seed_exclude`] excludes `.git/` from
//!     every shadow snapshot itself (defense in depth on top of git's own
//!     "never traverse a nested `.git`" scan behavior — belt AND suspenders,
//!     since this is the one place a bug destroys user data). A checkpoint's
//!     tree therefore never CONTAINS a `.git` entry, so [`restore`]'s
//!     `checkout <id> -- .` has nothing under that path to touch even in
//!     principle.
//!   - **Invariant C** — [`restore`] snapshots the CURRENT state first
//!     (`Trigger::PreRestore`) before checking anything out, so a restore is
//!     always itself undoable.
//!   - **Invariant D** — files created since the checkpoint are deleted ONLY
//!     when the caller passes `delete_new: true` (default `false` at every
//!     call site). `git checkout <id> -- .` only ever touches paths that
//!     exist in `<id>`'s tree, so untracked new work is left alone unless the
//!     caller explicitly opts in.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::error::{AppError, AppResult};

use super::git::{self, GitCtx};

/// Relative (from the project root) path of the shadow repo's git-dir.
const SHADOW_GIT_DIR: &str = ".cimp/shadow.git";

/// Field separator for the `for-each-ref` format string in [`list`] — a
/// control character ([`unit separator`](https://en.wikipedia.org/wiki/C0_and_C1_control_codes))
/// that will never appear in a commit subject or trailer value in practice,
/// unlike `|` or `,` which a prompt-derived label could plausibly contain.
const FIELD_SEP: &str = "\u{1f}";

/// A long checkpoint gc/restore call (a big first-time snapshot, or a
/// `checkout` on a large tree) gets more room than the harness's
/// [`git::DEFAULT_TIMEOUT`] — still bounded, just generous for bulk ops.
const BULK_TIMEOUT: Duration = Duration::from_secs(120);

/// **Invariant A**: the ONE place a shadow `GitCtx` is built. `git_dir` and
/// `index_file` are pinned under `<root>/.cimp/shadow.git`, entirely separate
/// from the user's own `<root>/.git`; `work_tree = root` is what lets a
/// snapshot/restore touch the user's real files without touching their repo
/// metadata. Every public function in this module funnels its git calls
/// through a `GitCtx` built here — never construct one ad hoc.
fn shadow_ctx(root: &Path) -> GitCtx {
    let git_dir = root.join(SHADOW_GIT_DIR);
    let index_file = git_dir.join("index");
    GitCtx::shadow(root, git_dir, index_file)
}

fn git_dir_of(root: &Path) -> PathBuf {
    root.join(SHADOW_GIT_DIR)
}

/// What fired a checkpoint. `PreRestore` is never chosen by a caller — it's
/// set internally by [`restore`] for the safety-net snapshot it takes before
/// checking anything out (invariant C).
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    Prompt,
    Burst,
    Manual,
    PreRestore,
}

impl Trigger {
    fn as_str(self) -> &'static str {
        match self {
            Trigger::Prompt => "prompt",
            Trigger::Burst => "burst",
            Trigger::Manual => "manual",
            Trigger::PreRestore => "pre-restore",
        }
    }

    /// Parse a trigger trailer value back into a `Trigger`. Unknown/missing
    /// values (a hand-made commit, a future trigger kind this build doesn't
    /// know about) fall back to `Manual` rather than failing the whole
    /// `list()` call over one row's metadata.
    fn parse(s: &str) -> Trigger {
        match s {
            "prompt" => Trigger::Prompt,
            "burst" => Trigger::Burst,
            "pre-restore" => Trigger::PreRestore,
            _ => Trigger::Manual,
        }
    }
}

/// The stable, public handle for one checkpoint — a shadow-repo tag name
/// (`"cp-<seq>"`). Frontend/IPC code treats this as opaque; only this module
/// resolves it to a commit sha.
pub type CheckpointId = String;

/// One row of the Timeline section (`workbench_checkpoints`).
#[derive(Clone, Debug, Serialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub seq: u32,
    /// The resolved commit sha — display/debug only; every operation takes
    /// `id` (the tag), not this.
    pub commit: String,
    /// ISO-8601, from git's own `creatordate` (the shadow commit's date).
    pub ts: String,
    /// Epoch seconds of `ts` — what [`gc`]'s age cutoff compares against.
    pub ts_unix: u64,
    pub label: String,
    pub trigger: Trigger,
    pub agent: Option<String>,
    pub files_changed: u32,
    // TODO(C5, soft-dep on V12 Phase A `run_check`): an optional
    // `check_summary: Option<HashMap<String, u32>>` (check name → error
    // count), captured from the most recent `CheckReport` when the checks
    // module broadcasts one. Deliberately NOT added this phase (C5 is
    // explicitly optional / non-blocking for Phase C) — would ride the same
    // `Key: value` commit-trailer mechanism as `label`/`Trigger`/`Agent`/
    // `Files-Changed` above (e.g. `Check-cargo: 3`), read back in `list`'s
    // `for-each-ref --format`, no schema/storage change needed elsewhere.
}

/// The `workbench_restore` result: what a restore touched, for the UI's
/// post-restore report (and the confirmation dialog's dry-run, which calls
/// [`diff_vs_now`] instead — see that function's doc comment for why this
/// type isn't reused there).
#[derive(Clone, Debug, Serialize)]
pub struct RestoreReport {
    /// Invariant C's safety-net checkpoint of the pre-restore state —
    /// restoring THIS id undoes the restore.
    pub pre_restore_id: CheckpointId,
    /// Paths whose content the `checkout` actually changed on disk (modified
    /// in place, or recreated because they'd been deleted since the
    /// checkpoint). FIX 5 / V13 code review: explicitly EXCLUDES
    /// `created_since` — a path `checkout <target> -- .` never touches,
    /// since it only ever writes paths present in `target`'s tree.
    pub changed: Vec<String>,
    /// Paths that exist on disk now but did not exist in the restored
    /// checkpoint — the "created since" set. Listed regardless of
    /// `delete_new`.
    pub created_since: Vec<String>,
    /// The subset of `created_since` actually removed. Empty unless the
    /// caller passed `delete_new: true` (invariant D).
    pub deleted: Vec<String>,
}

/// `git init` the shadow dir (idempotent — safe to call before every
/// operation in this module) and pin the config that makes restores
/// byte-faithful on Windows: `core.autocrlf=false`/`core.fileMode=false` so
/// line-ending/permission churn never shows up as a spurious diff, `user.*`
/// so `commit-tree` has an identity without touching the user's own git
/// config, `gc.auto=0` since [`gc`] runs `git gc` explicitly instead. Also
/// (re)seeds `info/exclude` — see [`seed_exclude`].
pub async fn ensure(root: &Path, extra_ignore: &[String]) -> AppResult<()> {
    let ctx = shadow_ctx(root);
    let git_dir = git_dir_of(root);
    std::fs::create_dir_all(root.join(".cimp"))
        .map_err(|e| AppError::Workbench(format!("create .cimp dir: {e}")))?;

    let init = git::run(&ctx, &["init", "-q"], None).await?;
    if !init.success() {
        return Err(AppError::Workbench(format!(
            "shadow git init failed: {}",
            init.stderr.trim()
        )));
    }
    for (key, value) in [
        ("core.autocrlf", "false"),
        ("core.fileMode", "false"),
        ("user.name", "cimp"),
        ("user.email", "cimp@local"),
        ("gc.auto", "0"),
    ] {
        let out = git::run(&ctx, &["config", key, value], None).await?;
        if !out.success() {
            return Err(AppError::Workbench(format!(
                "shadow git config {key}={value} failed: {}",
                out.stderr.trim()
            )));
        }
    }
    seed_exclude(&git_dir, extra_ignore)?;
    Ok(())
}

/// (Re)write `<git_dir>/info/exclude` from scratch with the patterns that
/// must NEVER enter a shadow snapshot: `.git/` (invariant B's defense in
/// depth — see this module's doc comment) and `.cimp/` (the shadow repo's
/// own object store, worktree checkouts, everything else cImp keeps there),
/// plus the project's `graph.ignore` globs so the shadow repo doesn't
/// snapshot the same generated/vendored noise the code graph already
/// excludes. A full overwrite (not an append) is safe and correct here
/// because these are exactly the patterns [`ensure`]'s caller has right now
/// — unlike oversized-file exclusion (handled per-snapshot in
/// [`stage_and_write_tree`] instead, since "too big" can change file to
/// file), there's no accumulated state to lose by re-deriving this file
/// every call.
fn seed_exclude(git_dir: &Path, extra_ignore: &[String]) -> AppResult<()> {
    let info_dir = git_dir.join("info");
    std::fs::create_dir_all(&info_dir)
        .map_err(|e| AppError::Workbench(format!("create shadow info dir: {e}")))?;
    let mut out = String::from("# Generated by cImp — do not edit, regenerated on every launch.\n.git/\n.cimp/\n");
    for pat in extra_ignore {
        let pat = pat.trim();
        if !pat.is_empty() {
            out.push_str(pat);
            out.push('\n');
        }
    }
    std::fs::write(info_dir.join("exclude"), out)
        .map_err(|e| AppError::Workbench(format!("write shadow info/exclude: {e}")))
}

/// `git add -A` (honoring `info/exclude` + the project's own `.gitignore`,
/// since they share a work tree) then drop any staged path whose ON-DISK
/// size exceeds `max_file_bytes` before returning the resulting tree sha via
/// `write-tree`. Re-evaluated fresh on every call rather than maintained as
/// persistent exclude-file state, so a file that shrinks back under the cap
/// is picked back up automatically next time — no stale-exclusion bug to
/// worry about. `max_file_bytes == 0` means "no cap" (defensive; the
/// settings default is non-zero).
async fn stage_and_write_tree(ctx: &GitCtx, root: &Path, max_file_bytes: u64) -> AppResult<String> {
    let add = git::run(ctx, &["add", "-A"], Some(BULK_TIMEOUT)).await?;
    if !add.success() {
        return Err(AppError::Workbench(format!("shadow git add -A failed: {}", add.stderr.trim())));
    }
    if max_file_bytes > 0 {
        let staged = git::run(ctx, &["diff", "--cached", "--no-renames", "--name-only", "-z"], None).await?;
        let oversize: Vec<String> = staged
            .stdout
            .split('\0')
            .filter(|s| !s.is_empty())
            .filter(|p| {
                std::fs::metadata(root.join(p))
                    .map(|m| m.len() > max_file_bytes)
                    .unwrap_or(false)
            })
            .map(|s| s.to_string())
            .collect();
        if !oversize.is_empty() {
            let mut args: Vec<&str> = vec!["reset", "-q", "--"];
            args.extend(oversize.iter().map(String::as_str));
            // Best-effort: an oversize-unstage failure just means a big blob
            // rides along in this one snapshot rather than failing the whole
            // checkpoint over a size-hygiene nicety.
            let _ = git::run(ctx, &args, None).await;
        }
    }
    let tree = git::run(ctx, &["write-tree"], None).await?;
    if !tree.success() {
        return Err(AppError::Workbench(format!("shadow write-tree failed: {}", tree.stderr.trim())));
    }
    Ok(tree.stdout.trim().to_string())
}

/// Parse `git status --porcelain=v1 -z` into the list of current-side paths
/// changed since the index (i.e. since the last snapshot's `add -A`) — used
/// ONLY to count `Files-Changed` for the commit trailer. Must be called
/// BEFORE [`stage_and_write_tree`] refreshes the index (see [`snapshot`]'s
/// doc comment).
///
/// **No longer** [`snapshot`]'s dedup check (FIX 1 / V13 code review — see
/// `snapshot`'s doc comment for the data-loss bug this stopped being used
/// for): comparing the working tree against the shadow repo's own persistent
/// index is only meaningful if nothing ELSE touches that index between two
/// `snapshot` calls, which [`diff_vs_now`]'s own staging violates. Kept
/// around purely as a display/count helper now.
///
/// Deliberately NOT `git status --porcelain` (which is what an earlier
/// version of this function used): every checkpoint is an orphan
/// `commit-tree` with no parent, and this shadow repo's branch ref is never
/// advanced (no `git commit`, ever) — so `git status`'s "staged vs HEAD"
/// column permanently reads "unborn/empty HEAD vs a fully-staged index",
/// i.e. EVERY previously-snapshotted file shows up as freshly `A`dded on
/// every subsequent call, even when nothing has changed on disk. That broke
/// the (now-removed) dedup guard in [`snapshot`] (verified against a real
/// repo: a second, no-op `snapshot` call was minting a new checkpoint every
/// time). The fix at the time was to compare the working tree against the
/// INDEX only (ignoring HEAD entirely) via `git diff --name-only` (tracked,
/// modified/deleted) plus `git ls-files --others --exclude-standard`
/// (untracked) — exactly "what would the next `add -A` touch". That's still
/// correct for a file COUNT; it just isn't a safe dedup signal any more.
async fn changed_since_index(ctx: &GitCtx) -> AppResult<Vec<String>> {
    let diff_out = git::run(ctx, &["diff", "--no-renames", "--name-only", "-z"], None).await?;
    if !diff_out.success() {
        return Err(AppError::Workbench(format!(
            "shadow diff --name-only failed: {}",
            diff_out.stderr.trim()
        )));
    }
    let mut paths: Vec<String> =
        diff_out.stdout.split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect();

    let untracked = git::run(ctx, &["ls-files", "--others", "--exclude-standard", "-z"], None).await?;
    if !untracked.success() {
        return Err(AppError::Workbench(format!(
            "shadow ls-files --others failed: {}",
            untracked.stderr.trim()
        )));
    }
    paths.extend(untracked.stdout.split('\0').filter(|s| !s.is_empty()).map(str::to_string));
    Ok(paths)
}

/// Cap a checkpoint label to a sane commit-subject length. Callers (the
/// prompt-tap trigger) are expected to already truncate to ~60 chars, but
/// this is a hard backstop against an unbounded label reaching `git
/// commit-tree` as a multi-KB commit subject.
fn truncate_label(label: &str) -> String {
    const MAX: usize = 200;
    let mut chars = label.chars();
    let truncated: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Next `cp-<seq>` number: one past the highest existing `cp-*` tag, or `1`
/// if there are none yet. Queried fresh each time (no counter file) — cheap,
/// and immune to a counter file getting out of sync with what tags actually
/// exist (e.g. after a `gc` prune).
async fn next_seq(ctx: &GitCtx) -> AppResult<u32> {
    let out = git::run(ctx, &["tag", "-l", "cp-*"], None).await?;
    let max = out
        .stdout
        .lines()
        .filter_map(|l| l.trim().strip_prefix("cp-"))
        .filter_map(|n| n.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    Ok(max + 1)
}

/// The highest-numbered existing `cp-<seq>` tag, if any — [`snapshot`]'s
/// dedup check's notion of "the last checkpoint". A tiny sibling of
/// [`next_seq`] (same `tag -l cp-*` scan) rather than routing through
/// [`list`], which also reads back commit messages/trailers dedup doesn't
/// need.
async fn latest_checkpoint_tag(ctx: &GitCtx) -> AppResult<Option<String>> {
    let out = git::run(ctx, &["tag", "-l", "cp-*"], None).await?;
    let max = out
        .stdout
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("cp-").and_then(|n| n.parse::<u32>().ok()).map(|n| (n, l.to_string())))
        .max_by_key(|(n, _)| *n);
    Ok(max.map(|(_, tag)| tag))
}

/// Resolve a checkpoint id (a `cp-<seq>` tag) to its commit sha. Every other
/// function in this module that needs a concrete commit goes through this,
/// so an unknown/garbage id fails with one consistent, typed message instead
/// of a raw `git` error surfacing from whichever call happened to choke on
/// it first.
async fn resolve_commit(ctx: &GitCtx, id: &str) -> AppResult<String> {
    let spec = format!("{id}^{{commit}}");
    let out = git::run(ctx, &["rev-parse", "-q", "--verify", &spec], None).await?;
    if !out.success() {
        return Err(AppError::Workbench(format!("unknown checkpoint: {id}")));
    }
    Ok(out.stdout.trim().to_string())
}

/// Snapshot the working tree: `add -A` (respecting excludes + the size cap)
/// → an orphan `commit-tree` (no parent — each checkpoint stands alone,
/// simplest thing that still supports diff/restore by commit id, per the
/// milestone's retention-design note) tagged `cp-<seq>`. Metadata (label,
/// trigger, agent, files-changed count) rides as the commit message: the
/// label is the subject line, the rest are `Key: value` trailers — no
/// separate sidecar file to keep in sync, and `git log`/`for-each-ref` can
/// read it back directly (see [`list`]).
///
/// **Dedup** (FIX 1 / V13 code review — see this module's git history for the
/// data-loss bug this replaced): a real snapshot is skipped, returning the
/// EXISTING latest checkpoint instead, only when the CURRENT tree sha
/// (freshly computed by `stage_and_write_tree` below, which this function
/// now ALWAYS runs first) equals the last checkpoint's own tree sha
/// (`<tag>^{tree}`). Comparing tree shas — not [`changed_since_index`]'s
/// "did the working tree move since the INDEX was last updated" check, which
/// is what this function used before — is what makes this robust to anything
/// ELSE that mutates the shadow repo's shared, persistent index between two
/// `snapshot` calls. [`diff_vs_now`] is exactly such a caller: its own
/// `stage_and_write_tree` (needed to see untracked files in a dry-run diff)
/// used to leave the index matching disk, so the NEXT `snapshot` — e.g.
/// `restore`'s invariant-C pre-restore safety snapshot — would see "nothing
/// changed since the index" and silently return a STALE old checkpoint
/// instead of a real one, leaving `restore` with no valid undo point and
/// destroying the user's uncommitted edits on `checkout`. Comparing tree
/// shas sidesteps the whole index-staleness question: whoever staged last,
/// the CONTENT either matches the last checkpoint or it doesn't.
///
/// If there is no prior checkpoint AND nothing to snapshot (a still-empty new
/// project), an empty baseline checkpoint is created anyway so
/// `list`/`diff_vs_now`/`restore` always have something to anchor to (`Some`
/// vs `None` on `latest_checkpoint_tag` makes the trees compare unequal
/// automatically in that case).
pub async fn snapshot(
    root: &Path,
    label: &str,
    trigger: Trigger,
    agent: Option<&str>,
    extra_ignore: &[String],
    max_file_bytes: u64,
) -> AppResult<CheckpointId> {
    ensure(root, extra_ignore).await?;
    let ctx = shadow_ctx(root);

    // For the `Files-Changed` trailer only — NOT the dedup decision anymore
    // (see this function's doc comment). Must run BEFORE staging below:
    // once the index is refreshed by `stage_and_write_tree`, a working-tree-
    // vs-index diff would trivially read empty.
    let changed = changed_since_index(&ctx).await?;

    // ALWAYS stage + write the tree now (this used to run only after the
    // dedup check returned early) — see the doc comment above for why.
    let current_tree = stage_and_write_tree(&ctx, root, max_file_bytes).await?;

    if let Some(last_tag) = latest_checkpoint_tag(&ctx).await? {
        let spec = format!("{last_tag}^{{tree}}");
        let last_tree = git::run(&ctx, &["rev-parse", "-q", "--verify", &spec], None).await?;
        if last_tree.success() && last_tree.stdout.trim() == current_tree {
            return Ok(last_tag);
        }
    }

    let seq = next_seq(&ctx).await?;
    let tag = format!("cp-{seq}");
    let message = format!(
        "{}\n\nTrigger: {}\nAgent: {}\nFiles-Changed: {}\n",
        truncate_label(label),
        trigger.as_str(),
        agent.unwrap_or("-"),
        changed.len(),
    );
    let commit = git::run_with_stdin(&ctx, &["commit-tree", &current_tree, "-F", "-"], message.as_bytes(), None).await?;
    if !commit.success() {
        return Err(AppError::Workbench(format!(
            "shadow commit-tree failed: {}",
            commit.stderr.trim()
        )));
    }
    let sha = commit.stdout.trim().to_string();
    let tag_out = git::run(&ctx, &["tag", &tag, &sha], None).await?;
    if !tag_out.success() {
        return Err(AppError::Workbench(format!(
            "shadow tag {tag} failed: {}",
            tag_out.stderr.trim()
        )));
    }
    Ok(tag)
}

/// Every checkpoint, oldest first, read back from `refs/tags/cp-*` via
/// `for-each-ref` (no separate metadata store — see [`snapshot`]'s doc
/// comment). Returns an empty list (not an error) when the shadow repo has
/// never been `ensure`d — a project with checkpoints off, or one that just
/// hasn't snapshotted yet, has "no checkpoints", not a broken Timeline.
pub async fn list(root: &Path) -> AppResult<Vec<Checkpoint>> {
    let git_dir = git_dir_of(root);
    if !git_dir.join("HEAD").exists() {
        return Ok(Vec::new());
    }
    let ctx = shadow_ctx(root);
    // `%(trailers:...)` tokens each carry their OWN trailing newline as part
    // of the field's value (documented git behavior, not an artifact) —
    // splitting records on `\n` would therefore fragment a single ref's
    // formatted line into several pieces. `%x00` is NOT recognized as a hex
    // escape by `for-each-ref --format` (verified against git 2.54: it comes
    // through as the four literal characters `%x00`, not a NUL byte) — so
    // terminate each record with a literal record-separator control
    // character (U+001E) instead, which for-each-ref prints as-is since it
    // isn't part of any `%(...)` atom. `\n` only ever shows up embedded
    // WITHIN a field after this (trailer values, plus git's own automatic
    // end-of-line newline trailing the RS), which the whole-record `.trim()`
    // and per-field `.trim()` below both strip.
    const REC_SEP: char = '\u{1e}';
    let format = format!(
        "%(objectname){sep}%(creatordate:unix){sep}%(creatordate:iso-strict){sep}%(contents:subject){sep}%(trailers:key=Trigger,valueonly){sep}%(trailers:key=Agent,valueonly){sep}%(trailers:key=Files-Changed,valueonly){sep}%(refname:short){rec_sep}",
        sep = FIELD_SEP,
        rec_sep = REC_SEP
    );
    let fmt_arg = format!("--format={format}");
    let out = git::run(&ctx, &["for-each-ref", "--sort=creatordate", &fmt_arg, "refs/tags/cp-*"], None).await?;
    if !out.success() {
        return Err(AppError::Workbench(format!(
            "shadow for-each-ref failed: {}",
            out.stderr.trim()
        )));
    }
    let mut checkpoints = Vec::new();
    for record in out.stdout.split(REC_SEP) {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.split(FIELD_SEP).collect();
        if fields.len() != 8 {
            continue; // malformed row (shouldn't happen) — skip, don't panic
        }
        let tag = fields[7].trim().to_string();
        let seq = tag.strip_prefix("cp-").and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
        let agent_raw = fields[5].trim();
        checkpoints.push(Checkpoint {
            id: tag,
            seq,
            commit: fields[0].trim().to_string(),
            ts_unix: fields[1].trim().parse().unwrap_or(0),
            ts: fields[2].trim().to_string(),
            label: fields[3].trim().to_string(),
            trigger: Trigger::parse(fields[4].trim()),
            agent: if agent_raw.is_empty() || agent_raw == "-" { None } else { Some(agent_raw.to_string()) },
            files_changed: fields[6].trim().parse().unwrap_or(0),
        });
    }
    checkpoints.sort_by_key(|c| c.seq);
    Ok(checkpoints)
}

/// Unified diff between checkpoint `id` and the CURRENT working tree (not
/// just the last checkpoint) — feeds `diff.rs::parse_unified` for both the
/// Timeline's "Diff vs now" viewer and the restore confirmation dialog's
/// dry-run file list. Computed tree-to-tree (`id`'s commit vs. a fresh
/// `write-tree` of the current work tree, taken WITHOUT committing) rather
/// than via a raw working-tree diff, so untracked/new files are included the
/// same way a real snapshot would see them — `git diff <tree> -- .` alone
/// does not show untracked files, but this does since they get staged into
/// the throwaway tree first.
///
/// **FIX 1 defense in depth**: this function's own `add -A` used to run
/// against the shadow repo's shared, persistent index (the same one
/// [`snapshot`] and [`restore`]'s `checkout` use) — a purely-read-only
/// dry-run call was mutating shared state as a side effect. [`snapshot`]'s
/// dedup no longer trusts that index's staleness (it compares tree shas
/// instead — see its doc comment), which is the actual fix for the data-loss
/// bug this caused; on top of that, this call now stages into a disposable
/// SCRATCH index of its own (`GIT_INDEX_FILE` pointed at a temp file,
/// cleaned up immediately after `write-tree`) so a dry-run diff has zero
/// observable effect on the shadow repo's persistent index at all, belt and
/// suspenders.
pub async fn diff_vs_now(root: &Path, id: &str, extra_ignore: &[String], max_file_bytes: u64) -> AppResult<String> {
    ensure(root, extra_ignore).await?;
    let ctx = shadow_ctx(root);
    let target = resolve_commit(&ctx, id).await?;

    let scratch_index = git_dir_of(root).join(format!("index.diffnow-{}", uuid::Uuid::new_v4()));
    let scratch_ctx = GitCtx { index_file: Some(scratch_index.clone()), ..ctx.clone() };
    let now_tree = stage_and_write_tree(&scratch_ctx, root, max_file_bytes).await?;
    let _ = std::fs::remove_file(&scratch_index);

    let out = git::run(&ctx, &["diff", "--no-color", "--no-renames", "--unified=3", &target, &now_tree], Some(BULK_TIMEOUT)).await?;
    if !out.success() {
        return Err(AppError::Workbench(format!("shadow diff failed: {}", out.stderr.trim())));
    }
    Ok(out.stdout)
}

/// Restore the working tree to checkpoint `id`. Sequence (see the module
/// doc comment for the invariant each step upholds):
///   1. **(C)** snapshot the CURRENT state (`Trigger::PreRestore`) — this
///      restore is now itself undoable.
///   2. Diff `id` against that pre-restore snapshot, commit-to-commit, to
///      get the exact "created since" set (`--diff-filter=A`) and the
///      "changed" set (what the checkout in step 3 will actually touch).
///   3. **(A/B)** `git checkout <id> -- .` via the shadow `GitCtx` — writes
///      ordinary files into the shared work tree; never touches `<root>/.git`
///      (excluded from every shadow tree — see the module doc comment) and
///      never deletes a path that isn't in `id`'s tree.
///   4. **(D)** delete the "created since" files ONLY if `delete_new` — the
///      default is to leave them alone.
pub async fn restore(
    root: &Path,
    id: &str,
    delete_new: bool,
    extra_ignore: &[String],
    max_file_bytes: u64,
) -> AppResult<RestoreReport> {
    ensure(root, extra_ignore).await?;
    let ctx = shadow_ctx(root);
    let target = resolve_commit(&ctx, id).await?;

    let pre_restore_id = snapshot(root, "pre-restore", Trigger::PreRestore, None, extra_ignore, max_file_bytes).await?;
    let pre_sha = resolve_commit(&ctx, &pre_restore_id).await?;

    let added = git::run(&ctx, &["diff", "--no-renames", "--name-only", "--diff-filter=A", "-z", &target, &pre_sha], None).await?;
    if !added.success() {
        return Err(AppError::Workbench(format!(
            "shadow diff --diff-filter=A failed: {}",
            added.stderr.trim()
        )));
    }
    let created_since: Vec<String> = added.stdout.split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect();

    let changed_out = git::run(&ctx, &["diff", "--no-renames", "--name-only", "-z", &pre_sha, &target], None).await?;
    if !changed_out.success() {
        return Err(AppError::Workbench(format!(
            "shadow diff --name-only failed: {}",
            changed_out.stderr.trim()
        )));
    }
    // FIX 5 / V13 code review: the unfiltered `pre_sha..target` diff also
    // lists every `created_since` path (present in `pre_sha`, the
    // pre-restore state, but absent from `target`) — from `target`'s
    // perspective those look like ordinary "differs" entries too. But
    // `checkout <target> -- .` below only ever writes paths that exist IN
    // `target`'s tree; a path that doesn't exist there is left on disk
    // exactly as-is (that's invariant D — untracked new work survives a
    // restore unless `delete_new`). So `created_since` paths are never
    // actually rewritten by this restore, and reporting them under
    // `changed` would tell the UI they were touched when they weren't.
    let created_set: HashSet<&str> = created_since.iter().map(String::as_str).collect();
    let changed: Vec<String> = changed_out
        .stdout
        .split('\0')
        .filter(|s| !s.is_empty() && !created_set.contains(s))
        .map(str::to_string)
        .collect();

    // (A/B) shadow checkout — see this function's doc comment.
    let checkout = git::run(&ctx, &["checkout", &target, "--", "."], Some(BULK_TIMEOUT)).await?;
    if !checkout.success() {
        return Err(AppError::Workbench(format!("shadow checkout failed: {}", checkout.stderr.trim())));
    }

    // (D) opt-in deletion of files created since the checkpoint.
    let mut deleted = Vec::new();
    if delete_new {
        for path in &created_since {
            let abs = root.join(path);
            if abs.is_file() || abs.is_symlink() {
                if std::fs::remove_file(&abs).is_ok() {
                    deleted.push(path.clone());
                }
            }
        }
    }

    Ok(RestoreReport { pre_restore_id, changed, created_since, deleted })
}

/// Drop checkpoints beyond `max` (ring buffer, oldest first) or older than
/// `max_age_days` (whichever applies — a checkpoint aged out doesn't also
/// need to be within the count), then reclaim the now-unreachable objects
/// with `git gc --prune=now`. `max == 0` / `max_age_days == 0` means "no
/// limit" for that axis. A no-op (including no `gc` spawn) when nothing
/// qualifies for deletion, and a no-op entirely when the shadow repo has
/// never been `ensure`d.
pub async fn gc(root: &Path, max: u32, max_age_days: u32) -> AppResult<()> {
    let git_dir = git_dir_of(root);
    if !git_dir.join("HEAD").exists() {
        return Ok(());
    }
    let ctx = shadow_ctx(root);
    let checkpoints = list(root).await?; // ascending by seq

    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let max_age_secs = (max_age_days as u64).saturating_mul(86_400);

    let mut to_delete: HashSet<String> = HashSet::new();
    if max_age_days > 0 {
        for cp in &checkpoints {
            if now.saturating_sub(cp.ts_unix) > max_age_secs {
                to_delete.insert(cp.id.clone());
            }
        }
    }
    if max > 0 {
        let remaining: Vec<&Checkpoint> = checkpoints.iter().filter(|c| !to_delete.contains(&c.id)).collect();
        if remaining.len() > max as usize {
            let excess = remaining.len() - max as usize;
            for cp in remaining.into_iter().take(excess) {
                to_delete.insert(cp.id.clone());
            }
        }
    }
    if to_delete.is_empty() {
        return Ok(());
    }

    let names: Vec<&str> = to_delete.iter().map(String::as_str).collect();
    let mut args: Vec<&str> = vec!["tag", "-d"];
    args.extend(names);
    let out = git::run(&ctx, &args, None).await?;
    if !out.success() {
        return Err(AppError::Workbench(format!("shadow tag -d failed: {}", out.stderr.trim())));
    }
    // Best-effort space reclaim: the tags (the part of the retention
    // contract that matters) are already gone even if this fails.
    let _ = git::run(&ctx, &["gc", "--prune=now", "-q"], Some(BULK_TIMEOUT)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_git() -> bool {
        crate::pty::resolve_command("git").is_ok()
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git").args(args).current_dir(dir).output().expect("git");
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-shadow-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A project dir with a REAL user git repo (`git init` + one commit) —
    /// used by every invariant-B test to prove the shadow repo never
    /// disturbs it. Commits a `.gitignore` for `.cimp/` (the realistic setup
    /// — cImp's own directory has no business showing up in the user's `git
    /// status`) so this fixture's status stays meaningfully comparable
    /// before/after shadow ops, rather than permanently showing `.cimp/` as
    /// untracked noise the moment the shadow repo is first created.
    fn user_repo(tag: &str) -> PathBuf {
        let dir = tempdir(tag);
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "user@example.com"]);
        git(&dir, &["config", "user.name", "User"]);
        git(&dir, &["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join("tracked.txt"), "hello\n").unwrap();
        std::fs::write(dir.join(".gitignore"), ".cimp/\n").unwrap();
        git(&dir, &["add", "tracked.txt", ".gitignore"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    /// A byte-level fingerprint of everything under `<dir>/.git`: every
    /// file's relative path AND content, hashed together. Stronger than
    /// comparing `status`/`HEAD` alone — this catches ANY shadow-op
    /// regression that wrote so much as one stray byte into the user's repo
    /// metadata (a new loose object, a ref, a config line, anything),
    /// even something `git status`/`rev-parse HEAD` wouldn't surface.
    ///
    /// Excludes `.git/index` itself: plain `git status`/`git diff` — called
    /// by this test's OWN helpers, not by anything under test — legitimately
    /// rewrites the index's cached filesystem stat info (mtime/size) as a
    /// well-known git optimization ("racy git" avoidance), with no change to
    /// the logically-staged content. That's expected background noise from
    /// exercising a real repo, not a shadow-op regression, so comparing its
    /// raw bytes here would be a false positive independent of this
    /// module's own correctness.
    fn hash_user_git_dir(dir: &Path) -> String {
        use std::hash::{Hash, Hasher};
        fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else if path.file_name().and_then(|n| n.to_str()) == Some("index") && path.parent() == Some(base) {
                    continue; // top-level index cache — see doc comment
                } else if let Ok(bytes) = std::fs::read(&path) {
                    let rel = path.strip_prefix(base).unwrap_or(&path).display().to_string().replace('\\', "/");
                    out.push((rel, bytes));
                }
            }
        }
        let git_dir = dir.join(".git");
        let mut entries = Vec::new();
        walk(&git_dir, &git_dir, &mut entries);
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (path, bytes) in &entries {
            path.hash(&mut hasher);
            bytes.hash(&mut hasher);
        }
        format!("{:016x} ({} files)", hasher.finish(), entries.len())
    }

    fn user_git_head(dir: &Path) -> String {
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    fn user_git_status(dir: &Path) -> String {
        std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(dir)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    // ── invariant A: shadow_ctx shape ───────────────────────────────────

    #[test]
    fn shadow_ctx_never_points_at_the_users_git_dir() {
        let root = PathBuf::from("/some/project");
        let ctx = shadow_ctx(&root);
        assert_eq!(ctx.git_dir, Some(root.join(".cimp").join("shadow.git")));
        assert_eq!(ctx.index_file, Some(root.join(".cimp").join("shadow.git").join("index")));
        assert_eq!(ctx.work_tree, Some(root.clone()));
        assert_ne!(ctx.git_dir, Some(root.join(".git")));
    }

    // ── ensure ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn ensure_seeds_exclude_with_git_and_cimp_and_extra_globs() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("ensure");
        ensure(&dir, &["*.generated.js".to_string()]).await.expect("ensure");
        let exclude = std::fs::read_to_string(dir.join(".cimp/shadow.git/info/exclude")).unwrap();
        assert!(exclude.contains(".git/"));
        assert!(exclude.contains(".cimp/"));
        assert!(exclude.contains("*.generated.js"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ensure_is_idempotent() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("ensure-idem");
        ensure(&dir, &[]).await.expect("ensure 1");
        ensure(&dir, &[]).await.expect("ensure 2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── snapshot / list / dedupe ─────────────────────────────────────────

    #[tokio::test]
    async fn snapshot_then_list_round_trips_metadata() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("meta");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let id = snapshot(&dir, "prompt: do the thing", Trigger::Prompt, Some("claude"), &[], 0)
            .await
            .expect("snapshot");
        assert_eq!(id, "cp-1");

        let cps = list(&dir).await.expect("list");
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].id, "cp-1");
        assert_eq!(cps[0].seq, 1);
        assert_eq!(cps[0].label, "prompt: do the thing");
        assert_eq!(cps[0].trigger, Trigger::Prompt);
        assert_eq!(cps[0].agent, Some("claude".to_string()));
        assert_eq!(cps[0].files_changed, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn snapshot_dedupes_when_work_tree_is_unchanged() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("dedupe");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let id1 = snapshot(&dir, "first", Trigger::Manual, None, &[], 0).await.expect("snapshot 1");
        // Nothing changed on disk since — must return the SAME id, not mint
        // a new checkpoint.
        let id2 = snapshot(&dir, "second (no-op)", Trigger::Manual, None, &[], 0).await.expect("snapshot 2");
        assert_eq!(id1, id2);
        assert_eq!(list(&dir).await.expect("list").len(), 1);

        // A real change DOES produce a new checkpoint.
        std::fs::write(dir.join("a.txt"), "one changed\n").unwrap();
        let id3 = snapshot(&dir, "third", Trigger::Manual, None, &[], 0).await.expect("snapshot 3");
        assert_ne!(id1, id3);
        assert_eq!(list(&dir).await.expect("list").len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn snapshot_of_a_truly_empty_project_still_creates_a_baseline() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("empty");
        let id = snapshot(&dir, "baseline", Trigger::Manual, None, &[], 0).await.expect("snapshot");
        assert_eq!(id, "cp-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── restore: round trip, CRLF-faithful, invariant D, invariant C ────

    #[tokio::test]
    async fn restore_round_trip_is_byte_faithful_including_crlf() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("roundtrip");
        let crlf_content = b"line1\r\nline2\r\nline3\r\n".to_vec();
        std::fs::write(dir.join("crlf.txt"), &crlf_content).unwrap();
        std::fs::write(dir.join("plain.txt"), "hello\n").unwrap();
        let cp = snapshot(&dir, "baseline", Trigger::Manual, None, &[], 0).await.expect("snapshot");

        // Mutate both files after the checkpoint.
        std::fs::write(dir.join("crlf.txt"), b"MUTATED\r\n").unwrap();
        std::fs::write(dir.join("plain.txt"), "MUTATED\n").unwrap();

        let report = restore(&dir, &cp, false, &[], 0).await.expect("restore");
        assert!(report.changed.iter().any(|p| p == "crlf.txt"));
        assert!(report.changed.iter().any(|p| p == "plain.txt"));

        let restored_crlf = std::fs::read(dir.join("crlf.txt")).unwrap();
        assert_eq!(restored_crlf, crlf_content, "CRLF bytes must round-trip exactly");
        let restored_plain = std::fs::read_to_string(dir.join("plain.txt")).unwrap();
        assert_eq!(restored_plain, "hello\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_keeps_new_files_by_default_deletes_only_with_delete_new() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("keepnew");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let cp = snapshot(&dir, "baseline", Trigger::Manual, None, &[], 0).await.expect("snapshot");

        // New, untracked-by-the-checkpoint work appears after the snapshot.
        std::fs::write(dir.join("new_work.txt"), "please don't delete me\n").unwrap();

        // Default: delete_new = false — the new file must survive.
        let report1 = restore(&dir, &cp, false, &[], 0).await.expect("restore keep");
        assert!(report1.created_since.iter().any(|p| p == "new_work.txt"));
        // FIX 5: `changed` must NOT also list `new_work.txt` — the checkout
        // never touched it (it isn't in `cp`'s tree at all), so reporting it
        // as "changed" would be wrong.
        assert!(
            !report1.changed.iter().any(|p| p == "new_work.txt"),
            "changed must exclude created_since paths the checkout never touched"
        );
        assert!(report1.deleted.is_empty(), "must not delete anything when delete_new is false");
        assert!(dir.join("new_work.txt").exists(), "new file must survive a default restore");

        // Opt in: delete_new = true — now it goes.
        // (The pre-restore checkpoint from report1 already re-snapshotted
        // new_work.txt, so restoring `cp` again still shows it as
        // "created since".)
        let report2 = restore(&dir, &cp, true, &[], 0).await.expect("restore delete_new");
        assert!(report2.deleted.iter().any(|p| p == "new_work.txt"));
        assert!(!dir.join("new_work.txt").exists(), "delete_new=true must remove files created since the checkpoint");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_recreates_files_deleted_since_the_checkpoint() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("recreate");
        std::fs::write(dir.join("keep_me.txt"), "important\n").unwrap();
        let cp = snapshot(&dir, "baseline", Trigger::Manual, None, &[], 0).await.expect("snapshot");

        std::fs::remove_file(dir.join("keep_me.txt")).unwrap();
        assert!(!dir.join("keep_me.txt").exists());

        restore(&dir, &cp, false, &[], 0).await.expect("restore");
        assert!(dir.join("keep_me.txt").exists(), "restore must recreate a file deleted since the checkpoint");
        assert_eq!(std::fs::read_to_string(dir.join("keep_me.txt")).unwrap(), "important\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_creates_a_pre_restore_checkpoint_that_is_itself_undoable() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("prerestore");
        std::fs::write(dir.join("a.txt"), "v1\n").unwrap();
        let cp1 = snapshot(&dir, "v1", Trigger::Manual, None, &[], 0).await.expect("snapshot v1");
        std::fs::write(dir.join("a.txt"), "v2\n").unwrap();
        let _cp2 = snapshot(&dir, "v2", Trigger::Manual, None, &[], 0).await.expect("snapshot v2");
        // A real change since v2's snapshot, so `restore`'s internal
        // pre-restore snapshot below can't dedupe against v2's (Manual)
        // checkpoint and must mint a genuine new `Trigger::PreRestore` one.
        std::fs::write(dir.join("uncommitted.txt"), "still here\n").unwrap();

        let report = restore(&dir, &cp1, false, &[], 0).await.expect("restore to v1");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "v1\n");

        let cps = list(&dir).await.expect("list");
        let pre = cps.iter().find(|c| c.id == report.pre_restore_id).expect("pre-restore checkpoint exists");
        assert_eq!(pre.trigger, Trigger::PreRestore);

        // Undo the restore by restoring the pre-restore checkpoint.
        restore(&dir, &report.pre_restore_id, false, &[], 0).await.expect("undo restore");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "v2\n", "restoring pre-restore must undo the restore");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **FIX 1 mandatory regression test** (V13 code review — CRITICAL DATA
    /// LOSS). Reproduces the exact sequence the bug report describes: a
    /// restore confirmation dialog's dry-run (`diff_vs_now`) runs
    /// `stage_and_write_tree`, which used to mutate the shadow repo's
    /// SHARED, persistent index. `snapshot`'s old dedup guard
    /// (`changed_since_index`, working-tree-vs-INDEX) then saw "nothing
    /// changed since the index" on the very next snapshot — even though the
    /// user had genuinely edited a file since the last checkpoint — because
    /// the dry-run had already silently brought the index up to date with
    /// disk. `restore`'s invariant-C pre-restore safety snapshot is exactly
    /// such a "next snapshot": it would wrongly dedup to the stale prior
    /// checkpoint, leaving no real undo point, and the user's uncommitted
    /// edit would be destroyed with no way back once `checkout` ran.
    ///
    /// Without the fix (tree-sha dedup in `snapshot`, plus the `diff_vs_now`
    /// scratch-index defense in depth): `report.pre_restore_id` comes back
    /// equal to `cp1` (the stale checkpoint, state A) instead of a genuine
    /// snapshot of state B, so this test's first assertion fails, and the
    /// second (undoing the restore must bring back state B) would also fail
    /// since restoring `cp1` again just gives state A back, not state B.
    /// With the fix: both assertions pass.
    #[tokio::test]
    async fn restore_after_a_dry_run_diff_preserves_uncommitted_edits() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("dryrun-restore");

        // Checkpoint cp-1: state A.
        std::fs::write(dir.join("tracked.txt"), "state A\n").unwrap();
        let cp1 = snapshot(&dir, "state A", Trigger::Manual, None, &[], 0).await.expect("snapshot A");

        // Edit to state B — uncommitted, and NOT checkpointed anywhere yet.
        std::fs::write(dir.join("tracked.txt"), "state B\n").unwrap();

        // Simulate the restore confirmation dialog's dry-run: this stages
        // the shadow repo's index as a side effect (or used to — see the
        // module doc comment).
        let _ = diff_vs_now(&dir, &cp1, &[], 0).await.expect("diff_vs_now (dry run)");

        // Now actually restore to cp1. This is where the bug bites: a
        // pre-restore snapshot that wrongly dedups against the stale cp1
        // instead of capturing the real state-B edit has no valid undo
        // point once `checkout` overwrites the file with state A.
        let report = restore(&dir, &cp1, false, &[], 0).await.expect("restore");

        assert_ne!(
            report.pre_restore_id, cp1,
            "restore must take a REAL pre-restore snapshot of state B, not dedup to the stale cp1 checkpoint"
        );

        // The working tree is now at the restore target, state A.
        assert_eq!(std::fs::read_to_string(dir.join("tracked.txt")).unwrap(), "state A\n");

        // Undo the restore by restoring the pre-restore checkpoint — state B
        // (the user's uncommitted edit) must come back.
        restore(&dir, &report.pre_restore_id, false, &[], 0).await.expect("undo restore");
        assert_eq!(
            std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
            "state B\n",
            "restoring pre_restore_id must bring back the uncommitted state-B edit that diff_vs_now's dry-run must not have destroyed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── invariant B: the user's own .git is never touched ────────────────

    #[tokio::test]
    async fn invariant_b_user_git_untouched_by_snapshot_and_restore() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("invariant-b");
        let head_before = user_git_head(&dir);
        let status_before = user_git_status(&dir); // "" — a fresh commit, clean tree
        let git_hash_before = hash_user_git_dir(&dir);

        // Checkpoint the CLEAN baseline first, so that a later restore back
        // to it is expected to reproduce `status_before`/`head_before`
        // exactly — the meaningful form of "unchanged": shadow ops must
        // never advance the user's HEAD (checked throughout), and a restore
        // to a checkpoint of a clean tree must leave that tree clean again,
        // not just "some new dirty state neither op explains".
        let baseline = snapshot(&dir, "clean baseline", Trigger::Manual, None, &[], 0).await.expect("snapshot baseline");
        assert_eq!(user_git_head(&dir), head_before, "a snapshot of a clean tree must not move the user's HEAD");

        // Exercise a realistic sequence: dirty the tree (tracked + untracked),
        // snapshot that dirty state, dirty further, then restore all the way
        // back to the clean baseline (delete_new=true so the untracked
        // scratch files don't linger) — all while the user's own repo sits
        // untouched at the git-metadata level throughout.
        std::fs::write(dir.join("tracked.txt"), "hello v2\n").unwrap();
        std::fs::write(dir.join("untracked.txt"), "scratch\n").unwrap();
        let cp = snapshot(&dir, "checkpoint", Trigger::Prompt, Some("claude"), &[], 0).await.expect("snapshot");
        assert_eq!(user_git_head(&dir), head_before, "an intermediate snapshot must not move the user's HEAD");

        std::fs::write(dir.join("tracked.txt"), "hello v3\n").unwrap();
        std::fs::write(dir.join("another_new.txt"), "more scratch\n").unwrap();
        let _ = restore(&dir, &cp, true, &[], 0).await.expect("restore to dirty checkpoint");
        assert_eq!(user_git_head(&dir), head_before, "a restore must not move the user's HEAD");

        // Undo all the way back to the pristine, just-committed baseline.
        let _ = restore(&dir, &baseline, true, &[], 0).await.expect("restore to clean baseline");

        let head_after = user_git_head(&dir);
        let status_after = user_git_status(&dir);
        assert_eq!(head_before, head_after, "user's HEAD must be unchanged by any shadow op");
        assert_eq!(status_before, status_after, "restoring to a checkpoint of the clean tree must leave it clean again");
        assert_eq!(
            git_hash_before,
            hash_user_git_dir(&dir),
            "the user's .git directory must be byte-identical before and after any snapshot/restore"
        );

        // Defense-in-depth check: the checkpoint's tree itself must never
        // contain a `.git` entry (see `seed_exclude`'s doc comment).
        let ctx = shadow_ctx(&dir);
        let sha = resolve_commit(&ctx, &cp).await.expect("resolve");
        let ls = git::run(&ctx, &["ls-tree", "-r", "--name-only", &sha], None).await.expect("ls-tree");
        assert!(
            !ls.stdout.lines().any(|l| l == ".git" || l.starts_with(".git/")),
            "a checkpoint's tree must never contain the user's .git: {}",
            ls.stdout
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── diff_vs_now ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn diff_vs_now_is_parseable_unified_text() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("diffnow");
        std::fs::write(dir.join("a.txt"), "line1\nline2\n").unwrap();
        let cp = snapshot(&dir, "baseline", Trigger::Manual, None, &[], 0).await.expect("snapshot");
        std::fs::write(dir.join("a.txt"), "line1\nline2X\n").unwrap();
        std::fs::write(dir.join("b.txt"), "brand new\n").unwrap();

        let text = diff_vs_now(&dir, &cp, &[], 0).await.expect("diff_vs_now");
        let parsed = crate::workbench::diff::parse_unified(&text);
        assert!(parsed.iter().any(|f| f.path == "a.txt"));
        assert!(parsed.iter().any(|f| f.path == "b.txt"), "new untracked file must show up in diff_vs_now");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── gc ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn gc_respects_count_retention() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("gc-count");
        for i in 0..5 {
            std::fs::write(dir.join("a.txt"), format!("v{i}\n")).unwrap();
            snapshot(&dir, &format!("v{i}"), Trigger::Manual, None, &[], 0).await.expect("snapshot");
        }
        assert_eq!(list(&dir).await.expect("list").len(), 5);

        gc(&dir, 2, 0).await.expect("gc");
        let cps = list(&dir).await.expect("list after gc");
        assert_eq!(cps.len(), 2, "gc must prune down to `max`");
        // The two newest survive.
        assert_eq!(cps[0].label, "v3");
        assert_eq!(cps[1].label, "v4");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gc_zero_means_no_limit_on_that_axis() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("gc-nolimit");
        for i in 0..3 {
            std::fs::write(dir.join("a.txt"), format!("v{i}\n")).unwrap();
            snapshot(&dir, &format!("v{i}"), Trigger::Manual, None, &[], 0).await.expect("snapshot");
        }
        gc(&dir, 0, 0).await.expect("gc no-op");
        assert_eq!(list(&dir).await.expect("list").len(), 3, "max=0, max_age_days=0 must prune nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gc_on_never_ensured_project_is_a_no_op() {
        let dir = tempdir("gc-none");
        gc(&dir, 5, 5).await.expect("gc on empty project must not error");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

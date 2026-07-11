//! Phase D — the worktree manager (`create`/`list`/`merge`/`discard`/
//! `prune`) at `<root>/.cimp/worktrees/<slug>`, built on the §0.2 git
//! harness. "Isolated worktrees make parallel agents safe" (the milestone's
//! Feature 3) — every operation here runs through a PLAIN [`GitCtx::discover`]
//! (unlike Phase C's shadow repo), because a worktree genuinely IS a part of
//! the user's real repo: it shares the same object store and branch
//! namespace, just a second checkout. That also means this module is the one
//! place in Workbench where a bug has real consequences for the user's
//! branches — the two hard safety rules, upheld throughout:
//!
//!   - [`merge`] NEVER leaves the tree half-merged: any failure past the
//!     `git merge` invocation itself runs `git merge --abort` before
//!     returning the error (see that function's doc comment for the exact
//!     sequence).
//!   - [`discard`]/[`merge`] only ever act on a path this module itself
//!     created (`<root>/.cimp/worktrees/<slug>`, branch `cimp/<slug>`) —
//!     [`create`] namespaces both the directory and the branch so there's no
//!     ambiguity later about what's "ours" to remove.
//!
//! [`create`] also seeds `<root>/.git/info/exclude` with `.cimp/`
//! ([`git_exclude_cimp`]) — without it, a project that has never gitignored
//! `.cimp/` would see every worktree (and, if enabled, Phase C's shadow repo)
//! as untracked noise in `git status`, which would make [`merge`]'s
//! "the main tree must be clean" precondition fail on essentially every
//! project that hasn't specifically thought to exclude cImp's own directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

use super::git::{self, GitCtx};

/// Project-relative directory every cImp-managed worktree lives under.
const WORKTREES_DIR: &str = ".cimp/worktrees";

/// Branch namespace every cImp-created worktree branch lives under — keeps
/// them visually grouped in `git branch`/`git log --all` and lets [`list`]
/// recognize "ours" without depending on the meta sidecar alone.
const BRANCH_PREFIX: &str = "cimp/";

/// A long `git worktree add`/`merge`/`checkout` gets more room than the
/// harness's default 30s — still bounded, generous for a big first checkout.
const BULK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The `.cimp/worktrees/<slug>.meta.json` sidecar `create` writes and every
/// later operation (`list`'s ahead/behind, `merge`'s base-branch check)
/// reads back. Deliberately tiny — the worktree's own git state (branch,
/// commits) is the source of truth for everything else.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorktreeMeta {
    /// The branch this worktree was cut FROM (`rev-parse --abbrev-ref HEAD`
    /// in the main tree at `create` time) — both the ahead/behind comparison
    /// base and the branch `merge` requires the main tree to be on.
    base: String,
}

/// One row of the Worktrees section table (`workbench_worktrees`).
/// `has_live_tab` is NOT filled in by this module — it needs `Settings` (an
/// AI tab's `cwd` matching this worktree's path), which lives above the
/// git-only layer this module operates at. See
/// `WorkbenchService::worktrees` for where it's stitched in.
#[derive(Clone, Debug, Serialize)]
pub struct WorktreeInfo {
    pub slug: String,
    pub path: String,
    pub branch: String,
    pub base: String,
    /// Commits on `branch` not on `base` (`rev-list --left-right --count
    /// base...branch`'s right side).
    pub ahead: u32,
    /// Commits on `base` not on `branch` (that same call's left side) — i.e.
    /// how far `base` has moved on without this worktree.
    pub behind: u32,
    /// Filled in by [`super::WorkbenchService::worktrees`], `false` from
    /// this module's own [`list`].
    pub has_live_tab: bool,
}

/// The `workbench_worktree_merge` result: which kind of merge landed, and the
/// resulting commit — both purely informational (the Worktrees row re-fetches
/// its ahead/behind afterward regardless).
#[derive(Clone, Debug, Serialize)]
pub struct MergeReport {
    pub fast_forward: bool,
    pub commit: String,
}

// ── slug + path helpers ─────────────────────────────────────────────────

/// Validate a user-supplied worktree name into a path-safe, branch-safe
/// slug: ASCII letters/digits/`-`/`_` only, must start with a letter or
/// digit (rules out a leading `-` being read as a flag by anything that
/// later shells out with it), 1–60 characters. Deliberately conservative —
/// this string becomes both a directory name (`.cimp/worktrees/<slug>`) and
/// a branch name (`cimp/<slug>`), so no `/`, no `..`, no whitespace, no git
/// ref-name special characters at all.
pub fn sanitize_slug(input: &str) -> AppResult<String> {
    let s = input.trim();
    if s.is_empty() {
        return Err(AppError::Workbench("worktree name cannot be empty".to_string()));
    }
    if s.len() > 60 {
        return Err(AppError::Workbench("worktree name is too long (max 60 characters)".to_string()));
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err(AppError::Workbench(
            "worktree name must start with a letter or digit".to_string(),
        ));
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(AppError::Workbench(
            "worktree name can only contain letters, digits, '-', and '_'".to_string(),
        ));
    }
    // Windows can't create a directory named after a reserved DOS device, so
    // such a slug would pass every check above only to fail deep inside `git
    // worktree add` with an opaque OS error. Reject it up front (case-
    // insensitive) with a clear message. Harmless to enforce on all platforms.
    const RESERVED: &[&str] = &[
        "con", "prn", "aux", "nul",
        "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
        "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if RESERVED.contains(&s.to_ascii_lowercase().as_str()) {
        return Err(AppError::Workbench(format!(
            "'{s}' is a reserved device name on Windows — choose another worktree name"
        )));
    }
    Ok(s.to_string())
}

fn worktree_path(root: &Path, slug: &str) -> PathBuf {
    root.join(WORKTREES_DIR).join(slug)
}

/// Resolve `slug` to its worktree directory, validating it's actually a
/// known cImp worktree (a readable meta file) first. Used by
/// `WorkbenchService::worktree_run_checks` (D3's merge-readiness chip) to get
/// a `cwd` for `checks::run` without duplicating the sanitize+meta-lookup
/// dance that [`merge`]/[`discard`] already do.
pub fn resolve_path(root: &Path, slug: &str) -> AppResult<PathBuf> {
    let slug = sanitize_slug(slug)?;
    read_meta(root, &slug)?;
    Ok(worktree_path(root, &slug))
}

fn meta_path(root: &Path, slug: &str) -> PathBuf {
    root.join(WORKTREES_DIR).join(format!("{slug}.meta.json"))
}

fn branch_name(slug: &str) -> String {
    format!("{BRANCH_PREFIX}{slug}")
}

fn read_meta(root: &Path, slug: &str) -> AppResult<WorktreeMeta> {
    let path = meta_path(root, slug);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| AppError::Workbench(format!("unknown worktree '{slug}' (no meta file: {e})")))?;
    serde_json::from_str(&text)
        .map_err(|e| AppError::Workbench(format!("worktree '{slug}' has a corrupt meta file: {e}")))
}

fn write_meta(root: &Path, slug: &str, meta: &WorktreeMeta) -> AppResult<()> {
    let path = meta_path(root, slug);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::Workbench(format!("create {}: {e}", parent.display())))?;
    }
    let text = serde_json::to_string_pretty(meta)
        .map_err(|e| AppError::Workbench(format!("serialize worktree meta: {e}")))?;
    std::fs::write(&path, text).map_err(|e| AppError::Workbench(format!("write {}: {e}", path.display())))
}

fn to_forward_slash(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// Best-effort: add `.cimp/` to `<root>/.git/info/exclude` — the LOCAL,
/// never-committed exclude list, mirroring `tabs::config::git_exclude_opencode`'s
/// treatment of the generated OpenCode plugin dir. Without this, a project
/// that hasn't gitignored `.cimp/` itself would show a freshly created
/// worktree (and, if checkpoints are on, the Phase C shadow repo) as
/// untracked noise in `git status` — which [`merge`]'s "the main tree must be
/// clean" precondition depends on, so an un-excluded `.cimp/` would make
/// every merge fail with a spurious "uncommitted changes" error on a project
/// that has simply never thought to ignore cImp's own directory. Idempotent
/// (checks for the line first) and silently skipped when there's no
/// `.git/info` directory to write into.
fn git_exclude_cimp(root: &Path) {
    let info_dir = root.join(".git").join("info");
    if !info_dir.is_dir() {
        return;
    }
    let exclude = info_dir.join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".cimp/") {
        return;
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(".cimp/\n");
    let _ = std::fs::write(&exclude, next);
}

// ── create ───────────────────────────────────────────────────────────────

/// Refuse `root` when it's itself a linked worktree or a submodule (the
/// milestone's V1 scope cut — "detect and disable with an explanatory
/// tooltip"). Two independent probes:
///   - **linked worktree**: `--git-common-dir` (the shared object-store git
///     dir every worktree of a repo points at) differs from `--git-dir` (this
///     particular checkout's own git dir, `<common>/worktrees/<name>` for a
///     linked one). Equal for the main working tree, unequal for a linked
///     one.
///   - **submodule**: `--show-superproject-working-tree` prints a non-empty
///     path when `root` is checked out AS a submodule of some other repo.
///
/// Both probes degrade to "not nested" on a `git` failure (e.g. the command
/// isn't recognized by an ancient git) rather than blocking worktree creation
/// outright over an inconclusive check — [`create`]'s own `git worktree add`
/// call is the real backstop and will fail loudly if this project genuinely
/// can't host worktrees.
async fn refuse_if_nested(root: &Path) -> AppResult<()> {
    let ctx = GitCtx::discover(root);

    let common = git::run(&ctx, &["rev-parse", "--git-common-dir"], None).await?;
    let dir = git::run(&ctx, &["rev-parse", "--git-dir"], None).await?;
    if common.success() && dir.success() {
        let common_resolved = resolve_git_path(root, common.stdout.trim());
        let dir_resolved = resolve_git_path(root, dir.stdout.trim());
        if common_resolved != dir_resolved {
            return Err(AppError::Workbench(
                "this project is itself a linked git worktree — cImp's worktree manager only runs from a repo's main working tree.".to_string(),
            ));
        }
    }

    if let Ok(super_wt) = git::run(&ctx, &["rev-parse", "--show-superproject-working-tree"], None).await {
        if super_wt.success() && !super_wt.stdout.trim().is_empty() {
            return Err(AppError::Workbench(
                "this project is a git submodule — cImp's worktree manager isn't supported inside a submodule.".to_string(),
            ));
        }
    }

    Ok(())
}

/// Best-effort canonicalization of a `git rev-parse --git-dir`/
/// `--git-common-dir` answer (which may be relative to `root`, or absolute)
/// into something comparable. Falls back to the joined-but-uncanonicalized
/// path when the filesystem probe itself fails (e.g. a path git reports that
/// doesn't literally exist as typed on this filesystem) — still a meaningful
/// comparison in the common case, just not fully normalized.
fn resolve_git_path(root: &Path, raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    let joined = if p.is_absolute() { p } else { root.join(p) };
    std::fs::canonicalize(&joined).unwrap_or(joined)
}

/// Create a new worktree + branch for `slug`, cut from the main tree's
/// current `HEAD`. Sequence:
///   1. sanitize `slug` ([`sanitize_slug`]).
///   2. refuse if `root` is itself nested ([`refuse_if_nested`]).
///   3. refuse if `slug` is already in use (the target directory or its meta
///      file already exists).
///   4. record the base branch (`rev-parse --abbrev-ref HEAD`) — refused if
///      `HEAD` is detached (`"HEAD"` isn't a real branch to later merge back
///      into).
///   5. `git worktree add <path> -b cimp/<slug>` (from `HEAD`, git's default
///      when no start-point is given).
///   6. write the `.meta.json` sidecar recording the base branch.
///
/// A failure at step 5 or 6 leaves no half-created worktree behind: git's own
/// `worktree add` is atomic (it either fully succeeds or creates nothing),
/// and a meta-write failure after a successful `add` is surfaced as an error
/// but the worktree itself is left in place (a `list`/`merge` reading a
/// missing meta file treats it as "not ours" rather than crashing — the user
/// can `discard` a partially-set-up worktree via a plain `git worktree
/// remove` if this rare case is ever hit).
pub async fn create(root: &Path, slug: &str) -> AppResult<PathBuf> {
    let slug = sanitize_slug(slug)?;
    refuse_if_nested(root).await?;

    let path = worktree_path(root, &slug);
    if path.exists() || meta_path(root, &slug).exists() {
        return Err(AppError::Workbench(format!(
            "a worktree named '{slug}' already exists"
        )));
    }

    git_exclude_cimp(root);

    let ctx = GitCtx::discover(root);
    let head = git::run(&ctx, &["rev-parse", "--abbrev-ref", "HEAD"], None).await?;
    if !head.success() {
        return Err(AppError::Workbench(format!(
            "couldn't determine the current branch: {}",
            head.stderr.trim()
        )));
    }
    let base = head.stdout.trim().to_string();
    if base.is_empty() || base == "HEAD" {
        return Err(AppError::Workbench(
            "cannot create a worktree while HEAD is detached — checkout a branch first.".to_string(),
        ));
    }

    // A leftover `cimp/<slug>` branch (a prior create whose best-effort
    // rollback couldn't delete it, or a user-made branch in our namespace)
    // would make `worktree add -b` fail with an opaque "branch already
    // exists" — surface it as instructions instead, since dir+meta absence
    // alone doesn't prove the slug is free.
    let branch = branch_name(&slug);
    let existing = git::run(&ctx, &["rev-parse", "-q", "--verify", &format!("refs/heads/{branch}")], None).await?;
    if existing.success() {
        return Err(AppError::Workbench(format!(
            "branch '{branch}' already exists (likely left over from an earlier worktree) — delete it with `git branch -D {branch}` or pick a different name."
        )));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::Workbench(format!("create {}: {e}", parent.display())))?;
    }

    let path_str = path.to_string_lossy().into_owned();
    let add = git::run(&ctx, &["worktree", "add", &path_str, "-b", &branch], Some(BULK_TIMEOUT)).await?;
    if !add.success() {
        return Err(AppError::Workbench(format!(
            "git worktree add failed: {}",
            add.stderr.trim()
        )));
    }

    if let Err(e) = write_meta(root, &slug, &WorktreeMeta { base }) {
        // Roll back the worktree + branch we just created. Without the meta
        // file `list` skips this worktree and `discard` refuses it, so a failed
        // meta write would otherwise wedge the slug as an orphan only a manual
        // `git worktree remove` could clear. Best-effort cleanup; the original
        // write error is what we surface.
        let _ = git::run(&ctx, &["worktree", "remove", "--force", &path_str], Some(BULK_TIMEOUT)).await;
        let _ = git::run(&ctx, &["branch", "-D", &branch], None).await;
        return Err(e);
    }
    git::invalidate_is_repo_cache(&path);
    Ok(path)
}

// ── list ─────────────────────────────────────────────────────────────────

/// One parsed block of `git worktree list --porcelain` output.
struct RawWorktree {
    path: PathBuf,
    branch: Option<String>,
}

/// Parse `git worktree list --porcelain`: records are separated by a blank
/// line, each a run of `key value` lines (`worktree <path>`, `HEAD <sha>`,
/// `branch refs/heads/<name>`, or bare `detached`/`bare` flags with no
/// value). Only `worktree`/`branch` are needed here.
fn parse_worktree_porcelain(raw: &str) -> Vec<RawWorktree> {
    let mut out = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;

    let flush = |path: &mut Option<PathBuf>, branch: &mut Option<String>, out: &mut Vec<RawWorktree>| {
        if let Some(p) = path.take() {
            out.push(RawWorktree { path: p, branch: branch.take() });
        }
        *branch = None;
    };

    for line in raw.lines() {
        if line.is_empty() {
            flush(&mut path, &mut branch, &mut out);
            continue;
        }
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, &mut out);
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = b.strip_prefix("refs/heads/").map(str::to_string).or_else(|| Some(b.to_string()));
        }
    }
    flush(&mut path, &mut branch, &mut out);
    out
}

/// `git rev-list --left-right --count <base>...<branch>` → `(ahead, behind)`
/// where `ahead` = commits on `branch` not on `base`, `behind` = commits on
/// `base` not on `branch`. Degrades to `(0, 0)` on any git failure (a
/// deleted/renamed base branch, say) rather than failing the whole
/// [`list`] call over one row's stat.
async fn ahead_behind(ctx: &GitCtx, base: &str, branch: &str) -> (u32, u32) {
    let spec = format!("{base}...{branch}");
    let Ok(out) = git::run(ctx, &["rev-list", "--left-right", "--count", &spec], None).await else {
        return (0, 0);
    };
    if !out.success() {
        return (0, 0);
    }
    let mut parts = out.stdout.split_whitespace();
    let behind: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let ahead: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (ahead, behind)
}

/// Every cImp-managed worktree of `root`'s repo — parsed from `git worktree
/// list --porcelain`, filtered to entries whose path sits under
/// `<root>/.cimp/worktrees/` (the only place [`create`] ever puts one) with a
/// readable `.meta.json` sidecar. A worktree directory that doesn't parse as
/// `git worktree list` output (removed out from under git, say) or has no
/// meta file is silently skipped — this is a "what does cImp know about"
/// listing, not a full-repo audit. Returns an empty list (not an error) when
/// `root` isn't a git repo or has no worktrees at all.
pub async fn list(root: &Path) -> AppResult<Vec<WorktreeInfo>> {
    if !git::is_repo(root).await {
        return Ok(Vec::new());
    }
    let ctx = GitCtx::discover(root);
    let out = git::run(&ctx, &["worktree", "list", "--porcelain"], None).await?;
    if !out.success() {
        return Err(AppError::Workbench(format!(
            "git worktree list failed: {}",
            out.stderr.trim()
        )));
    }
    // Canonicalize both sides: `git worktree list` reports the resolved real
    // path (symlink/junction/8.3/`\\?\`-normalized), so comparing it against an
    // as-passed `root.join(...)` would spuriously drop every cImp worktree when
    // `root` reached us via a symlink, junction, or short path — making them
    // invisible and un-mergeable/un-discardable in the UI.
    let worktrees_root = git::canonical_path(&root.join(WORKTREES_DIR));
    let mut infos = Vec::new();
    for raw in parse_worktree_porcelain(&out.stdout) {
        let Some(parent) = raw.path.parent() else { continue };
        if git::canonical_path(parent) != worktrees_root {
            continue; // not one of ours
        }
        let Some(slug) = raw.path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(branch) = &raw.branch else { continue }; // detached — never ours
        if branch != &branch_name(slug) {
            continue; // a directory here that isn't the branch we'd have created
        }
        let Ok(meta) = read_meta(root, slug) else { continue }; // not cImp-managed (no meta)
        let (ahead, behind) = ahead_behind(&ctx, &meta.base, branch).await;
        infos.push(WorktreeInfo {
            slug: slug.to_string(),
            path: to_forward_slash(&raw.path),
            branch: branch.clone(),
            base: meta.base,
            ahead,
            behind,
            has_live_tab: false,
        });
    }
    infos.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(infos)
}

// ── merge ────────────────────────────────────────────────────────────────

/// Merge `cimp/<slug>` into the branch it was cut from, run entirely in
/// `root`'s MAIN working tree (never the linked worktree itself). Every
/// precondition below is checked BEFORE the `git merge` call runs, and any
/// failure of `git merge` itself is followed immediately by `git merge
/// --abort` — see the milestone's hard safety rule in this module's doc
/// comment: a merge attempt NEVER leaves the main tree half-merged.
///
/// Preconditions (each a plain typed error, not a `git` error dump, so the
/// UI can show instructions):
///   - `slug` must resolve to a known cImp worktree ([`read_meta`]).
///   - the main tree must have no uncommitted changes to TRACKED files
///     (`git status --porcelain --untracked-files=no` empty) — a `git merge`
///     into a dirty tree can itself fail confusingly or, worse, partially
///     apply; refused outright instead. Untracked files are fine: git only
///     objects when the merge would overwrite one, and protects it.
///   - the main tree must be ON the recorded base branch — merging
///     `cimp/<slug>` into whatever branch happens to be checked out would
///     silently merge it into the wrong place.
///
/// On success, `git merge --no-edit cimp/<slug>` either fast-forwards (no new
/// commit; git prints `Fast-forward` to stdout, which is how
/// [`MergeReport::fast_forward`] is determined) or creates a merge commit
/// (git's default `-m` message via `--no-edit`, so no interactive editor ever
/// opens). Both are reported; V1 doesn't offer a merge-strategy choice.
pub async fn merge(root: &Path, slug: &str) -> AppResult<MergeReport> {
    let slug = sanitize_slug(slug)?;
    let meta = read_meta(root, &slug)?;
    let ctx = GitCtx::discover(root);

    let current = git::run(&ctx, &["rev-parse", "--abbrev-ref", "HEAD"], None).await?;
    if !current.success() {
        return Err(AppError::Workbench(format!(
            "couldn't determine the main tree's current branch: {}",
            current.stderr.trim()
        )));
    }
    let current_branch = current.stdout.trim();
    if current_branch != meta.base {
        return Err(AppError::Workbench(format!(
            "the main working tree is on '{current_branch}', not '{}' (the branch this worktree was cut from) — checkout '{}' in the main tree first, then merge again.",
            meta.base, meta.base
        )));
    }

    // `--untracked-files=no`: untracked files don't make a merge unsafe (git
    // itself refuses only when the merge would overwrite one, and that refusal
    // surfaces through the conflict path below) — without it, one stray build
    // artifact or agent-written scratch file in the main tree would block
    // every merge with a misleading "uncommitted changes" error.
    let status = git::run(&ctx, &["status", "--porcelain", "--untracked-files=no"], None).await?;
    if !status.success() {
        return Err(AppError::Workbench(format!(
            "couldn't check the main tree's status: {}",
            status.stderr.trim()
        )));
    }
    if !status.stdout.trim().is_empty() {
        return Err(AppError::Workbench(
            "the main working tree has uncommitted changes — commit or stash them before merging a worktree back in.".to_string(),
        ));
    }

    let branch = branch_name(&slug);
    let merge_out = git::run(&ctx, &["merge", "--no-edit", &branch], Some(BULK_TIMEOUT)).await?;
    if !merge_out.success() {
        // Did the merge actually start? If there's no `MERGE_HEAD`, git failed
        // BEFORE touching the tree — a deleted/renamed worktree branch, "not
        // something we can merge", an unborn base — so the main tree is
        // unchanged. Report a plain error rather than the alarming
        // "half-merged" one below: in this case `git merge --abort` legitimately
        // fails with "no merge to abort", which must NOT be read as a corrupt
        // tree.
        let merge_started = matches!(
            git::run(&ctx, &["rev-parse", "-q", "--verify", "MERGE_HEAD"], None).await,
            Ok(out) if out.success()
        );
        if !merge_started {
            return Err(AppError::Workbench(format!(
                "couldn't merge '{branch}' — the merge did not start (the worktree branch may be missing or there is nothing to merge). The main working tree is unchanged. git said: {}",
                merge_out.stderr.trim().lines().next().unwrap_or(merge_out.stderr.trim())
            )));
        }
        // A merge is genuinely in progress (conflicts). Hard safety rule: never
        // CLAIM the tree is clean when it isn't. `git merge --abort`'s own
        // result must be checked (FIX 2 / V13 code review) — a failed abort
        // (e.g. it couldn't reset the index) means the main tree's state is
        // genuinely unknown, not "aborted and unchanged". Reporting the
        // reassuring message in that case would tell the user it's safe to keep
        // going when it might not be.
        let abort_out = git::run(&ctx, &["merge", "--abort"], None).await;
        let abort_ok = matches!(&abort_out, Ok(out) if out.success());
        if !abort_ok {
            let abort_detail = match &abort_out {
                Ok(out) => out.stderr.trim().to_string(),
                Err(e) => e.to_string(),
            };
            return Err(AppError::WorktreeMergeUnclean(format!(
                "merge of '{branch}' failed AND the follow-up 'git merge --abort' also failed — the main working tree may be left half-merged. Resolve it manually in a shell (check `git status`; you likely need to run `git merge --abort` yourself, or finish resolving conflicts and commit). Original merge error: {}. Abort error: {}",
                merge_out.stderr.trim().lines().next().unwrap_or(merge_out.stderr.trim()),
                abort_detail.lines().next().unwrap_or(&abort_detail),
            )));
        }
        return Err(AppError::Workbench(format!(
            "merge conflict — the merge was aborted and the main working tree is unchanged. Resolve manually in a shell (worktree branch: '{branch}'), or edit the worktree and try again. git said: {}",
            merge_out.stderr.trim().lines().next().unwrap_or(merge_out.stderr.trim())
        )));
    }

    let fast_forward = merge_out.stdout.contains("Fast-forward");
    let head = git::run(&ctx, &["rev-parse", "HEAD"], None).await?;
    let commit = if head.success() { head.stdout.trim().to_string() } else { String::new() };

    Ok(MergeReport { fast_forward, commit })
}

// ── diff (D3's Diff row action) ─────────────────────────────────────────

/// D3's per-row **Diff** action: `git diff <base>...cimp/<slug>` (the
/// three-dot "what did this branch do since it forked from base" form, not
/// a plain two-dot diff — matches the same left/right-of-fork semantics
/// [`ahead_behind`] already uses) parsed through [`super::diff::parse_unified`]
/// — the exact same parser Phase B's live diff pane uses, so the frontend can
/// render this with the same file/hunk shapes. Read-only: this is a diff
/// between two commits, not a working-tree diff, so there is no revert
/// action here (unlike Phase B's `DiffView`).
pub async fn diff_against_base(root: &Path, slug: &str, context: u32) -> AppResult<Vec<super::diff::FileDiff>> {
    let slug = sanitize_slug(slug)?;
    let meta = read_meta(root, &slug)?;
    let ctx = GitCtx::discover(root);
    let branch = branch_name(&slug);
    let spec = format!("{}...{}", meta.base, branch);
    let unified = format!("--unified={}", context.min(super::diff::MAX_CONTEXT));
    let out = git::run(&ctx, &["diff", "--no-color", "--no-renames", &unified, &spec], Some(BULK_TIMEOUT)).await?;
    if !out.success() {
        return Err(AppError::Workbench(format!(
            "git diff {spec} failed: {}",
            out.stderr.trim()
        )));
    }
    Ok(super::diff::parse_unified(&out.stdout))
}

// ── discard ──────────────────────────────────────────────────────────────

/// Remove worktree `slug` and its branch. Only ever acts on a path/branch
/// this module itself would have created (`<root>/.cimp/worktrees/<slug>`,
/// `cimp/<slug>`) — refuses up front if `slug` doesn't resolve to a known
/// meta file, so this can never be pointed at an arbitrary directory or
/// branch. Double-confirmation is the UI's job (the milestone's D3); this is
/// the unconditional backend action once the user has confirmed.
///
/// `git worktree remove --force` (force: the worktree may have uncommitted
/// changes the user has already decided to discard — that's the entire point
/// of "Discard") then `git branch -D` (force-delete: the branch's commits may
/// not be merged anywhere, same reasoning), then the meta sidecar. Each step
/// is attempted even if an earlier one fails partially (e.g. the directory
/// was already deleted out-of-band) so a discard always converges toward
/// "gone", rather than leaving an orphaned branch because the directory
/// removal already succeeded on a previous attempt.
pub async fn discard(root: &Path, slug: &str) -> AppResult<()> {
    let slug = sanitize_slug(slug)?;
    // Confirms this is actually one of ours before touching anything.
    let _meta = read_meta(root, &slug)?;

    let ctx = GitCtx::discover(root);
    let path = worktree_path(root, &slug);
    let path_str = path.to_string_lossy().into_owned();
    let branch = branch_name(&slug);

    let remove = git::run(&ctx, &["worktree", "remove", "--force", &path_str], Some(BULK_TIMEOUT)).await?;
    if !remove.success() {
        // If the directory is simply already gone, `worktree remove` fails
        // but there's nothing left to clean up on that front — `prune` (or
        // this same call, run again) reconciles git's own bookkeeping.
        // Anything else is a genuine failure worth surfacing.
        if path.exists() {
            return Err(AppError::Workbench(format!(
                "git worktree remove failed: {}",
                remove.stderr.trim()
            )));
        }
        let _ = git::run(&ctx, &["worktree", "prune"], None).await;
    }

    // Best-effort: an already-deleted branch (e.g. a retried discard) is not
    // an error.
    let _ = git::run(&ctx, &["branch", "-D", &branch], None).await;

    let meta = meta_path(root, &slug);
    let _ = std::fs::remove_file(&meta);

    git::invalidate_is_repo_cache(&path);
    Ok(())
}

// ── prune ────────────────────────────────────────────────────────────────

/// `git worktree prune` — reconciles git's worktree bookkeeping with reality
/// (a worktree directory deleted out-of-band, e.g. by the user in a file
/// manager). Run at app start (see `main.rs`'s Workbench-service setup) and
/// safe to call at any other time too; a no-op when `root` isn't a git repo
/// or there's nothing to prune.
pub async fn prune(root: &Path) -> AppResult<()> {
    if !git::is_repo(root).await {
        return Ok(());
    }
    let ctx = GitCtx::discover(root);
    let out = git::run(&ctx, &["worktree", "prune"], None).await?;
    if !out.success() {
        return Err(AppError::Workbench(format!(
            "git worktree prune failed: {}",
            out.stderr.trim()
        )));
    }
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

    fn user_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-wt-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "user@example.com"]);
        git(&dir, &["config", "user.name", "User"]);
        git(&dir, &["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    // ── sanitize_slug ────────────────────────────────────────────────────

    #[test]
    fn sanitize_slug_accepts_plain_names() {
        assert_eq!(sanitize_slug("fix-login-bug").unwrap(), "fix-login-bug");
        assert_eq!(sanitize_slug("  task_2  ").unwrap(), "task_2");
    }

    #[test]
    fn sanitize_slug_rejects_path_traversal_and_separators() {
        assert!(sanitize_slug("../escape").is_err());
        assert!(sanitize_slug("a/b").is_err());
        assert!(sanitize_slug("a\\b").is_err());
        assert!(sanitize_slug("").is_err());
        assert!(sanitize_slug("   ").is_err());
        assert!(sanitize_slug("-leading-dash").is_err());
    }

    #[test]
    fn sanitize_slug_rejects_windows_reserved_names() {
        for name in ["con", "CON", "nul", "Aux", "prn", "com1", "COM9", "lpt1", "LPT9"] {
            assert!(sanitize_slug(name).is_err(), "{name} should be rejected as reserved");
        }
        // A reserved stem with extra characters is a normal, allowed name.
        assert!(sanitize_slug("console").is_ok());
        assert!(sanitize_slug("com10").is_ok());
        assert!(sanitize_slug("com0").is_ok());
    }

    // ── create / list / ahead-behind / meta ─────────────────────────────

    #[tokio::test]
    async fn create_then_list_records_base_and_ahead_behind() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("create-list");

        let wt_path = create(&dir, "my-task").await.expect("create");
        assert!(wt_path.exists());
        assert_eq!(wt_path, dir.join(".cimp/worktrees/my-task"));

        let meta: WorktreeMeta =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".cimp/worktrees/my-task.meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.base, "main");

        // One commit ahead in the worktree, zero behind.
        std::fs::write(wt_path.join("b.txt"), "two\n").unwrap();
        git(&wt_path, &["add", "b.txt"]);
        git(&wt_path, &["commit", "-q", "-m", "worktree change"]);

        let infos = list(&dir).await.expect("list");
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].slug, "my-task");
        assert_eq!(infos[0].branch, "cimp/my-task");
        assert_eq!(infos[0].base, "main");
        assert_eq!(infos[0].ahead, 1);
        assert_eq!(infos[0].behind, 0);
        assert!(!infos[0].has_live_tab);

        // Advance main so the worktree is also behind.
        std::fs::write(dir.join("c.txt"), "three\n").unwrap();
        git(&dir, &["add", "c.txt"]);
        git(&dir, &["commit", "-q", "-m", "main advances"]);

        let infos2 = list(&dir).await.expect("list 2");
        assert_eq!(infos2[0].ahead, 1);
        assert_eq!(infos2[0].behind, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn create_refuses_duplicate_slug() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("dup");
        create(&dir, "task").await.expect("create 1");
        let err = create(&dir, "task").await.unwrap_err();
        assert!(matches!(err, AppError::Workbench(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn create_refuses_when_head_is_detached() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("detached");
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
        git(&dir, &["checkout", "-q", &sha]);

        let err = create(&dir, "task").await.unwrap_err();
        assert!(matches!(err, AppError::Workbench(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn create_refuses_inside_a_linked_worktree() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("nested");
        let wt_path = create(&dir, "outer").await.expect("create outer");

        // Attempting to create ANOTHER worktree FROM the outer worktree
        // itself must be refused (root == the linked worktree, not the main
        // tree).
        let err = create(&wt_path, "inner").await.unwrap_err();
        assert!(matches!(err, AppError::Workbench(_)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn create_excludes_cimp_from_the_main_trees_git_status() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        // No `.gitignore` for `.cimp/` in this fixture (unlike `shadow.rs`'s
        // own tests) — this is the realistic "user never thought about it"
        // case `git_exclude_cimp` exists to cover.
        let dir = user_repo("exclude");
        create(&dir, "task").await.expect("create");

        let status = git::run(&GitCtx::discover(&dir), &["status", "--porcelain"], None).await.unwrap();
        assert!(
            status.stdout.trim().is_empty(),
            "the main tree must stay clean after creating a worktree, got: {:?}",
            status.stdout
        );

        // Idempotent: creating a second worktree doesn't duplicate the line.
        create(&dir, "task2").await.expect("create 2");
        let exclude = std::fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(".cimp/").count(), 1, "the exclude line must not be duplicated");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── merge: fast-forward, merge-commit, conflict-abort ───────────────

    #[tokio::test]
    async fn merge_fast_forwards_when_main_has_not_moved() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("merge-ff");
        let wt_path = create(&dir, "feature").await.expect("create");
        std::fs::write(wt_path.join("feat.txt"), "feature work\n").unwrap();
        git(&wt_path, &["add", "feat.txt"]);
        git(&wt_path, &["commit", "-q", "-m", "feature commit"]);

        let report = merge(&dir, "feature").await.expect("merge");
        assert!(report.fast_forward, "expected a fast-forward merge");
        assert!(dir.join("feat.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn merge_creates_a_merge_commit_when_main_has_diverged() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("merge-commit");
        let wt_path = create(&dir, "feature").await.expect("create");
        std::fs::write(wt_path.join("feat.txt"), "feature work\n").unwrap();
        git(&wt_path, &["add", "feat.txt"]);
        git(&wt_path, &["commit", "-q", "-m", "feature commit"]);

        // Diverge main with an unrelated, non-conflicting change.
        std::fs::write(dir.join("main_only.txt"), "main work\n").unwrap();
        git(&dir, &["add", "main_only.txt"]);
        git(&dir, &["commit", "-q", "-m", "main commit"]);

        let report = merge(&dir, "feature").await.expect("merge");
        assert!(!report.fast_forward, "expected a real merge commit");
        assert!(dir.join("feat.txt").exists());
        assert!(dir.join("main_only.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn diff_against_base_shows_only_the_worktrees_own_commits() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("wt-diff");
        let wt_path = create(&dir, "feature").await.expect("create");
        std::fs::write(wt_path.join("feat.txt"), "feature work\n").unwrap();
        git(&wt_path, &["add", "feat.txt"]);
        git(&wt_path, &["commit", "-q", "-m", "feature commit"]);

        // Main also advances — diff_against_base must NOT show main's own
        // unrelated change, only what the worktree's branch did since it
        // forked (the three-dot semantics).
        std::fs::write(dir.join("main_only.txt"), "main work\n").unwrap();
        git(&dir, &["add", "main_only.txt"]);
        git(&dir, &["commit", "-q", "-m", "main commit"]);

        let files = diff_against_base(&dir, "feature", 3).await.expect("diff_against_base");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "feat.txt");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn merge_conflict_aborts_cleanly_and_leaves_main_tree_untouched() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("merge-conflict");
        let wt_path = create(&dir, "feature").await.expect("create");
        std::fs::write(wt_path.join("a.txt"), "worktree edit\n").unwrap();
        git(&wt_path, &["commit", "-qam", "worktree edits a.txt"]);

        // Conflicting edit to the SAME file on main.
        std::fs::write(dir.join("a.txt"), "main edit\n").unwrap();
        git(&dir, &["commit", "-qam", "main edits a.txt"]);

        let head_before = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&dir)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap();

        let err = merge(&dir, "feature").await.unwrap_err();
        assert!(matches!(err, AppError::Workbench(_)));

        // The hard safety rule: no MERGE_HEAD left behind, HEAD unchanged,
        // status clean — a genuinely half-merged tree would fail all three.
        let merge_head = std::process::Command::new("git")
            .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(!merge_head.status.success(), "MERGE_HEAD must not exist after an aborted merge");

        let head_after = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&dir)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap();
        assert_eq!(head_before, head_after, "HEAD must be unchanged after an aborted merge");

        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&dir)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap();
        assert!(status.is_empty(), "main tree must be clean after an aborted merge, got: {status:?}");

        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "main edit\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn merge_refuses_when_main_tree_is_dirty() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("merge-dirty");
        create(&dir, "feature").await.expect("create");
        // Modify a TRACKED file (`a.txt` is committed by `user_repo`) —
        // untracked files deliberately don't count as dirty (see the
        // untracked test below).
        std::fs::write(dir.join("a.txt"), "uncommitted edit\n").unwrap();

        let err = merge(&dir, "feature").await.unwrap_err();
        assert!(matches!(err, AppError::Workbench(_)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Untracked files in the main tree must NOT block a merge — `git merge`
    /// only objects when it would overwrite one (and then protects it). A
    /// build artifact or agent scratch file used to refuse every merge with a
    /// misleading "uncommitted changes" error.
    #[tokio::test]
    async fn merge_proceeds_when_main_tree_has_only_untracked_files() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("merge-untracked");
        let wt_path = create(&dir, "feature").await.expect("create");
        std::fs::write(wt_path.join("feat.txt"), "feature work\n").unwrap();
        git(&wt_path, &["add", "feat.txt"]);
        git(&wt_path, &["commit", "-q", "-m", "feature commit"]);

        // An untracked scratch file in the main tree, unrelated to the merge.
        std::fs::write(dir.join("scratch.log"), "build noise\n").unwrap();

        let report = merge(&dir, "feature").await.expect("merge with untracked file present");
        assert!(report.fast_forward);
        assert!(dir.join("feat.txt").exists());
        assert!(dir.join("scratch.log").exists(), "the untracked file must be left alone");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A leftover `cimp/<slug>` branch (failed rollback, user-made) must be
    /// reported as a clear typed error, not git's opaque "branch already
    /// exists" out of `worktree add`.
    #[tokio::test]
    async fn create_refuses_when_the_branch_already_exists() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("branch-collision");
        git(&dir, &["branch", "cimp/feature"]);

        let err = create(&dir, "feature").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cimp/feature"), "error should name the branch: {msg}");
        assert!(msg.contains("already exists"), "error should say it exists: {msg}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn merge_refuses_when_main_tree_is_on_the_wrong_branch() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("merge-wrong-branch");
        create(&dir, "feature").await.expect("create");
        git(&dir, &["checkout", "-qb", "other"]);

        let err = merge(&dir, "feature").await.unwrap_err();
        assert!(matches!(err, AppError::Workbench(_)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── discard ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn discard_removes_worktree_dir_branch_and_meta() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("discard");
        let wt_path = create(&dir, "throwaway").await.expect("create");
        assert!(wt_path.exists());

        discard(&dir, "throwaway").await.expect("discard");

        assert!(!wt_path.exists(), "worktree directory must be removed");
        assert!(!meta_path(&dir, "throwaway").exists(), "meta file must be removed");
        let branch = git::run(&GitCtx::discover(&dir), &["branch", "--list", "cimp/throwaway"], None)
            .await
            .unwrap();
        assert!(branch.stdout.trim().is_empty(), "branch must be deleted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn discard_refuses_a_slug_with_no_meta_file() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("discard-unknown");
        let err = discard(&dir, "never-created").await.unwrap_err();
        assert!(matches!(err, AppError::Workbench(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── prune ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn prune_reconciles_a_manually_deleted_worktree_dir() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = user_repo("prune");
        let wt_path = create(&dir, "gone").await.expect("create");
        // Simulate the user deleting the worktree directory out-of-band
        // (file manager, `rm -rf`, ...) without telling git.
        std::fs::remove_dir_all(&wt_path).unwrap();

        prune(&dir).await.expect("prune");

        let out = git::run(&GitCtx::discover(&dir), &["worktree", "list", "--porcelain"], None).await.unwrap();
        assert!(!out.stdout.contains("gone"), "pruned worktree must no longer be listed");

        let _ = std::fs::remove_file(meta_path(&dir, "gone"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prune_on_non_repo_is_a_no_op() {
        let dir = std::env::temp_dir().join(format!("wb-wt-nogit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        prune(&dir).await.expect("prune on non-repo must not error");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

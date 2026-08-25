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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::error::{AppError, AppResult};

use super::git::{self, GitCtx};

/// Per-root serialization for shadow-repo MUTATIONS. `snapshot`/`restore`/`gc`/
/// `diff_vs_now` all touch the single shared shadow index and the `cp-<seq>`
/// tag space; run two concurrently on one root and they race — both can
/// allocate the same `cp-N` (the loser fails "tag already exists", and if that
/// loser is a restore's pre-restore safety snapshot the whole restore aborts),
/// or contend on `index.lock`. git's own locking keeps the repo from
/// corrupting, but turns the collision into a spurious hard failure. Holding
/// one async lock per root for the duration serializes them cleanly (and closes
/// the restore snapshot→checkout window against a concurrent snapshot). Keyed
/// by [`git::canonical_path`] so two spellings of one root share the lock.
static SHADOW_LOCKS: StdMutex<Option<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    StdMutex::new(None);

/// The per-root shadow lock (created on first use). The registry mutex is held
/// only long enough to clone the `Arc` — never across the actual git work.
fn shadow_lock(root: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let key = git::canonical_path(root);
    let mut guard = SHADOW_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

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

/// RAII cleanup for [`diff_vs_now`]'s scratch index: removes the temp index
/// file (and any `<index>.lock` git may have left) when it goes out of scope,
/// on both the success and the error path.
struct ScratchIndex(PathBuf);

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        // git's lock for a custom index file is `<indexfile>.lock` — append the
        // suffix to the full path (NOT `with_extension`, which would rewrite the
        // real `index.lock`).
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.lock", self.0.display())));
    }
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
    /// **V33 Phase F** — taken immediately before a filesystem-mutating TOOL
    /// call, attributed to that exact call via [`Origin::source`]. Composes
    /// with `Prompt`/`Burst` rather than replacing either: a prompt checkpoint
    /// answers "what did this turn start from", a tool checkpoint answers
    /// "what did *this edit* start from".
    ///
    /// An older build reading a `tool` checkpoint sees `Manual` (see
    /// [`Trigger::parse`]) — the accepted degradation, not a bug.
    Tool,
}

impl Trigger {
    fn as_str(self) -> &'static str {
        match self {
            Trigger::Prompt => "prompt",
            Trigger::Burst => "burst",
            Trigger::Manual => "manual",
            Trigger::PreRestore => "pre-restore",
            Trigger::Tool => "tool",
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
            "tool" => Trigger::Tool,
            _ => Trigger::Manual,
        }
    }
}

/// The stable, public handle for one checkpoint — a shadow-repo tag name
/// (`"cp-<seq>"`). Frontend/IPC code treats this as opaque; only this module
/// resolves it to a commit sha.
pub type CheckpointId = String;

/// The trailer value that means "this checkpoint has no such identity" —
/// written by [`trailer_identity`], read back as `None` by
/// [`identity_field`].
///
/// A placeholder rather than an empty value because git only recognizes a
/// commit's last paragraph as a trailer block when its lines actually parse as
/// trailers; `Session:` with nothing after it is exactly the shape that makes
/// that fragile, and a trailer block that stops being recognized takes the
/// *other* trailers down with it. `Agent: -` has used this convention since
/// Phase C — the two new fields simply share it.
const IDENTITY_ABSENT: &str = "-";

/// Length ceiling for an identity trailer value. A conversation id is a UUID
/// (36 chars) and a tab id is a short slug; anything past this is not one, and
/// the trailer block is not a place to store an unbounded caller-supplied
/// string.
const MAX_IDENTITY_LEN: usize = 200;

/// Who a checkpoint belongs to: the conversation identity a Timeline row needs
/// to be joined to a `Screen::Contamination` activity row.
///
/// **A struct, not three adjacent `Option<String>` parameters.** All three are
/// optional strings of the same type sitting next to each other, so a call site
/// that transposed `session` and `tab` would compile silently and mis-attribute
/// every checkpoint it made — the same reasoning that made
/// `tabs::config::OpencodePluginFlags` and `offload::toolclass::CallGuards`
/// structs.
///
/// `Default` is "no identity at all", which is the honest answer for the
/// triggers that have no conversation behind them: a burst-triggered snapshot,
/// a manual "Checkpoint now" click, and [`restore`]'s invariant-C pre-restore
/// safety snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Origin {
    /// The harness NAME (`"claude"` / `"opencode"`) — shared by every tab of
    /// that kind, which is exactly why it is not sufficient on its own.
    pub agent: Option<String>,
    /// The conversation the prompt belongs to (the harness's own session id).
    pub session: Option<String>,
    /// The cImp TAB id. The one field that tells two same-agent tabs on one
    /// project root apart.
    pub tab: Option<String>,
    /// **V33 Phase F** — the TOOL CALL this checkpoint was taken immediately
    /// before, as `harness:tool_name` (`claude:Bash`, `offload:run_command`,
    /// `opencode:edit`). `None` for every other trigger.
    ///
    /// **A NAMED field, set through [`Origin::with_source`] — deliberately NOT
    /// a fourth positional argument to [`Origin::new`].** This struct exists
    /// because four optional same-typed strings in a row is exactly the shape a
    /// call site transposes silently (see the type doc above); adding a fourth
    /// positional would have made the hazard worse, not the same.
    pub source: Option<String>,
}

impl Origin {
    /// Build an origin, normalizing each field to `None` when it is
    /// absent/blank so "" and "   " can never read as an identity. Values are
    /// *not* sanitized here — that happens at the write boundary
    /// ([`trailer_identity`]), which is where the framing they could break
    /// lives.
    ///
    /// Still THREE arguments after V33 Phase F: `source` rides
    /// [`Self::with_source`] instead — see that field's doc.
    pub fn new(agent: Option<String>, session: Option<String>, tab: Option<String>) -> Self {
        Self {
            agent: norm_identity(agent),
            session: norm_identity(session),
            tab: norm_identity(tab),
            source: None,
        }
    }

    /// V33 Phase F: attach the `harness:tool_name` this checkpoint was taken
    /// before. Normalized on the same terms as the other three fields, so a
    /// blank source reads as "no tool behind this checkpoint" rather than as an
    /// empty tool name.
    pub fn with_source(mut self, source: Option<String>) -> Self {
        self.source = norm_identity(source);
        self
    }
}

/// Blank/absent ⇒ `None`, shared by [`Origin::new`] and
/// [`Origin::with_source`] so the four fields can never normalize differently.
fn norm_identity(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

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
    /// The harness name. `None` for a checkpoint no conversation triggered,
    /// AND for every checkpoint written before this field existed.
    pub agent: Option<String>,
    pub files_changed: u32,
    /// The conversation this checkpoint was taken for ([`Origin::session`]).
    /// `None` for a burst/manual/pre-restore checkpoint and for any checkpoint
    /// written before this field existed (see [`list`]).
    pub session: Option<String>,
    /// The cImp tab this checkpoint was taken for ([`Origin::tab`]) — what
    /// makes two same-agent tabs on one root distinguishable in the Timeline.
    /// `None` on the same terms as `session`.
    pub tab: Option<String>,
    /// **V33 Phase F** — the tool call this checkpoint was taken immediately
    /// before, `harness:tool_name` ([`Origin::source`]). `None` for every
    /// non-`Tool` trigger.
    ///
    /// **The frontend distinguishes two absences and this field must preserve
    /// that.** `Checkpoint.source?: string | null` in `src/lib/workbench.ts`:
    /// `undefined` means "the backend predates this field", `null` means "no
    /// tool behind this checkpoint". This struct derives plain `Serialize` with
    /// no `skip_serializing_if`, so an `Option::None` is emitted as JSON `null`
    /// — the second reading, which is the correct one for every checkpoint THIS
    /// build writes. The `undefined` reading is produced by an older backend not
    /// emitting the key at all, which is exactly what it means. Adding
    /// `skip_serializing_if = "Option::is_none"` here would collapse the two
    /// and must not be done.
    pub source: Option<String>,
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
/// config, `gc.auto=0` since [`gc`] runs `git gc` explicitly instead, and
/// `core.hooksPath` pointed at a never-existing directory so a global
/// hooksPath can't run user hooks inside shadow operations. The init+config
/// block runs once per shadow repo (marker-keyed); `info/exclude` is
/// (re)seeded every call — see [`seed_exclude`].
pub async fn ensure(root: &Path, extra_ignore: &[String]) -> AppResult<()> {
    let ctx = shadow_ctx(root);
    let git_dir = git_dir_of(root);
    std::fs::create_dir_all(root.join(".cimp"))
        .map_err(|e| AppError::Workbench(format!("create .cimp dir: {e}")))?;

    // One-time init + config pinning, keyed on a marker written only AFTER
    // the whole block succeeds. `ensure` runs before EVERY snapshot/diff/
    // restore, and re-running init + 6 config writes is ~7 redundant
    // subprocess spawns per checkpoint (real milliseconds on Windows). The
    // marker is versioned rather than keying on `HEAD` existing, so a shadow
    // repo created before a config key was added (core.hooksPath) gets
    // re-pinned exactly once. `info/exclude` is still re-seeded every call —
    // `extra_ignore` tracks settings and can change between calls.
    let config_marker = git_dir.join("cimp-config-v2");
    if !config_marker.exists() {
        let init = git::run(&ctx, &["init", "-q"], None).await?;
        if !init.success() {
            return Err(AppError::Workbench(format!(
                "shadow git init failed: {}",
                init.stderr.trim()
            )));
        }
        // hooksPath: an absolute path to a directory that never exists, so a
        // user's GLOBAL core.hooksPath can't inject hooks into shadow-repo
        // operations (restore's `checkout` would otherwise run a
        // post-checkout hook with GIT_DIR pointed at the shadow repo).
        // Deliberately not "" — an empty value's behavior is underspecified,
        // and a RELATIVE path would resolve against the work tree, which the
        // user controls.
        let hooks_disabled = git_dir
            .join("hooks-disabled")
            .to_string_lossy()
            .into_owned();
        for (key, value) in [
            ("core.autocrlf", "false"),
            ("core.fileMode", "false"),
            ("core.hooksPath", hooks_disabled.as_str()),
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
        std::fs::write(&config_marker, b"2\n")
            .map_err(|e| AppError::Workbench(format!("write shadow config marker: {e}")))?;
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
    let mut out = String::from(
        "# Generated by cImp — do not edit, regenerated on every launch.\n.git/\n.cimp/\n",
    );
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
        return Err(AppError::Workbench(format!(
            "shadow git add -A failed: {}",
            add.stderr.trim()
        )));
    }
    if max_file_bytes > 0 {
        let staged = git::run(
            ctx,
            &["diff", "--cached", "--no-renames", "--name-only", "-z"],
            None,
        )
        .await?;
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
        // Chunk the unstage so a large oversize set can't build a single
        // command line past Windows' ~32K argv limit (which would make the
        // spawn itself fail, silently unstaging nothing). Best-effort either
        // way: an oversize blob riding along in one snapshot is a size-hygiene
        // nicety, not a correctness issue.
        for batch in oversize.chunks(100) {
            let mut args: Vec<&str> = vec!["reset", "-q", "--"];
            args.extend(batch.iter().map(String::as_str));
            let _ = git::run(ctx, &args, None).await;
        }
    }
    let tree = git::run(ctx, &["write-tree"], None).await?;
    if !tree.success() {
        return Err(AppError::Workbench(format!(
            "shadow write-tree failed: {}",
            tree.stderr.trim()
        )));
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
    let mut paths: Vec<String> = diff_out
        .stdout
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let untracked = git::run(
        ctx,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        None,
    )
    .await?;
    if !untracked.success() {
        return Err(AppError::Workbench(format!(
            "shadow ls-files --others failed: {}",
            untracked.stderr.trim()
        )));
    }
    paths.extend(
        untracked
            .stdout
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    );
    Ok(paths)
}

/// Cap a checkpoint label to a sane commit-subject length. Callers (the
/// prompt-tap trigger) are expected to already truncate to ~60 chars, but
/// this is a hard backstop against an unbounded label reaching `git
/// commit-tree` as a multi-KB commit subject.
fn truncate_label(label: &str) -> String {
    const MAX: usize = 200;
    // Replace control characters BEFORE truncating: a raw newline in a
    // prompt-derived label makes git store only the first line as the commit
    // subject (a wrong/short Timeline label), and an embedded `\u{1e}`/`\u{1f}`
    // record/field separator would fragment the `for-each-ref` record in
    // [`list`] so the whole checkpoint silently vanishes from the Timeline.
    let sanitized: String = label
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut chars = sanitized.chars();
    let truncated: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Render one identity value ([`Origin`]'s `agent`/`session`/`tab`) for the
/// commit-message trailer block — **the parsing boundary**, not a place that
/// trusts upstream.
///
/// Everything the trailer block's framing can be broken by is rejected here,
/// and rejection means the value is recorded as [`IDENTITY_ABSENT`] rather than
/// repaired:
///
/// - a **newline** would end the trailer line and let the value forge a whole
///   extra trailer (`Session: x\nTab: someone-elses-tab`), or — as the first
///   character of a multi-line value — split the commit's last paragraph so
///   git stops recognizing it as a trailer block at all;
/// - the `\u{1e}` / `\u{1f}` record/field separators [`list`] parses on would
///   fragment the `for-each-ref` record — a `\u{1e}` splits one checkpoint into
///   two half-rows, which is the failure mode that makes a checkpoint *silently
///   vanish* from the Timeline. (Since the trailer block became the record's
///   LAST field, a `\u{1f}` no longer shifts the fields before it, and [`list`]
///   rejoins the pieces past it rather than truncating the block — but a value
///   that has to be repaired to be read is still not the value that was
///   asserted, so it stays rejected here.)
/// - any other control character, on the same "not a value, a framing hazard"
///   footing (this is the rule [`truncate_label`] already applies to labels);
/// - an implausibly long value ([`MAX_IDENTITY_LEN`]).
///
/// **Rejected, not sanitized**, because these are machine identifiers whose
/// only use is an equality join against a contamination row. A repaired
/// identifier (`"a\u{1f}b"` → `"ab"`) is a *different* identifier presented as
/// fact, and could collide with a real one; absent is the honest answer and is
/// the same value the join already treats as "cannot attribute this row".
/// (Labels are the opposite case — prose, read by a human, where dropping the
/// whole label to punish one stray character would be worse than repairing it.)
///
/// Applies to `agent` too, which reached the trailer unsanitized before this
/// step: it is `ContextRetrieveBody::agent`, a caller-asserted string, and the
/// hazard was never specific to the two new fields.
fn trailer_identity(raw: Option<&str>) -> &str {
    let Some(value) = raw else {
        return IDENTITY_ABSENT;
    };
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_IDENTITY_LEN
        || value.chars().any(char::is_control)
        || value == IDENTITY_ABSENT
    {
        return IDENTITY_ABSENT;
    }
    value
}

/// The inverse of [`trailer_identity`] for [`list`]: one `for-each-ref` field
/// back into an `Option<String>`.
///
/// `None` for all three ways a value can be absent, which must stay
/// indistinguishable: the field was never written (a checkpoint from before
/// this build — see [`list`]'s backward-compatibility note), it was written as
/// the [`IDENTITY_ABSENT`] placeholder, or `for-each-ref` produced nothing for
/// the trailer key.
fn identity_field(raw: Option<&str>) -> Option<String> {
    let value = raw?.trim();
    (!value.is_empty() && value != IDENTITY_ABSENT).then(|| value.to_string())
}

/// One checkpoint commit's trailer block — the `Key: value` lines git emitted
/// for a single ref via `%(trailers:unfold)` — parsed **locally**, in Rust.
///
/// # Why this exists at all
///
/// [`list`] used to let git split the block for it, with one
/// `%(trailers:key=…,valueonly)` atom per field. That is broken on git 2.43.0
/// (stock Ubuntu 24.04; Debian 12 ships 2.39): with more than one trailer atom
/// in a format string, EVERY atom prints the union of all the requested keys'
/// values. Asking for the whole block once — a single atom, correct on every
/// git — and keying it apart here is correct without sniffing a git version.
/// See [`list`] for the symptom that produced.
///
/// # Parsing rules (all asserted by `tests::trailer_block_parsing_rules`)
///
/// 1. The block is split on `\n`; one trailing `\r` per line is stripped, so a
///    commit message stored with CRLF parses the same as one stored with LF.
/// 2. A line is a trailer iff it contains a `:` and the text before the FIRST
///    `:` is non-empty and free of whitespace. The key is that text; the value
///    is everything after that `:`, trimmed.
/// 3. A line that does not match — a blank line, a non-trailer line git
///    included because `%(trailers)` was not asked for `only`, or a folded
///    continuation line (which begins with whitespace, so rule 2 rejects it) —
///    is IGNORED, not treated as a key. `unfold` normally joins continuation
///    lines onto their trailer's line before we ever see them; this rule is
///    what makes the parser correct if it ever does not, and is also what stops
///    a folded value from forging a key of its own.
/// 4. Keys are matched **case-insensitively** (ASCII), which is what
///    `%(trailers:key=…)` did.
/// 5. A duplicate key resolves to the **first** occurrence. Our writer emits
///    each key exactly once, so a duplicate means a hand-made or tampered
///    commit — and since a trailer can only be appended AFTER the ones
///    [`snapshot`] wrote, first-wins is the occurrence we actually authored.
///    (git's `key=` atom concatenated all matches instead, which is strictly
///    worse: two values fused into one string.)
/// 6. A key that is not in the block is `None`, exactly as an absent field was
///    before — that is what keeps pre-V33 checkpoints listing.
struct TrailerBlock<'a> {
    /// In block order, first occurrence of a key first. Small (single digits),
    /// so a linear scan beats a map and keeps the "first wins" rule visible.
    entries: Vec<(&'a str, &'a str)>,
}

impl<'a> TrailerBlock<'a> {
    fn parse(block: &'a str) -> Self {
        let mut entries = Vec::new();
        for line in block.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let Some((key, value)) = line.split_once(':') else {
                continue; // not a trailer line
            };
            if key.is_empty() || key.chars().any(char::is_whitespace) {
                // Leading whitespace (a folded continuation line) or an empty
                // key: not a trailer, and deliberately not a key we will match.
                continue;
            }
            entries.push((key, value.trim()));
        }
        Self { entries }
    }

    /// The value for `key`, or `None` if the commit does not carry it. See the
    /// type's rules 4–6 for case-insensitivity, duplicates, and absence.
    fn get(&self, key: &str) -> Option<&'a str> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| *v)
    }
}

/// `true` if `id` is a well-formed checkpoint tag name (`cp-<digits>`). The one
/// gate every [`resolve_commit`] caller relies on to keep untrusted ids out of
/// `git rev-parse`'s option/rev grammar.
fn is_checkpoint_id(id: &str) -> bool {
    id.strip_prefix("cp-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
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
        .filter_map(|l| {
            l.strip_prefix("cp-")
                .and_then(|n| n.parse::<u32>().ok())
                .map(|n| (n, l.to_string()))
        })
        .max_by_key(|(n, _)| *n);
    Ok(max.map(|(_, tag)| tag))
}

/// Resolve a checkpoint id (a `cp-<seq>` tag) to its commit sha. Every other
/// function in this module that needs a concrete commit goes through this,
/// so an unknown/garbage id fails with one consistent, typed message instead
/// of a raw `git` error surfacing from whichever call happened to choke on
/// it first.
async fn resolve_commit(ctx: &GitCtx, id: &str) -> AppResult<String> {
    // Every checkpoint id this module mints is `cp-<seq>`. Rejecting anything
    // else up front means a frontend-supplied id can never reach `git rev-parse`
    // as a leading-dash option or any other injected rev expression — it fails
    // with the same clean typed error instead. (argv, not a shell, so this was
    // never code-execution — just defense in depth + a clearer failure.)
    if !is_checkpoint_id(id) {
        return Err(AppError::Workbench(format!("unknown checkpoint: {id}")));
    }
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
///
/// **What a dedup hit records — and deliberately does NOT record.** When the
/// tree is unchanged this returns an EXISTING checkpoint, whose `origin` may
/// name a different conversation — two tabs on one project root share this
/// shadow repo, and since V33 they no longer share
/// `WorkbenchService::maybe_snapshot`'s min-gap throttle (it is keyed per
/// `(root, tab)`), so a second tab reaching this function moments after the
/// first is now the ORDINARY case rather than one the throttle mostly
/// prevented. The existing checkpoint's `Session`/`Tab`/`Source` trailers are
/// left **exactly as they were written**: they are a record of who took *that*
/// snapshot, and retro-writing the current caller's identity onto it would be
/// the "silently mislabel an existing checkpoint" failure — it would also
/// rewrite the commit (changing the `commit` sha every consumer displays) for
/// an event where nothing was snapshotted. The honest reading of a dedup hit
/// is "no checkpoint was created, because there was nothing new to capture";
/// the returned id names a tree state, and that tree state is identical no
/// matter which tab observed it. Callers must therefore not assume the id they
/// get back carries their own identity.
///
/// **V33 Phase F made that "must not" checkable** — see [`SnapshotOutcome`] and
/// [`snapshot_detailed`]. This function keeps returning the bare id for the
/// callers that legitimately do not care (they want a restorable tree state,
/// not an attribution).
pub async fn snapshot(
    root: &Path,
    label: &str,
    trigger: Trigger,
    origin: &Origin,
    extra_ignore: &[String],
    max_file_bytes: u64,
) -> AppResult<CheckpointId> {
    require_id(
        snapshot_detailed(
            root,
            label,
            trigger,
            origin,
            extra_ignore,
            max_file_bytes,
            None,
        )
        .await?,
    )
}

/// The id out of an outcome taken **without** a deadline, where
/// [`SnapshotOutcome::Abandoned`] is unreachable by construction.
///
/// One shared place rather than an `unreachable!()` at each of the two
/// deadline-less call sites ([`snapshot`] and [`restore`]'s invariant-C
/// pre-restore snapshot): if a future edit ever hands one of them a deadline,
/// this degrades to a plain error on a path that already returns `AppResult`,
/// instead of panicking inside a background task.
fn require_id(outcome: SnapshotOutcome) -> AppResult<CheckpointId> {
    match outcome {
        SnapshotOutcome::Created(id) | SnapshotOutcome::Deduped(id) => Ok(id),
        SnapshotOutcome::Abandoned => Err(AppError::Workbench(
            "shadow snapshot abandoned against a deadline this caller never set".to_string(),
        )),
    }
}

/// What [`snapshot_detailed`] answers: **which of the three things happened**,
/// and the checkpoint id in the two cases where there is one.
///
/// # Why this is an enum (V33 Phase F, locked contract)
///
/// [`snapshot`]'s dedup returns an EXISTING checkpoint when the tree is
/// unchanged, and deliberately does not relabel it (see [`snapshot`]'s doc).
/// For the prompt/burst/manual triggers that is invisible — nobody reports the
/// id anywhere identity-bearing. For the Phase F **tool** trigger it is a
/// correctness hazard: a pre-tool checkpoint over an unchanged tree gets back an
/// id belonging to another trigger and possibly another TAB, and a caller that
/// then said "this tool call's checkpoint is cp-7" would be attributing another
/// conversation's snapshot to this tool call — a fabricated causal claim in the
/// one record the Timeline exists to be trusted on after an incident.
///
/// **The locked rule: a caller must not claim a checkpoint it did not create,
/// and must never relabel another trigger's checkpoint.** [`Deduped`] is how a
/// caller honours it. Nothing about the git storage needs to change for this —
/// a dedup hit writes no commit, so no `Source:`/`Tab:` trailer of the current
/// caller's ever exists on disk. The variant closes the *reporting* half.
///
/// [`Abandoned`] is the 2026-08-13 amendment's half of the same rule, one step
/// further out: the caller's pre-tool budget expired, so this call refuses to
/// write a `Trigger::Tool` commit **at all**. See [`snapshot_detailed`]'s
/// `deadline`.
///
/// [`Deduped`]: SnapshotOutcome::Deduped
/// [`Abandoned`]: SnapshotOutcome::Abandoned
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotOutcome {
    /// This call wrote a new commit and tagged it. The id is the caller's own.
    Created(CheckpointId),
    /// The tree was byte-identical to the latest checkpoint, so nothing was
    /// written. The id names an EXISTING checkpoint with someone else's
    /// identity on it — a caller may restore it, and may not claim it.
    Deduped(CheckpointId),
    /// The `deadline` expired before a commit could be written, so **nothing
    /// was written and there is no id**. See [`snapshot_detailed`].
    Abandoned,
}

impl SnapshotOutcome {
    /// The checkpoint id, for the two outcomes that name one.
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Created(id) | Self::Deduped(id) => Some(id.as_str()),
            Self::Abandoned => None,
        }
    }

    /// Whether THIS call is the one that wrote the checkpoint — the predicate an
    /// identity-bearing caller must pass before naming an id as its own.
    pub fn created(&self) -> bool {
        matches!(self, Self::Created(_))
    }
}

/// [`snapshot`], plus which of the three [`SnapshotOutcome`]s happened — the
/// entry point every identity-bearing caller must use.
///
/// # `deadline` — the pre-tool budget (2026-08-13 amendment, locked)
///
/// `None` for every trigger whose caller waits indefinitely: the prompt tap and
/// the burst tap (both fire-and-forget), the manual button, `restore`'s
/// invariant-C snapshot, and the offload worker's own `dispatch` (which has not
/// spawned anything yet and can simply wait).
///
/// `Some(instant)` for the two **out-of-process** Phase F seams — the Claude
/// `PreToolUse` shim and the OpenCode `tool.execute.before` plugin hook — whose
/// wait on the app is bounded (~2 s) because the agent's tool runs the moment
/// they stop waiting. Past that instant, a `Trigger::Tool` checkpoint would be
/// a checkpoint whose staging **overlapped the very tool call it names**, and a
/// row that sometimes contains the change it claims to predate is worse than no
/// row: it silently misleads a restore. So the deadline is enforced HERE, at
/// the one place that decides whether a commit is written, rather than in the
/// caller — a caller that merely stops waiting does not stop this function from
/// minting the row.
///
/// The deadline bounds the *decision*, not git: an already-running `git add -A`
/// is left to finish (killing it would leave the shadow index locked behind a
/// detached child). Overlap therefore still happens — what cannot happen is a
/// checkpoint being *claimed* for it.
pub async fn snapshot_detailed(
    root: &Path,
    label: &str,
    trigger: Trigger,
    origin: &Origin,
    extra_ignore: &[String],
    max_file_bytes: u64,
    deadline: Option<Instant>,
) -> AppResult<SnapshotOutcome> {
    let lock = shadow_lock(root);
    // Deliberately inside the budget: contending for another trigger's shadow
    // lock is one of the two real ways a pre-tool snapshot runs long (the other
    // is a large `git add -A`), and a caller that waited out its budget on a
    // lock is in exactly the position the deadline exists to detect.
    let _guard = lock.lock().await;
    snapshot_inner(
        root,
        label,
        trigger,
        origin,
        extra_ignore,
        max_file_bytes,
        deadline,
    )
    .await
}

/// Whether a pre-tool budget has run out. `None` (no budget) is never expired —
/// that is the whole of the pre-2026-08-13 behaviour, preserved by construction
/// rather than by a branch at each call site.
fn past_deadline(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

/// The body of [`snapshot`], WITHOUT acquiring the per-root shadow lock — so
/// [`restore`] (which already holds that lock for its whole sequence) can take
/// its invariant-C pre-restore snapshot without deadlocking on a re-entrant
/// acquire. Never call this directly from outside the module; go through
/// [`snapshot`] so the serialization guarantee holds.
async fn snapshot_inner(
    root: &Path,
    label: &str,
    trigger: Trigger,
    origin: &Origin,
    extra_ignore: &[String],
    max_file_bytes: u64,
    deadline: Option<Instant>,
) -> AppResult<SnapshotOutcome> {
    // Cheapest possible abandonment: the budget was already gone before any git
    // ran (a long wait on the shadow lock, or a caller that arrived late). Doing
    // the `git add -A` first and *then* discarding it would cost the user the
    // same work for the same nothing.
    if past_deadline(deadline) {
        return Ok(SnapshotOutcome::Abandoned);
    }
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

    // **The load-bearing check.** Staging is the expensive step and the one that
    // races the tool call: if it did not finish inside the caller's budget, the
    // tree just written may already contain the edit this checkpoint exists to
    // precede. Checked BEFORE the dedup arm on purpose — a dedup hit past the
    // budget would report "nothing changed", which reads as a fact about the
    // tree when all we actually know is that we ran out of time to have an
    // opinion about it.
    if past_deadline(deadline) {
        return Ok(SnapshotOutcome::Abandoned);
    }

    if let Some(last_tag) = latest_checkpoint_tag(&ctx).await? {
        let spec = format!("{last_tag}^{{tree}}");
        let last_tree = git::run(&ctx, &["rev-parse", "-q", "--verify", &spec], None).await?;
        if last_tree.success() && last_tree.stdout.trim() == current_tree {
            // NOTHING is written here — not a commit, not a relabel of the
            // existing tag's trailers. `Deduped` is what tells the caller the id
            // it is holding is someone else's (see [`SnapshotOutcome`]).
            return Ok(SnapshotOutcome::Deduped(last_tag));
        }
    }

    let seq = next_seq(&ctx).await?;
    let tag = format!("cp-{seq}");
    // Identity trailers go LAST, after the three Phase C ones, and each new one
    // is APPENDED at the tail. That ordering is load-bearing for [`list`]'s
    // backward compatibility — see its note — and it keeps the message a
    // checkpoint with no identity produces byte-identical to the old one apart
    // from appended placeholder lines.
    //
    // V33 Phase F appends `Source:` after `Tab:` under exactly that rule.
    //
    // Every value goes through [`trailer_identity`]: nothing caller-asserted
    // reaches this commit message without passing the framing check. `source` is
    // composed by cImp (`harness:tool_name`) rather than asserted by a caller,
    // but it still carries a harness-supplied tool NAME, so it goes through the
    // same boundary as the rest — the hazard is in the value, not in who sent it.
    let message = format!(
        "{}\n\nTrigger: {}\nAgent: {}\nFiles-Changed: {}\nSession: {}\nTab: {}\nSource: {}\n",
        truncate_label(label),
        trigger.as_str(),
        trailer_identity(origin.agent.as_deref()),
        changed.len(),
        trailer_identity(origin.session.as_deref()),
        trailer_identity(origin.tab.as_deref()),
        trailer_identity(origin.source.as_deref()),
    );
    let commit = git::run_with_stdin(
        &ctx,
        &["commit-tree", &current_tree, "-F", "-"],
        message.as_bytes(),
        None,
    )
    .await?;
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
    Ok(SnapshotOutcome::Created(tag))
}

/// Record separator for [`list_format`]: a literal
/// [record separator](https://en.wikipedia.org/wiki/C0_and_C1_control_codes)
/// (U+001E) rather than `\n`, because the trailer block is printed as-is —
/// newlines and all — so splitting records on `\n` would fragment a single
/// ref's output into several pieces. `%x00` is NOT recognized as a hex escape
/// by `for-each-ref --format` (verified against git 2.54: it comes through as
/// the four literal characters `%x00`, not a NUL byte), which is why the
/// separator is embedded literally in the format string — for-each-ref prints
/// it as-is since it isn't part of any `%(...)` atom. `\n` only ever shows up
/// embedded WITHIN the trailing trailer-block field after this, plus git's own
/// automatic end-of-line newline trailing the RS, which [`list`]'s whole-record
/// and per-field `.trim()`s strip.
const REC_SEP: char = '\u{1e}';

/// The `for-each-ref --format` [`list`] reads every checkpoint's metadata with.
///
/// **One `%(trailers)` atom, keyed apart locally by [`TrailerBlock`]** —
/// deliberately NOT the six `%(trailers:key=…,valueonly)` atoms this read
/// used before. On git 2.43.0 (stock Ubuntu 24.04; Debian 12 ships 2.39)
/// every trailer atom in a MULTI-atom format prints the UNION of all the
/// requested keys' values instead of its own, so all six fields came back as
/// the same concatenated blob: every Timeline row read `trigger: Manual`
/// (the [`Trigger::parse`] fallback), a garbage `agent`/`session`/`tab`/
/// `source`, and `files_changed: 0`. Newer gits (2.54 verified) get it
/// right, and one atom is correct on both — which is the whole point:
/// requesting the WHOLE block once and doing the key lookup in Rust is
/// version-independent BY CONSTRUCTION, so nothing here sniffs a git
/// version, and it is still one subprocess for the entire listing rather
/// than a per-ref fan-out. (`unfold` asks git to join a folded multi-line
/// trailer value back onto one line; [`TrailerBlock::parse`] does not depend
/// on it having done so.)
///
/// The trailer block is the LAST field of the record because it is the only
/// field that can contain newlines.
///
/// Backward compatibility is mandatory here: a user's `.cimp/shadow.git` is
/// full of checkpoints whose commit messages carry only the three Phase C
/// trailers, and an upgrade that emptied their Timeline would be a
/// data-loss-shaped bug. A key the commit does not have is simply absent
/// from the block and reads back as `None` (or the field's default), and the
/// record's field COUNT no longer depends on which trailers a checkpoint
/// carries at all — so no missing trailer can shift a field, and adding a
/// seventh trailer some day cannot either. That is asserted against a real
/// repo of hand-built old-format commits by
/// `tests::old_eight_field_checkpoints_still_list_after_the_identity_fields`
/// rather than assumed.
///
/// Built here rather than inline in [`list`] so
/// `tests::the_listing_format_asks_for_the_whole_trailer_block_once` can pin
/// that property directly, without needing git.
fn list_format() -> String {
    format!(
        "%(objectname){sep}%(creatordate:unix){sep}%(creatordate:iso-strict){sep}%(contents:subject){sep}%(refname:short){sep}%(trailers:unfold){rec_sep}",
        sep = FIELD_SEP,
        rec_sep = REC_SEP
    )
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
    let format = list_format();
    let fmt_arg = format!("--format={format}");
    let out = git::run(
        &ctx,
        &[
            "for-each-ref",
            "--sort=creatordate",
            &fmt_arg,
            "refs/tags/cp-*",
        ],
        None,
    )
    .await?;
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
        // The count that must be present for a row to mean anything: the five
        // git-native fields, which every checkpoint has regardless of which
        // trailers it carries. The trailer block past them is read through
        // `get`, so a format widening can never turn "a field I don't have"
        // into "skip this row" — which is how a schema addition silently
        // empties a Timeline.
        const CORE_FIELDS: usize = 5;
        if fields.len() < CORE_FIELDS {
            continue; // malformed row (shouldn't happen) — skip, don't panic
        }
        let tag = fields[4].trim().to_string();
        let seq = tag
            .strip_prefix("cp-")
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(0);
        // Everything from index 5 on is the trailer block. It is joined back
        // together rather than read as `fields[5]`: `FIELD_SEP` cannot reach a
        // trailer through [`trailer_identity`], but a hand-made commit could
        // still carry one, and truncating the block at it would silently drop
        // every trailer after it instead of mangling one value.
        let block = fields[CORE_FIELDS..].join(FIELD_SEP);
        let trailers = TrailerBlock::parse(&block);
        checkpoints.push(Checkpoint {
            id: tag,
            seq,
            commit: fields[0].trim().to_string(),
            ts_unix: fields[1].trim().parse().unwrap_or(0),
            ts: fields[2].trim().to_string(),
            label: fields[3].trim().to_string(),
            trigger: Trigger::parse(trailers.get("Trigger").unwrap_or("")),
            agent: identity_field(trailers.get("Agent")),
            files_changed: trailers
                .get("Files-Changed")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            session: identity_field(trailers.get("Session")),
            tab: identity_field(trailers.get("Tab")),
            // V33 Phase F — like its two neighbours, absent (not empty, not
            // `"-"`) on a checkpoint written before the field existed.
            source: identity_field(trailers.get("Source")),
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
pub async fn diff_vs_now(
    root: &Path,
    id: &str,
    extra_ignore: &[String],
    max_file_bytes: u64,
    context: u32,
) -> AppResult<String> {
    let lock = shadow_lock(root);
    let _guard = lock.lock().await;
    ensure(root, extra_ignore).await?;
    let ctx = shadow_ctx(root);
    let target = resolve_commit(&ctx, id).await?;

    let scratch_index = git_dir_of(root).join(format!("index.diffnow-{}", uuid::Uuid::new_v4()));
    // RAII cleanup so the scratch index (and any leftover `.lock`) is removed on
    // EVERY exit path — a `stage_and_write_tree` error used to `?`-return before
    // the manual `remove_file`, littering `.cimp/shadow.git/` with dead indexes.
    let _scratch = ScratchIndex(scratch_index.clone());
    let scratch_ctx = GitCtx {
        index_file: Some(scratch_index.clone()),
        ..ctx.clone()
    };
    let now_tree = stage_and_write_tree(&scratch_ctx, root, max_file_bytes).await?;

    let unified = format!("--unified={}", context.min(super::diff::MAX_CONTEXT));
    let out = git::run(
        &ctx,
        &[
            "diff",
            "--no-color",
            "--no-renames",
            &unified,
            &target,
            &now_tree,
        ],
        Some(BULK_TIMEOUT),
    )
    .await?;
    if !out.success() {
        return Err(AppError::Workbench(format!(
            "shadow diff failed: {}",
            out.stderr.trim()
        )));
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
    // Hold the per-root shadow lock for the WHOLE restore: this both serializes
    // against concurrent snapshot/gc and keeps a background snapshot from
    // slipping in between the pre-restore safety snapshot and the checkout.
    let lock = shadow_lock(root);
    let _guard = lock.lock().await;
    ensure(root, extra_ignore).await?;
    let ctx = shadow_ctx(root);
    let target = resolve_commit(&ctx, id).await?;

    // `snapshot_inner`, not `snapshot`: we already hold the lock, and the async
    // mutex is not re-entrant.
    // `Origin::default()`: a pre-restore safety snapshot belongs to the restore
    // action, not to any conversation — the same "no identity" answer the
    // manual and burst triggers give.
    // `Created` vs `Deduped` is deliberately ignored here: a pre-restore
    // snapshot over an unchanged tree legitimately reuses the existing
    // checkpoint (that tree state IS the undo point, whoever recorded it), and
    // `Origin::default()` means there is no identity to mis-attribute in the
    // first place. `None` deadline — a restore is a user action that waits for
    // its own safety net, so `Abandoned` is unreachable ([`require_id`]).
    let pre_restore_id = require_id(
        snapshot_inner(
            root,
            "pre-restore",
            Trigger::PreRestore,
            &Origin::default(),
            extra_ignore,
            max_file_bytes,
            None,
        )
        .await?,
    )?;
    let pre_sha = resolve_commit(&ctx, &pre_restore_id).await?;

    let added = git::run(
        &ctx,
        &[
            "diff",
            "--no-renames",
            "--name-only",
            "--diff-filter=A",
            "-z",
            &target,
            &pre_sha,
        ],
        None,
    )
    .await?;
    if !added.success() {
        return Err(AppError::Workbench(format!(
            "shadow diff --diff-filter=A failed: {}",
            added.stderr.trim()
        )));
    }
    // Work at the BYTE level for the `-z` entries here: these are filenames,
    // and on Linux a filename can be non-UTF-8 — the lossy `stdout` decode
    // would corrupt it, so the `delete_new` removal below would silently miss
    // the real file and the created/changed set math would misclassify it.
    // The lossy form is derived alongside purely for the report (display).
    let created_raw: Vec<Vec<u8>> = added
        .stdout_bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    let created_since: Vec<String> = created_raw
        .iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect();

    let changed_out = git::run(
        &ctx,
        &[
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            &pre_sha,
            &target,
        ],
        None,
    )
    .await?;
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
    let created_set: HashSet<&[u8]> = created_raw.iter().map(Vec::as_slice).collect();
    let changed: Vec<String> = changed_out
        .stdout_bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty() && !created_set.contains(s))
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect();

    // (A/B) shadow checkout — see this function's doc comment.
    let checkout = git::run(&ctx, &["checkout", &target, "--", "."], Some(BULK_TIMEOUT)).await?;
    if !checkout.success() {
        return Err(AppError::Workbench(format!(
            "shadow checkout failed: {}",
            checkout.stderr.trim()
        )));
    }

    // (D) opt-in deletion of files created since the checkpoint. Paths come
    // from the RAW bytes (`bytes_to_path`), not the lossy report strings —
    // see the comment on `created_raw` above.
    let mut deleted = Vec::new();
    if delete_new {
        for (raw, display) in created_raw.iter().zip(&created_since) {
            let abs = root.join(git::bytes_to_path(raw));
            if (abs.is_file() || abs.is_symlink()) && std::fs::remove_file(&abs).is_ok() {
                deleted.push(display.clone());
            }
        }
    }

    Ok(RestoreReport {
        pre_restore_id,
        changed,
        created_since,
        deleted,
    })
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
    let lock = shadow_lock(root);
    let _guard = lock.lock().await;
    let ctx = shadow_ctx(root);
    let checkpoints = list(root).await?; // ascending by seq

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
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
        let remaining: Vec<&Checkpoint> = checkpoints
            .iter()
            .filter(|c| !to_delete.contains(&c.id))
            .collect();
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

    // Chunk `tag -d` for the same argv-length reason as the oversize unstage
    // above: age-based deletion can select many tags at once, and unlike that
    // path this one is not best-effort, so a spawn failure would surface as a
    // gc error.
    let names: Vec<&str> = to_delete.iter().map(String::as_str).collect();
    for batch in names.chunks(100) {
        let mut args: Vec<&str> = vec!["tag", "-d"];
        args.extend(batch.iter().copied());
        let out = git::run(&ctx, &args, None).await?;
        if !out.success() {
            return Err(AppError::Workbench(format!(
                "shadow tag -d failed: {}",
                out.stderr.trim()
            )));
        }
    }
    // Best-effort space reclaim: the tags (the part of the retention
    // contract that matters) are already gone even if this fails.
    let _ = git::run(&ctx, &["gc", "--prune=now", "-q"], Some(BULK_TIMEOUT)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::testutil::{git, has_git};
    use super::*;

    #[test]
    fn is_checkpoint_id_accepts_only_cp_digits() {
        assert!(is_checkpoint_id("cp-1"));
        assert!(is_checkpoint_id("cp-9001"));
        assert!(!is_checkpoint_id("cp-"));
        assert!(!is_checkpoint_id("cp-1a"));
        assert!(!is_checkpoint_id("cp-1.2"));
        assert!(!is_checkpoint_id("-foo")); // leading-dash rev/option injection
        assert!(!is_checkpoint_id("HEAD"));
        assert!(!is_checkpoint_id("../etc/passwd"));
    }

    #[test]
    fn truncate_label_strips_control_chars() {
        // Newlines and the \u{1e}/\u{1f} separators `list` parses on must be
        // neutralized so a checkpoint label can't corrupt or drop its own row.
        let out = truncate_label("one\ntwo\u{1f}three\u{1e}four\r");
        assert_eq!(out, "one two three four ");
    }

    /// The pre-V33 origin shape: a harness name and nothing else. Used by the
    /// tests that predate the identity fields, so they keep exercising the
    /// "agent only" row a burst-era build produced.
    fn agent_origin(agent: &str) -> Origin {
        Origin::new(Some(agent.to_string()), None, None)
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
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else if path.file_name().and_then(|n| n.to_str()) == Some("index")
                    && path.parent() == Some(base)
                {
                    continue; // top-level index cache — see doc comment
                } else if let Ok(bytes) = std::fs::read(&path) {
                    let rel = path
                        .strip_prefix(base)
                        .unwrap_or(&path)
                        .display()
                        .to_string()
                        .replace('\\', "/");
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
        assert_eq!(
            ctx.index_file,
            Some(root.join(".cimp").join("shadow.git").join("index"))
        );
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
        ensure(&dir, &["*.generated.js".to_string()])
            .await
            .expect("ensure");
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
        let id = snapshot(
            &dir,
            "prompt: do the thing",
            Trigger::Prompt,
            &Origin::new(
                Some("claude".into()),
                Some("f0c1a2b3-4d5e-6f70-8192-a3b4c5d6e7f8".into()),
                Some("claude-2".into()),
            ),
            &[],
            0,
        )
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
        // The V33 identity join: read back out of a REAL shadow repo, through
        // `commit-tree` and `for-each-ref`, not out of the format string.
        assert_eq!(
            cps[0].session,
            Some("f0c1a2b3-4d5e-6f70-8192-a3b4c5d6e7f8".to_string())
        );
        assert_eq!(cps[0].tab, Some("claude-2".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: the automatic triggers (`WorkbenchService::maybe_snapshot`)
    /// normalize the root through `git::canonical_path`, which on Windows
    /// returns the `\\?\` verbatim form — and MSYS git rejects verbatim paths
    /// in the `GIT_DIR`-family env vars (`fatal: not a git repository:
    /// '\\?\…'`). That silently killed every prompt/burst checkpoint (their
    /// failures are warn-and-drop by design) while manual checkpoints, which
    /// pass the raw root, kept working. `git::run` now strips the verbatim
    /// prefix at the spawn boundary; this exercises the exact call shape the
    /// automatic path makes. Only bites on Windows — elsewhere `canonical_path`
    /// returns a plain path and this is an ordinary snapshot test.
    #[tokio::test]
    async fn snapshot_and_gc_work_from_a_canonicalized_root() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("verbatim");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let canon = git::canonical_path(&dir);
        let id = snapshot(
            &canon,
            "prompt: auto",
            Trigger::Prompt,
            &agent_origin("claude"),
            &[],
            0,
        )
        .await
        .expect("snapshot from canonicalized root");
        assert_eq!(id, "cp-1");
        gc(&canon, 5, 5).await.expect("gc from canonicalized root");
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
        let id1 = snapshot(&dir, "first", Trigger::Manual, &Origin::default(), &[], 0)
            .await
            .expect("snapshot 1");
        // Nothing changed on disk since — must return the SAME id, not mint
        // a new checkpoint.
        let id2 = snapshot(
            &dir,
            "second (no-op)",
            Trigger::Manual,
            &Origin::default(),
            &[],
            0,
        )
        .await
        .expect("snapshot 2");
        assert_eq!(id1, id2);
        assert_eq!(list(&dir).await.expect("list").len(), 1);

        // A real change DOES produce a new checkpoint.
        std::fs::write(dir.join("a.txt"), "one changed\n").unwrap();
        let id3 = snapshot(&dir, "third", Trigger::Manual, &Origin::default(), &[], 0)
            .await
            .expect("snapshot 3");
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
        let id = snapshot(
            &dir,
            "baseline",
            Trigger::Manual,
            &Origin::default(),
            &[],
            0,
        )
        .await
        .expect("snapshot");
        assert_eq!(id, "cp-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── V33: checkpoint identity (session + tab) ─────────────────────────

    /// Run git against `root`'s SHADOW repo and return trimmed stdout.
    /// Deliberately spells `--git-dir` itself instead of going through
    /// [`shadow_ctx`]: these helpers build the *legacy* on-disk shapes this
    /// module must keep reading, so they must not inherit this module's current
    /// idea of what a checkpoint looks like.
    fn shadow_git_out(root: &Path, args: &[&str]) -> String {
        let mut full: Vec<&str> = vec!["--git-dir", ".cimp/shadow.git"];
        full.extend_from_slice(args);
        let out = std::process::Command::new("git")
            .args(&full)
            .current_dir(root)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {full:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Tag `cp-<seq>` onto a commit carrying **exactly the pre-V33 commit
    /// message**: subject, blank line, and the three Phase C trailers with no
    /// `Session`/`Tab`. This is the upgrade case — what is already sitting in
    /// every existing user's `.cimp/shadow.git` — reproduced byte-for-byte
    /// rather than simulated by feeding `None` to the current writer (which
    /// would still emit `Session: -` and prove nothing about the old shape).
    ///
    /// Reuses `cp-1`'s tree, so the repo needs one real snapshot first.
    fn write_legacy_checkpoint(root: &Path, seq: u32, label: &str, agent: &str, files: u32) {
        let tree = shadow_git_out(root, &["rev-parse", "-q", "--verify", "cp-1^{tree}"]);
        let message =
            format!("{label}\n\nTrigger: manual\nAgent: {agent}\nFiles-Changed: {files}\n");
        // Under `.cimp/`, which `seed_exclude` keeps out of every snapshot, so
        // this fixture file can never show up as checkpoint content.
        let msg_path = root.join(".cimp").join(format!("legacy-msg-{seq}.txt"));
        std::fs::write(&msg_path, &message).unwrap();
        let msg_arg = msg_path.to_string_lossy().into_owned();
        let sha = shadow_git_out(root, &["commit-tree", &tree, "-F", &msg_arg]);
        shadow_git_out(root, &["tag", &format!("cp-{seq}"), &sha]);
    }

    /// **The upgrade case, and the sharpest trap in this change.** Metadata is
    /// read back with one `for-each-ref --format` whose field count the parser
    /// used to assert exactly (`!= 8`), so widening the format without widening
    /// the guard makes every pre-existing checkpoint fail the arity check and
    /// disappear from the Timeline *silently* — no error, just an empty
    /// history. (The record's arity no longer depends on which trailers a
    /// checkpoint carries — the whole block is one trailing field, keyed apart
    /// by [`TrailerBlock`] — so this now guards the read END to end: legacy
    /// commits must still yield their `Trigger`/`Agent`/`Files-Changed` and an
    /// absent identity, whichever way the fields are laid out.)
    ///
    /// This asserts against genuinely old-format commits (see
    /// [`write_legacy_checkpoint`]), not against a re-serialized `None`.
    ///
    /// **What it would still pass with if the change regressed:** almost
    /// nothing — flipping the guard back to `!= 8`, or splicing `Session`/`Tab`
    /// in beside `Agent` instead of appending them (which would shift
    /// `refname:short` and blank every legacy row's id), both fail here. It
    /// would NOT catch a regression in the *writer*; that is what the round-trip
    /// test covers, which is why both assertions live in this one test: the new
    /// row's identity is checked alongside the legacy rows', so a "fix" that
    /// restored legacy rows by dropping the identity fields fails too.
    #[tokio::test]
    async fn old_eight_field_checkpoints_still_list_after_the_identity_fields() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("legacy");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        snapshot(
            &dir,
            "new format",
            Trigger::Prompt,
            &Origin::new(
                Some("claude".into()),
                Some("sess-new".into()),
                Some("claude-2".into()),
            ),
            &[],
            0,
        )
        .await
        .expect("snapshot");

        // Two pre-V33 checkpoints, as an upgraded install would have them.
        write_legacy_checkpoint(&dir, 2, "old with agent", "claude", 3);
        write_legacy_checkpoint(&dir, 3, "old without agent", "-", 0);

        let cps = list(&dir).await.expect("list");
        assert_eq!(
            cps.len(),
            3,
            "an upgrade must not empty an existing Timeline"
        );

        let legacy = cps.iter().find(|c| c.id == "cp-2").expect("cp-2 listed");
        assert_eq!(legacy.label, "old with agent");
        assert_eq!(legacy.trigger, Trigger::Manual);
        assert_eq!(legacy.agent, Some("claude".to_string()));
        assert_eq!(legacy.files_changed, 3);
        assert_eq!(legacy.seq, 2, "the tag field must not have shifted");
        assert!(!legacy.commit.is_empty());
        // Absent, not empty-string, not "-".
        assert_eq!(legacy.session, None);
        assert_eq!(legacy.tab, None);

        let legacy_anon = cps.iter().find(|c| c.id == "cp-3").expect("cp-3 listed");
        assert_eq!(legacy_anon.agent, None);
        assert_eq!(legacy_anon.session, None);
        assert_eq!(legacy_anon.tab, None);

        // …and the new-format row beside them still carries its identity.
        let fresh = cps.iter().find(|c| c.id == "cp-1").expect("cp-1 listed");
        assert_eq!(fresh.session, Some("sess-new".to_string()));
        assert_eq!(fresh.tab, Some("claude-2".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A checkpoint with no conversation behind it — a manual click, a burst,
    /// [`restore`]'s pre-restore snapshot — lists cleanly with the identity
    /// fields ABSENT rather than as `"-"`, `""`, or a row that fails to parse.
    ///
    /// **What it would still pass with:** it would not catch a writer that
    /// omitted the trailers entirely (git would then report them empty and this
    /// still reads `None`). It DOES catch the placeholder leaking to consumers
    /// as a literal `"-"` tab id — which would make every identityless
    /// checkpoint look like it belonged to a tab named `-`.
    #[tokio::test]
    async fn a_checkpoint_with_no_conversation_lists_with_identity_absent() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("no-origin");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        snapshot(
            &dir,
            "manual checkpoint",
            Trigger::Manual,
            &Origin::default(),
            &[],
            0,
        )
        .await
        .expect("snapshot");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        snapshot(&dir, "activity", Trigger::Burst, &Origin::default(), &[], 0)
            .await
            .expect("burst snapshot");

        let cps = list(&dir).await.expect("list");
        assert_eq!(cps.len(), 2);
        for cp in &cps {
            assert_eq!(cp.agent, None, "{}", cp.id);
            assert_eq!(cp.session, None, "{}", cp.id);
            assert_eq!(cp.tab, None, "{}", cp.id);
        }
        assert_eq!(cps[0].trigger, Trigger::Manual);
        assert_eq!(cps[1].trigger, Trigger::Burst);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The point of this step.** Two Claude tabs on ONE project root must
    /// produce checkpoints that are distinguishable. `agent` cannot do it —
    /// it is the harness name and reads `"claude"` for both — so a test that
    /// only asserted "the tab field is present" would pass with a writer that
    /// hardcoded one value for every tab.
    ///
    /// **What it would still pass with if the change regressed:** nothing that
    /// matters here. It fails if `tab` is dropped, if both rows get the same
    /// tab, if `session` and `tab` are transposed (the sessions and tabs are
    /// deliberately not interchangeable strings), or if identity is written
    /// once per ROOT rather than once per checkpoint.
    #[tokio::test]
    async fn two_tabs_on_one_root_produce_distinguishable_checkpoints() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("two-tabs");
        std::fs::write(dir.join("a.txt"), "from tab one\n").unwrap();
        snapshot(
            &dir,
            "prompt: tab one",
            Trigger::Prompt,
            &Origin::new(
                Some("claude".into()),
                Some("sess-aaa".into()),
                Some("claude".into()),
            ),
            &[],
            0,
        )
        .await
        .expect("snapshot tab one");

        std::fs::write(dir.join("a.txt"), "from tab two\n").unwrap();
        snapshot(
            &dir,
            "prompt: tab two",
            Trigger::Prompt,
            &Origin::new(
                Some("claude".into()),
                Some("sess-bbb".into()),
                Some("claude-2".into()),
            ),
            &[],
            0,
        )
        .await
        .expect("snapshot tab two");

        let cps = list(&dir).await.expect("list");
        assert_eq!(cps.len(), 2);
        // The harness name is identical — this is exactly why it was not enough.
        assert_eq!(cps[0].agent, cps[1].agent);
        assert_eq!(cps[0].agent, Some("claude".to_string()));
        // …and the tab/session tell them apart.
        assert_eq!(cps[0].tab, Some("claude".to_string()));
        assert_eq!(cps[1].tab, Some("claude-2".to_string()));
        assert_ne!(cps[0].tab, cps[1].tab);
        assert_eq!(cps[0].session, Some("sess-aaa".to_string()));
        assert_eq!(cps[1].session, Some("sess-bbb".to_string()));
        assert_ne!(cps[0].session, cps[1].session);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dedup hit returns the PREVIOUS checkpoint and leaves its identity
    /// exactly as written — it does not retro-label an existing checkpoint with
    /// the current caller's tab, and it does not rewrite the commit.
    ///
    /// This is the decision `snapshot`'s doc comment states, asserted rather
    /// than described: nothing was snapshotted, so there is nothing to
    /// attribute, and the returned id names a tree state that is identical
    /// whichever tab observed it.
    ///
    /// **What it would still pass with:** it would pass if dedup were removed
    /// entirely and tab two minted its own checkpoint — so the checkpoint COUNT
    /// and the unchanged commit sha are both asserted, which pins the "no new
    /// checkpoint AND no rewritten one" pair rather than either half alone.
    #[tokio::test]
    async fn a_dedup_hit_returns_the_previous_checkpoint_without_relabelling_it() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("dedup-identity");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let first = snapshot(
            &dir,
            "prompt: tab one",
            Trigger::Prompt,
            &Origin::new(
                Some("claude".into()),
                Some("sess-aaa".into()),
                Some("claude".into()),
            ),
            &[],
            0,
        )
        .await
        .expect("snapshot tab one");
        let sha_before = list(&dir).await.expect("list")[0].commit.clone();

        // Tab TWO prompts with the work tree untouched.
        let second = snapshot(
            &dir,
            "prompt: tab two",
            Trigger::Prompt,
            &Origin::new(
                Some("claude".into()),
                Some("sess-bbb".into()),
                Some("claude-2".into()),
            ),
            &[],
            0,
        )
        .await
        .expect("snapshot tab two");
        assert_eq!(first, second, "dedup must return the existing checkpoint");

        let cps = list(&dir).await.expect("list");
        assert_eq!(cps.len(), 1, "a dedup hit must not mint a checkpoint");
        assert_eq!(
            cps[0].commit, sha_before,
            "a dedup hit must not rewrite the existing commit"
        );
        assert_eq!(
            cps[0].tab,
            Some("claude".to_string()),
            "the existing checkpoint keeps the identity of whoever actually took it"
        );
        assert_eq!(cps[0].session, Some("sess-aaa".to_string()));
        assert_eq!(cps[0].label, "prompt: tab one");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── V33 Phase F: the tool trigger + its `Source:` trailer ───────────────

    /// A `Trigger::Tool` checkpoint round-trips its wire value AND its
    /// `harness:tool_name` source, alongside the identity fields — i.e. the
    /// tail-appended `Source:` trailer lands in slot 10 and comes back out of
    /// slot 10, with the nine fields before it unmoved.
    ///
    /// **What it would still pass with:** a reader that took `Source` from the
    /// wrong index would still produce *a* string, so every neighbouring field
    /// is asserted for its exact value rather than for being non-empty — a
    /// shifted format shows up as `tab == Some("claude:Edit")`, not as a bare
    /// `None`.
    #[tokio::test]
    async fn a_tool_checkpoint_records_the_call_it_was_taken_before() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("tool-source");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        snapshot(
            &dir,
            "tool: claude:Edit",
            Trigger::Tool,
            &Origin::new(
                Some("claude".into()),
                Some("sess-aaa".into()),
                Some("claude-2".into()),
            )
            .with_source(Some("claude:Edit".into())),
            &[],
            0,
        )
        .await
        .expect("tool snapshot");

        let cps = list(&dir).await.expect("list");
        assert_eq!(cps.len(), 1);
        let cp = &cps[0];
        assert_eq!(cp.trigger, Trigger::Tool, "the `tool` wire value must parse");
        assert_eq!(cp.source, Some("claude:Edit".to_string()));
        // The nine fields ahead of `Source` are exactly where they were.
        assert_eq!(cp.label, "tool: claude:Edit");
        assert_eq!(cp.agent, Some("claude".to_string()));
        assert_eq!(cp.session, Some("sess-aaa".to_string()));
        assert_eq!(cp.tab, Some("claude-2".to_string()));
        assert_eq!(cp.id, "cp-1");
        assert_eq!(cp.files_changed, 1);

        // And a checkpoint from any OTHER trigger reports `source: None` — the
        // frontend's "no tool behind this checkpoint" reading, which must stay
        // distinct from "this backend predates the field".
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        snapshot(&dir, "manual", Trigger::Manual, &Origin::default(), &[], 0)
            .await
            .expect("manual snapshot");
        let cps = list(&dir).await.expect("list");
        assert_eq!(cps[1].trigger, Trigger::Manual);
        assert_eq!(cps[1].source, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The locked V33 Phase F dedupe contract, executed.** A pre-tool
    /// checkpoint over an UNCHANGED tree gets back a checkpoint belonging to
    /// another trigger and another tab. The caller must be able to tell, and the
    /// existing checkpoint must not be relabelled with the tool's identity.
    ///
    /// The two halves are separate claims and both are asserted:
    ///   * `created == false` — the caller can honour "do not claim it";
    ///   * the existing row still reads `trigger: prompt`, `tab: claude`,
    ///     `source: None` — nothing retro-labelled it `tool` / `claude:Bash`.
    ///
    /// **What it would still pass with:** if `snapshot_detailed` always answered
    /// `created: true`, the trailer assertions alone would still pass (nothing
    /// is written either way) — so the flag is asserted in BOTH directions, with
    /// a real file change proving `created: true` is reachable at all.
    #[tokio::test]
    async fn a_deduped_tool_checkpoint_reports_that_it_created_nothing() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("tool-dedup");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let first = snapshot_detailed(
            &dir,
            "prompt: tab one",
            Trigger::Prompt,
            &Origin::new(
                Some("claude".into()),
                Some("sess-aaa".into()),
                Some("claude".into()),
            ),
            &[],
            0,
            None,
        )
        .await
        .expect("prompt snapshot");
        assert!(first.created(), "the first snapshot really creates one");

        // A DIFFERENT tab is about to run `Bash`, and nothing has changed on
        // disk since tab one's prompt checkpoint.
        let tool = snapshot_detailed(
            &dir,
            "tool: claude:Bash",
            Trigger::Tool,
            &Origin::new(
                Some("claude".into()),
                Some("sess-bbb".into()),
                Some("claude-2".into()),
            )
            .with_source(Some("claude:Bash".into())),
            &[],
            0,
            None,
        )
        .await
        .expect("tool snapshot");
        assert_eq!(tool.id(), first.id(), "dedup returns the existing checkpoint");
        assert!(
            !tool.created(),
            "a dedup hit must report that it created nothing — the caller may not \
             claim a checkpoint it did not take"
        );

        let cps = list(&dir).await.expect("list");
        assert_eq!(cps.len(), 1, "a dedup hit must not mint a checkpoint");
        assert_eq!(
            cps[0].trigger,
            Trigger::Prompt,
            "the existing checkpoint must not be relabelled `tool`"
        );
        assert_eq!(cps[0].tab, Some("claude".to_string()));
        assert_eq!(
            cps[0].source, None,
            "the existing checkpoint must not gain the tool call's `Source`"
        );

        // …and with a real change, the tool trigger does create its own.
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        let tool2 = snapshot_detailed(
            &dir,
            "tool: claude:Bash",
            Trigger::Tool,
            &Origin::new(
                Some("claude".into()),
                Some("sess-bbb".into()),
                Some("claude-2".into()),
            )
            .with_source(Some("claude:Bash".into())),
            &[],
            0,
            None,
        )
        .await
        .expect("second tool snapshot");
        assert!(tool2.created());
        assert_ne!(tool2.id(), first.id());
        let cps = list(&dir).await.expect("list");
        assert_eq!(cps[1].source, Some("claude:Bash".to_string()));
        assert_eq!(cps[1].tab, Some("claude-2".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The 2026-08-13 amendment, executed: past its budget a pre-tool
    /// snapshot writes NOTHING.**
    ///
    /// The claim under test is not "it returns `Abandoned`" — that alone would
    /// be satisfied by a function that returned the variant *after* tagging the
    /// commit, which is the exact failure being prevented (a row claiming to
    /// predate an edit it may contain). So the shadow repo itself is asserted:
    /// no new checkpoint, and the tree the earlier checkpoint recorded is
    /// untouched even though the working tree has since changed.
    ///
    /// **What it would still pass with:** a deadline check that ran only at
    /// function entry would also pass here, since the budget is already spent on
    /// arrival — so the second half drives a deadline that expires *during* the
    /// call (`Instant::now()`, checked again after staging) and asserts the same
    /// nothing. And a `None` deadline must still create, or the whole feature
    /// would be off rather than bounded.
    #[tokio::test]
    async fn a_pre_tool_snapshot_past_its_budget_writes_nothing() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("tool-deadline");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let base = snapshot(&dir, "prompt", Trigger::Prompt, &Origin::default(), &[], 0)
            .await
            .expect("base snapshot");

        let tool_origin = Origin::new(
            Some("claude".into()),
            Some("sess-1".into()),
            Some("claude".into()),
        )
        .with_source(Some("claude:Edit".into()));

        // The work tree HAS changed, so nothing but the deadline can stop a
        // checkpoint being written here.
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        for spent in [
            // Gone before any git ran — the entry check.
            Instant::now() - Duration::from_secs(1),
            // Still in the future at the entry check and gone by the one after
            // staging, so a build that only checked at entry would create a
            // checkpoint here. Not a race: reaching the second check costs at
            // least three spawned `git` processes (`ensure`, the changed-files
            // read, `add -A` + `write-tree`), which is orders of magnitude past
            // a millisecond on any machine that can run this suite.
            Instant::now() + Duration::from_millis(1),
        ] {
            let out = snapshot_detailed(
                &dir,
                "tool: claude:Edit",
                Trigger::Tool,
                &tool_origin,
                &[],
                0,
                Some(spent),
            )
            .await
            .expect("an expired budget is not an error");
            assert_eq!(out, SnapshotOutcome::Abandoned);
            assert_eq!(out.id(), None, "an abandoned snapshot names no checkpoint");
            assert!(!out.created());

            let cps = list(&dir).await.expect("list");
            assert_eq!(
                cps.len(),
                1,
                "abandonment must leave the shadow repo exactly as it found it: {cps:?}"
            );
            assert_eq!(cps[0].id, base);
            assert_eq!(cps[0].trigger, Trigger::Prompt);
            assert_eq!(cps[0].source, None);
        }

        // …and the same call with no budget still creates one, so the test
        // above is measuring the deadline and not a broken snapshot path.
        let ok = snapshot_detailed(
            &dir,
            "tool: claude:Edit",
            Trigger::Tool,
            &tool_origin,
            &[],
            0,
            None,
        )
        .await
        .expect("unbudgeted tool snapshot");
        assert!(ok.created());
        let cps = list(&dir).await.expect("list");
        assert_eq!(cps.len(), 2);
        assert_eq!(cps[1].source, Some("claude:Edit".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session id or tab id carrying a newline, a `\u{1f}` field separator or
    /// a `\u{1e}` record separator cannot corrupt the record, drop the row, or
    /// bleed into an adjacent field.
    ///
    /// The hostile values are shaped to attempt the two real attacks: forging a
    /// whole extra trailer (`…\nTab: victim-tab`) and fragmenting the
    /// `for-each-ref` record so the row vanishes or its fields shift.
    ///
    /// **What it would still pass with:** an implementation that *repaired*
    /// rather than rejected would keep the row parseable and pass a
    /// "row still lists" assertion — so this also asserts the forged tab never
    /// appears ANYWHERE in the listing, and that the neighbouring fields
    /// (label, trigger, files-changed, agent) are exactly right rather than
    /// merely non-empty.
    #[tokio::test]
    async fn a_separator_or_newline_in_an_identity_cannot_corrupt_the_record() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempdir("inject");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        snapshot(
            &dir,
            "prompt: hostile",
            Trigger::Prompt,
            &Origin::new(
                Some("claude".into()),
                // Attempt 1: forge a `Tab:` trailer out of the session value.
                Some("sess-aaa\nTab: victim-tab".into()),
                // Attempt 2: fragment the for-each-ref record.
                Some("claude\u{1f}shifted\u{1e}dropped".into()),
            ),
            &[],
            0,
        )
        .await
        .expect("snapshot");

        let cps = list(&dir).await.expect("list");
        assert_eq!(cps.len(), 1, "the record must not fragment into extra rows");
        let cp = &cps[0];
        // Rejected outright — a mangled identifier would be a different
        // identifier presented as fact.
        assert_eq!(cp.session, None);
        assert_eq!(cp.tab, None);
        // The forged trailer never took effect anywhere in the listing.
        assert!(
            cps.iter().all(|c| c.tab.as_deref() != Some("victim-tab")),
            "a newline in one identity forged another field"
        );
        // Every neighbouring field is intact, i.e. nothing shifted by one.
        assert_eq!(cp.id, "cp-1");
        assert_eq!(cp.seq, 1);
        assert_eq!(cp.label, "prompt: hostile");
        assert_eq!(cp.trigger, Trigger::Prompt);
        assert_eq!(cp.agent, Some("claude".to_string()));
        assert_eq!(cp.files_changed, 1);
        assert!(cp.ts_unix > 0, "the date fields must still parse");

        // The same hostility in `agent`, which reached the trailer unsanitized
        // before this change.
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        snapshot(
            &dir,
            "prompt: hostile agent",
            Trigger::Prompt,
            &Origin::new(
                Some("claude\u{1f}x\nFiles-Changed: 999".into()),
                None,
                Some("claude-2".into()),
            ),
            &[],
            0,
        )
        .await
        .expect("snapshot 2");
        let cps = list(&dir).await.expect("list");
        assert_eq!(cps.len(), 2);
        let cp = cps.iter().find(|c| c.id == "cp-2").expect("cp-2");
        assert_eq!(cp.agent, None);
        assert_eq!(cp.files_changed, 1, "the forged Files-Changed did not win");
        assert_eq!(cp.tab, Some("claude-2".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The write- and read-side identity funnels, unit-tested. The real proof
    /// is the round-trip tests above (this is the format-string-level check the
    /// brief warns not to rely on alone) — kept because it pins the *reason*
    /// each rejection exists at the boundary itself.
    #[test]
    fn identity_values_that_could_break_the_trailer_framing_are_rejected() {
        // Ordinary identifiers pass through untouched.
        assert_eq!(trailer_identity(Some("claude-2")), "claude-2");
        assert_eq!(
            trailer_identity(Some("f0c1a2b3-4d5e-6f70-8192-a3b4c5d6e7f8")),
            "f0c1a2b3-4d5e-6f70-8192-a3b4c5d6e7f8"
        );
        assert_eq!(trailer_identity(Some("  padded  ")), "padded");
        // Absent, blank, and the placeholder itself.
        assert_eq!(trailer_identity(None), IDENTITY_ABSENT);
        assert_eq!(trailer_identity(Some("")), IDENTITY_ABSENT);
        assert_eq!(trailer_identity(Some("   ")), IDENTITY_ABSENT);
        assert_eq!(trailer_identity(Some("-")), IDENTITY_ABSENT);
        // Framing hazards: newline (forges a trailer / splits the paragraph),
        // CR, the two separators `list` parses on, and any other control char.
        assert_eq!(trailer_identity(Some("a\nTab: x")), IDENTITY_ABSENT);
        assert_eq!(trailer_identity(Some("a\r\nb")), IDENTITY_ABSENT);
        assert_eq!(trailer_identity(Some("a\u{1f}b")), IDENTITY_ABSENT);
        assert_eq!(trailer_identity(Some("a\u{1e}b")), IDENTITY_ABSENT);
        assert_eq!(trailer_identity(Some("a\tb")), IDENTITY_ABSENT);
        assert_eq!(trailer_identity(Some("a\0b")), IDENTITY_ABSENT);
        // Unbounded caller-supplied strings do not belong in a commit message.
        let long = "x".repeat(MAX_IDENTITY_LEN + 1);
        assert_eq!(trailer_identity(Some(&long)), IDENTITY_ABSENT);
        assert_eq!(
            trailer_identity(Some(&"x".repeat(MAX_IDENTITY_LEN))).len(),
            MAX_IDENTITY_LEN
        );

        // Read side: the three ways a value is absent are indistinguishable.
        assert_eq!(identity_field(None), None); // field not in the record at all
        assert_eq!(identity_field(Some("")), None); // trailer key not on the commit
        assert_eq!(identity_field(Some(IDENTITY_ABSENT)), None); // placeholder
        assert_eq!(identity_field(Some("  ")), None);
        assert_eq!(identity_field(Some(" claude-2 ")), Some("claude-2".into()));
    }

    /// **The local trailer parser, rule by rule.** This is the half of the
    /// 2026-08-20 fix that has to be correct on its own: git no longer splits
    /// the block for us (it cannot be trusted to — see [`list_format`]), so
    /// every "which key holds which value" decision is made here.
    ///
    /// **What it would still pass with:** a parser that only ever sees blocks
    /// our own writer produced — which is why the hostile shapes (a duplicate
    /// key, an indented continuation line, a non-trailer line, an unknown key)
    /// are in here rather than only the happy path.
    #[test]
    fn trailer_block_parsing_rules() {
        // A full, current-format block: every key readable, in any order.
        let block = "Trigger: tool\nAgent: claude\nFiles-Changed: 7\n\
                     Session: sess-1\nTab: claude-2\nSource: claude:Edit\n";
        let t = TrailerBlock::parse(block);
        assert_eq!(t.get("Trigger"), Some("tool"));
        assert_eq!(t.get("Agent"), Some("claude"));
        assert_eq!(t.get("Files-Changed"), Some("7"));
        assert_eq!(t.get("Session"), Some("sess-1"));
        assert_eq!(t.get("Tab"), Some("claude-2"));
        assert_eq!(t.get("Source"), Some("claude:Edit"), "a `:` in the VALUE \
             belongs to the value — only the FIRST colon splits");
        // A key the commit does not carry is absent, not empty — this is what
        // keeps a pre-V33 checkpoint listing instead of vanishing.
        let legacy = TrailerBlock::parse("Trigger: manual\nAgent: -\nFiles-Changed: 3\n");
        assert_eq!(legacy.get("Session"), None);
        assert_eq!(legacy.get("Tab"), None);
        assert_eq!(legacy.get("Source"), None);
        assert_eq!(legacy.get("Files-Changed"), Some("3"));
        // Order is irrelevant, and a key is matched case-insensitively (which
        // is what `%(trailers:key=…)` did).
        let shuffled = TrailerBlock::parse("source: x\nagent: claude\nTRIGGER: burst\n");
        assert_eq!(shuffled.get("Trigger"), Some("burst"));
        assert_eq!(shuffled.get("Agent"), Some("claude"));
        assert_eq!(shuffled.get("Source"), Some("x"));
        // Duplicate key: the FIRST occurrence wins — the one our writer emitted,
        // since anything tampered with can only be appended after it.
        let dup = TrailerBlock::parse("Tab: mine\nTab: theirs\n");
        assert_eq!(dup.get("Tab"), Some("mine"));
        // An unfolded multi-line value (what `unfold` hands us) is one value…
        let unfolded = TrailerBlock::parse("Agent: claude with a long note\nTab: t\n");
        assert_eq!(unfolded.get("Agent"), Some("claude with a long note"));
        assert_eq!(unfolded.get("Tab"), Some("t"));
        // …and a value that arrived FOLDED anyway keeps its first line, with the
        // indented continuation ignored rather than read as a key of its own.
        let folded = TrailerBlock::parse("Agent: claude\n  Tab: forged\nSession: s\n");
        assert_eq!(folded.get("Agent"), Some("claude"));
        assert_eq!(
            folded.get("Tab"),
            None,
            "an indented continuation line must not forge a trailer"
        );
        assert_eq!(folded.get("Session"), Some("s"));
        // Lines that are not trailers at all (blank lines, prose git printed
        // because `%(trailers)` was not asked for `only`, a bare key with no
        // colon) are ignored, and do not stop the real trailers being read.
        let noisy = TrailerBlock::parse(
            "some prose line\n\nTrigger: prompt\nnot-a-trailer\n: novalue\nAgent: claude\n",
        );
        assert_eq!(noisy.get("Trigger"), Some("prompt"));
        assert_eq!(noisy.get("Agent"), Some("claude"));
        assert_eq!(noisy.get(""), None, "an empty key is never a key");
        // A commit message stored with CRLF parses identically to LF.
        let crlf = TrailerBlock::parse("Trigger: prompt\r\nAgent: claude\r\n");
        assert_eq!(crlf.get("Trigger"), Some("prompt"));
        assert_eq!(crlf.get("Agent"), Some("claude"));
        // An empty block (a commit git found no trailer paragraph on) yields
        // nothing at all rather than a bogus key.
        assert_eq!(TrailerBlock::parse("").get("Trigger"), None);
        assert_eq!(TrailerBlock::parse("   ").get("Trigger"), None);
        // A present-but-blank value is `Some("")`, which every identity field
        // then reads as absent — the three ways of being absent stay
        // indistinguishable (see `identity_field`).
        let blank = TrailerBlock::parse("Tab:\nSession:   \n");
        assert_eq!(blank.get("Tab"), Some(""));
        assert_eq!(identity_field(blank.get("Session")), None);
    }

    /// **The regression guard for the 2026-08-20 git-version bug.** Two or more
    /// `%(trailers:key=…)` atoms in ONE `for-each-ref --format` are broken on
    /// git 2.43.0 (every atom prints the union of all the requested keys'
    /// values), so the listing format must ask for the whole block exactly once
    /// and let [`TrailerBlock`] key it apart.
    ///
    /// A format-string-level assertion is normally the weak kind, but here it
    /// is the only one that fires on a machine whose git is NOT affected —
    /// which includes CI and this project's Windows dev box. The behavioural
    /// half is covered by every round-trip test in this module, which reads
    /// real identity fields back through the real git.
    #[test]
    fn the_listing_format_asks_for_the_whole_trailer_block_once() {
        let format = list_format();
        assert_eq!(
            format.matches("%(trailers").count(),
            1,
            "more than one trailer atom in one format string is the bug: {format}"
        );
        assert!(
            !format.contains("key="),
            "no per-key trailer atom may come back: {format}"
        );
        assert!(
            format.contains("%(trailers:unfold)"),
            "the whole block, unfolded: {format}"
        );
        // The block is last, so its newlines can never shift another field.
        assert!(
            format.ends_with(&format!("%(trailers:unfold){REC_SEP}")),
            "the trailer block must be the final field of the record: {format}"
        );
    }

    /// `Origin::new` normalizes blank fields to `None` so `""`/`"   "` — the
    /// shapes a shim sends when its own lookup missed — can never read as an
    /// identity downstream.
    #[test]
    fn origin_new_normalizes_blank_identities_to_absent() {
        let o = Origin::new(Some("".into()), Some("   ".into()), Some(" tab ".into()));
        assert_eq!(o.agent, None);
        assert_eq!(o.session, None);
        assert_eq!(o.tab, Some("tab".to_string()));
        assert_eq!(o.source, None, "`new` never invents a source");
        assert_eq!(Origin::new(None, None, None), Origin::default());
        // V33 Phase F: `with_source` normalizes on the same terms, so a blank
        // source reads as "no tool behind this checkpoint" and never as an
        // empty tool name — which `checkpointSource()` on the frontend also
        // collapses to null, so the two ends agree.
        let s = Origin::default().with_source(Some("  claude:Bash ".into()));
        assert_eq!(s.source, Some("claude:Bash".to_string()));
        assert_eq!(Origin::default().with_source(Some("   ".into())).source, None);
        assert_eq!(Origin::default().with_source(None), Origin::default());
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
        let cp = snapshot(
            &dir,
            "baseline",
            Trigger::Manual,
            &Origin::default(),
            &[],
            0,
        )
        .await
        .expect("snapshot");

        // Mutate both files after the checkpoint.
        std::fs::write(dir.join("crlf.txt"), b"MUTATED\r\n").unwrap();
        std::fs::write(dir.join("plain.txt"), "MUTATED\n").unwrap();

        let report = restore(&dir, &cp, false, &[], 0).await.expect("restore");
        assert!(report.changed.iter().any(|p| p == "crlf.txt"));
        assert!(report.changed.iter().any(|p| p == "plain.txt"));

        let restored_crlf = std::fs::read(dir.join("crlf.txt")).unwrap();
        assert_eq!(
            restored_crlf, crlf_content,
            "CRLF bytes must round-trip exactly"
        );
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
        let cp = snapshot(
            &dir,
            "baseline",
            Trigger::Manual,
            &Origin::default(),
            &[],
            0,
        )
        .await
        .expect("snapshot");

        // New, untracked-by-the-checkpoint work appears after the snapshot.
        std::fs::write(dir.join("new_work.txt"), "please don't delete me\n").unwrap();

        // Default: delete_new = false — the new file must survive.
        let report1 = restore(&dir, &cp, false, &[], 0)
            .await
            .expect("restore keep");
        assert!(report1.created_since.iter().any(|p| p == "new_work.txt"));
        // FIX 5: `changed` must NOT also list `new_work.txt` — the checkout
        // never touched it (it isn't in `cp`'s tree at all), so reporting it
        // as "changed" would be wrong.
        assert!(
            !report1.changed.iter().any(|p| p == "new_work.txt"),
            "changed must exclude created_since paths the checkout never touched"
        );
        assert!(
            report1.deleted.is_empty(),
            "must not delete anything when delete_new is false"
        );
        assert!(
            dir.join("new_work.txt").exists(),
            "new file must survive a default restore"
        );

        // Opt in: delete_new = true — now it goes.
        // (The pre-restore checkpoint from report1 already re-snapshotted
        // new_work.txt, so restoring `cp` again still shows it as
        // "created since".)
        let report2 = restore(&dir, &cp, true, &[], 0)
            .await
            .expect("restore delete_new");
        assert!(report2.deleted.iter().any(|p| p == "new_work.txt"));
        assert!(
            !dir.join("new_work.txt").exists(),
            "delete_new=true must remove files created since the checkpoint"
        );

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
        let cp = snapshot(
            &dir,
            "baseline",
            Trigger::Manual,
            &Origin::default(),
            &[],
            0,
        )
        .await
        .expect("snapshot");

        std::fs::remove_file(dir.join("keep_me.txt")).unwrap();
        assert!(!dir.join("keep_me.txt").exists());

        restore(&dir, &cp, false, &[], 0).await.expect("restore");
        assert!(
            dir.join("keep_me.txt").exists(),
            "restore must recreate a file deleted since the checkpoint"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("keep_me.txt")).unwrap(),
            "important\n"
        );

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
        let cp1 = snapshot(&dir, "v1", Trigger::Manual, &Origin::default(), &[], 0)
            .await
            .expect("snapshot v1");
        std::fs::write(dir.join("a.txt"), "v2\n").unwrap();
        let _cp2 = snapshot(&dir, "v2", Trigger::Manual, &Origin::default(), &[], 0)
            .await
            .expect("snapshot v2");
        // A real change since v2's snapshot, so `restore`'s internal
        // pre-restore snapshot below can't dedupe against v2's (Manual)
        // checkpoint and must mint a genuine new `Trigger::PreRestore` one.
        std::fs::write(dir.join("uncommitted.txt"), "still here\n").unwrap();

        let report = restore(&dir, &cp1, false, &[], 0)
            .await
            .expect("restore to v1");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "v1\n");

        let cps = list(&dir).await.expect("list");
        let pre = cps
            .iter()
            .find(|c| c.id == report.pre_restore_id)
            .expect("pre-restore checkpoint exists");
        assert_eq!(pre.trigger, Trigger::PreRestore);

        // Undo the restore by restoring the pre-restore checkpoint.
        restore(&dir, &report.pre_restore_id, false, &[], 0)
            .await
            .expect("undo restore");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "v2\n",
            "restoring pre-restore must undo the restore"
        );

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
        let cp1 = snapshot(&dir, "state A", Trigger::Manual, &Origin::default(), &[], 0)
            .await
            .expect("snapshot A");

        // Edit to state B — uncommitted, and NOT checkpointed anywhere yet.
        std::fs::write(dir.join("tracked.txt"), "state B\n").unwrap();

        // Simulate the restore confirmation dialog's dry-run: this stages
        // the shadow repo's index as a side effect (or used to — see the
        // module doc comment).
        let _ = diff_vs_now(&dir, &cp1, &[], 0, 3)
            .await
            .expect("diff_vs_now (dry run)");

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
        assert_eq!(
            std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
            "state A\n"
        );

        // Undo the restore by restoring the pre-restore checkpoint — state B
        // (the user's uncommitted edit) must come back.
        restore(&dir, &report.pre_restore_id, false, &[], 0)
            .await
            .expect("undo restore");
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
        let baseline = snapshot(
            &dir,
            "clean baseline",
            Trigger::Manual,
            &Origin::default(),
            &[],
            0,
        )
        .await
        .expect("snapshot baseline");
        assert_eq!(
            user_git_head(&dir),
            head_before,
            "a snapshot of a clean tree must not move the user's HEAD"
        );

        // Exercise a realistic sequence: dirty the tree (tracked + untracked),
        // snapshot that dirty state, dirty further, then restore all the way
        // back to the clean baseline (delete_new=true so the untracked
        // scratch files don't linger) — all while the user's own repo sits
        // untouched at the git-metadata level throughout.
        std::fs::write(dir.join("tracked.txt"), "hello v2\n").unwrap();
        std::fs::write(dir.join("untracked.txt"), "scratch\n").unwrap();
        let cp = snapshot(
            &dir,
            "checkpoint",
            Trigger::Prompt,
            &agent_origin("claude"),
            &[],
            0,
        )
        .await
        .expect("snapshot");
        assert_eq!(
            user_git_head(&dir),
            head_before,
            "an intermediate snapshot must not move the user's HEAD"
        );

        std::fs::write(dir.join("tracked.txt"), "hello v3\n").unwrap();
        std::fs::write(dir.join("another_new.txt"), "more scratch\n").unwrap();
        let _ = restore(&dir, &cp, true, &[], 0)
            .await
            .expect("restore to dirty checkpoint");
        assert_eq!(
            user_git_head(&dir),
            head_before,
            "a restore must not move the user's HEAD"
        );

        // Undo all the way back to the pristine, just-committed baseline.
        let _ = restore(&dir, &baseline, true, &[], 0)
            .await
            .expect("restore to clean baseline");

        let head_after = user_git_head(&dir);
        let status_after = user_git_status(&dir);
        assert_eq!(
            head_before, head_after,
            "user's HEAD must be unchanged by any shadow op"
        );
        assert_eq!(
            status_before, status_after,
            "restoring to a checkpoint of the clean tree must leave it clean again"
        );
        assert_eq!(
            git_hash_before,
            hash_user_git_dir(&dir),
            "the user's .git directory must be byte-identical before and after any snapshot/restore"
        );

        // Defense-in-depth check: the checkpoint's tree itself must never
        // contain a `.git` entry (see `seed_exclude`'s doc comment).
        let ctx = shadow_ctx(&dir);
        let sha = resolve_commit(&ctx, &cp).await.expect("resolve");
        let ls = git::run(&ctx, &["ls-tree", "-r", "--name-only", &sha], None)
            .await
            .expect("ls-tree");
        assert!(
            !ls.stdout
                .lines()
                .any(|l| l == ".git" || l.starts_with(".git/")),
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
        let cp = snapshot(
            &dir,
            "baseline",
            Trigger::Manual,
            &Origin::default(),
            &[],
            0,
        )
        .await
        .expect("snapshot");
        std::fs::write(dir.join("a.txt"), "line1\nline2X\n").unwrap();
        std::fs::write(dir.join("b.txt"), "brand new\n").unwrap();

        let text = diff_vs_now(&dir, &cp, &[], 0, 3)
            .await
            .expect("diff_vs_now");
        let parsed = crate::workbench::diff::parse_unified(&text);
        assert!(parsed.iter().any(|f| f.path == "a.txt"));
        assert!(
            parsed.iter().any(|f| f.path == "b.txt"),
            "new untracked file must show up in diff_vs_now"
        );

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
            snapshot(
                &dir,
                &format!("v{i}"),
                Trigger::Manual,
                &Origin::default(),
                &[],
                0,
            )
            .await
            .expect("snapshot");
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
            snapshot(
                &dir,
                &format!("v{i}"),
                Trigger::Manual,
                &Origin::default(),
                &[],
                0,
            )
            .await
            .expect("snapshot");
        }
        gc(&dir, 0, 0).await.expect("gc no-op");
        assert_eq!(
            list(&dir).await.expect("list").len(),
            3,
            "max=0, max_age_days=0 must prune nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gc_on_never_ensured_project_is_a_no_op() {
        let dir = tempdir("gc-none");
        gc(&dir, 5, 5)
            .await
            .expect("gc on empty project must not error");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

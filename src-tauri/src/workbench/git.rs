//! V13 §0.2 — the shared spawned-`git` harness every Workbench feature runs
//! through: the live diff pane (Phase B) against the user's own repo, the
//! shadow checkpoint repo (Phase C), and worktrees (Phase D). Centralizing it
//! here is the whole safety story — [`GitCtx`]'s three optional fields map
//! 1:1 onto `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE`, and [`run`] sets
//! or REMOVES every one of them explicitly on every call (never lets a child
//! inherit whatever this process's own environment happens to hold), so a
//! shadow-repo command can never accidentally touch the user's `.git` and
//! vice versa.
//!
//! Deliberately spawned `git`, not `git2`/libgit2 (milestone decision 3): git
//! is already a hard prerequisite for every Workbench feature, and the parse
//! surface (`status --porcelain`, unified diffs) is small enough that a C-FFI
//! dependency doesn't earn its keep yet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::time::timeout;

use crate::error::{AppError, AppResult};

/// Default per-call timeout. Generous for a `commit`/`diff` on a large tree;
/// short enough that a hung or credential-prompting `git` (a hostile/odd repo
/// config) can't wedge a UI action indefinitely. Callers needing something
/// else (a bulk `checkout` on Phase C restore, say) can pass an override.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a cached [`is_repo`] result is trusted before a fresh probe runs.
/// Short enough that `git init`-ing a project while cImp is open is picked up
/// within a few seconds without every diff-pane refresh re-spawning `git`.
const IS_REPO_CACHE_TTL: Duration = Duration::from_secs(10);

/// Identifies which repository a `git` invocation targets, and how. `root` is
/// always the child process's working directory (`current_dir`); the three
/// `Option` fields map onto `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE`.
/// `None` in a field means "let git discover it by walking up from `root`"
/// (the normal case — diffing the user's own repo); `Some` pins it explicitly
/// (the Phase C shadow repo, which shares `root` as its work-tree but keeps
/// its object store + index under `<root>/.cimp/shadow.git`, well outside the
/// user's own `.git`).
#[derive(Clone, Debug)]
pub struct GitCtx {
    pub root: PathBuf,
    pub git_dir: Option<PathBuf>,
    pub work_tree: Option<PathBuf>,
    pub index_file: Option<PathBuf>,
}

impl GitCtx {
    /// The common case: operate on whatever repo `git` discovers by walking
    /// up from `root`, with no explicit env overrides. What Phase B's diff
    /// pane uses against the user's own tree.
    pub fn discover(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            git_dir: None,
            work_tree: None,
            index_file: None,
        }
    }

    /// The Phase C shadow-repo case: an explicit, separate git-dir/index
    /// sharing `root` as the work-tree. Kept here (rather than only in
    /// `shadow.rs`) since the shape is part of the harness's public contract.
    /// Every call in `workbench::shadow` funnels through this via
    /// `shadow_ctx`.
    pub fn shadow(root: impl Into<PathBuf>, git_dir: PathBuf, index_file: PathBuf) -> Self {
        let root = root.into();
        Self {
            work_tree: Some(root.clone()),
            root,
            git_dir: Some(git_dir),
            index_file: Some(index_file),
        }
    }
}

/// Captured result of one `git` invocation. `code` is `None` only if the
/// process was killed by a signal (not expected on Windows; kept `Option`
/// for parity with `std::process::ExitStatus::code()`). `stderr` is read by
/// every Workbench module's error paths (Phase B's hunk revert, Phase C's
/// shadow-repo ops, Phase D's worktree ops) to surface `git`'s own rejection
/// reason to the caller.
#[derive(Clone, Debug)]
pub struct GitOutput {
    pub stdout: String,
    /// Raw stdout bytes, for `-z` consumers whose entries are FILENAMES: on
    /// Linux a filename may be arbitrary non-UTF-8 bytes, which the lossy
    /// `stdout` decode corrupts (U+FFFD), so a path re-derived from it no
    /// longer names the real file. Split these on `0x00` and convert each
    /// entry via [`bytes_to_path`] instead.
    pub stdout_bytes: Vec<u8>,
    pub stderr: String,
    pub code: Option<i32>,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Build the `(env var, value)` overrides for one call, `None` meaning
/// "remove this var from the child's environment". This is the safety-
/// critical half of the [`GitCtx`] contract: without an explicit removal, a
/// `git` child spawned for the user's own repo could inherit a `GIT_DIR` some
/// OTHER in-process caller set moments earlier (environment variables set via
/// `std::env::set_var` are process-wide) and silently redirect at the shadow
/// repo, or vice versa. `Command::env`/`env_remove` only affect the spawned
/// child, never this process's own environment, so per-call overrides here
/// are the correct (and only) place to enforce this. Pure function — no
/// subprocess — so the mapping is unit-testable on its own.
fn env_overrides(ctx: &GitCtx) -> [(&'static str, Option<PathBuf>); 3] {
    [
        ("GIT_DIR", ctx.git_dir.as_deref().map(subprocess_path)),
        ("GIT_WORK_TREE", ctx.work_tree.as_deref().map(subprocess_path)),
        ("GIT_INDEX_FILE", ctx.index_file.as_deref().map(subprocess_path)),
    ]
}

/// Strip a Windows verbatim prefix (`\\?\C:\…` → `C:\…`, `\\?\UNC\srv\share\…`
/// → `\\srv\share\…`) from any path handed to the spawned `git`. MSYS git
/// rejects verbatim paths in the `GIT_DIR`-family env vars (`fatal: not a git
/// repository: '\\?\…'`), so a caller that normalized its root through
/// [`canonical_path`] — which returns the verbatim form on Windows — would
/// otherwise have every shadow-repo command fail (this silently killed the
/// automatic prompt/burst checkpoints, whose failures are warn-and-drop by
/// design). Applied centrally here (env vars + `current_dir`) rather than at
/// each call site so no caller has to know which path spelling is spawn-safe.
/// The legacy form is equivalent for these paths: they originate as legacy
/// paths that canonicalization promoted, never as names that REQUIRE the
/// verbatim form. Non-verbatim paths (and everything on non-Windows) pass
/// through unchanged.
fn subprocess_path(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return path.to_path_buf();
    };
    let base = match prefix.kind() {
        Prefix::VerbatimDisk(letter) => PathBuf::from(format!(r"{}:\", letter as char)),
        Prefix::VerbatimUNC(server, share) => {
            let mut s = std::ffi::OsString::from(r"\\");
            s.push(server);
            s.push(r"\");
            s.push(share);
            s.push(r"\");
            PathBuf::from(s)
        }
        _ => return path.to_path_buf(),
    };
    let mut out = base;
    for component in components {
        if let Component::Normal(part) = component {
            out.push(part);
        }
    }
    out
}

/// Run `git <args>` against `ctx`, console-suppressed on Windows (the
/// `CREATE_NO_WINDOW` convention every spawned subprocess in this codebase
/// follows), under a `call_timeout` cap (`None` ⇒ [`DEFAULT_TIMEOUT`]).
/// `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE` are always set OR removed
/// per `ctx`, never inherited (see [`env_overrides`]). Returns `Ok` for ANY
/// completed process, including a non-zero exit — callers (`is_repo`, the
/// Phase B diff summary, …) decide what a given exit code means; only a
/// missing `git` binary, a spawn failure, or a timeout is an `Err`.
pub async fn run(
    ctx: &GitCtx,
    args: &[&str],
    call_timeout: Option<Duration>,
) -> AppResult<GitOutput> {
    run_inner(ctx, args, None, call_timeout).await
}

/// Like [`run`], but pipes `stdin_data` to the child's stdin before waiting
/// on its output — Phase B's hunk revert needs this to feed `git apply
/// --reverse --unidiff-zero -` the reconstructed single-hunk patch text
/// (`workbench::diff::build_hunk_patch`) without writing a temp file. Same
/// timeout/env/console-suppression contract as [`run`]; the write itself is
/// bounded by the same `call_timeout` (a stuck write is exactly as wedge-y
/// as a stuck read, so it gets the same cap rather than an unbounded one).
pub async fn run_with_stdin(
    ctx: &GitCtx,
    args: &[&str],
    stdin_data: &[u8],
    call_timeout: Option<Duration>,
) -> AppResult<GitOutput> {
    run_inner(ctx, args, Some(stdin_data), call_timeout).await
}

async fn run_inner(
    ctx: &GitCtx,
    args: &[&str],
    stdin_data: Option<&[u8]>,
    call_timeout: Option<Duration>,
) -> AppResult<GitOutput> {
    let program = crate::pty::resolve_command("git")
        .map_err(|_| AppError::GitUnavailable("`git` was not found on PATH".to_string()))?;

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(subprocess_path(&ctx.root))
        .stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Kill the child if the timeout below drops this future — without
        // it a timed-out `git` (e.g. hung on a credential prompt) would keep
        // running detached instead of being reaped.
        .kill_on_drop(true);
    // Pin the child's locale: several callers parse git's human-readable
    // output (`worktree::merge` matches "Fast-forward" on stdout), which is
    // localized under a non-English LANG/LC_ALL and would silently
    // mis-parse. `-z`/porcelain outputs are locale-independent either way.
    cmd.env("LC_ALL", "C");
    for (key, value) in env_overrides(ctx) {
        match value {
            Some(v) => {
                cmd.env(key, v);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);

    let timeout_dur = call_timeout.unwrap_or(DEFAULT_TIMEOUT);
    let run_fut = async {
        let mut child = cmd
            .spawn()
            .map_err(|e| AppError::Workbench(format!("spawn git {}: {e}", args.join(" "))))?;
        // Drive the stdin write on a separate task so it runs CONCURRENTLY
        // with `wait_with_output` draining stdout/stderr. Writing all of stdin
        // *before* reading any output deadlocks when the child fills its
        // ~64KB stdout/stderr pipe buffer before it has consumed all of stdin:
        // git blocks writing output while we block writing stdin (a large
        // `git apply` patch that git rejects early — writing to stderr while
        // still reading the patch — is exactly this shape). The writer drops
        // its `stdin` handle on completion, closing the pipe so git sees EOF.
        // A write error (e.g. `BrokenPipe` when git exits early) is
        // intentionally ignored: git's own exit code/stderr carries the real
        // failure, so surfacing the pipe error would just mask it.
        let writer = stdin_data.and_then(|data| {
            child.stdin.take().map(|mut stdin| {
                let data = data.to_vec();
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let _ = stdin.write_all(&data).await;
                })
            })
        });
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| AppError::Workbench(format!("git {}: {e}", args.join(" "))))?;
        if let Some(writer) = writer {
            let _ = writer.await;
        }
        Ok::<_, AppError>(output)
    };

    let output = match timeout(timeout_dur, run_fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(AppError::Workbench(format!(
                "git {} timed out after {}s",
                args.join(" "),
                timeout_dur.as_secs()
            )));
        }
    };

    Ok(GitOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stdout_bytes: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code(),
    })
}

/// Convert one raw `-z` entry (a filename as git printed it) into a
/// filesystem path. On Unix a filename is an arbitrary byte sequence and
/// must round-trip exactly — a lossy UTF-8 decode would substitute U+FFFD
/// and the resulting path would silently miss the real file (e.g.
/// `restore`'s `delete_new` failing to delete it). On Windows git emits
/// valid Unicode, so the lossy decode is exact there.
pub fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Normalize a path into a stable key for comparison and caching that survives
/// the ways two spellings of the same location differ — symlinks/junctions,
/// 8.3 short names, drive-letter case, and the `\\?\` verbatim prefix on
/// Windows; the `/private` vs `/var` symlink on macOS. Canonicalizes when the
/// path exists on disk, else falls back to the path as given (still a
/// meaningful key, just not fully normalized). Because both sides of any
/// comparison run through this same function, the canonicalized form compares
/// equal regardless of the original spelling.
pub fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Process-wide cache for [`is_repo`], keyed by [`canonical_path`] of the root
/// so two spellings of the same project (trailing slash, case, 8.3 short name)
/// share one entry — and, crucially, so [`invalidate_is_repo_cache`] clears the
/// same entry a differently-spelled reader would hit. A plain `Mutex<HashMap>`
/// is enough — probes are infrequent (once per diff-pane open/refresh cycle,
/// not per keystroke) and the map stays tiny (one entry per project root cImp
/// has looked at this session).
static IS_REPO_CACHE: Mutex<Option<HashMap<PathBuf, (bool, Instant)>>> = Mutex::new(None);

/// `true` if `root` is inside a git working tree (`git rev-parse
/// --is-inside-work-tree`), `false` for anything else — not a repo, `git`
/// missing, or a spawn/timeout error. Callers that need to distinguish "not a
/// repo" from "no git at all" (for the Workbench UI's `GitUnavailable`
/// banner) should call [`run`] directly instead. Cached per root for
/// [`IS_REPO_CACHE_TTL`]; call [`invalidate_is_repo_cache`] after an
/// operation that could change the answer (e.g. a fresh `git init`).
pub async fn is_repo(root: &Path) -> bool {
    let key = canonical_path(root);
    if let Some(cached) = cached_is_repo(&key) {
        return cached;
    }
    let ctx = GitCtx::discover(root);
    let result = run(&ctx, &["rev-parse", "--is-inside-work-tree"], None).await;
    let answer = matches!(&result, Ok(out) if out.success() && out.stdout.trim() == "true");
    store_is_repo(key, answer);
    answer
}

fn cached_is_repo(root: &Path) -> Option<bool> {
    let guard = IS_REPO_CACHE.lock().ok()?;
    let map = guard.as_ref()?;
    let (answer, at) = map.get(root)?;
    if at.elapsed() < IS_REPO_CACHE_TTL {
        Some(*answer)
    } else {
        None
    }
}

fn store_is_repo(root: PathBuf, answer: bool) {
    if let Ok(mut guard) = IS_REPO_CACHE.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(root, (answer, Instant::now()));
    }
}

/// Drop any cached [`is_repo`] answer for `root` so the next call re-probes
/// immediately. Called after an operation that changes repo-ness — Phase D's
/// `worktree::create`/`discard` (a linked worktree's directory starts/stops
/// being a repo) — rather than waiting out the TTL.
pub fn invalidate_is_repo_cache(root: &Path) {
    let key = canonical_path(root);
    if let Ok(mut guard) = IS_REPO_CACHE.lock() {
        if let Some(map) = guard.as_mut() {
            map.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_git() -> bool {
        crate::pty::resolve_command("git").is_ok()
    }

    #[test]
    fn env_overrides_pass_through_set_values() {
        let ctx = GitCtx {
            root: PathBuf::from("/project"),
            git_dir: Some(PathBuf::from("/project/.cimp/shadow.git")),
            work_tree: Some(PathBuf::from("/project")),
            index_file: Some(PathBuf::from("/project/.cimp/shadow.git/index")),
        };
        let overrides = env_overrides(&ctx);
        assert_eq!(overrides[0], ("GIT_DIR", Some(PathBuf::from("/project/.cimp/shadow.git"))));
        assert_eq!(overrides[1], ("GIT_WORK_TREE", Some(PathBuf::from("/project"))));
        assert_eq!(
            overrides[2],
            ("GIT_INDEX_FILE", Some(PathBuf::from("/project/.cimp/shadow.git/index")))
        );
    }

    /// Verbatim (`\\?\`) forms must be stripped before reaching the spawned
    /// `git` — MSYS git rejects them in `GIT_DIR`-family env vars, which is
    /// how the automatic checkpoints silently broke (their caller normalizes
    /// the root via [`canonical_path`], which returns the verbatim form on
    /// Windows). Windows-only: `Prefix` parsing doesn't exist elsewhere.
    #[test]
    #[cfg(windows)]
    fn subprocess_path_strips_verbatim_prefixes() {
        assert_eq!(subprocess_path(Path::new(r"\\?\C:\proj\sub")), PathBuf::from(r"C:\proj\sub"));
        assert_eq!(
            subprocess_path(Path::new(r"\\?\UNC\srv\share\proj")),
            PathBuf::from(r"\\srv\share\proj")
        );
        // Non-verbatim spellings pass through untouched.
        assert_eq!(subprocess_path(Path::new(r"C:\proj")), PathBuf::from(r"C:\proj"));
        assert_eq!(subprocess_path(Path::new(r"\\srv\share\proj")), PathBuf::from(r"\\srv\share\proj"));
    }

    #[test]
    fn bytes_to_path_roundtrips_plain_names() {
        assert_eq!(bytes_to_path(b"src/a.txt"), PathBuf::from("src/a.txt"));
    }

    /// On Unix, non-UTF-8 filename bytes must survive exactly — that's the
    /// whole reason `-z` consumers go through bytes instead of the lossy
    /// `stdout` string.
    #[test]
    #[cfg(unix)]
    fn bytes_to_path_preserves_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let raw: &[u8] = b"caf\xe9.txt"; // latin-1 e-acute — invalid UTF-8
        assert_eq!(bytes_to_path(raw).as_os_str().as_bytes(), raw);
    }

    #[test]
    fn subprocess_path_passes_through_prefixless_paths() {
        assert_eq!(subprocess_path(Path::new("/project/sub")), PathBuf::from("/project/sub"));
        assert_eq!(subprocess_path(Path::new("relative/path")), PathBuf::from("relative/path"));
    }

    #[test]
    #[cfg(windows)]
    fn env_overrides_strip_verbatim_from_every_var() {
        let ctx = GitCtx::shadow(
            PathBuf::from(r"\\?\C:\proj"),
            PathBuf::from(r"\\?\C:\proj\.cimp\shadow.git"),
            PathBuf::from(r"\\?\C:\proj\.cimp\shadow.git\index"),
        );
        let overrides = env_overrides(&ctx);
        assert_eq!(overrides[0].1, Some(PathBuf::from(r"C:\proj\.cimp\shadow.git")));
        assert_eq!(overrides[1].1, Some(PathBuf::from(r"C:\proj")));
        assert_eq!(overrides[2].1, Some(PathBuf::from(r"C:\proj\.cimp\shadow.git\index")));
    }

    /// The safety property: a `GitCtx::discover` (all `None`) must produce
    /// `None` for every var so `run` removes them from the child's env —
    /// never silently inherits a value some other in-process caller set.
    #[test]
    fn env_overrides_none_when_discovering() {
        let ctx = GitCtx::discover("/project");
        let overrides = env_overrides(&ctx);
        assert!(overrides.iter().all(|(_, v)| v.is_none()));
    }

    #[test]
    fn shadow_ctx_shares_root_as_work_tree() {
        let ctx = GitCtx::shadow(
            "/project",
            PathBuf::from("/project/.cimp/shadow.git"),
            PathBuf::from("/project/.cimp/shadow.git/index"),
        );
        assert_eq!(ctx.root, PathBuf::from("/project"));
        assert_eq!(ctx.work_tree, Some(PathBuf::from("/project")));
        assert_eq!(ctx.git_dir, Some(PathBuf::from("/project/.cimp/shadow.git")));
    }

    #[tokio::test]
    async fn run_with_stdin_pipes_data_to_the_child() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("wb-git-stdin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = GitCtx::discover(&dir);
        run(&ctx, &["init", "-q"], None).await.expect("git init");
        // `git hash-object --stdin` reads its stdin and prints the blob hash
        // it would be — a minimal, apply-independent check that bytes
        // actually cross the pipe (`run`'s stdin is always null, so this is
        // the one thing `run_with_stdin` adds).
        let out = run_with_stdin(&ctx, &["hash-object", "--stdin"], b"hello\n", None)
            .await
            .expect("hash-object");
        assert!(out.success(), "{out:?}");
        // `git hash-object hello.txt` on the same content must agree —
        // proves the exact bytes we wrote were what git hashed, not an
        // empty/truncated stream.
        std::fs::write(dir.join("hello.txt"), b"hello\n").unwrap();
        let via_file = run(&ctx, &["hash-object", "hello.txt"], None).await.expect("hash-object file");
        assert_eq!(out.stdout.trim(), via_file.stdout.trim());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn is_repo_true_in_fresh_git_init() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("wb-git-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = GitCtx::discover(&dir);
        run(&ctx, &["init", "-q"], None).await.expect("git init");
        assert!(is_repo(&dir).await);
        invalidate_is_repo_cache(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn is_repo_false_outside_any_repo() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        // A plain temp dir with no `git init`, directly under the OS temp
        // root — never itself a git working tree in practice.
        let dir = std::env::temp_dir().join(format!("wb-nogit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_repo(&dir).await);
        invalidate_is_repo_cache(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_reports_missing_git_as_typed_error() {
        // Can't easily hide a real `git` from PATH in-process; this documents
        // the mapping instead by exercising `resolve_command` directly, which
        // is what `run` relies on.
        if has_git() {
            return;
        }
        let ctx = GitCtx::discover(std::env::temp_dir());
        let err = run(&ctx, &["status"], None).await.unwrap_err();
        assert!(matches!(err, AppError::GitUnavailable(_)));
    }
}

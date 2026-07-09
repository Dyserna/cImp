//! Git helper for `changed_only` filtering (`checks::run`) — the set of files
//! touched in the working tree: `git diff --name-only HEAD` (tracked,
//! modified/staged) ∪ the untracked entries of `git status --porcelain`.
//!
//! Kept small and specific to `checks::run`'s needs; the IMPL-PLAN notes this
//! is the same shape Phase B (`graph_impact`) will want for its diff→symbol
//! mapping — promote to a shared module then if warranted, rather than
//! guessing the abstraction now.

use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;

use crate::error::{AppError, AppResult};

/// The union of files touched in `root`'s working tree since `HEAD`: tracked
/// changes (`git diff --name-only HEAD`) plus untracked files (`git status
/// --porcelain`'s `??` entries). Paths are project-relative, forward-slash
/// normalized (git's own convention). `Err` when `root` isn't a git repo (or
/// `git` isn't on PATH, or has no commits yet) — callers decide how to
/// degrade; `checks::run` treats it as "don't filter".
pub async fn changed_files(root: &Path) -> AppResult<HashSet<String>> {
    let mut set: HashSet<String> = HashSet::new();
    for line in run_git(root, &["diff", "--name-only", "HEAD"]).await?.lines() {
        let line = line.trim();
        if !line.is_empty() {
            set.insert(line.replace('\\', "/"));
        }
    }
    for line in run_git(root, &["status", "--porcelain"]).await?.lines() {
        // Porcelain format: a 2-char status code, a space, then the path.
        // `??` marks untracked; everything else (tracked modifications) is
        // already covered by the `diff` pass above.
        if let Some(rest) = line.strip_prefix("?? ") {
            set.insert(rest.trim().replace('\\', "/"));
        }
    }
    Ok(set)
}

/// Run `git <args>` with cwd = `root`, console-suppressed, and return its
/// captured stdout. A non-zero exit (not a repo, no `HEAD` yet, ...) is an
/// error — the caller decides how to degrade.
async fn run_git(root: &Path, args: &[&str]) -> AppResult<String> {
    let program = crate::pty::resolve_command("git")?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);

    let output = cmd
        .output()
        .await
        .map_err(|e| AppError::Checks(format!("git {}: {e}", args.join(" "))))?;
    if !output.status.success() {
        return Err(AppError::Checks(format!(
            "git {} exited with {:?} (not a git repo, or no commits yet?)",
            args.join(" "),
            output.status.code()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    /// A throwaway git repo with a committed file and one tracked
    /// modification plus one untracked file, so `changed_files` sees both
    /// halves of its union.
    fn setup() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("checks-gitls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = StdCommand::new("git").args(args).current_dir(&dir).output().expect("git");
            assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("tracked.rs"), "fn a() {}\n").unwrap();
        git(&["add", "tracked.rs"]);
        git(&["commit", "-q", "-m", "init"]);
        // Now dirty the working tree: modify the tracked file, add an
        // untracked one.
        std::fs::write(dir.join("tracked.rs"), "fn a() { }\n").unwrap();
        std::fs::write(dir.join("untracked.rs"), "fn b() {}\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn union_of_diff_and_untracked() {
        let dir = setup();
        let changed = changed_files(&dir).await.expect("changed_files");
        assert!(changed.contains("tracked.rs"), "{changed:?}");
        assert!(changed.contains("untracked.rs"), "{changed:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn not_a_repo_errors() {
        let dir = std::env::temp_dir().join(format!("checks-gitls-norepo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(changed_files(&dir).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

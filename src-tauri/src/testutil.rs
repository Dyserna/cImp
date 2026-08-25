//! Test-only fixtures shared across this crate's suites.
//!
//! # Why this module exists (V42 Phase-F review, F-8)
//!
//! The service split gave eight test modules the same two needs — somewhere
//! disposable to point a [`SettingsHandle`](crate::settings::SettingsHandle)
//! at, and a way to drive real `git` at a scratch repo — and each one grew its
//! own byte-alike copy of the answer. Eight `ScratchDir`s (one calling itself
//! `TempCwd`), each with the same `Drop`; eight `fn git`s, seven of them
//! identical and the eighth quietly different.
//!
//! That is the same shape the milestone spent itself removing from production
//! code, left standing in the tests. It has the same failure mode too: the
//! divergent `git` copy was the one that set a deterministic author identity,
//! and nothing said whether the other seven not doing so was a decision or an
//! omission. One definition ends that question — see [`git`].
//!
//! Both helpers take a `tag` so a failing run still says which suite left a
//! directory behind. That was the one thing worth keeping from the copies.

use std::path::{Path, PathBuf};

/// A throwaway directory that removes itself on drop.
///
/// Every caller wants the same thing: somewhere the debounced settings saver
/// (or a warm graph index, or a shadow repo) can write without touching the
/// developer's real config or the repo under test. `tag` names the suite, so
/// `cimp-tabsvc-<uuid>` in a temp listing still points at the test that made
/// it.
///
/// Hand-rolled rather than a `tempfile` dev-dependency — one `Drop` is cheaper
/// than a crate in the lock file — and removal is BEST-EFFORT on purpose: a
/// settings saver lives on Tauri's runtime and can land its write after the
/// test's own runtime is gone, and on Windows an index's SQLite handle can
/// outlive this call. A temp directory that survives a test run is litter; a
/// test that fails because it could not delete one is a false alarm.
pub(crate) struct ScratchDir(pub(crate) PathBuf);

impl ScratchDir {
    pub(crate) fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("cimp-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("scratch dir");
        Self(path)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `git` in `dir` and assert it succeeded, surfacing its stderr when it
/// did not.
///
/// **The identity is forced.** Seven of the eight copies this replaces relied
/// on their fixture having run `git config user.name/user.email` first; the
/// eighth passed `GIT_AUTHOR_*`/`GIT_COMMITTER_*` in the environment instead,
/// which is the version that works whether or not the fixture remembered. It
/// is also the version that cannot pick up a developer's global identity, or
/// fail outright on a machine that has none configured — so it is the one that
/// survives here. No test in this crate asserts on a commit's author, so
/// pinning it costs nothing and buys a repo that behaves the same on every
/// machine.
pub(crate) fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "cimp-test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "cimp-test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Whether `git` resolves at all — the guard every git-backed suite opens
/// with, so a machine without git skips rather than fails.
pub(crate) fn has_git() -> bool {
    crate::pty::resolve_command("git").is_ok()
}

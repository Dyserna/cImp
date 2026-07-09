//! V14 Phase B: session-scoped temp storage for images pasted or dropped
//! into the compose overlay (`ComposeOverlay.svelte`).
//!
//! Layout: `%TEMP%/cimp-attach/<session>/n.png`, where `session` is this
//! app run's launch id (`AppState::launch.launch_id`, a UUID minted once in
//! `main.rs` at startup) — one directory per run, so a crashed or killed
//! run's attachments never collide with a fresh one's. Dropped files are
//! NOT copied here (`ComposeOverlay.svelte` references them in place); only
//! pasted clipboard images (already re-encoded to PNG bytes on the frontend
//! — see `lib/compose/attachments.ts`'s `readClipboardImagePng`) land in
//! this directory via [`save_png`].
//!
//! Lifecycle: [`prune`] runs once at startup (age > 3 days catches
//! directories orphaned by a previous run that crashed or was killed before
//! it could clean up after itself) and again, best-effort, from the
//! graceful-exit path in `main.rs`'s `CloseRequested` handler. Neither call
//! deletes the *current* run's own directory — its images may still be
//! referenced by a prompt the user just submitted.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::AppResult;

/// Subdirectory of the OS temp dir all attach sessions live under.
const ATTACH_DIR_NAME: &str = "cimp-attach";

/// `%TEMP%/cimp-attach/<session>/`. Doesn't create anything — [`save_png`]
/// creates the directory lazily on first write, so a run that never attaches
/// an image never leaves an empty directory behind.
pub fn attach_dir(session: &str) -> PathBuf {
    std::env::temp_dir().join(ATTACH_DIR_NAME).join(session)
}

/// Writes `bytes` (PNG-encoded image data) to the next `n.png` in
/// `session`'s attach dir and returns the saved path. The index is the
/// highest existing `n` in the directory plus one — monotonic within a
/// session, so a burst of pastes in one compose session never collides.
/// Cheap to compute (a single `read_dir`) since a session's attach dir
/// holds at most a handful of images.
pub fn save_png(session: &str, bytes: &[u8]) -> AppResult<PathBuf> {
    let dir = attach_dir(session);
    fs::create_dir_all(&dir)?;
    let index = next_index(&dir);
    let path = dir.join(format!("{index}.png"));
    fs::write(&path, bytes)?;
    Ok(path)
}

/// V14 Phase F: reserve the next `n.png` path in `session`'s attach dir — for
/// [`crate::preview::preview_capture`], whose WebView2 `CapturePreview` COM
/// call writes PNG bytes directly to a file-backed `IStream`
/// (`SHCreateStreamOnFileW`, opened `STGM_CREATE`, so it happily overwrites
/// the empty placeholder below) rather than returning them to Rust as a byte
/// buffer the way a pasted clipboard image does.
///
/// Touches an EMPTY file at the reserved path (rather than only computing
/// the name) so [`next_index`] — shared with [`save_png`] for the monotonic
/// numbering — sees it and a concurrent/subsequent `save_png`/`reserve_path`
/// call advances past it instead of reusing the same index. Without this, a
/// reserved-but-not-yet-captured slot (the COM call runs asynchronously,
/// well after this returns) would be numerically invisible to the next
/// caller, which would then claim the SAME `n.png` — a real collision, not
/// just a numbering gap — if a capture and a paste happen to race.
pub fn reserve_path(session: &str) -> AppResult<PathBuf> {
    let dir = attach_dir(session);
    fs::create_dir_all(&dir)?;
    let index = next_index(&dir);
    let path = dir.join(format!("{index}.png"));
    fs::write(&path, [])?;
    Ok(path)
}

/// Highest `n.png` stem in `dir` plus one, or 0 for an empty/fresh
/// directory. Non-numeric or non-`.png` entries are ignored rather than
/// treated as an error — the attach dir is exclusively ours, but being
/// defensive here costs nothing.
fn next_index(dir: &Path) -> u64 {
    let mut next = 0u64;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(n) = entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok())
            {
                next = next.max(n + 1);
            }
        }
    }
    next
}

/// Removes every session directory under `cimp-attach/` whose most recent
/// modification is older than `max_age_days`. Called at startup and again,
/// best-effort, on graceful exit (`main.rs`) — both call sites treat this as
/// opportunistic cleanup: a missing root directory (nothing has ever been
/// attached) or a failure removing one session is logged/skipped, never
/// allowed to block startup or shutdown.
pub fn prune(max_age_days: u32) {
    let root = std::env::temp_dir().join(ATTACH_DIR_NAME);
    let entries = match fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return, // no attach dir yet — nothing to prune
    };
    let max_age = Duration::from_secs(u64::from(max_age_days) * 24 * 60 * 60);
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok());
        if age.is_some_and(|a| a > max_age) {
            if let Err(e) = fs::remove_dir_all(&path) {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "attach prune: remove_dir_all failed"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Isolates each test's attach root under a unique temp subdir rather
    /// than the real `cimp-attach` root shared by the live app (and by every
    /// other test in this module, if they ran against the literal constant
    /// concurrently). `attach_dir`/`prune` are exercised indirectly through
    /// this helper's own root by monkey-patching the environment isn't
    /// available in stable Rust, so instead each test builds its own
    /// `session` under the REAL root but with a unique UUID prefix, and
    /// cleans up after itself — the same pattern used throughout the
    /// codebase (see `ipc::note`'s tests) for temp-dir tests that can't
    /// inject a root.
    fn unique_session() -> String {
        format!("test-{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn attach_dir_is_under_temp_cimp_attach() {
        let session = "abc123";
        let dir = attach_dir(session);
        assert!(dir.ends_with(Path::new(ATTACH_DIR_NAME).join(session)));
        assert!(dir.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn save_png_writes_monotonic_indices() {
        let session = unique_session();
        let p0 = save_png(&session, b"first").unwrap();
        let p1 = save_png(&session, b"second").unwrap();
        let p2 = save_png(&session, b"third").unwrap();

        assert_eq!(p0.file_name().unwrap().to_str().unwrap(), "0.png");
        assert_eq!(p1.file_name().unwrap().to_str().unwrap(), "1.png");
        assert_eq!(p2.file_name().unwrap().to_str().unwrap(), "2.png");
        assert_eq!(fs::read(&p0).unwrap(), b"first");
        assert_eq!(fs::read(&p2).unwrap(), b"third");

        let _ = fs::remove_dir_all(attach_dir(&session));
    }

    #[test]
    fn save_png_creates_the_session_dir() {
        let session = unique_session();
        let dir = attach_dir(&session);
        assert!(!dir.exists());
        save_png(&session, b"data").unwrap();
        assert!(dir.is_dir());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_removes_a_directory_older_than_max_age_and_keeps_a_fresh_one() {
        let old_session = unique_session();
        let fresh_session = unique_session();
        let old_dir = attach_dir(&old_session);
        let fresh_dir = attach_dir(&fresh_session);
        fs::create_dir_all(&old_dir).unwrap();
        fs::create_dir_all(&fresh_dir).unwrap();

        // Backdate the "old" directory's mtime well past any max_age_days
        // this test will use. `filetime` (already resolved in the
        // workspace's lockfile via a transitive dependency) is the
        // standard-library-adjacent way to set an arbitrary mtime,
        // including on directories, cross-platform.
        let ancient = SystemTime::now() - Duration::from_secs(10 * 24 * 60 * 60);
        filetime::set_file_mtime(&old_dir, filetime::FileTime::from_system_time(ancient))
            .unwrap();

        prune(3);

        assert!(!old_dir.exists(), "aged-out session dir should be removed");
        assert!(fresh_dir.exists(), "fresh session dir should survive");

        let _ = fs::remove_dir_all(&old_dir);
        let _ = fs::remove_dir_all(&fresh_dir);
    }

    #[test]
    fn prune_on_a_missing_root_is_a_silent_no_op() {
        // Nothing to assert beyond "doesn't panic" — the root may
        // legitimately not exist (fresh install, nothing ever attached).
        // Use an enormous max_age so even a populated real root (from other
        // tests running concurrently) is untouched by this call.
        prune(u32::MAX);
    }

    // ── V14 Phase F: reserve_path (preview::preview_capture's attach path) ──

    #[test]
    fn reserve_path_creates_the_session_dir_and_an_empty_placeholder() {
        let session = unique_session();
        let dir = attach_dir(&session);
        assert!(!dir.exists());

        let path = reserve_path(&session).unwrap();
        assert!(dir.is_dir(), "reserve_path must create the session dir");
        assert!(
            path.exists(),
            "reserve_path must touch a placeholder so next_index sees it"
        );
        assert_eq!(fs::read(&path).unwrap().len(), 0, "placeholder starts empty");
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), "0.png");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reserve_path_shares_monotonic_numbering_with_save_png() {
        let session = unique_session();
        let p0 = save_png(&session, b"first").unwrap();
        let reserved = reserve_path(&session).unwrap();
        let p2 = save_png(&session, b"third").unwrap();

        assert_eq!(p0.file_name().unwrap().to_str().unwrap(), "0.png");
        // Because reserve_path touches an empty placeholder, next_index sees
        // it — the following save_png call advances past it (to "2.png")
        // rather than colliding with the reserved-but-not-yet-captured slot.
        assert_eq!(reserved.file_name().unwrap().to_str().unwrap(), "1.png");
        assert_eq!(p2.file_name().unwrap().to_str().unwrap(), "2.png");

        let _ = fs::remove_dir_all(attach_dir(&session));
    }
}

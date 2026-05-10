//! V1.4-04 D: cross-restart scrollback persistence.
//!
//! On graceful exit (`tauri::RunEvent::ExitRequested`) the live ring
//! buffer in `PtyManager` is written to `<exe-dir>/scrollback/<tab-id>.bin`.
//! On the next `pty_start` for that tab the file is read once, replayed
//! into the new xterm, used to seed the new ring, then deleted (so a
//! crash mid-run doesn't replay it twice).
//!
//! Hard-kill scenarios (SIGKILL, power loss, taskkill) lose the
//! scrollback because `ExitRequested` doesn't fire — this is best-effort
//! recovery, not durable storage. Documented in DESIGN.md.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use crate::error::{AppError, AppResult};
use crate::state::TabId;

/// Resolve `<exe-dir>/scrollback/`. Lives next to `settings.json` and
/// `logs/` so the entire portable folder is self-contained.
fn scrollback_dir() -> AppResult<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| AppError::Settings(format!("current_exe failed: {e}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| AppError::Settings("exe has no parent dir".into()))?;
    Ok(dir.join("scrollback"))
}

/// Per-tab scrollback file path. Tab IDs are sanitized defensively —
/// `claude` / `aider` are alphabetic; user shells use UUID-derived
/// strings; either way we strip anything outside `[A-Za-z0-9._-]` to
/// keep filenames safe across Windows / macOS / Linux conventions.
fn scrollback_file_for(tab: &TabId) -> AppResult<PathBuf> {
    let raw = tab.as_str();
    let safe: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(scrollback_dir()?.join(format!("{}.bin", safe)))
}

/// Write the ring contents to disk. Creates the directory if needed.
/// Failure logs but doesn't poison subsequent persists for other tabs.
pub fn persist_to_disk(tab: &TabId, bytes: &[u8]) -> AppResult<()> {
    let path = scrollback_file_for(tab)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    fs::write(&path, bytes).map_err(AppError::Io)?;
    Ok(())
}

/// Read the per-tab persisted scrollback without deleting it. Returns
/// `None` if the file doesn't exist (cold-installed tab, or already
/// consumed). Pair with `consume_after_read` once the caller has
/// successfully replayed and seeded the bytes — that way a transient
/// `seed_scrollback` failure (poisoned mutex, ring contention) leaves
/// the file in place for the next launch to retry, rather than
/// dropping the user's scrollback on the floor between read and seed.
pub fn read(tab: &TabId) -> Option<Vec<u8>> {
    let path = scrollback_file_for(tab).ok()?;
    if !path.exists() {
        return None;
    }
    match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "scrollback read failed");
            None
        }
    }
}

/// Delete the per-tab scrollback file. No-op if the file is already gone.
/// Called from `pty_start` after a successful `seed_scrollback`. Failure
/// here is non-fatal — the orphan-prune sweep at next launch catches it
/// if the tab has been removed; otherwise the next read returns the same
/// bytes again, which is acceptable because seed is idempotent (the new
/// ring is empty before the seed and the seed itself is a write).
pub fn consume_after_read(tab: &TabId) {
    let Ok(path) = scrollback_file_for(tab) else { return };
    if let Err(e) = fs::remove_file(&path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %e, path = %path.display(), "scrollback delete failed");
        }
    }
}

/// Delete a per-tab file unconditionally. No-op if absent. Used when a
/// tab is removed via the UI; the scrollback for a deleted tab is no
/// longer relevant.
pub fn delete(tab: &TabId) -> AppResult<()> {
    let path = scrollback_file_for(tab)?;
    if path.exists() {
        fs::remove_file(&path).map_err(AppError::Io)?;
    }
    Ok(())
}

/// Walk the scrollback directory and delete any file whose stem isn't
/// in `known`. Defensive sweep at app startup: tabs deleted between
/// sessions (or files written for tab IDs that no longer exist) get
/// cleaned up so the disk usage doesn't grow unboundedly.
///
/// Tolerant of missing dir / unparseable filenames — those are
/// silently skipped or removed (a `.bin` file that doesn't deserialize
/// to a known tab ID is safe to drop).
pub fn prune_orphans(known: &HashSet<String>) {
    let Ok(dir) = scrollback_dir() else { return };
    if !dir.exists() {
        return;
    }
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "scrollback prune: read_dir failed");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            // Couldn't parse the filename — drop it.
            let _ = fs::remove_file(&path);
            continue;
        };
        // Re-sanitize the stem the same way we wrote it; if `known`
        // doesn't list it, the file is an orphan.
        let safe: String = stem
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if !known.contains(&safe) {
            let _ = fs::remove_file(&path);
        }
    }
}

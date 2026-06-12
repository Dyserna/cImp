//! Per-tab raw PTY content capture.
//!
//! When enabled in settings, every byte read off each tab's master PTY
//! is appended to a per-tab daily-rolling file at
//! `<portable-root>/logs/content/<tab-id>.log.<YYYY-MM-DD>`. Rotation is
//! handled by `tracing-appender::rolling::daily`; cleanup deletes files
//! whose mtime is older than the retention window.
//!
//! The fast path is an `AtomicBool::load(Relaxed)` per byte burst —
//! disabled is effectively free. The capture writer for each tab is
//! created lazily on first write and re-created after a `delete_all()`
//! drops file handles (Windows can't delete a file with an open
//! handle).

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use tracing_appender::rolling::RollingFileAppender;

use crate::settings::LogRetention;
use crate::state::TabId;

struct ContentCapture {
    enabled: AtomicBool,
    /// Per-tab appenders, keyed by sanitized tab id. `None` until first
    /// write or after `delete_all()` drops handles. `Mutex` is the
    /// serialization point for concurrent reader threads — terminal
    /// output is bursty, contention is minimal in practice.
    writers: Mutex<HashMap<String, RollingFileAppender>>,
}

static INSTANCE: OnceLock<ContentCapture> = OnceLock::new();

fn instance() -> &'static ContentCapture {
    INSTANCE.get_or_init(|| ContentCapture {
        enabled: AtomicBool::new(false),
        writers: Mutex::new(HashMap::new()),
    })
}

/// `<portable-root>/logs/content/`.
pub fn dir() -> PathBuf {
    crate::logging::logs_dir().join("content")
}

/// Apply the user's enabled flag. Disabling drops every cached writer
/// so today's file isn't held open — matters on Windows where
/// `delete_all` can't unlink a file with an active handle.
pub fn set_enabled(enabled: bool) {
    let cap = instance();
    cap.enabled.store(enabled, Ordering::Relaxed);
    if !enabled {
        if let Ok(mut writers) = cap.writers.lock() {
            writers.clear();
        }
    }
}

/// Write a chunk of raw PTY bytes to the per-tab daily file. No-op when
/// capture is disabled (fast path: one atomic load). Per-write errors
/// are logged at warn-level and swallowed — capture should never wedge
/// the PTY processor.
pub fn write(tab: &TabId, bytes: &[u8]) {
    let cap = instance();
    if !cap.enabled.load(Ordering::Relaxed) {
        return;
    }
    if bytes.is_empty() {
        return;
    }
    let key = sanitize(tab.as_str());
    let dir = dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(dir = %dir.display(), error = %e, "content capture: mkdir failed");
        return;
    }
    let mut writers = match cap.writers.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let writer = writers.entry(key.clone()).or_insert_with(|| {
        // tracing-appender appends a `.YYYY-MM-DD` suffix to the
        // filename prefix on each rotation. The visible filename
        // becomes e.g. `claude.log.2026-05-08`.
        tracing_appender::rolling::daily(&dir, format!("{key}.log"))
    });
    let _ = writer
        .write_all(bytes)
        .inspect_err(|e| tracing::warn!(tab = %key, error = %e, "content capture: write failed"));
}

/// Sanitize a tab id for use as a filename. Tab ids are already
/// well-formed in practice (kebab-case slugs or UUIDs) so this is
/// defense-in-depth: replace path separators and other shell-hostile
/// characters with `_`.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect()
}

/// Delete every file in the content folder. Drops every cached writer
/// first so Windows can unlink today's still-open file. Returns the
/// number of files removed; per-file errors are logged at warn-level
/// and skipped.
pub fn delete_all() -> u32 {
    let cap = instance();
    if let Ok(mut writers) = cap.writers.lock() {
        writers.clear();
    }
    let dir = dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut count = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => count += 1,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "content delete_all: failed");
            }
        }
    }
    count
}

/// Delete files whose mtime is older than `retention`. `Never` is a
/// no-op. Same shape as `logging::run_cleanup` but scoped to the
/// content subdirectory and matches every regular file (not just
/// `cctts.log.*` — every file under `content/` is owned by us).
pub fn run_cleanup(retention: LogRetention) {
    let Some(max_age) = retention.max_age() else {
        return;
    };
    let dir = dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age <= max_age {
            continue;
        }
        // Drop the cached writer for this tab if its current file is
        // about to be deleted (Windows). The writer will lazily
        // recreate on next write.
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if let Some(prefix) = name.split(".log.").next() {
                if let Ok(mut writers) = instance().writers.lock() {
                    writers.remove(prefix);
                }
            }
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "content cleanup: remove failed");
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, retention = ?retention, "content cleanup: removed old files");
    }
}

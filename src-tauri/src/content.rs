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
use std::sync::mpsc::{Receiver, Sender};
use std::sync::OnceLock;

use tracing_appender::rolling::RollingFileAppender;

use crate::settings::LogRetention;
use crate::state::TabId;

/// Messages to the dedicated capture writer thread. All disk I/O happens on
/// that thread so `write()` (called on the PTY reader's async task) never blocks
/// a tokio worker. Per-tab ordering is preserved because the channel is FIFO and
/// a single thread drains it.
enum Msg {
    Write { key: String, bytes: Vec<u8> },
    /// Drop every open appender and acknowledge — used before deleting files
    /// (Windows can't unlink a file with a live handle).
    DropAll(Sender<()>),
}

struct ContentCapture {
    enabled: AtomicBool,
    tx: Sender<Msg>,
}

static INSTANCE: OnceLock<ContentCapture> = OnceLock::new();

/// Owns the per-tab appenders and serves [`Msg`]s from the channel. Runs on a
/// dedicated OS thread for the life of the process.
fn writer_loop(rx: Receiver<Msg>) {
    let mut writers: HashMap<String, RollingFileAppender> = HashMap::new();
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Write { key, bytes } => {
                let dir = dir();
                // Only stat/create the directory when opening a NEW writer.
                if !writers.contains_key(&key) {
                    if let Err(e) = std::fs::create_dir_all(&dir) {
                        tracing::warn!(dir = %dir.display(), error = %e, "content capture: mkdir failed");
                        continue;
                    }
                }
                let writer = writers.entry(key.clone()).or_insert_with(|| {
                    // tracing-appender appends a `.YYYY-MM-DD` suffix to the
                    // filename prefix on each rotation, e.g. `claude.log.2026-05-08`.
                    tracing_appender::rolling::daily(&dir, format!("{key}.log"))
                });
                let _ = writer
                    .write_all(&bytes)
                    .inspect_err(|e| tracing::warn!(tab = %key, error = %e, "content capture: write failed"));
            }
            Msg::DropAll(ack) => {
                writers.clear();
                let _ = ack.send(());
            }
        }
    }
}

fn instance() -> &'static ContentCapture {
    INSTANCE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("content-capture".into())
            .spawn(move || writer_loop(rx))
            .expect("spawn content-capture thread");
        ContentCapture {
            enabled: AtomicBool::new(false),
            tx,
        }
    })
}

/// Tell the writer thread to drop every open appender and block until it has —
/// so a subsequent file delete won't hit a still-open handle.
fn drop_all_handles(cap: &ContentCapture) {
    let (ack_tx, ack_rx) = std::sync::mpsc::channel();
    if cap.tx.send(Msg::DropAll(ack_tx)).is_ok() {
        let _ = ack_rx.recv();
    }
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
        drop_all_handles(cap);
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
    // Hand the bytes to the dedicated writer thread. `send` on an unbounded
    // channel never blocks (and only errors if the thread died), so the PTY
    // hot path stays off the disk entirely.
    let _ = cap.tx.send(Msg::Write { key, bytes: bytes.to_vec() });
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
    // Drop open handles on the writer thread first, then unlink (Windows).
    drop_all_handles(cap);
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
/// `ccimp.log.*` — every file under `content/` is owned by us).
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
        // NOTE: we deliberately do NOT drop any cached writer here. Only files
        // aged past `max_age` reach this point, and tracing-appender holds open
        // only TODAY's file — never one of these old dated files. The previous
        // code computed the writer key from the old file's name prefix (e.g.
        // `claude.log.2026-05-08` → `claude`) and evicted `writers["claude"]`,
        // which is the appender for *today's* file — needlessly forcing a
        // reopen (and risking buffered-state loss) on the active writer every
        // time one of that tab's historical files aged out.
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

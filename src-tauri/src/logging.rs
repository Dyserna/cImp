//! File-logger init for the portable build.
//!
//! Writes daily-rolling files to `<exe-dir>/../logs/cimp.log.<YYYY-MM-DD>` via
//! `tracing-appender` — the `logs/` folder sits next to `bin/` and `models/`
//! at the portable root, not inside `bin/`. The filter is wrapped in a
//! `reload::Layer` so changes to `settings.logging.level` apply live without
//! a restart.
//!
//! Order of operations from `main`:
//!   1. `init(LogLevel::default())` → returns a `WorkerGuard` that must be
//!      held for the lifetime of the program (the non-blocking writer drops
//!      pending writes when the guard is dropped).
//!   2. After settings load: `set_level(saved_level)` to apply the user's
//!      saved level.
//!   3. The settings-broadcast loop calls `set_level` whenever the level
//!      field of a broadcast settings update differs from the previous one.
//!
//! `RUST_LOG`, when set (and valid), overrides the saved level: `init`
//! records the override in `ENV_OVERRIDE` and `main` skips step 2 when
//! [`env_override_active`] reports true — the dev workflow
//! (`RUST_LOG=cimp=trace npm run tauri dev`) keeps working for the whole
//! session. A LIVE level change from Settings (step 3) still wins over the
//! env var: picking a level mid-session is an explicit user action.

use std::path::PathBuf;
use std::sync::OnceLock;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::{filter::EnvFilter, fmt, prelude::*, reload, Registry};

use crate::settings::{LogLevel, LogRetention};

/// Reload handle for the file-logger's EnvFilter. Set once during `init`
/// and read every time the user changes the level from settings.
static RELOAD_HANDLE: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

/// Whether `init` built its filter from a valid `RUST_LOG`. `main` consults
/// this to skip the startup `set_level(saved_level)` call — without the
/// check, the saved settings level unconditionally clobbered the env
/// override milliseconds after startup, breaking the documented
/// `RUST_LOG=cimp=trace` dev workflow every run.
static ENV_OVERRIDE: OnceLock<bool> = OnceLock::new();

/// True when a valid `RUST_LOG` filter was installed at `init` time.
pub fn env_override_active() -> bool {
    ENV_OVERRIDE.get().copied().unwrap_or(false)
}

/// `<exe-dir>/../logs/` — sibling of `bin/` and `models/` at the portable
/// root. Falls back to `./logs/` if `current_exe()` is unavailable
/// (sandbox / weird platform).
pub fn logs_dir() -> PathBuf {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|d| d.parent()).map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    dir.join("logs")
}

/// Initialize the global tracing subscriber. Must be called exactly once,
/// at the very top of `main`. The returned `WorkerGuard` keeps the
/// non-blocking writer alive — drop it and pending log writes are lost.
pub fn init(initial: LogLevel) -> WorkerGuard {
    let dir = logs_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        // `eprintln!` is the only sensible thing here — tracing isn't up
        // yet, and we want the failure visible in the dev console even
        // though the file logger can't write.
        eprintln!("logging: failed to create {}: {}", dir.display(), e);
    }
    let appender = rolling::daily(&dir, "cimp.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let filter = match EnvFilter::try_from_default_env() {
        Ok(f) => {
            let _ = ENV_OVERRIDE.set(true);
            f
        }
        Err(e) => {
            // `try_from_default_env` fails both when RUST_LOG is unset
            // (normal) and when it's set but malformed — only the latter
            // deserves a diagnostic, and tracing isn't up yet, so stderr
            // is the only place it can go (same as the create_dir_all
            // failure above).
            if std::env::var_os("RUST_LOG").is_some() {
                eprintln!("logging: invalid RUST_LOG ({e}); using the default level");
            }
            let _ = ENV_OVERRIDE.set(false);
            EnvFilter::new(initial.as_filter_str())
        }
    };

    let (filter_layer, handle) = reload::Layer::new(filter);
    let _ = RELOAD_HANDLE.set(handle);

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(
            fmt::layer()
                .with_writer(writer)
                .with_target(true)
                .with_thread_ids(false)
                .with_ansi(false),
        )
        .init();

    guard
}

/// Delete rolled log files whose mtime is older than `retention`. The
/// active day's file (the one tracing-appender is currently appending
/// to) has a fresh mtime so it's never eligible. `Never` is a no-op.
/// Best-effort: each I/O failure is logged at warn-level and we move
/// on — a stuck file shouldn't prevent the rest of the pass.
pub fn run_cleanup(retention: LogRetention) {
    let Some(max_age) = retention.max_age() else {
        return;
    };
    let dir = logs_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "log cleanup: read_dir failed");
            return;
        }
    };
    let now = std::time::SystemTime::now();
    let mut removed: u32 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Match the appender's filename pattern: "cimp.log" prefix plus
        // a date suffix. Anything else in `logs/` is ignored.
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if n.starts_with("cimp.log") => n.to_string(),
            _ => continue,
        };
        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(file = %name, error = %e, "log cleanup: stat failed");
                continue;
            }
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age <= max_age {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::warn!(file = %name, error = %e, "log cleanup: remove failed");
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, retention = ?retention, "log cleanup: removed old files");
    }
}

/// Hot-swap the EnvFilter. No-op if `init` hasn't been called or if the
/// filter string is malformed (we surface the parse error as a warn-level
/// log line and leave the existing filter in place).
pub fn set_level(level: LogLevel) {
    let Some(handle) = RELOAD_HANDLE.get() else {
        return;
    };
    let filter_str = level.as_filter_str();
    match EnvFilter::try_new(filter_str) {
        Ok(next) => {
            if let Err(e) = handle.reload(next) {
                tracing::warn!(error = %e, "log level reload failed");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, level = %filter_str, "log level parse failed");
        }
    }
}

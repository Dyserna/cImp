//! Command resolution with a bundled-tool `ebin` directory.
//!
//! Every external command the app launches — PTY tabs, `llama-server`, the
//! MCP-host servers, and `run_command` — goes through [`resolve_command`].
//! Historically that was a bare PATH lookup; now it first considers an `ebin/`
//! directory shipped alongside the app, so the portable release can carry tools
//! (broot, rustnet, …) without the user installing them, and so new tools can
//! be added later by dropping a binary in.
//!
//! Resolution is simply **`ebin` first, then PATH**: if a bare command name is
//! found in `ebin` it's used; otherwise we fall back to a normal PATH lookup.
//! No version comparison — the bundled copy is the curated one and wins when
//! present, which is deterministic and needs no per-tool probing.
//!
//! `ebin` is located relative to the executable, covering both the packaged
//! layout (`<zip>/bin/ccimp.exe` with a sibling `<zip>/ebin/`) and a flat
//! `<exe-dir>/ebin/` (what `build.rs` stages next to the dev binary). An
//! explicit path argument (absolute, or containing a separator) bypasses
//! `ebin` and is resolved verbatim.

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

/// Resolve a command name to a binary on disk: `ebin` first, then PATH.
pub fn resolve_command(name: &str) -> AppResult<PathBuf> {
    // An explicit path (absolute or containing a separator) is honored as-is —
    // `ebin` is only consulted for bare command names like "broot".
    if is_explicit_path(name) {
        return which::which(name).map_err(|_| AppError::CommandNotFound(name.to_string()));
    }

    if let Some(bundled) = lookup_in_dirs(&ebin_dirs(), name) {
        return Ok(bundled);
    }

    which::which(name).map_err(|_| AppError::CommandNotFound(name.to_string()))
}

/// `true` if `name` already designates a path (absolute or with a separator),
/// in which case `ebin` resolution is skipped.
fn is_explicit_path(name: &str) -> bool {
    Path::new(name).is_absolute() || name.contains('/') || name.contains('\\')
}

/// Candidate `ebin` directories, most-specific first: next to the exe, then a
/// sibling of the exe's directory (the packaged `bin/` + `ebin/` layout).
fn ebin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.join("ebin"));
            if let Some(parent) = dir.parent() {
                dirs.push(parent.join("ebin"));
            }
        }
    }
    dirs
}

/// Filenames to try for a bare command name. On Windows the executable
/// extensions are tried first (covering both real `.exe`s and `.cmd`/`.bat`
/// shims), then the name verbatim. A name that already carries an extension is
/// trusted as-is.
fn candidate_names(name: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        if Path::new(name).extension().is_some() {
            return vec![name.to_string()];
        }
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            format!("{name}.bat"),
            name.to_string(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![name.to_string()]
    }
}

/// First existing file named like `name` across `dirs`.
fn lookup_in_dirs(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    for dir in dirs {
        for candidate in candidate_names(name) {
            let p = dir.join(&candidate);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_paths_are_detected() {
        assert!(is_explicit_path("/usr/bin/broot"));
        assert!(is_explicit_path("C:\\tools\\broot.exe"));
        assert!(is_explicit_path("./broot"));
        assert!(is_explicit_path("sub/dir/tool"));
        assert!(!is_explicit_path("broot"));
        assert!(!is_explicit_path("llama-server"));
    }

    #[test]
    fn lookup_finds_bundled_binary_and_misses_cleanly() {
        // Unique temp dir so parallel tests don't collide.
        let dir = std::env::temp_dir().join(format!("ccimp-ebin-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Create a file under the first candidate name for "demo-tool" so this
        // works on both Windows (demo-tool.exe) and Unix (demo-tool).
        let file_name = candidate_names("demo-tool").remove(0);
        let file = dir.join(&file_name);
        std::fs::write(&file, b"x").unwrap();

        let dirs = vec![dir.clone()];
        assert_eq!(lookup_in_dirs(&dirs, "demo-tool"), Some(file));
        assert_eq!(lookup_in_dirs(&dirs, "not-there"), None);

        std::fs::remove_dir_all(&dir).ok();
    }
}

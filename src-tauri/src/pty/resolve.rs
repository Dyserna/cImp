//! Command resolution with a drop-in `ebin` tool directory.
//!
//! Every external command the app launches — PTY tabs, `llama-server`, the
//! MCP-host servers, and `run_command` — goes through [`resolve_command`].
//! Historically that was a bare PATH lookup; now it first considers an `ebin/`
//! directory alongside the app, so users can make tools (broot, rustnet, …)
//! available to cImp by dropping a binary in, without touching PATH. The
//! release ships `ebin/` empty — nothing is bundled.
//!
//! Resolution is simply **`ebin` first, then PATH**: if a bare command name is
//! found in `ebin` it's used; otherwise we fall back to a normal PATH lookup.
//! No version comparison — the `ebin` copy is the curated one and wins when
//! present, which is deterministic and needs no per-tool probing.
//!
//! `ebin` is located relative to the executable, covering both the packaged
//! layout (`<zip>/bin/cimp.exe` with a sibling `<zip>/ebin/`) and a flat
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
/// shims), then the name verbatim. Only a name that already carries a real
/// EXECUTABLE extension is trusted as-is — `Path::extension()` splits on the
/// last dot with no notion of "is this an extension", so a versioned name
/// like `aws2.1` or `python3.11` reports `Some("1")` and would otherwise
/// never get its `aws2.1.exe` trial, silently missing the bundled copy.
fn candidate_names(name: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        let has_exec_ext = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "exe" | "cmd" | "bat" | "com"
                )
            });
        if has_exec_ext {
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

/// Whether a resolved `ebin` hit is actually runnable. On Unix a bundled
/// file that lost its `+x` bit (zip extraction without permission bits, a
/// hand-dropped file) would otherwise "resolve" here and then fail at spawn
/// time with EACCES — skipping it lets the PATH fallback take over instead.
/// On Windows existence is sufficient.
fn is_runnable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

/// First existing (and runnable) file named like `name` across `dirs`.
fn lookup_in_dirs(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    for dir in dirs {
        for candidate in candidate_names(name) {
            let p = dir.join(&candidate);
            if p.is_file() && is_runnable(&p) {
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
        let dir = std::env::temp_dir().join(format!("cimp-ebin-test-{}", uuid::Uuid::new_v4()));
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

    /// Regression (2026-07 review): a versioned name like `aws2.1` must
    /// still get its `.exe`/`.cmd`/`.bat` trials — `Path::extension()`
    /// reports `Some("1")` for it, which used to short-circuit to the
    /// verbatim name only.
    #[cfg(windows)]
    #[test]
    fn dotted_but_not_extensioned_names_still_try_exe() {
        let names = candidate_names("aws2.1");
        assert!(names.contains(&"aws2.1.exe".to_string()));
        assert!(names.contains(&"aws2.1".to_string()));
        // A real executable extension is still trusted as-is.
        assert_eq!(candidate_names("tool.exe"), vec!["tool.exe".to_string()]);
        assert_eq!(candidate_names("shim.CMD"), vec!["shim.CMD".to_string()]);
    }

    /// A bundled file without the executable bit must be skipped so the
    /// PATH fallback can win, instead of resolving and failing with EACCES
    /// at spawn time.
    #[cfg(unix)]
    #[test]
    fn non_executable_ebin_file_is_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cimp-ebin-noexec-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("plain-file");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

        let dirs = vec![dir.clone()];
        assert_eq!(lookup_in_dirs(&dirs, "plain-file"), None);

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(lookup_in_dirs(&dirs, "plain-file"), Some(file));

        std::fs::remove_dir_all(&dir).ok();
    }
}

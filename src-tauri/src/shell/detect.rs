//! Default-shell auto-detection per platform.
//!
//! See `DESIGN.md` § "Cross-Platform Considerations" for the probe order and
//! rationale; in short, we prefer Git Bash on Windows because it ships the
//! standard Linux toolset most users want, and we honor `$SHELL` on Linux
//! because that's what the user has already chosen.
//!
//! ## Verified detection matrix (M4)
//!
//! Manual verification matrix from `MILESTONE-V3-04-polish.md` step 6.
//! Update the Verified column when re-running on a new platform; check
//! marks indicate the case has been observed working at least once on a
//! real install. Linux validation is deferred — Windows is the primary
//! target for v1.2 ship.
//!
//! | Platform | Configuration                                                    | Expected default                          | Verified |
//! |----------|------------------------------------------------------------------|-------------------------------------------|----------|
//! | Windows  | Git for Windows installed in `C:\Program Files\Git`              | `C:\Program Files\Git\bin\bash.exe`       | yes      |
//! | Windows  | Git for Windows installed in `C:\Program Files (x86)\Git`        | `C:\Program Files (x86)\Git\bin\bash.exe` | n/a (rare) |
//! | Windows  | Git for Windows installed elsewhere (e.g. `D:\dev\Git`)          | Path read from `HKLM`/`HKCU` `\SOFTWARE\GitForWindows\InstallPath` | n/a (rare) |
//! | Windows  | No Git for Windows; `bash.exe` on PATH from MSYS2                | The PATH-resolved bash                    | n/a (rare) |
//! | Windows  | No Git, no MSYS2, no bash on PATH                                | `powershell.exe -NoLogo` with banner      | yes      |
//! | Linux    | `$SHELL=/bin/bash` (typical)                                     | `/bin/bash -i`                            | deferred |
//! | Linux    | `$SHELL=/usr/bin/zsh`                                            | `/usr/bin/zsh -i`                         | deferred |
//! | Linux    | `$SHELL` unset                                                   | `/bin/bash -i`                            | deferred |
//! | Linux    | `$SHELL` set to a non-existent path                              | `/bin/bash -i` (fallback)                 | deferred |
//!
//! Known-good registry quirk handling: `git_bash_from_registry` reads
//! `InstallPath` and appends `bin\bash.exe`. `PathBuf::push` swallows a
//! trailing slash if the registry value contains one, so no explicit
//! sanitization is required.

#[cfg(any(unix, windows))]
use std::path::Path;
use std::path::PathBuf;

#[cfg(unix)]
use tracing::debug;
use tracing::info;
#[cfg(windows)]
use tracing::warn;

use super::{ShellSource, ShellSpec};

/// Resolve the default shell + its source for the current platform. The two
/// pieces of information are returned together because callers either want
/// just the spec (M1 startup spawn) or also the source (M2 banner).
pub fn default_shell_resolution() -> (ShellSpec, ShellSource) {
    #[cfg(unix)]
    {
        return resolve_unix();
    }
    #[cfg(windows)]
    {
        resolve_windows()
    }
    #[cfg(not(any(unix, windows)))]
    {
        // Targets like wasm — the app doesn't actually build for these,
        // but keep the function total. Fully qualified: the `warn` import
        // above is `#[cfg(windows)]`-gated, so a bare `warn!` here would
        // fail to compile on exactly the targets this arm exists for.
        tracing::warn!("shell detection: unknown platform; using /bin/sh");
        (
            ShellSpec {
                command: PathBuf::from("/bin/sh"),
                args: vec!["-i".to_string()],
            },
            ShellSource::ShFallback,
        )
    }
}

/// Convenience for callers that only want the spec.
pub fn default_shell() -> ShellSpec {
    let (spec, source) = default_shell_resolution();
    info!(
        command = %spec.command.display(),
        args = ?spec.args,
        ?source,
        "default shell resolved"
    );
    spec
}

/// True when `source` is any of the three Windows Git Bash probes. M2's
/// new-shell-tab dialog uses this (via `default_shell_spec`) to decide
/// whether to render the install-Git banner. Takes the already-resolved
/// source rather than re-running the whole probe chain (file checks,
/// registry read, PATH walk) a second time per dialog open.
pub fn is_git_bash_source(source: &ShellSource) -> bool {
    matches!(
        source,
        ShellSource::GitBashProgramFiles | ShellSource::GitBashRegistry | ShellSource::GitBashPath
    )
}

#[cfg(unix)]
fn resolve_unix() -> (ShellSpec, ShellSource) {
    if let Ok(shell) = std::env::var("SHELL") {
        let path = PathBuf::from(&shell);
        if is_executable(&path) {
            return (
                ShellSpec {
                    command: path,
                    args: vec!["-i".to_string()],
                },
                ShellSource::EnvShell,
            );
        } else {
            debug!(
                shell = %shell,
                "shell detection: $SHELL set but not executable; falling back"
            );
        }
    }

    let bash = PathBuf::from("/bin/bash");
    if is_executable(&bash) {
        return (
            ShellSpec {
                command: bash,
                args: vec!["-i".to_string()],
            },
            ShellSource::BashFallback,
        );
    }

    (
        ShellSpec {
            command: PathBuf::from("/bin/sh"),
            args: vec!["-i".to_string()],
        },
        ShellSource::ShFallback,
    )
}

#[cfg(windows)]
fn resolve_windows() -> (ShellSpec, ShellSource) {
    // Probe order matches DESIGN.md.
    let standard_paths = [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
    ];
    for p in standard_paths {
        let path = PathBuf::from(p);
        if path.is_file() {
            return (
                ShellSpec {
                    command: path,
                    args: vec!["--login".to_string(), "-i".to_string()],
                },
                ShellSource::GitBashProgramFiles,
            );
        }
    }

    if let Some(path) = git_bash_from_registry() {
        return (
            ShellSpec {
                command: path,
                args: vec!["--login".to_string(), "-i".to_string()],
            },
            ShellSource::GitBashRegistry,
        );
    }

    if let Some(path) = bash_from_path() {
        return (
            ShellSpec {
                command: path,
                args: vec!["--login".to_string(), "-i".to_string()],
            },
            ShellSource::GitBashPath,
        );
    }

    warn!(
        "shell detection: Git Bash not found; defaulting to PowerShell. \
         Install Git for Windows to enable Git Bash."
    );
    (
        ShellSpec {
            command: PathBuf::from("powershell.exe"),
            args: vec!["-NoLogo".to_string()],
        },
        ShellSource::PowerShellFallback,
    )
}

#[cfg(windows)]
fn git_bash_from_registry() -> Option<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    // Git for Windows writes `InstallPath` under HKLM only for admin
    // installs; a per-user "just for me" install (common on locked-down
    // machines, lands in %LocalAppData%\Programs\Git) writes HKCU instead.
    // Probe both, validating per hive so a stale HKLM leftover from an
    // uninstall still falls through to a live HKCU install.
    for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        let Ok(key) = RegKey::predef(hive).open_subkey(r"SOFTWARE\GitForWindows") else {
            continue;
        };
        let Ok(install_path) = key.get_value::<String, _>("InstallPath") else {
            continue;
        };
        let mut path = PathBuf::from(install_path);
        path.push("bin");
        path.push("bash.exe");
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// A PATH-resolved `bash.exe` that is NOT the WSL launcher shim. Windows
/// ships `C:\Windows\System32\bash.exe` whenever the WSL optional feature
/// is (or ever was) enabled, and System32 is unconditionally on PATH — a
/// bare `which("bash.exe")` on a Git-Bash-less box with WSL would resolve
/// that shim, mislabel it `GitBashPath` (suppressing the install-Git
/// banner), and pre-fill the new-tab dialog with a shell that boots a WSL
/// distro (or errors when none is installed) instead of Git Bash.
#[cfg(windows)]
fn bash_from_path() -> Option<PathBuf> {
    let windir = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("windir"))
        .map(PathBuf::from);
    for candidate in which::which_all("bash.exe").ok()? {
        if windir
            .as_ref()
            .is_some_and(|w| path_starts_with_ci(&candidate, w))
        {
            warn!(
                path = %candidate.display(),
                "shell detection: skipping the WSL bash.exe shim on PATH"
            );
            continue;
        }
        return Some(candidate);
    }
    None
}

/// Case-insensitive "is `path` strictly under `dir`". Windows paths compare
/// case-insensitively and PATH entries' casing is whatever the user's
/// environment happens to contain (`C:\WINDOWS\system32` vs
/// `C:\Windows\System32`), so a plain `Path::starts_with` is not enough.
#[cfg(windows)]
fn path_starts_with_ci(path: &Path, dir: &Path) -> bool {
    let p: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    let d: Vec<String> = dir
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    !d.is_empty() && p.len() > d.len() && p[..d.len()] == d[..]
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_returns_a_spec() {
        // We can't assert anything strong about the result without faking
        // the env or filesystem — just verify the function returns *some*
        // spec on the host that runs the test.
        let (spec, _source) = default_shell_resolution();
        assert!(!spec.command.as_os_str().is_empty());
        assert!(!spec.args.is_empty(), "all defaults set at least one arg");
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_uses_known_args() {
        let (spec, source) = default_shell_resolution();
        match source {
            ShellSource::GitBashProgramFiles
            | ShellSource::GitBashRegistry
            | ShellSource::GitBashPath => {
                assert_eq!(spec.args, vec!["--login".to_string(), "-i".to_string()]);
            }
            ShellSource::PowerShellFallback => {
                assert_eq!(spec.args, vec!["-NoLogo".to_string()]);
            }
            other => panic!("unexpected source on windows: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_default_uses_interactive_flag() {
        let (spec, _) = default_shell_resolution();
        assert_eq!(spec.args, vec!["-i".to_string()]);
    }

    /// Regression (2026-07 review): the PATH probe must skip the WSL shim
    /// at `%SystemRoot%\System32\bash.exe` — and Windows path comparison is
    /// case-insensitive, so the guard must be too.
    #[cfg(windows)]
    #[test]
    fn wsl_shim_detection_is_case_insensitive_and_strict() {
        let dir = Path::new(r"C:\Windows");
        assert!(path_starts_with_ci(
            Path::new(r"C:\WINDOWS\System32\bash.exe"),
            dir
        ));
        assert!(path_starts_with_ci(
            Path::new(r"c:\windows\system32\bash.exe"),
            dir
        ));
        assert!(!path_starts_with_ci(
            Path::new(r"C:\Program Files\Git\bin\bash.exe"),
            dir
        ));
        // Equal path is not "under" the dir.
        assert!(!path_starts_with_ci(Path::new(r"C:\Windows"), dir));
        // A sibling whose name merely shares the prefix must not match.
        assert!(!path_starts_with_ci(
            Path::new(r"C:\Windows2\bash.exe"),
            dir
        ));
    }

    #[test]
    fn git_bash_sources_are_recognized() {
        assert!(is_git_bash_source(&ShellSource::GitBashProgramFiles));
        assert!(is_git_bash_source(&ShellSource::GitBashRegistry));
        assert!(is_git_bash_source(&ShellSource::GitBashPath));
        assert!(!is_git_bash_source(&ShellSource::PowerShellFallback));
    }
}

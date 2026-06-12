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
//! | Windows  | Git for Windows installed elsewhere (e.g. `D:\dev\Git`)          | Path read from `HKLM\SOFTWARE\GitForWindows\InstallPath` | n/a (rare) |
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

use std::path::PathBuf;
#[cfg(unix)]
use std::path::Path;

use tracing::info;
#[cfg(unix)]
use tracing::debug;
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
        // but keep the function total.
        warn!("shell detection: unknown platform; using /bin/sh");
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

/// True when the resolved default came from a Git Bash probe (any of the
/// three Windows paths). M2's new-shell-tab dialog uses this to decide
/// whether to render the install-Git banner.
pub fn was_default_git_bash_found() -> bool {
    matches!(
        default_shell_resolution().1,
        ShellSource::GitBashProgramFiles
            | ShellSource::GitBashRegistry
            | ShellSource::GitBashPath
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
        if path.is_file() {
            return (
                ShellSpec {
                    command: path,
                    args: vec!["--login".to_string(), "-i".to_string()],
                },
                ShellSource::GitBashRegistry,
            );
        }
    }

    if let Ok(path) = which::which("bash.exe") {
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
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm.open_subkey(r"SOFTWARE\GitForWindows").ok()?;
    let install_path: String = key.get_value("InstallPath").ok()?;
    let mut path = PathBuf::from(install_path);
    path.push("bin");
    path.push("bash.exe");
    Some(path)
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
}

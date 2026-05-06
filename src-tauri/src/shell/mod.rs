//! Cross-platform shell detection.
//!
//! Produces the default [`ShellSpec`] (command + args) for the current
//! platform: `$SHELL` on Linux with sensible fallbacks; Git Bash auto-detected
//! on Windows with a PowerShell fallback. Phase 6 of MILESTONE-V3-01 caches
//! the resolved spec on `AppState` at startup; M1 re-runs detection on each
//! call (a handful of file-existence checks plus one optional registry read,
//! all sub-millisecond).

pub mod detect;

use std::path::PathBuf;

/// Concrete launch parameters for a shell subprocess. The PTY layer turns
/// this into a `PtyLaunchSpec` in Phase 3 (Shell tabs skip the
/// `tts_injection` / `extra_cli_flags` plumbing the AI tabs use).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellSpec {
    pub command: PathBuf,
    pub args: Vec<String>,
}

/// Where the resolved default shell came from. Lets the new-shell-tab
/// dialog (M2) know whether to surface the "Git Bash not detected, defaulting
/// to PowerShell" banner — and lets startup logging make the fallback
/// path visible without inferring it from the binary path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellSource {
    /// Linux: `$SHELL` env var resolved to an existing binary.
    EnvShell,
    /// Linux: `/bin/bash` fallback.
    BashFallback,
    /// Linux: `/bin/sh` fallback (last resort — bash absent too).
    ShFallback,
    /// Windows: Git Bash found under Program Files at a standard location.
    GitBashProgramFiles,
    /// Windows: Git Bash found via the `HKLM\SOFTWARE\GitForWindows`
    /// registry key.
    GitBashRegistry,
    /// Windows: Git Bash resolved from the PATH (less common; covers
    /// portable installs).
    GitBashPath,
    /// Windows: no Git Bash; falling back to `powershell.exe`. The new-
    /// shell-tab dialog renders the install-Git banner when this fires.
    PowerShellFallback,
}

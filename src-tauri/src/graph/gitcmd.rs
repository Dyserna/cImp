//! Shared synchronous `git` spawn helper for the graph module's git-derived
//! features (`impact::changed_symbols`, `gitmeta::collect`/`collect_for`) —
//! previously duplicated verbatim in both modules (V12 review). Deliberately
//! synchronous: both callers run from sync contexts (a rebuild/watcher
//! thread, or a one-shot diff-analysis call), never the async runtime.
//!
//! `checks::gitls::changed_files`'s own `run_git` stays separate on purpose —
//! it's async (awaited from the `run_check` hot path) and returns
//! `AppError::Checks` rather than `AppError::Graph`, so merging it here would
//! either force this helper async (penalizing the two sync callers) or leak
//! a graph-specific error type into `checks`. Two helpers, same shape, is the
//! right amount of duplication for that split.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{AppError, AppResult};

/// Run `git <args>` with cwd = `root`, console-suppressed on Windows (the
/// `CREATE_NO_WINDOW` convention shared by every spawned subprocess in this
/// codebase), returning captured stdout. `Err` on a non-zero exit (not a
/// repo, no `git` on PATH, no `HEAD` yet, ...) — callers decide how to
/// degrade.
pub(crate) fn run_git(root: &Path, args: &[&str]) -> AppResult<String> {
    let program = crate::pty::resolve_command("git")?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let output = cmd
        .output()
        .map_err(|e| AppError::Graph(format!("git {}: {e}", args.join(" "))))?;
    if !output.status.success() {
        return Err(AppError::Graph(format!(
            "git {} exited with {:?}",
            args.join(" "),
            output.status.code()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

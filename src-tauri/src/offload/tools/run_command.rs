//! Native `run_command` tool — allowlisted, read-only-intent command
//! execution for build/test/`git log`-style probes. Deny by default: a
//! command runs only if its program name matches `command_allowlist`. Run
//! in the first allowed root, time-bounded, output captured and truncated.

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::offload::openai::ToolDef;

use super::ToolCtx;

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Deserialize)]
struct Args {
    /// Program to run (must match the allowlist by name).
    command: String,
    #[serde(default)]
    args: Vec<String>,
}

pub fn def() -> ToolDef {
    ToolDef::function(
        "run_command",
        "Run an allowlisted, read-only command (e.g. build/test/git-log probes) \
         in the project root and return its captured output. Only programs on \
         the configured allowlist may run; anything else is refused.",
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Program to run (must be allowlisted)." },
                "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments." }
            },
            "required": ["command"]
        }),
    )
}

/// Match the requested program against the allowlist by its file-stem
/// name, case-insensitively (so `git` and `git.exe` both match an
/// allowlist entry of `git`).
fn is_allowed(command: &str, allowlist: &[String]) -> bool {
    let stem = std::path::Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(command);
    allowlist
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(stem) || allowed.eq_ignore_ascii_case(command))
}

/// True only for a bare program name with no path component (`git`,
/// `git.exe`) — not `/usr/bin/git`, `./git`, `..\git`, or `C:\evil\git.exe`.
///
/// This is the security boundary: a stem-only allowlist check is
/// meaningless on its own because the model could pass an absolute path
/// to an arbitrary binary named `git` and we would spawn that exact file.
/// By requiring a bare name we force resolution through PATH (see
/// [`resolve_command`]), so only the operator's PATH decides which `git`
/// runs.
fn is_bare_command(command: &str) -> bool {
    !command.is_empty() && !command.contains(['/', '\\', ':'])
}

/// Reject argument patterns that turn an allowlisted, "read-only" program
/// into arbitrary code execution or let it escape the allowed root. Scoped by
/// program stem so each rule only applies where the flag is actually dangerous
/// (avoids false positives like `rg -c` = --count). Returns `Some(reason)` to
/// refuse, `None` to allow.
fn dangerous_args(command: &str, args: &[String]) -> Option<String> {
    let stem = std::path::Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    if stem == "git" {
        // `-c k=v` / `--config-env` inject config (alias.*=!sh, core.pager,
        // core.sshCommand, …) → arbitrary command execution.
        // `-C <path>` / `--exec-path` change the working/exec directory →
        // escape the allowed root.
        // `--upload-pack` / `--receive-pack` run an arbitrary helper binary.
        for a in args {
            let blocked = a == "-c"
                || a == "-C"
                || a.starts_with("--config-env")
                || a.starts_with("--exec-path")
                || a.starts_with("--upload-pack")
                || a.starts_with("--receive-pack");
            if blocked {
                return Some(format!(
                    "`git {a}` is refused: it can execute arbitrary commands or escape \
                     the project root. run_command is for read-only probes only."
                ));
            }
        }
    }
    None
}

pub async fn execute(args: serde_json::Value, ctx: &ToolCtx) -> Result<String, String> {
    let args: Args = serde_json::from_value(args).map_err(|e| format!("invalid run_command args: {e}"))?;
    if ctx.command_allowlist.is_empty() {
        return Err("run_command is disabled — no commands are allowlisted".into());
    }
    if !is_bare_command(&args.command) {
        return Err(format!(
            "`{}` must be a bare program name with no path — only allowlisted \
             programs resolved through PATH may run",
            args.command
        ));
    }
    if !is_allowed(&args.command, &ctx.command_allowlist) {
        return Err(format!(
            "`{}` is not allowlisted (allowed: {})",
            args.command,
            ctx.command_allowlist.join(", ")
        ));
    }
    // Best-effort arg hardening. The allowlist is the real boundary (operators
    // must only allowlist genuinely read-only programs), but some allowlisted
    // tools expose global flags that turn them into arbitrary code execution
    // or let them escape the allowed root. We can't denylist generically
    // without false positives (e.g. `rg -c` = --count), so we guard the
    // known-dangerous flags of the tools we explicitly document as examples.
    if let Some(reason) = dangerous_args(&args.command, &args.args) {
        return Err(reason);
    }
    // Resolve through PATH so we spawn the operator's `git`, never a binary
    // the model pointed us at (path components are already rejected above,
    // but this also pins the result against a PATH/CWD-resolution surprise).
    let program = crate::pty::resolve_command(&args.command)
        .map_err(|_| format!("`{}` was not found on PATH", args.command))?;

    let cwd = ctx.allowed_roots.first().cloned().ok_or_else(|| {
        "run_command has no allowed root to execute in".to_string()
    })?;
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args.args)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Kill the child if this future is dropped — without it, a timeout
        // (below) drops `child` while the process keeps running detached,
        // leaking one orphan per hung command.
        .kill_on_drop(true);
    // Don't flash a console window for each spawned command on Windows.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn `{}`: {e}", args.command))?;

    // Read stdout/stderr concurrently with waiting, each capped so a command
    // that floods output (e.g. `git log -p` on a huge repo) can't balloon RSS
    // before truncation — the cap bounds memory, not just the returned string.
    // We keep draining past the cap (discarding) so the child never blocks on
    // a full pipe and can exit cleanly.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let run = async {
        let (out, err, status) = tokio::join!(
            read_capped(stdout, MAX_OUTPUT_BYTES),
            read_capped(stderr, MAX_OUTPUT_BYTES),
            child.wait(),
        );
        (out, err, status)
    };
    let (out, err, status) = match tokio::time::timeout(TIMEOUT, run).await {
        Ok((out, err, status)) => (out, err, status),
        // `run` (and the `child` it borrows) is dropped here → kill_on_drop reaps it.
        Err(_) => return Err(format!("`{}` timed out after {}s", args.command, TIMEOUT.as_secs())),
    };
    let status = status.map_err(|e| format!("`{}` failed: {e}", args.command))?;

    let mut truncated = out.capped || err.capped;
    let mut combined = String::new();
    if !out.bytes.is_empty() {
        combined.push_str(&String::from_utf8_lossy(&out.bytes));
    }
    if !err.bytes.is_empty() {
        if !combined.is_empty() {
            combined.push_str("\n--- stderr ---\n");
        }
        combined.push_str(&String::from_utf8_lossy(&err.bytes));
    }
    let status = status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".into());

    if combined.len() > MAX_OUTPUT_BYTES {
        let cut = combined.char_indices().take_while(|(i, _)| *i < MAX_OUTPUT_BYTES).last().map(|(i, c)| i + c.len_utf8()).unwrap_or(0);
        combined.truncate(cut);
        truncated = true;
    }
    if truncated {
        combined.push_str("\n[output truncated]");
    }
    Ok(format!("(exit {status})\n{combined}"))
}

/// Bytes captured from one stream, plus whether more was produced than the cap.
struct Captured {
    bytes: Vec<u8>,
    capped: bool,
}

/// Read `reader` to EOF, retaining at most `cap` bytes but continuing to drain
/// (and discard) the rest so the child isn't blocked on a full pipe. `None`
/// readers (a stream that wasn't piped) yield empty output.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, cap: usize) -> Captured {
    let mut bytes = Vec::new();
    let mut capped = false;
    if let Some(mut reader) = reader {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if bytes.len() < cap {
                        let take = n.min(cap - bytes.len());
                        bytes.extend_from_slice(&chunk[..take]);
                        if take < n {
                            capped = true;
                        }
                    } else {
                        capped = true;
                    }
                }
            }
        }
    }
    Captured { bytes, capped }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matches_by_stem() {
        let allow = vec!["git".to_string(), "cargo".to_string()];
        assert!(is_allowed("git", &allow));
        assert!(is_allowed("git.exe", &allow));
        assert!(is_allowed("/usr/bin/git", &allow));
        assert!(!is_allowed("rm", &allow));
        assert!(!is_allowed("npm", &allow));
    }

    #[test]
    fn empty_allowlist_denies() {
        assert!(!is_allowed("git", &[]));
    }

    #[test]
    fn bare_command_rejects_paths() {
        // Bare names are allowed (resolved via PATH).
        assert!(is_bare_command("git"));
        assert!(is_bare_command("git.exe"));
        // Anything with a path component is rejected — this is the control
        // that stops the model spawning an arbitrary on-disk binary that
        // merely *stems* to an allowlisted name.
        assert!(!is_bare_command("/usr/bin/git"));
        assert!(!is_bare_command("./git"));
        assert!(!is_bare_command("..\\git"));
        assert!(!is_bare_command("C:\\evil\\git.exe"));
        assert!(!is_bare_command("C:git"));
        assert!(!is_bare_command(""));
    }
}

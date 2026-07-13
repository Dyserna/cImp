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
use crate::settings::CommandPolicy;

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

/// The lowercased file-stem of a program name (`git.exe` → `git`), used to
/// match a [`CommandPolicy`] to the program being run.
fn command_stem(command: &str) -> String {
    std::path::Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase()
}

/// The security policy (if any) that applies to `command`, matched by stem
/// case-insensitively.
fn policy_for<'a>(command: &str, policies: &'a [CommandPolicy]) -> Option<&'a CommandPolicy> {
    let stem = command_stem(command);
    policies.iter().find(|p| p.program.eq_ignore_ascii_case(&stem))
}

/// True for a POSIX short flag (`-c`, `-C`) — a single dash followed by exactly
/// one non-dash byte. Such flags accept their value glued on with no separator.
fn is_short_flag(d: &str) -> bool {
    let b = d.as_bytes();
    b.len() == 2 && b[0] == b'-' && b[1] != b'-'
}

/// Whether `arg` is refused by `denied`: it matches when it equals an entry OR
/// starts with `<entry>=` (covers both `--flag value` and `--flag=value`). For
/// short flags it ALSO matches the glued form `-ccore.hooksPath=/x` — git (and
/// any getopt program) accepts a short flag's value with no separator, so
/// without this the glued spelling slips past the `-c` guard entirely.
fn flag_denied(arg: &str, denied: &[String]) -> bool {
    denied.iter().any(|d| {
        arg == d
            || arg.starts_with(&format!("{d}="))
            || (is_short_flag(d) && arg.len() > d.len() && arg.starts_with(d.as_str()))
    })
}

/// Reject argument patterns, per the program's [`CommandPolicy`], that would
/// turn an allowlisted "read-only" program into arbitrary code execution or let
/// it escape the allowed root (e.g. `git config`, `git -c`, `git --git-dir`).
/// Policy-driven so the rules are visible/editable in Settings rather than
/// hardcoded, and applicable to any program — not just git. Returns
/// `Some(reason)` to refuse, `None` to allow.
fn dangerous_args(command: &str, args: &[String], policies: &[CommandPolicy]) -> Option<String> {
    let policy = policy_for(command, policies)?;
    // Denied flags anywhere in argv.
    for a in args {
        if flag_denied(a, &policy.denied_flags) {
            return Some(format!(
                "`{} {a}` is refused by the `{}` command policy: it can execute \
                 arbitrary commands or escape the project root. run_command is for \
                 read-only probes only.",
                policy.program, policy.program
            ));
        }
    }
    // Denied subcommand = the first non-flag token. SECURITY: this is only
    // sound if every value-CONSUMING global flag is in `denied_flags` and so
    // refused by the loop above — otherwise a non-denied value-taking flag
    // (e.g. `git --namespace x config`) shifts the first non-flag token onto
    // the flag's value, hiding the real subcommand. The default `git` policy
    // enumerates all of git's value-taking globals for exactly this reason; a
    // custom policy with `denied_subcommands` MUST do the same for its program.
    if !policy.denied_subcommands.is_empty() {
        if let Some(sub) = args.iter().find(|a| !a.starts_with('-')) {
            if policy
                .denied_subcommands
                .iter()
                .any(|d| d.eq_ignore_ascii_case(sub))
            {
                return Some(format!(
                    "`{} {sub}` is refused by the `{}` command policy: this \
                     subcommand can persist state that lets a later command execute \
                     arbitrary code. run_command is for read-only probes only.",
                    policy.program, policy.program
                ));
            }
        }
    }
    // Allowed-subcommand allowlist (V21 F7): when non-empty, the first non-flag
    // token MUST be one of these — every other subcommand, and a bare
    // invocation, is refused. This is the strict counterpart to the denylist
    // above: it keeps an allowlisted program pinned to a few read-only verbs
    // (e.g. `cargo metadata`/`cargo tree`) so it can never reach `cargo
    // run`/`build` — including cargo's built-in aliases (`r`/`b`), which a
    // denylist of full names would miss. Soundness of "first non-flag token = the
    // subcommand" relies on the program's value-taking globals being in
    // `denied_flags` (refused by the loop above), exactly as the `git` policy
    // requires for its `denied_subcommands`.
    if !policy.allowed_subcommands.is_empty() {
        match args.iter().find(|a| !a.starts_with('-')) {
            Some(sub)
                if policy
                    .allowed_subcommands
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(sub)) => {}
            Some(sub) => {
                return Some(format!(
                    "`{} {sub}` is not an allowed subcommand for the `{}` command policy \
                     (allowed: {}). run_command is for read-only probes only.",
                    policy.program,
                    policy.program,
                    policy.allowed_subcommands.join(", ")
                ))
            }
            None => {
                return Some(format!(
                    "`{}` needs one of its allowed subcommands ({}) — a bare `{}` is refused \
                     by its command policy.",
                    policy.program,
                    policy.allowed_subcommands.join(", "),
                    policy.program
                ))
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
    // Per-program security policy. The allowlist is the real boundary
    // (operators must only allowlist genuinely read-only programs), but some
    // allowlisted tools expose global flags/subcommands that turn them into
    // arbitrary code execution or let them escape the allowed root. The
    // applicable `CommandPolicy` (visible/editable in Settings) names the
    // denied flags/subcommands; a program with no policy gets only the
    // allowlist + bare-name guard.
    if let Some(reason) = dangerous_args(&args.command, &args.args, &ctx.command_policies) {
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
    // Defense in depth: apply the program's policy env at spawn. For git these
    // neutralize config-driven hooks even if a hostile repo already set them
    // (GIT_PAGER=cat overrides core.pager; an empty GIT_SSH_COMMAND disarms a
    // config-injected ssh helper; NOSYSTEM/GLOBAL/PROMPT keep the probe from
    // honoring ambient config or hanging on a credential prompt).
    if let Some(policy) = policy_for(&args.command, &ctx.command_policies) {
        for ev in &policy.env {
            cmd.env(&ev.key, &ev.value);
        }
    }
    // Don't flash a console window for each spawned command on Windows.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn `{}`: {e}", args.command))?;
    // Backstop: reap this command subprocess via the kill-on-job-close job if
    // cImp dies hard before kill_on_drop can fire.
    crate::process_guard::guard_child(&child);

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
                Ok(0) => break,
                Err(_) => {
                    // A read error mid-stream means the captured output may be
                    // incomplete — flag it like a cap so the caller doesn't
                    // present partial output as the whole result.
                    capped = true;
                    break;
                }
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

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_git_policy_blocks_exec_vectors() {
        let policies = crate::settings::default_command_policies();
        // Denied flags (exact and `=`-form), via stem match incl. `git.exe`.
        assert!(dangerous_args("git", &argv(&["-c", "core.pager=!sh"]), &policies).is_some());
        assert!(dangerous_args("git.exe", &argv(&["-C", "/etc"]), &policies).is_some());
        assert!(dangerous_args("git", &argv(&["--git-dir=/other/.git", "log"]), &policies).is_some());
        assert!(dangerous_args("git", &argv(&["--work-tree", "/"]), &policies).is_some());
        // Denied subcommand as the first non-flag token.
        assert!(dangerous_args("git", &argv(&["config", "core.pager", "x"]), &policies).is_some());
    }

    #[test]
    fn glued_short_flag_value_is_blocked() {
        // Regression (S1): a POSIX short flag accepts its value glued on with no
        // separator. `-ccore.hooksPath=/x` must be refused exactly like
        // `-c core.hooksPath=/x` — the glued spelling previously bypassed the
        // `-c` guard entirely.
        let policies = crate::settings::default_command_policies();
        assert!(dangerous_args("git", &argv(&["-ccore.hooksPath=/x", "status"]), &policies).is_some());
        assert!(dangerous_args("git", &argv(&["-C/etc"]), &policies).is_some());
        // A long flag that merely starts with a denied short flag's letters is
        // NOT a glued short flag and must not be over-matched.
        assert!(dangerous_args("git", &argv(&["--color", "log"]), &policies).is_none());
    }

    #[test]
    fn value_taking_global_cannot_shift_the_subcommand_check() {
        // Regression: a value-consuming global flag must not push the
        // first-non-flag-token off the real `config` subcommand. Every
        // value-taking git global is denied, so these are refused at the flag
        // loop before the (positional) subcommand check even runs.
        let policies = crate::settings::default_command_policies();
        assert!(dangerous_args("git", &argv(&["--namespace", "x", "config", "--local", "alias.p", "!sh"]), &policies).is_some());
        assert!(dangerous_args("git", &argv(&["--super-prefix", "p/", "config", "core.pager", "!sh"]), &policies).is_some());
        assert!(dangerous_args("git", &argv(&["--attr-source", "HEAD", "config", "x", "y"]), &policies).is_some());
        // A legitimate read probe whose ARGUMENT is "config" still runs.
        assert!(dangerous_args("git", &argv(&["grep", "config"]), &policies).is_none());
        assert!(dangerous_args("git", &argv(&["log", "--grep", "config"]), &policies).is_none());
    }

    #[test]
    fn default_git_policy_allows_read_probes() {
        let policies = crate::settings::default_command_policies();
        assert!(dangerous_args("git", &argv(&["log", "--oneline", "-n", "5"]), &policies).is_none());
        assert!(dangerous_args("git", &argv(&["diff", "--stat"]), &policies).is_none());
        assert!(dangerous_args("git", &argv(&["status"]), &policies).is_none());
        // `config` only as a later pathspec, not the subcommand, is fine.
        assert!(dangerous_args("git", &argv(&["log", "--", "config"]), &policies).is_none());
    }

    #[test]
    fn custom_policy_enforced_for_non_git_program() {
        let policies = vec![CommandPolicy {
            program: "cargo".to_string(),
            denied_flags: vec!["--config".to_string()],
            denied_subcommands: vec!["publish".to_string()],
            allowed_subcommands: vec![],
            env: vec![],
        }];
        assert!(dangerous_args("cargo", &argv(&["--config", "x=y", "build"]), &policies).is_some());
        assert!(dangerous_args("cargo", &argv(&["publish"]), &policies).is_some());
        assert!(dangerous_args("cargo", &argv(&["build", "--release"]), &policies).is_none());
    }

    #[test]
    fn readonly_cargo_policy_allows_only_metadata_and_tree() {
        // V21 F7: the preset's `cargo` policy pins cargo to read-only verbs.
        let policies = vec![crate::settings::readonly_cargo_policy()];
        // The two allowed subcommands (with their own flags) pass.
        assert!(dangerous_args("cargo", &argv(&["metadata"]), &policies).is_none());
        assert!(dangerous_args("cargo", &argv(&["metadata", "--format-version", "1"]), &policies).is_none());
        assert!(dangerous_args("cargo", &argv(&["tree"]), &policies).is_none());
        assert!(dangerous_args("cargo", &argv(&["tree", "-e", "features"]), &policies).is_none());
        // Everything that builds/runs/executes project code is refused.
        assert!(dangerous_args("cargo", &argv(&["run"]), &policies).is_some());
        assert!(dangerous_args("cargo", &argv(&["build", "--release"]), &policies).is_some());
        assert!(dangerous_args("cargo", &argv(&["test"]), &policies).is_some());
        assert!(dangerous_args("cargo", &argv(&["install", "ripgrep"]), &policies).is_some());
        // Built-in aliases a denylist would miss are refused by the allowlist.
        assert!(dangerous_args("cargo", &argv(&["r"]), &policies).is_some());
        assert!(dangerous_args("cargo", &argv(&["b"]), &policies).is_some());
        // A bare `cargo` (no subcommand) is refused, not silently allowed.
        assert!(dangerous_args("cargo", &argv(&[]), &policies).is_some());
    }

    #[test]
    fn readonly_cargo_policy_blocks_code_exec_and_escape_globals() {
        // V21 F7: cargo's value-taking / code-executing globals are denied, in
        // both `--flag value`, `--flag=value`, and glued short-flag forms — so
        // they can neither inject a runner nor shift the subcommand check.
        let policies = vec![crate::settings::readonly_cargo_policy()];
        // `--config` can install a runner/rustc-wrapper → arbitrary exec.
        assert!(dangerous_args("cargo", &argv(&["--config", "target.x.runner='sh'", "metadata"]), &policies).is_some());
        assert!(dangerous_args("cargo", &argv(&["--config=build.rustc-wrapper=/x", "tree"]), &policies).is_some());
        // `-C dir` escapes the working root; glued form must also be blocked.
        assert!(dangerous_args("cargo", &argv(&["-C", "/etc", "tree"]), &policies).is_some());
        assert!(dangerous_args("cargo", &argv(&["-C/etc", "tree"]), &policies).is_some());
        // `-Z` unstable flags, glued too.
        assert!(dangerous_args("cargo", &argv(&["-Z", "unstable-options", "metadata"]), &policies).is_some());
        assert!(dangerous_args("cargo", &argv(&["-Zbuild-std", "tree"]), &policies).is_some());
    }

    #[test]
    fn program_without_policy_passes_through() {
        let policies = crate::settings::default_command_policies(); // only git
        // No policy for `rg`/`cargo` → arg hardening is a no-op (allowlist +
        // bare-name remain the guard).
        assert!(dangerous_args("rg", &argv(&["-c", "pattern"]), &policies).is_none());
        assert!(dangerous_args("cargo", &argv(&["--config", "x"]), &policies).is_none());
    }
}

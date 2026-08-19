//! Native `run_command` tool — allowlisted, read-only-intent command
//! execution for build/test/`git log`-style probes. Deny by default: a
//! command runs only if its program name matches `command_allowlist`. Run
//! in the first allowed root, time-bounded, output captured and truncated,
//! and — since V33 contract C2 — with an environment built up from an explicit
//! allowlist ([`crate::sandbox::child_env::CHILD_ENV`]) rather than inherited
//! from cImp.
//!
//! # V38 Phase D — a registered `command`-kind plugin tool is its own entry
//!
//! Design authority, "Registry semantics": *a `command`-kind registry entry
//! (explicit path + enabled) BECOMES the allowlist entry and the path
//! resolution, superseding a separate allowlist for registered tools.* So there
//! are two ways a program may run here, and they answer different questions:
//!
//! | | who says it may run | which binary |
//! |---|---|---|
//! | allowlist | `command_allowlist` names the stem | PATH (`resolve_command`) |
//! | registry | the tool is enabled AND has a path | that exact path |
//!
//! The registry arm is **narrower on both axes**, which is why it is allowed to
//! supersede: enabling a plugin tool and pointing it at a binary is a stronger,
//! more specific act than typing a name into a list, and it names the file
//! rather than trusting whatever PATH resolves that name to today. Everything
//! else is unchanged and applies to both: the bare-name guard, the
//! [`CommandPolicy`] argv rules (policies are about ARGUMENTS, not about which
//! file), the timeout, the caps, and the V33 sandbox.
//!
//! The registry is read at INVOCATION time (invariant 9) — not baked into
//! [`ToolCtx`] — so enabling a tool or changing its path takes effect on the
//! next call rather than on the next worker spawn.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::offload::openai::ToolDef;
use crate::plugins::manifest::ToolKind;
use crate::plugins::posture::ToolPosture;
use crate::plugins::registry::EffectiveTool;
use crate::sandbox::child_env::minimal_env;
use crate::settings::CommandPolicy;

use super::ToolCtx;

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

/// The caller-side backstop on `spawn_and_capture` for THIS seam's fixed
/// [`TIMEOUT`] (2026-08-18 incident). The rule, the slack and the reasoning are
/// [`crate::sandbox::backstop_for`]'s — every sandboxed seam derives its own
/// backstop from its own child timeout through that one function, so no two of
/// them can drift apart. `sandbox_backstop_exceeds_the_child_timeout` pins the
/// relation here.
///
/// `allow(dead_code)` off Windows for the same reason `sandbox::mod`'s helpers
/// carry it: the only non-test consumer is the Windows AppContainer path.
#[cfg_attr(not(windows), allow(dead_code))]
const SANDBOX_BACKSTOP: Duration = crate::sandbox::backstop_for(TIMEOUT);

/// The caller-side backstop on sandbox *preparation* — shared, because
/// preparation costs the same on every seam (profile creation, ACL grants,
/// drive mapping) and is bounded for the same reason. See
/// [`crate::sandbox::PREPARE_BACKSTOP`].
const PREPARE_BACKSTOP: Duration = crate::sandbox::PREPARE_BACKSTOP;

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

/// The `command`-kind plugin tools this project may run, read LIVE.
///
/// Invariant 9: nothing about the registry is spawn-baked. `ToolCtx` carries a
/// settings snapshot taken when the agent run started, which is the right shape
/// for the allowlist (it is part of the run's policy) and the wrong one for a
/// registry the user is editing in another window — enabling a tool would then
/// take effect on the next worker spawn instead of the next call. So this reads
/// the plugin set and the settings container here, at the invocation.
///
/// `cwd` keys the per-project binary-path map, exactly as the audit fan-out and
/// the check seam do.
fn registered_commands(cwd: &Path) -> Vec<EffectiveTool> {
    let settings = crate::settings::load_readonly(cwd);
    crate::plugins::registry::runnable_tools(
        &crate::plugins::snapshot_or_scan(),
        &settings.tool_plugins,
        Some(cwd),
    )
    .into_iter()
    .filter(|t| t.kind() == ToolKind::Command)
    .collect()
}

/// How a requested program earned the right to run, and which file it is.
#[derive(Debug)]
enum Resolution {
    /// A `command_allowlist` entry: resolved through PATH.
    Allowlist,
    /// A registered `command`-kind plugin tool: the configured path IS the
    /// resolution (decision 7 — cImp never picks a plugin's binary).
    Registered(Box<EffectiveTool>),
}

/// Match a requested program against the registered command tools, by the
/// manifest's tool **id**, case-insensitively.
///
/// **Case-insensitively, and that is a Windows decision** made deliberately for
/// both platforms: `is_allowed` has always matched the allowlist that way, the
/// model is not the author of either name, and a `run_command{command:"Git"}`
/// that ran under the allowlist but not under the registry would be a
/// difference nobody could explain. Matching on the id rather than on the
/// namespaced key keeps the model-facing vocabulary a bare program name, which
/// is what the bare-name guard requires anyway.
fn registered_match<'a>(command: &str, registered: &'a [EffectiveTool]) -> Option<&'a EffectiveTool> {
    let stem = command_stem(command);
    registered
        .iter()
        .find(|t| t.tool_id.eq_ignore_ascii_case(&stem) || t.tool_id.eq_ignore_ascii_case(command))
}

/// The admission decision, pure so every branch of it is assertable.
///
/// Order matters: the registry is consulted FIRST, because a registered entry
/// is both the permission and the path, and falling through to the allowlist
/// would resolve the same name through PATH — losing the one guarantee
/// registration buys. A registry miss falls through to the pre-V38 behaviour
/// unchanged.
fn admit(
    command: &str,
    allowlist: &[String],
    registered: &[EffectiveTool],
) -> Result<Resolution, String> {
    // "Disabled" now means BOTH surfaces are empty. An allowlist-less project
    // with a registered command tool is configured, not disabled — the registry
    // entry IS the allowlist entry.
    if allowlist.is_empty() && registered.is_empty() {
        return Err("run_command is disabled — no commands are allowlisted and no plugin \
                    command tools are registered"
            .into());
    }
    if !is_bare_command(command) {
        return Err(format!(
            "`{command}` must be a bare program name with no path — only allowlisted \
             programs resolved through PATH may run"
        ));
    }
    if let Some(tool) = registered_match(command, registered) {
        return Ok(Resolution::Registered(Box::new(tool.clone())));
    }
    if !is_allowed(command, allowlist) {
        // Name both surfaces, because "not allowlisted" alone reads as a wrong
        // answer to a caller looking at a plugin tool it can see in Settings.
        let mut allowed: Vec<String> = allowlist.to_vec();
        allowed.extend(registered.iter().map(|t| t.tool_id.clone()));
        return Err(format!(
            "`{command}` is not allowlisted (allowed: {})",
            allowed.join(", ")
        ));
    }
    Ok(Resolution::Allowlist)
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
    policies
        .iter()
        .find(|p| p.program.eq_ignore_ascii_case(&stem))
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

// ── V33 contract C2 — the child's minimal environment ──────────────────────
//
// The table itself lives in [`crate::sandbox::child_env`] since the V33
// increment that sandboxed the `run_check` and audit seams: those children are
// the same threat class, and a security allowlist with two copies has none.
// What stays here is this seam's USE of it — `apply_minimal_env`, which is
// `tokio::process::Command`-shaped and therefore plain-path-only.

/// Give `cmd` the minimal environment: clear whatever it inherited, then set
/// exactly the allowlisted names.
///
/// `env_clear` FIRST is the whole point — `tokio::process::Command` starts out
/// inheriting cImp's environment, so anything short of clearing it leaves the
/// inherit-and-subtract posture the spec rejects.
fn apply_minimal_env(
    cmd: &mut tokio::process::Command,
    lookup: &dyn Fn(&str) -> Option<OsString>,
) {
    cmd.env_clear();
    for (name, value) in minimal_env(lookup) {
        cmd.env(name, value);
    }
}

pub async fn execute(args: serde_json::Value, ctx: &ToolCtx) -> Result<String, String> {
    let args: Args =
        serde_json::from_value(args).map_err(|e| format!("invalid run_command args: {e}"))?;
    // The root is resolved FIRST now: it is both where the command runs and the
    // key the registry's per-project binary paths are stored under, so the
    // admission decision below needs it.
    let cwd = ctx
        .allowed_roots
        .first()
        .cloned()
        .ok_or_else(|| "run_command has no allowed root to execute in".to_string())?;
    let registered = registered_commands(&cwd);
    let resolution = admit(&args.command, &ctx.command_allowlist, &registered)?;
    // Per-program security policy. The allowlist is the real boundary
    // (operators must only allowlist genuinely read-only programs), but some
    // allowlisted tools expose global flags/subcommands that turn them into
    // arbitrary code execution or let them escape the allowed root. The
    // applicable `CommandPolicy` (visible/editable in Settings) names the
    // denied flags/subcommands; a program with no policy gets only the
    // allowlist + bare-name guard.
    //
    // V38: applied to a REGISTERED tool too, and matched by the same stem. A
    // policy is a statement about a program's ARGUMENTS (`git --exec-path`,
    // `cargo --config`), which does not stop being true because the user
    // configured the binary's path through a plugin instead of typing its name
    // into the allowlist.
    if let Some(reason) = dangerous_args(&args.command, &args.args, &ctx.command_policies) {
        return Err(reason);
    }
    let program = match &resolution {
        // Resolve through PATH so we spawn the operator's `git`, never a binary
        // the model pointed us at (path components are already rejected above,
        // but this also pins the result against a PATH/CWD-resolution surprise).
        Resolution::Allowlist => crate::pty::resolve_command(&args.command)
            .map_err(|_| format!("`{}` was not found on PATH", args.command))?,
        // Decision 7: the user supplied this path, and cImp runs THAT file. No
        // PATH search — the whole point of registering a tool is that the
        // binary is named rather than looked up. Checked for existence here so
        // a stale path reads as a configuration problem rather than as an
        // opaque spawn error (the audit runner's `PathInvalid` chip, in the one
        // shape this seam has for saying it).
        Resolution::Registered(tool) => {
            let path = PathBuf::from(tool.path.as_deref().unwrap_or_default());
            if !path.is_file() {
                return Err(format!(
                    "`{}` is registered by the plugin tool `{}`, but its configured path does \
                     not exist: {} — fix it in Settings → Tool Plugins",
                    args.command,
                    tool.tool_key,
                    path.display()
                ));
            }
            path
        }
    };
    // V38: the manifest's sandbox posture for a registered tool; the historical
    // one (infer, degrade loudly, widen nothing) for an allowlisted program,
    // which declares nothing.
    let posture = match &resolution {
        Resolution::Allowlist => ToolPosture::default(),
        Resolution::Registered(tool) => ToolPosture::resolve(
            crate::sandbox::SEAM_RUN_COMMAND,
            &cwd,
            &ctx.sandbox,
            tool.manifest.runtime,
            tool.manifest.sandbox,
            &tool.manifest.extra_grants,
        ),
    };
    let subject = crate::sandbox::program_subject(&program);
    crate::plugins::posture::runtime_canary(
        crate::sandbox::SEAM_RUN_COMMAND,
        &cwd,
        &subject,
        &posture.runtime,
        &program,
    );
    // `unsupported` means the boundary is not ATTEMPTED — a disabled config is
    // how that reaches `plan`, so nothing is stamped or mapped for a tool that
    // declared it can use neither.
    let unsupported = crate::plugins::posture::unsupported_cfg(
        crate::sandbox::SEAM_RUN_COMMAND,
        &cwd,
        &subject,
        posture.sandbox,
    );
    let sandbox_cfg = unsupported.as_ref().unwrap_or(&ctx.sandbox);
    // V33 Phase A: decide whether this child runs inside the OS sandbox before
    // building the plain command, because the sandboxed path is a different
    // spawn mechanism (a bespoke `CreateProcessW` — std/tokio cannot attach the
    // AppContainer attribute list) rather than a flag on this one.
    //
    // The environment is composed ONCE, here, and both paths consume it: the
    // minimal-env table (contract C2) is unconditional per decision 17, so a
    // sandbox that is off or unavailable changes the OS boundary and nothing
    // about which variables the child sees.
    let base_env = minimal_env(&|key| std::env::var_os(key));
    let policy_env: Vec<(String, OsString)> = policy_for(&args.command, &ctx.command_policies)
        .map(|p| {
            p.env
                .iter()
                .map(|ev| (ev.key.clone(), OsString::from(&ev.value)))
                .collect()
        })
        .unwrap_or_default();
    let plan = match tokio::time::timeout(
        PREPARE_BACKSTOP,
        crate::sandbox::plan(
            sandbox_cfg,
            crate::sandbox::SEAM_RUN_COMMAND,
            &program,
            // Nothing to infer: the model named the program, cImp resolved it,
            // its install dir is granted by `prepare` itself, and everything it
            // writes goes in the (already granted) root. V38 adds a registered
            // tool's declared runtime profile and its screened `extra_grants`;
            // both are empty for an allowlisted program.
            &crate::sandbox::GrantHints {
                runtime: posture.runtime.clone(),
                rows: posture.rows.clone(),
                ..Default::default()
            },
            &cwd,
            &base_env,
        ),
    )
    .await
    {
        Ok(plan) => plan,
        Err(_) => {
            // Wedged BEFORE the spawn: the command was never attempted, and it
            // must not fall back to a plain spawn — degrading the boundary
            // silently is worse than refusing (decision 5, loudly).
            crate::sandbox::record_event(
                crate::sandbox::SEAM_RUN_COMMAND,
                &cwd,
                "wedged",
                crate::sandbox::state_target("wedged", &crate::sandbox::program_subject(&program)),
                format!(
                    "sandbox preparation for `{}` did not settle within {}s \
                     (profile / ACL grants / drive mapping). The command was NOT run — \
                     refusing rather than silently dropping the sandbox boundary. The \
                     preparation thread may still be blocked; if this repeats, restart cImp \
                     and check the sandbox lane for what preceded it.",
                    args.command,
                    PREPARE_BACKSTOP.as_secs(),
                ),
                false,
            );
            return Err(format!(
                "sandbox preparation did not settle within {}s — treating as wedged \
                 (see sandbox lane); `{}` was not run",
                PREPARE_BACKSTOP.as_secs(),
                args.command
            ));
        }
    };
    #[cfg(windows)]
    if let crate::sandbox::Plan::Sandboxed(prepared) = &plan {
        return run_sandboxed(
            prepared,
            &program,
            &args,
            &base_env,
            &policy_env,
            ctx,
            &cwd,
        )
        .await;
    }
    if let crate::sandbox::Plan::Plain(reason) = &plan {
        // V38: `required` means never run unprotected — including when the
        // master switch is off. The command is simply not run, loudly, in both
        // the lane and the result the model sees.
        if let Some(refusal) = crate::plugins::posture::required_refusal(
            crate::sandbox::SEAM_RUN_COMMAND,
            &cwd,
            &subject,
            posture.sandbox,
            reason,
        ) {
            return Err(refusal);
        }
        // Decision 5: degradation is loud, never silent. Deduplicated by reason
        // per session inside `record_skip`, so this cannot flood its lane —
        // except where `unsupported` already minted a more specific row.
        if unsupported.is_none() {
            crate::sandbox::record_skip(
                crate::sandbox::SEAM_RUN_COMMAND,
                reason,
                &subject,
                &cwd,
            );
        }
    }
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
    // V33 contract C2: the child gets an environment built up from nothing —
    // see [`CHILD_ENV`]. This must run BEFORE the policy grants below, because
    // it starts with `env_clear` and would otherwise wipe them.
    apply_minimal_env(&mut cmd, &|key| std::env::var_os(key));
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
    // V33 C3: Unix-only — own process group, so the timeout path below can
    // `killpg` the whole tree. The model names the program here, so a `cargo`
    // or `npm` that forks a build is exactly the shape whose grandchildren must
    // not outlive the timeout.
    crate::procutil::own_process_group(&mut cmd);

    // V33 Phase D — on Linux this IS the sandboxed path. Where Windows needs a
    // whole second spawn mechanism, Landlock is applied to the command built
    // above: the composed environment (C2 base → the program's policy env →
    // the sandbox's TMPDIR/HOME redirections) and a `pre_exec` that restricts
    // the child between fork and exec. An error here REFUSES the run — it never
    // falls through to an unconfined spawn (Phase D decision D3).
    #[cfg(target_os = "linux")]
    if let crate::sandbox::Plan::Sandboxed(prepared) = &plan {
        prepared.apply(
            &mut cmd,
            &base_env,
            policy_env.iter().map(|(k, v)| (k.as_str(), v.clone())),
        )?;
    }

    // Through the spawn gate like every other cImp spawn — see `spawn_gate`.
    // This is the seam whose SANDBOXED twin takes the gate exclusively, so an
    // ungated plain spawn here would be the exact race the gate exists to close.
    // (The Linux sandbox needs no exclusive window: it opens no inheritable
    // handles, so it takes the gate SHARED right here, like any other spawn.)
    let mut child = crate::spawn_gate::spawn_tokio(&mut cmd)
        .map_err(|e| format!("failed to spawn `{}`: {e}", args.command))?;
    // Backstop: reap this command subprocess via the kill-on-job-close job if
    // cImp dies hard before kill_on_drop can fire.
    crate::process_guard::guard_child(&child);
    // The spawn succeeded, so on Linux the boundary is real for this program:
    // say so once, positively — the same confirmation row `run_sandboxed` mints
    // on Windows, and for the same reason (an empty lane must not mean two
    // different things). Deduped per program inside `record_sandboxed`.
    #[cfg(target_os = "linux")]
    if matches!(&plan, crate::sandbox::Plan::Sandboxed(_)) {
        crate::sandbox::record_sandboxed(
            crate::sandbox::SEAM_RUN_COMMAND,
            &cwd,
            &crate::sandbox::program_subject(&program),
            &ctx.sandbox,
        );
    }

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
    // Bound the borrow of `child` to this one statement: `run` holds `&mut
    // child`, and the timeout's result does not, so `child` is usable again on
    // the next line. That is what lets the timeout arm below reach for
    // `kill_tree` instead of settling for `kill_on_drop`.
    let outcome = tokio::time::timeout(TIMEOUT, run).await;
    let (out, err, status) = match outcome {
        Ok((out, err, status)) => (out, err, status),
        Err(_) => {
            // V33 C3: whole-tree kill, not just `kill_on_drop`. Dropping the
            // child only kills the process cImp holds — a `cargo build` or
            // `npm install` that timed out mid-flight leaves its compiler /
            // installer children running, unattached and unbounded, long after
            // the tool call returned an error to the model. `kill_tree` reaps
            // the Windows pid tree and the Unix process group established
            // above; the drop still fires afterwards as the backstop.
            crate::procutil::kill_tree(&mut child).await;
            return Err(format!(
                "`{}` timed out after {}s",
                args.command,
                TIMEOUT.as_secs()
            ));
        }
    };
    let status = status.map_err(|e| format!("`{}` failed: {e}", args.command))?;

    // V33 Phase D — the Linux denial row. This is the last point at which the
    // raw exit code and stderr exist (a nonzero exit is returned to the model as
    // text), which is exactly why `run_sandboxed` mints its equivalent here too.
    // A timeout mints nothing: a hang matches no access-denial signature.
    #[cfg(target_os = "linux")]
    if matches!(&plan, crate::sandbox::Plan::Sandboxed(_)) {
        let stderr_text = String::from_utf8_lossy(&err.bytes);
        if let Some(class) =
            crate::sandbox::denial_signature(status.code(), &stderr_text, ctx.sandbox.allow_network)
        {
            crate::sandbox::record_denial(
                crate::sandbox::SEAM_RUN_COMMAND,
                &cwd,
                &crate::sandbox::program_subject(&program),
                &args.args,
                status.code(),
                &stderr_text,
                class,
                &ctx.sandbox,
            );
        }
    }

    Ok(format_run_output(
        &out.bytes,
        &err.bytes,
        out.capped || err.capped,
        status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into()),
    ))
}

/// Render one finished run the way the model sees it: `(exit N)` then stdout,
/// then stderr under a marker, with a single truncation notice if anything was
/// cut — either by a per-stream cap or by the combined ceiling.
///
/// Shared by the plain and sandboxed paths so the two cannot drift in what a
/// model is shown; V33 Phase A added the second caller and this function with
/// it (the logic is unchanged from the single-path original).
fn format_run_output(
    stdout: &[u8],
    stderr: &[u8],
    capped: bool,
    status: String,
) -> String {
    let mut truncated = capped;
    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str(&String::from_utf8_lossy(stdout));
    }
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push_str("\n--- stderr ---\n");
        }
        combined.push_str(&String::from_utf8_lossy(stderr));
    }
    if combined.len() > MAX_OUTPUT_BYTES {
        let cut = combined
            .char_indices()
            .take_while(|(i, _)| *i < MAX_OUTPUT_BYTES)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        combined.truncate(cut);
        truncated = true;
    }
    if truncated {
        combined.push_str("\n[output truncated]");
    }
    format!("(exit {status})\n{combined}")
}

/// V33 Phase A — run one allowlisted child INSIDE the AppContainer.
///
/// Mirrors the plain path's contract exactly (same env, same caps, same
/// timeout, same rendering) and differs only in the OS boundary: the child's
/// cwd is the sandbox root's mapped drive (so `getcwd` never walks the
/// unlistable `C:\` — spike S1's canonicalization gotcha), and TEMP/HOME are
/// redirected inside the root so a tool that writes state has one writable
/// place to write it.
///
/// The kill-on-close job assignment happens inside `spawn_and_capture` (the
/// same `guard_pid` the PTY child uses), so a hard cImp death still reaps this
/// child — decision 17's "job objects stay unconditional" holds on both paths.
///
/// # What this path tells the `sandbox` lane
///
/// Two things, and they are minted HERE because this is the only place the raw
/// exit code and stderr exist — the function returns `Ok(format_run_output(…))`
/// for a nonzero exit, so by the time the model sees a failure it is just text.
///
/// * A **confirmation** row on a successful spawn (once per program per
///   session): before it, an empty lane meant either "everything ran sandboxed"
///   or "nothing ever spawned", which is not an answer.
/// * A **denial-suspicion** row, every time a failed child's output matches
///   [`crate::sandbox::denial_signature`] — including the spawn-error path
///   below, whose `Err` string is classified before it propagates. A spawn
///   error the classifier does not recognize mints a `refused` row instead
///   ([`crate::sandbox::record_spawn_failure`]), so "no child ever started" is
///   never a silent outcome.
///
/// A timeout mints neither: a hang is not a denial signature, and guessing
/// would put noise in the one lane that is supposed to mean something.
#[cfg(windows)]
async fn run_sandboxed(
    prepared: &crate::sandbox::windows::Prepared,
    program: &std::path::Path,
    args: &Args,
    base_env: &[(&str, OsString)],
    policy_env: &[(String, OsString)],
    ctx: &ToolCtx,
    root: &std::path::Path,
) -> Result<String, String> {
    // Same composition order as the plain path, through the one shared
    // composer: minimal env first, then the program's policy env, then the
    // sandbox's own redirections last — those point at the mapped drive and
    // must win over an inherited TEMP/HOME.
    let mut env = crate::sandbox::child_env::ChildEnv::from_base(base_env);
    env.overlay(policy_env.iter().map(|(k, v)| (k.as_str(), v.clone())));
    env.overlay(
        prepared
            .env_overrides
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone())),
    );
    let env = env.into_pairs();

    // The backstop (2026-08-18): the engine bounds its own waits now, but a
    // path whose only deadline lives inside itself has no deadline at all. If
    // this elapses the child may well have run — we simply do not know, which
    // is precisely what makes it worth a row.
    let settled = tokio::time::timeout(
        SANDBOX_BACKSTOP,
        crate::sandbox::windows::spawn_and_capture(
            prepared,
            crate::sandbox::windows::SpawnRequest {
                program,
                args: &args.args,
                // This seam builds argv itself, so the CRT quoting rules apply.
                raw_tail: None,
                env: &env,
                cwd: &prepared.cwd(),
                cap: MAX_OUTPUT_BYTES,
                timeout: TIMEOUT,
                // No cancel: this seam has no cancel channel and never had one.
                cancel: None,
            },
        ),
    )
    .await;
    let run = match settled {
        Err(_) => {
            // The lane's whole point is that an incident leaves a trace. On the
            // night this was diagnosed it showed nothing at all, because every
            // row was minted downstream of a call that never returned.
            crate::sandbox::record_event(
                crate::sandbox::SEAM_RUN_COMMAND,
                root,
                "wedged",
                crate::sandbox::state_target("wedged", &crate::sandbox::program_subject(program)),
                format!(
                    "`{}` did not settle within {}s (child timeout {}s + {}s settle slack). \
                     The sandboxed spawn helper never returned; the child may have run, may \
                     still be running, or may never have started — cImp cannot tell, so this \
                     row asserts only the wedge. Job-object membership still reaps the tree on \
                     cImp's death.",
                    args.command,
                    SANDBOX_BACKSTOP.as_secs(),
                    TIMEOUT.as_secs(),
                    crate::sandbox::SANDBOX_SETTLE_SLACK.as_secs(),
                ),
                false,
            );
            return Err(format!(
                "sandboxed spawn did not settle within {}s — the child may have run; \
                 treating as wedged (see sandbox lane)",
                SANDBOX_BACKSTOP.as_secs()
            ));
        }
        Ok(Ok(run)) => run,
        Ok(Err(e)) => {
            // Decision 4: the bespoke `CreateProcessW` refusing to start the
            // child is itself a denial shape (a container that cannot read the
            // program image fails right here), so its error string goes through
            // the same classifier as a child's stderr — with no exit code,
            // because nothing ran.
            // An error the classifier does not recognize mints a `refused` row
            // instead of nothing — same funnel as the other two seams, see
            // `sandbox::record_spawn_failure`.
            crate::sandbox::record_spawn_failure(
                crate::sandbox::SEAM_RUN_COMMAND,
                root,
                &crate::sandbox::program_subject(program),
                &args.args,
                &e,
                &ctx.sandbox,
            );
            return Err(e);
        }
    };
    // The spawn succeeded, so the boundary is real for this program: say so
    // once, positively. Deduped per program inside `record_sandboxed`.
    crate::sandbox::record_sandboxed(
        crate::sandbox::SEAM_RUN_COMMAND,
        root,
        &crate::sandbox::program_subject(program),
        &ctx.sandbox,
    );

    if run.timed_out {
        // No denial row: a hang matches no access-denial signature, and
        // labeling one would be the guess this lane must not make.
        return Err(format!(
            "`{}` timed out after {}s",
            args.command,
            TIMEOUT.as_secs()
        ));
    }
    // A nonzero exit returns `Ok` to the model (the output IS the answer), so
    // this is the last point at which the raw exit code and stderr exist.
    let stderr_text = String::from_utf8_lossy(&run.stderr);
    if let Some(class) =
        crate::sandbox::denial_signature(run.exit_code, &stderr_text, ctx.sandbox.allow_network)
    {
        crate::sandbox::record_denial(
            crate::sandbox::SEAM_RUN_COMMAND,
            root,
            &crate::sandbox::program_subject(program),
            &args.args,
            run.exit_code,
            &stderr_text,
            class,
            &ctx.sandbox,
        );
    }
    let mut out = format_run_output(
        &run.stdout,
        &run.stderr,
        run.stdout_capped || run.stderr_capped,
        run.exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".into()),
    );
    if run.drains_leaked {
        // The child finished but one of its pipes stayed open in a process that
        // inherited a copy of the write end. The output above is therefore
        // MISSING a stream, and a model told nothing would read the gap as
        // "the command printed nothing".
        tracing::warn!(
            command = %args.command,
            "sandbox: a pipe drain never finished (leaked write end) — captured output is incomplete"
        );
        out.push_str(
            "\n[sandbox: one output stream could not be drained — a copy of its pipe leaked to \
             another process, so part of this output is missing]",
        );
    }
    Ok(out)
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

    /// The caller-side backstop must outlast the child's own deadline, with
    /// room for the engine to terminate, reap and drain afterwards. If it ever
    /// did not, a perfectly ordinary `git log` hitting its 120 s cap would be
    /// reported to the user as a *wedge* — the one row in that lane that is
    /// supposed to mean "something is broken in cImp, not in your command".
    ///
    /// The two constants are derived from one expression precisely so this
    /// cannot drift; the assertion is what notices if someone un-derives them.
    #[test]
    fn sandbox_backstop_exceeds_the_child_timeout() {
        assert!(
            SANDBOX_BACKSTOP > TIMEOUT,
            "backstop {:?} must exceed the child timeout {:?}",
            SANDBOX_BACKSTOP,
            TIMEOUT
        );
        // The slack must cover the engine's own worst case: terminate + 2 s
        // reap wait + two serial drain collections (5 s grace + 2 s cancel
        // grace each).
        assert!(
            crate::sandbox::SANDBOX_SETTLE_SLACK >= Duration::from_secs(16),
            "settle slack {:?} is under the engine's worst-case settle time",
            crate::sandbox::SANDBOX_SETTLE_SLACK
        );
        assert_eq!(SANDBOX_BACKSTOP, TIMEOUT + crate::sandbox::SANDBOX_SETTLE_SLACK);
    }

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

    // ── V38 Phase D: registered `command`-kind plugin tools ─────────────────

    /// One plugin with a `command` tool, resolved through the real loader and
    /// registry so these assertions run against the same join the live seam
    /// uses — not against a hand-built struct that could drift from it.
    fn registered_fixture(path: &str) -> (Vec<EffectiveTool>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("cimp-runcmd-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("acme.json"),
            r#"{
              "manifest_version": 1,
              "name": "acme",
              "version": "1.0.0",
              "categories": [{ "id": "vcs", "label": "VCS", "tools": ["svn"] }],
              "tools": [{ "id": "svn", "label": "Subversion", "kind": "command" }]
            }"#,
        )
        .expect("write manifest");
        let set = crate::plugins::loader::scan_dir(&dir, crate::plugins::manifest::Provenance::User);
        assert!(set.errors.is_empty(), "{:?}", set.errors);
        let mut cfg = crate::settings::ToolPluginsSettings::default();
        cfg.global_paths
            .insert("acme@1.0.0/svn".to_string(), path.to_string());
        let tools = crate::plugins::registry::runnable_tools(&set, &cfg, None)
            .into_iter()
            .filter(|t| t.kind() == ToolKind::Command)
            .collect();
        (tools, dir)
    }

    /// The design authority's rule: **a registered entry IS the allowlist entry
    /// and the path resolution.** With an EMPTY allowlist the tool still runs,
    /// and it resolves to the configured file rather than to PATH.
    #[test]
    fn a_registered_command_runs_with_no_allowlist_at_all() {
        let (registered, dir) = registered_fixture("C:\\tools\\svn.exe");
        match admit("svn", &[], &registered).expect("a registered tool is admitted") {
            Resolution::Registered(t) => {
                assert_eq!(t.tool_key, "acme@1.0.0/svn");
                assert_eq!(t.path.as_deref(), Some("C:\\tools\\svn.exe"));
            }
            Resolution::Allowlist => panic!("must resolve through the registry, not PATH"),
        }
        // …and the case-insensitive spelling matches too, like the allowlist's.
        assert!(matches!(
            admit("SVN.exe", &[], &registered),
            Ok(Resolution::Registered(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A registry MISS falls through to the pre-V38 path unchanged: the
    /// allowlist decides, and PATH resolves.
    #[test]
    fn a_registry_miss_falls_through_to_the_allowlist() {
        let (registered, dir) = registered_fixture("C:\\tools\\svn.exe");
        let allow = vec!["git".to_string()];
        assert!(matches!(
            admit("git", &allow, &registered),
            Ok(Resolution::Allowlist)
        ));
        let err = admit("rm", &allow, &registered).expect_err("still denied");
        assert!(err.contains("not allowlisted"), "{err}");
        // The refusal names BOTH surfaces, so a caller looking at a plugin tool
        // in Settings is not told a half-truth about what may run.
        assert!(err.contains("git") && err.contains("svn"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// "Disabled" means both surfaces are empty. The wording has to stay honest
    /// about that — an operator with a registered tool and no allowlist is
    /// configured, not disabled.
    #[test]
    fn disabled_means_no_allowlist_and_no_registered_commands() {
        let err = admit("git", &[], &[]).expect_err("nothing configured ⇒ disabled");
        assert!(err.contains("disabled"), "{err}");
        assert!(err.contains("plugin command tools"), "{err}");

        let (registered, dir) = registered_fixture("C:\\tools\\svn.exe");
        let err = admit("git", &[], &registered).expect_err("git is still not allowed");
        assert!(!err.contains("disabled"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bare-name guard is NOT relaxed for a registered tool: the model
    /// still cannot hand over a path. Registration decides which file runs;
    /// the caller only names the tool.
    #[test]
    fn a_registered_tool_does_not_relax_the_bare_name_guard() {
        let (registered, dir) = registered_fixture("C:\\tools\\svn.exe");
        for spelling in ["C:\\evil\\svn.exe", "./svn", "..\\svn"] {
            let err = admit(spelling, &[], &registered).expect_err("paths stay refused");
            assert!(err.contains("bare program name"), "{err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `command_policy` is a statement about ARGUMENTS, so it survives the
    /// registry arm untouched — `dangerous_args` matches by stem and never
    /// looks at how the program was admitted.
    #[test]
    fn command_policies_still_apply_to_a_registered_tool() {
        let policies = vec![CommandPolicy {
            program: "svn".to_string(),
            denied_flags: vec!["--config-dir".to_string()],
            ..CommandPolicy::default()
        }];
        assert!(dangerous_args("svn", &["--config-dir".into(), "x".into()], &policies).is_some());
        assert!(dangerous_args("svn", &["status".into()], &policies).is_none());
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
        assert!(
            dangerous_args("git", &argv(&["--git-dir=/other/.git", "log"]), &policies).is_some()
        );
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
        assert!(
            dangerous_args("git", &argv(&["-ccore.hooksPath=/x", "status"]), &policies).is_some()
        );
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
        assert!(dangerous_args(
            "git",
            &argv(&["--namespace", "x", "config", "--local", "alias.p", "!sh"]),
            &policies
        )
        .is_some());
        assert!(dangerous_args(
            "git",
            &argv(&["--super-prefix", "p/", "config", "core.pager", "!sh"]),
            &policies
        )
        .is_some());
        assert!(dangerous_args(
            "git",
            &argv(&["--attr-source", "HEAD", "config", "x", "y"]),
            &policies
        )
        .is_some());
        // A legitimate read probe whose ARGUMENT is "config" still runs.
        assert!(dangerous_args("git", &argv(&["grep", "config"]), &policies).is_none());
        assert!(dangerous_args("git", &argv(&["log", "--grep", "config"]), &policies).is_none());
    }

    #[test]
    fn default_git_policy_allows_read_probes() {
        let policies = crate::settings::default_command_policies();
        assert!(
            dangerous_args("git", &argv(&["log", "--oneline", "-n", "5"]), &policies).is_none()
        );
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
        assert!(dangerous_args(
            "cargo",
            &argv(&["metadata", "--format-version", "1"]),
            &policies
        )
        .is_none());
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
        assert!(dangerous_args(
            "cargo",
            &argv(&["--config", "target.x.runner='sh'", "metadata"]),
            &policies
        )
        .is_some());
        assert!(dangerous_args(
            "cargo",
            &argv(&["--config=build.rustc-wrapper=/x", "tree"]),
            &policies
        )
        .is_some());
        // `-C dir` escapes the working root; glued form must also be blocked.
        assert!(dangerous_args("cargo", &argv(&["-C", "/etc", "tree"]), &policies).is_some());
        assert!(dangerous_args("cargo", &argv(&["-C/etc", "tree"]), &policies).is_some());
        // `-Z` unstable flags, glued too.
        assert!(dangerous_args(
            "cargo",
            &argv(&["-Z", "unstable-options", "metadata"]),
            &policies
        )
        .is_some());
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

    // ── V33 C2 — the minimal child environment ─────────────────────────────

    /// Windows environment names are case-insensitive, so membership is asked
    /// that way everywhere; on Unix the child sees exactly the bytes we wrote,
    /// so the looser comparison costs nothing.
    fn is_allowlisted(name: &str) -> bool {
        crate::sandbox::child_env::CHILD_ENV
            .iter()
            .any(|g| g.name.eq_ignore_ascii_case(name))
    }

    /// Marker for the child dump below. Built with `concat!` so a grep for the
    /// prefix does not also hit this constant.
    const DUMP: &str = concat!("CIMP_ENV", "_DUMP=");

    /// The probe the spawn test re-executes this test binary to run: it prints
    /// its own environment, one `NAME=VALUE` per line behind [`DUMP`]. Running
    /// it directly (as the suite does) is a harmless no-op — the parent test is
    /// the only reader.
    #[test]
    fn env_dump_probe_prints_its_own_environment() {
        // Terminate libtest's own partial line FIRST. Under `--nocapture` the
        // harness writes `test <name> ... ` with NO trailing newline and then
        // runs the test, so the first `println!` below lands on that line and
        // its marker is no longer at column 0. That cost exactly one variable —
        // the alphabetically first one, because Windows keeps the environment
        // block sorted — and it looked like a leak in the allowlist rather than
        // a parsing bug. Do not remove this.
        println!();
        for (k, v) in std::env::vars_os() {
            println!(
                "{DUMP}{}={}",
                k.to_string_lossy(),
                v.to_string_lossy().replace('\n', " ")
            );
        }
    }

    /// Ask the child what it actually received. Re-executes this test binary
    /// with a single-test filter, under exactly the environment
    /// [`apply_minimal_env`] composes — no shell involved, so nothing but our
    /// own table can add a name.
    async fn child_env(lookup: &dyn Fn(&str) -> Option<OsString>) -> Vec<(String, String)> {
        let exe = std::env::current_exe().expect("the test binary's own path");
        let mut cmd = tokio::process::Command::new(&exe);
        cmd.args([
            "--exact",
            "offload::tools::run_command::tests::env_dump_probe_prints_its_own_environment",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        apply_minimal_env(&mut cmd, lookup);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);

        let out = tokio::time::timeout(Duration::from_secs(90), cmd.output())
            .await
            .expect("the env probe must not hang")
            .expect("the env probe must spawn — if it cannot even start, the allowlist is \
                     missing something the OS needs to load a binary");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "the env probe failed (exit {:?}).\nstdout:\n{stdout}\nstderr:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let vars: Vec<(String, String)> = stdout
            .lines()
            .filter_map(|l| l.trim_end_matches('\r').strip_prefix(DUMP))
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        // An empty parse must fail, not pass: a probe whose output we failed to
        // read would otherwise "prove" that no secret reached the child.
        assert!(
            !vars.is_empty(),
            "the env probe produced no readable output — the assertions below would be \
             vacuous.\nstdout:\n{stdout}"
        );
        vars
    }

    /// V33 C2, bar 1 — the child sees the allowlist and nothing else, and the
    /// things cImp's own environment carries do not cross.
    #[tokio::test]
    async fn a_run_command_child_sees_only_the_allowlisted_environment() {
        let vars = child_env(&|k| std::env::var_os(k)).await;
        let leaked: Vec<&String> = vars
            .iter()
            .map(|(k, _)| k)
            .filter(|k| !is_allowlisted(k))
            .collect();
        assert!(
            leaked.is_empty(),
            "these names reached the child but are not in `CHILD_ENV`: {leaked:?}"
        );

        // The control that makes the assertion above mean something: this test
        // process really does carry names that are NOT on the list (cargo alone
        // injects a dozen), and every one of them was dropped. Without this,
        // an `env_clear` that silently no-op'd would still pass.
        let mine: Vec<String> = std::env::vars()
            .map(|(k, _)| k)
            .filter(|k| !is_allowlisted(k))
            .collect();
        assert!(
            !mine.is_empty(),
            "the test process carries only allowlisted names, so this test proves nothing \
             — pick a different control"
        );
        for name in &mine {
            assert!(
                !vars.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)),
                "`{name}` is in cImp's environment and reached the child"
            );
        }
    }

    /// V33 C2, bar 1 — the concrete threat, spelled out: an API key and the
    /// loopback bearer token sitting in cImp's environment do not cross into a
    /// child, no matter what they are called.
    ///
    /// The secrets are planted in the *lookup* rather than in the test
    /// process's real environment on purpose — `set_var` is process-wide and
    /// this suite runs 32 threads. That is not a weaker test: `apply_minimal_env`
    /// reads the environment ONLY through this closure, so a planted name the
    /// closure would happily return is exactly a name in cImp's environment.
    #[tokio::test]
    async fn a_secret_in_cimps_environment_never_reaches_the_child() {
        let planted = [
            ("ANTHROPIC_API_KEY", "sk-ant-planted-must-not-cross"),
            ("OPENAI_API_KEY", "sk-planted-must-not-cross"),
            ("GITHUB_TOKEN", "ghp-planted-must-not-cross"),
            ("CIMP_LOOPBACK_TOKEN", "loopback-planted-must-not-cross"),
            // …and a secret whose NAME nobody guessed, which is the case a
            // denylist of secret-shaped names would have missed.
            ("WIDGET_CO_SIGNING_SEED", "seed-planted-must-not-cross"),
        ];
        let vars = child_env(&|k| {
            planted
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| OsString::from(*v))
                .or_else(|| std::env::var_os(k))
        })
        .await;
        let dump = vars
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n");
        for (name, value) in planted {
            assert!(
                !vars.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)),
                "`{name}` reached the child"
            );
            assert!(
                !dump.contains(value),
                "`{name}`'s VALUE reached the child under another name"
            );
        }
    }

    /// V33 C2, bar 2 — the half that a maximally-strict allowlist would fail.
    /// `PATH` survives, and so do the toolchain state pointers the live-verify
    /// probes need; a child with no PATH cannot run anything it depends on.
    #[tokio::test]
    async fn path_and_the_toolchain_state_pointers_survive() {
        let vars = child_env(&|k| std::env::var_os(k)).await;
        let get = |name: &str| {
            vars.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        };
        let path = get("PATH").expect("PATH must reach the child");
        assert!(!path.is_empty(), "PATH reached the child empty");
        assert_eq!(
            path,
            std::env::var("PATH").unwrap_or_default(),
            "PATH must cross unmodified"
        );
        // Every name this process actually has, and that the table grants, must
        // be present in the child — the table is a ceiling, and nothing between
        // the table and the spawn may drop a granted name.
        for grant in crate::sandbox::child_env::CHILD_ENV {
            if std::env::var_os(grant.name).is_some() {
                assert!(
                    get(grant.name).is_some(),
                    "`{}` is granted by CHILD_ENV and set in this process, but never \
                     reached the child. The child saw: {:?}",
                    grant.name,
                    vars.iter().map(|(k, _)| k).collect::<Vec<_>>()
                );
            }
        }
        // The live-verify-item-7 toolchains: whichever of these this machine
        // has, the child got. (Asserted conditionally because a build box need
        // not have all of them; the loop above already covers the general case,
        // this names the three the milestone will actually probe.)
        for name in ["HOME", "USERPROFILE", "CARGO_HOME", "RUSTUP_HOME"] {
            assert_eq!(
                std::env::var_os(name).is_some(),
                get(name).is_some(),
                "`{name}` must cross exactly when this process has it — it is what a \
                 `git log` / `cargo` / `npm` probe resolves its own state from"
            );
        }
    }

    /// V33 C2, bar 2, end to end — the whole production path, not a
    /// reconstruction of it: `execute` resolves the program, applies the
    /// policy, composes the minimal environment and spawns. `git log` and a
    /// `cargo` probe are two of the three commands live-verify item 7 runs, and
    /// this is the half of the bar a maximally-strict allowlist fails.
    ///
    /// Guarded on the toolchain being present, following the `workbench::diff`
    /// house pattern — but it prints when it skips, because a silent skip is
    /// how a green suite hides a broken probe.
    #[tokio::test]
    async fn git_log_and_a_cargo_probe_still_work_under_the_minimal_environment() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("src-tauri has a parent")
            .to_path_buf();
        let ctx = ToolCtx {
            allowed_roots: vec![root],
            command_allowlist: vec!["git".to_string(), "cargo".to_string()],
            command_policies: crate::settings::default_command_policies(),
            // V33 Phase F: no Workbench service in a unit test, and this test is
            // about the minimal environment, not about checkpoints.
            checkpoint: None,
            // V33 Phase A: deliberately UNsandboxed. This test asserts the C2
            // minimal environment still lets `git log` and a `cargo` probe run;
            // routing it through the AppContainer would test the sandbox's grant
            // ladder instead, and would ACL-stamp the developer's real toolchain
            // dirs as a side effect of running the suite.
            sandbox: crate::sandbox::SandboxCfg::disabled(),
        };

        if crate::pty::resolve_command("git").is_ok() {
            let out = execute(json!({ "command": "git", "args": ["log", "-1", "--oneline"] }), &ctx)
                .await
                .expect("`git log` must still run under the minimal environment");
            assert!(
                out.starts_with("(exit 0)"),
                "`git log` failed under the minimal environment — a state pointer it needs \
                 is missing from CHILD_ENV: {out}"
            );
            assert!(out.len() > "(exit 0)\n".len(), "`git log` printed nothing: {out}");
        } else {
            println!("SKIPPED the `git log` leg: no git on PATH");
        }

        if crate::pty::resolve_command("cargo").is_ok() {
            let out = execute(json!({ "command": "cargo", "args": ["--version"] }), &ctx)
                .await
                .expect("a `cargo` probe must still run under the minimal environment");
            assert!(
                out.starts_with("(exit 0)") && out.contains("cargo"),
                "the cargo probe failed under the minimal environment — CARGO_HOME/\
                 RUSTUP_HOME/PATH are what it resolves its toolchain from: {out}"
            );
        } else {
            println!("SKIPPED the `cargo` leg: no cargo on PATH");
        }
    }
}

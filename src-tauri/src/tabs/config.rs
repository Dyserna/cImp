//! Per-tab launch configuration — translates a `TabConfig` (binary, flags,
//! TTS injection) from settings into a `PtyLaunchSpec`. Encapsulates
//! Claude's `--append-system-prompt` injection mechanism here so the
//! PtyManager stays generic.
//!
//! V3-M3: settings is the single source of truth — there is no per-tab
//! side table any more. `build_launch_spec` looks the tab up by id; an
//! unknown id is a hard error (the registry shouldn't know about a tab
//! whose entry is missing from settings).
//!
//! V14: AI tabs cover Claude Code (`claude` / `claude-local`) and Aider
//! (`aider` / `aider-local`). The `use_local_provider` flag on each AI
//! tab gates env synthesis: Claude pairs read from `claude_local` and
//! receive `ANTHROPIC_*` env vars; Aider pairs read from `aider_local`
//! and receive `OPENAI_*` env vars (plus a `--model <model>` arg when
//! set). Per-tab `env` entries take precedence over synthesized values.
//! TTS prompt injection (`--append-system-prompt`) is Claude-only —
//! Aider's CLI has no equivalent flag, so the spawn path drops pre-args
//! for Aider tabs regardless of the per-tab `tts_injection.enabled`
//! setting.

use std::collections::HashMap;
use std::path::Path;

use crate::error::{AppError, AppResult};
use crate::pty::{resolve_command, PtyLaunchSpec};
use crate::settings::{AiToolTabConfig, Settings, TabConfig};
use crate::state::TabId;

pub fn build_launch_spec(
    tab: TabId,
    settings: &Settings,
    launch_cwd: &Path,
    invocation_args: &[String],
) -> AppResult<PtyLaunchSpec> {
    let id = tab.as_str();
    let entry = settings.find_tab(id).ok_or_else(|| {
        AppError::Pty(format!("tab {id} has no settings entry"))
    })?;

    match entry {
        TabConfig::AiTool(cfg) => build_ai_tool_spec(tab, cfg, settings, launch_cwd, invocation_args),
        TabConfig::Shell(cfg) => {
            // The detection module verified the default Shell-1 binary;
            // user-supplied paths from the New Shell Tab dialog are
            // validated up-front in `create_shell_tab` (M2 Phase B).
            // Bare-name paths still go through `resolve_command` so a
            // settings entry pointing at "bash" picks up the PATH
            // resolution every launch.
            let binary = resolve_command(&cfg.command)?;
            let working_dir = cfg
                .cwd
                .clone()
                .unwrap_or_else(|| launch_cwd.to_path_buf());
            Ok(PtyLaunchSpec {
                tab,
                binary,
                pre_args: Vec::new(),
                extra_args: cfg.args.clone(),
                working_dir,
                env: cfg.env.clone(),
            })
        }
    }
}

fn build_ai_tool_spec(
    tab: TabId,
    cfg: &AiToolTabConfig,
    settings: &Settings,
    launch_cwd: &Path,
    invocation_args: &[String],
) -> AppResult<PtyLaunchSpec> {
    let binary = resolve_command(&cfg.command)?;
    let pre_args = build_pre_args(cfg, settings);
    let extra_args = build_extra_args(cfg, settings, invocation_args);
    let working_dir = cfg
        .cwd
        .clone()
        .unwrap_or_else(|| launch_cwd.to_path_buf());
    let env = compose_ai_env(cfg, settings);
    Ok(PtyLaunchSpec {
        tab,
        binary,
        pre_args,
        extra_args,
        working_dir,
        env,
    })
}

/// True when `command` resolves to the named binary, comparing on the
/// file stem so `"claude"`, `"claude.exe"`, and `"/usr/bin/claude"` all
/// match `"claude"`. AI launch behavior keys off this (and
/// `use_local_provider`) rather than the `TabId` variant, so a `+`-spawned
/// duplicate — which copies its template's `command` — gets identical
/// treatment to the reserved tab it came from.
fn command_is(command: &str, name: &str) -> bool {
    Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case(name))
        .unwrap_or(false)
}

/// Pre-args injected ahead of the tab's own `args` and the wrapper's
/// invocation args. Claude-only — both injections target Claude Code's
/// CLI and Aider understands neither, so Aider tabs get empty pre-args:
///
///   * `--append-system-prompt <instructions>` — TTS markup convention,
///     gated on the per-tab `tts_injection` toggle.
///   * `--settings <json>` — a session-scoped overlay pointing Claude
///     Code's `statusLine` at our `ccimp --statusline` renderer, gated on
///     the global `statusline.enabled`. The overlay merges with the
///     user's own Claude settings (only `statusLine` is set), so it
///     scopes the context bar to ccImp without touching `~/.claude`.
fn build_pre_args(cfg: &AiToolTabConfig, settings: &Settings) -> Vec<String> {
    if !command_is(&cfg.command, "claude") {
        return Vec::new();
    }
    let mut args = Vec::new();

    // Compose a single `--append-system-prompt` from every addendum we
    // inject (TTS markup convention + the V8-01 offload guidance nudge),
    // joined with a blank line. Merging into one flag avoids relying on
    // Claude Code concatenating repeated `--append-system-prompt` flags.
    let mut addendum = String::new();
    if cfg.tts_injection.enabled && !cfg.tts_injection.instructions.is_empty() {
        addendum.push_str(&cfg.tts_injection.instructions);
    }
    if settings.offload.enabled && settings.offload.inject_guidance {
        if !addendum.is_empty() {
            addendum.push_str("\n\n");
        }
        addendum.push_str(OFFLOAD_GUIDANCE);
    }
    if !addendum.is_empty() {
        args.push("--append-system-prompt".to_string());
        args.push(addendum);
    }

    if settings.statusline.enabled {
        if let Some(command) = crate::statusline::launch_command() {
            let overlay = serde_json::json!({
                "statusLine": { "type": "command", "command": command }
            });
            args.push("--settings".to_string());
            args.push(overlay.to_string());
        }
    }

    // V8-01: point Claude at our own `ccimp --offload-mcp` MCP server so
    // the `offload_task` tool is available in this session. Session-scoped
    // (a `--mcp-config` overlay), never written to `~/.claude`. Claude
    // spawns the `command` as argv (no shell), so the raw exe path is
    // correct — no shell-quoting needed (unlike the statusLine command).
    if settings.offload.enabled {
        if let Ok(exe) = std::env::current_exe() {
            let mcp = serde_json::json!({
                "mcpServers": {
                    "ccimp-offload": {
                        "command": exe.to_string_lossy(),
                        "args": ["--offload-mcp"]
                    }
                }
            });
            args.push("--mcp-config".to_string());
            args.push(mcp.to_string());
        }
    }

    args
}

/// V8-01: the system-prompt addendum telling Opus *when* to reach for
/// `offload_task`. Without this nudge the model rarely offloads. Gated by
/// `offload.inject_guidance`.
const OFFLOAD_GUIDANCE: &str = "You have an `offload_task` tool (from the ccimp-offload MCP server) \
backed by a local model. For token-heavy subtasks — broad codebase searches, summarizing large \
files or logs, or web research — prefer calling `offload_task` with a self-contained instruction \
instead of doing the work yourself: it returns only a synthesized result, conserving your context \
window. Keep work that needs your full reasoning or the conversation's context here. Set the \
`thinking` arg to 'off' for simple lookups/extraction, 'on' for analysis, or leave it 'auto'.";

fn build_extra_args(
    cfg: &AiToolTabConfig,
    settings: &Settings,
    invocation_args: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // Aider against a local provider: prepend `--model <name>` if
    // configured. Placed ahead of the user's own `cfg.args` so a user can
    // still override by adding `--model …` themselves (Aider takes the
    // last `--model` it sees).
    if command_is(&cfg.command, "aider")
        && cfg.use_local_provider
        && !settings.aider_local.model.is_empty()
    {
        out.push("--model".to_string());
        out.push(settings.aider_local.model.clone());
    }

    out.extend(cfg.args.iter().filter(|s| !s.is_empty()).cloned());

    // ccimp is documented as a drop-in replacement for `claude`, so
    // invocation args (`ccimp --resume <id>`, etc.) flow into every
    // Claude tab. Aider ignores unknown flags less gracefully than
    // Claude, so we only forward invocation args to Claude tabs.
    if command_is(&cfg.command, "claude") {
        for arg in invocation_args {
            if !arg.is_empty() {
                out.push(arg.clone());
            }
        }
    }
    out
}

/// V1.4-07 / V14: compose the spawn environment for an AI tab. Per-tab
/// `env` entries are the user's most-specific scope and take
/// precedence over synthesized values. The merge order is:
/// synthesized → tab.env (per-tab keys never get overwritten).
///
/// Synthesis is gated on `use_local_provider` and the resolved binary,
/// not the `TabId` variant — so a `+`-spawned duplicate is treated
/// exactly like the reserved tab it was cloned from:
///
/// - Claude binary + `use_local_provider`: synthesize
///   `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL`
///   from `claude_local`.
/// - Aider binary + `use_local_provider`: synthesize
///   `OPENAI_API_BASE` / `OPENAI_API_KEY` from `aider_local`. The
///   model selection is passed via `--model` (see `build_extra_args`)
///   rather than env, since Aider has stronger CLI-flag conventions.
/// - Anything else (cloud Claude/Aider, `use_local_provider` off): no
///   synthesized env — the user's existing configuration is in charge.
fn compose_ai_env(cfg: &AiToolTabConfig, settings: &Settings) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();
    if cfg.use_local_provider {
        if command_is(&cfg.command, "claude") {
            let cl = &settings.claude_local;
            if !cl.base_url.is_empty() {
                env.insert("ANTHROPIC_BASE_URL".to_string(), cl.base_url.clone());
            }
            if !cl.auth_token.is_empty() {
                env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), cl.auth_token.clone());
            }
            if !cl.model_alias.is_empty() {
                // Claude Code primarily uses --model flag for model
                // selection, but ANTHROPIC_MODEL is honored by some
                // proxies; setting both is harmless.
                env.insert("ANTHROPIC_MODEL".to_string(), cl.model_alias.clone());
            }
        } else if command_is(&cfg.command, "aider") {
            let al = &settings.aider_local;
            if !al.base_url.is_empty() {
                env.insert("OPENAI_API_BASE".to_string(), al.base_url.clone());
            }
            if !al.auth_token.is_empty() {
                env.insert("OPENAI_API_KEY".to_string(), al.auth_token.clone());
            }
        }
    }
    // Per-tab env wins over synthesized values.
    for (k, v) in &cfg.env {
        env.insert(k.clone(), v.clone());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{default_aider_tab, default_claude_tab};

    fn claude_cfg() -> AiToolTabConfig {
        match default_claude_tab() {
            TabConfig::AiTool(c) => c,
            _ => unreachable!("default_claude_tab is an AI tool tab"),
        }
    }

    fn aider_cfg() -> AiToolTabConfig {
        match default_aider_tab() {
            TabConfig::AiTool(c) => c,
            _ => unreachable!("default_aider_tab is an AI tool tab"),
        }
    }

    /// The value following the first `--settings` flag in `args`, parsed
    /// as JSON. `None` if no `--settings` flag is present.
    fn settings_overlay(args: &[String]) -> Option<serde_json::Value> {
        let i = args.iter().position(|a| a == "--settings")?;
        let raw = args.get(i + 1)?;
        Some(serde_json::from_str(raw).expect("--settings value is valid JSON"))
    }

    #[test]
    fn injects_statusline_overlay_for_claude_when_enabled() {
        let mut settings = Settings::default();
        settings.statusline.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings);

        let overlay = settings_overlay(&args).expect("statusLine overlay present");
        assert_eq!(overlay["statusLine"]["type"], "command");
        let cmd = overlay["statusLine"]["command"]
            .as_str()
            .expect("command is a string");
        // Points back at this binary's hidden subcommand, forward-slashed.
        assert!(cmd.ends_with(" --statusline"), "got: {cmd}");
        assert!(!cmd.contains('\\'), "path must be forward-slashed: {cmd}");
    }

    #[test]
    fn no_statusline_overlay_when_disabled() {
        let mut settings = Settings::default();
        settings.statusline.enabled = false;
        let args = build_pre_args(&claude_cfg(), &settings);
        assert!(settings_overlay(&args).is_none());
    }

    #[test]
    fn statusline_overlay_is_claude_only() {
        // Aider understands neither --append-system-prompt nor --settings,
        // so its pre-args stay empty even with the global toggle on.
        let mut settings = Settings::default();
        settings.statusline.enabled = true;
        let args = build_pre_args(&aider_cfg(), &settings);
        assert!(args.is_empty(), "aider must get no pre-args, got: {args:?}");
    }

    #[test]
    fn tts_and_statusline_coexist() {
        // A default Claude tab has TTS injection enabled; with the status
        // line also on, both pre-arg pairs are present.
        let mut settings = Settings::default();
        settings.statusline.enabled = true;
        let mut cfg = claude_cfg();
        cfg.tts_injection.enabled = true;
        cfg.tts_injection.instructions = "wrap prose".to_string();
        let args = build_pre_args(&cfg, &settings);

        assert!(args.iter().any(|a| a == "--append-system-prompt"));
        assert!(args.iter().any(|a| a == "--settings"));
    }

    #[test]
    fn injects_offload_mcp_config_for_claude_when_enabled() {
        let mut settings = Settings::default();
        settings.offload.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings);

        let i = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present");
        let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
        assert_eq!(cfg["mcpServers"]["ccimp-offload"]["args"][0], "--offload-mcp");
    }

    #[test]
    fn offload_guidance_merges_with_tts_addendum() {
        let mut settings = Settings::default();
        settings.offload.enabled = true;
        settings.offload.inject_guidance = true;
        let mut cfg = claude_cfg();
        cfg.tts_injection.enabled = true;
        cfg.tts_injection.instructions = "wrap prose".to_string();
        let args = build_pre_args(&cfg, &settings);

        // Exactly one --append-system-prompt, carrying both addenda.
        let count = args.iter().filter(|a| *a == "--append-system-prompt").count();
        assert_eq!(count, 1, "addenda must merge into one flag");
        let i = args.iter().position(|a| a == "--append-system-prompt").unwrap();
        assert!(args[i + 1].contains("wrap prose"));
        assert!(args[i + 1].contains("offload_task"));
    }

    #[test]
    fn no_offload_injection_when_disabled() {
        let settings = Settings::default(); // offload off by default
        let args = build_pre_args(&claude_cfg(), &settings);
        assert!(!args.iter().any(|a| a == "--mcp-config"));
    }

    #[test]
    fn offload_injection_is_claude_only() {
        let mut settings = Settings::default();
        settings.offload.enabled = true;
        let args = build_pre_args(&aider_cfg(), &settings);
        assert!(args.is_empty(), "aider must get no pre-args, got: {args:?}");
    }
}

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
    let pre_args = build_pre_args(&tab, cfg);
    let extra_args = build_extra_args(&tab, cfg, settings, invocation_args);
    let working_dir = cfg
        .cwd
        .clone()
        .unwrap_or_else(|| launch_cwd.to_path_buf());
    let env = compose_ai_env(&tab, cfg, settings);
    Ok(PtyLaunchSpec {
        tab,
        binary,
        pre_args,
        extra_args,
        working_dir,
        env,
    })
}

/// Pre-args injected ahead of the tab's own `args` and the wrapper's
/// invocation args. Currently only Claude Code understands
/// `--append-system-prompt`; Aider tabs return empty pre-args
/// regardless of the per-tab toggle.
fn build_pre_args(tab: &TabId, cfg: &AiToolTabConfig) -> Vec<String> {
    if !matches!(tab, TabId::Claude | TabId::ClaudeLocal) {
        return Vec::new();
    }
    if !cfg.tts_injection.enabled || cfg.tts_injection.instructions.is_empty() {
        return Vec::new();
    }
    vec![
        "--append-system-prompt".to_string(),
        cfg.tts_injection.instructions.clone(),
    ]
}

fn build_extra_args(
    tab: &TabId,
    cfg: &AiToolTabConfig,
    settings: &Settings,
    invocation_args: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // Aider local: prepend `--model <name>` if configured. Placed
    // ahead of the user's own `cfg.args` so a user can still override
    // by adding `--model …` themselves (Aider takes the last `--model`
    // it sees).
    if matches!(tab, TabId::AiderLocal) && !settings.aider_local.model.is_empty() {
        out.push("--model".to_string());
        out.push(settings.aider_local.model.clone());
    }

    out.extend(cfg.args.iter().filter(|s| !s.is_empty()).cloned());

    // cctts is documented as a drop-in replacement for `claude`, so
    // invocation args (`cctts --resume <id>`, etc.) flow into every
    // AI tab. Aider ignores unknown flags less gracefully than Claude,
    // so we only forward invocation args to Claude tabs.
    if matches!(tab, TabId::Claude | TabId::ClaudeLocal) {
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
/// - `claude` / `claude-local` (when `use_local_provider`): synthesize
///   `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL`
///   from `claude_local`.
/// - `aider-local` (when `use_local_provider`): synthesize
///   `OPENAI_API_BASE` / `OPENAI_API_KEY` from `aider_local`. The
///   model selection is passed via `--model` (see `build_extra_args`)
///   rather than env, since Aider has stronger CLI-flag conventions.
/// - `aider` (cloud): no synthesized env — the user's existing aider
///   configuration is in charge.
fn compose_ai_env(
    tab: &TabId,
    cfg: &AiToolTabConfig,
    settings: &Settings,
) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();
    if cfg.use_local_provider {
        match tab {
            TabId::Claude | TabId::ClaudeLocal => {
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
            }
            TabId::AiderLocal => {
                let al = &settings.aider_local;
                if !al.base_url.is_empty() {
                    env.insert("OPENAI_API_BASE".to_string(), al.base_url.clone());
                }
                if !al.auth_token.is_empty() {
                    env.insert("OPENAI_API_KEY".to_string(), al.auth_token.clone());
                }
            }
            // Cloud Aider tab: no synthesized env even if
            // use_local_provider was hand-edited to true (the integrity
            // check would correct that on next load).
            TabId::Aider | TabId::Shell(_) => {}
        }
    }
    // Per-tab env wins over synthesized values.
    for (k, v) in &cfg.env {
        env.insert(k.clone(), v.clone());
    }
    env
}

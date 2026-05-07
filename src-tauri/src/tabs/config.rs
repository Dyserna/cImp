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
//! V1.4-07: AI tabs are now Claude-only (Aider was dropped). The
//! `use_local_provider` flag on each AI tab gates env synthesis from
//! the global `claude_local` settings group; per-tab `env` entries take
//! precedence over synthesized values.

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
    let pre_args = build_pre_args(cfg);
    let extra_args = build_extra_args(cfg, invocation_args);
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

fn build_pre_args(cfg: &AiToolTabConfig) -> Vec<String> {
    if !cfg.tts_injection.enabled || cfg.tts_injection.instructions.is_empty() {
        return Vec::new();
    }
    vec![
        "--append-system-prompt".to_string(),
        cfg.tts_injection.instructions.clone(),
    ]
}

fn build_extra_args(cfg: &AiToolTabConfig, invocation_args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    out.extend(cfg.args.iter().filter(|s| !s.is_empty()).cloned());

    // cctts is documented as a drop-in replacement for `claude`, so
    // invocation args (`cctts --resume <id>`, etc.) flow into every
    // AI tab. (Pre-V1.4-07 they only flowed into the subscription
    // Claude tab; with Aider gone and both AI tabs now running
    // `claude`, both can honor the wrapper's invocation args.)
    for arg in invocation_args {
        if !arg.is_empty() {
            out.push(arg.clone());
        }
    }
    out
}

/// V1.4-07: compose the spawn environment for an AI tab. Per-tab
/// `env` entries are the user's most-specific scope and take
/// precedence over values synthesized from the global `claude_local`
/// settings group when `use_local_provider` is true. The merge order
/// is: synthesized → tab.env (using `entry().or_insert()` so per-tab
/// keys never get overwritten).
fn compose_ai_env(cfg: &AiToolTabConfig, settings: &Settings) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();
    if cfg.use_local_provider {
        let cl = &settings.claude_local;
        if !cl.base_url.is_empty() {
            env.insert("ANTHROPIC_BASE_URL".to_string(), cl.base_url.clone());
        }
        if !cl.auth_token.is_empty() {
            env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), cl.auth_token.clone());
        }
        if !cl.model_alias.is_empty() {
            // Claude Code primarily uses --model flag for model selection,
            // but ANTHROPIC_MODEL is honored by some proxies; setting
            // both is harmless. Users typically configure model mapping
            // in their LiteLLM/proxy config rather than relying on this.
            env.insert("ANTHROPIC_MODEL".to_string(), cl.model_alias.clone());
        }
    }
    // Per-tab env wins over synthesized values.
    for (k, v) in &cfg.env {
        env.insert(k.clone(), v.clone());
    }
    env
}

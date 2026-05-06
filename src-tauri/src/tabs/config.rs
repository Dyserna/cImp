//! Per-tab launch configuration — translates a `TabConfig` (binary, flags,
//! TTS injection) from settings into a `PtyLaunchSpec`. Encapsulates
//! Claude's `--append-system-prompt` injection mechanism here so the
//! PtyManager stays generic.
//!
//! V3-M3: settings is the single source of truth — there is no per-tab
//! side table any more. `build_launch_spec` looks the tab up by id; an
//! unknown id is a hard error (the registry shouldn't know about a tab
//! whose entry is missing from settings).

use std::path::Path;

use crate::error::{AppError, AppResult};
use crate::pty::{resolve_command, PtyLaunchSpec};
use crate::settings::{AiToolKindWire, AiToolTabConfig, Settings, TabConfig};
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
        TabConfig::AiTool(cfg) => build_ai_tool_spec(tab, cfg, launch_cwd, invocation_args),
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
    Ok(PtyLaunchSpec {
        tab,
        binary,
        pre_args,
        extra_args,
        working_dir,
        env: cfg.env.clone(),
    })
}

fn build_pre_args(cfg: &AiToolTabConfig) -> Vec<String> {
    if !cfg.tts_injection.enabled || cfg.tts_injection.instructions.is_empty() {
        return Vec::new();
    }
    match cfg.ai_tool_kind {
        AiToolKindWire::ClaudeCode => vec![
            "--append-system-prompt".to_string(),
            cfg.tts_injection.instructions.clone(),
        ],
        AiToolKindWire::Aider => {
            // Aider has no equivalent CLI mechanism; the toggle exists in the
            // schema for forward-compat (see FUTURE-FEATURES.md) but the v2
            // milestone calls out injection as a no-op for the aider tab.
            tracing::info!(
                "aider tab: TTS injection requested but aider has no CLI mechanism; skipping"
            );
            Vec::new()
        }
    }
}

fn build_extra_args(cfg: &AiToolTabConfig, invocation_args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // Built-in baseline flags. These come BEFORE the user's persistent
    // args, so a user who really wants to override one (e.g. point at a
    // different metadata file) can add their own flag and rely on aider's
    // last-flag-wins parsing.
    out.extend(builtin_args(cfg.ai_tool_kind));

    out.extend(cfg.args.iter().filter(|s| !s.is_empty()).cloned());

    // cctts is documented as a drop-in replacement for `claude`, so
    // invocation args (`cctts --resume <id>`, etc.) flow only into the
    // claude tab. The aider tab gets its persistent flags only.
    if matches!(cfg.ai_tool_kind, AiToolKindWire::ClaudeCode) {
        for arg in invocation_args {
            if !arg.is_empty() {
                out.push(arg.clone());
            }
        }
    }
    out
}

/// Always-on flags per AI tool. Kept here (not in settings defaults) so the
/// flag set takes effect even on existing user settings files where `args`
/// is already empty — i.e. nobody has to delete their settings.json to
/// pick up new defaults.
fn builtin_args(kind: AiToolKindWire) -> Vec<String> {
    match kind {
        AiToolKindWire::ClaudeCode => Vec::new(),
        AiToolKindWire::Aider => vec![
            // Aider's built-in model metadata is incomplete for newer
            // models (and lacks any project-specific tuning). Pointing it
            // at a project-local file lets each project ship its own
            // metadata; the path is relative to aider's cwd (= cctts
            // launch dir), so each project's `.aider.model.metadata.json`
            // is picked up automatically. If the file is absent, aider
            // logs a warning and falls back to its built-in defaults.
            "--model-metadata-file".to_string(),
            ".aider.model.metadata.json".to_string(),
        ],
    }
}

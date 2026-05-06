//! Per-tab launch configuration — translates `TabSettings` (binary, flags,
//! TTS injection) into a `PtyLaunchSpec`. Encapsulates Claude's
//! `--append-system-prompt` injection mechanism here so the PtyManager stays
//! generic.

use std::path::Path;

use crate::error::AppResult;
use crate::pty::{resolve_command, PtyLaunchSpec};
use crate::settings::{Settings, TabSettings};
use crate::tabs::ShellTabConfig;
use crate::state::TabId;

pub fn build_launch_spec(
    tab: TabId,
    settings: &Settings,
    shell_config: Option<&ShellTabConfig>,
    launch_cwd: &Path,
    invocation_args: &[String],
) -> AppResult<PtyLaunchSpec> {
    match &tab {
        TabId::Claude | TabId::Aider => {
            let tab_settings = if matches!(tab, TabId::Claude) {
                &settings.tabs.claude
            } else {
                &settings.tabs.aider
            };
            let binary = resolve_command(&tab_settings.command)?;
            let pre_args = build_pre_args(&tab, tab_settings);
            let extra_args = build_extra_args(&tab, tab_settings, invocation_args);
            Ok(PtyLaunchSpec {
                tab,
                binary,
                pre_args,
                extra_args,
                working_dir: launch_cwd.to_path_buf(),
                env: std::collections::HashMap::new(),
            })
        }
        TabId::Shell(id) => {
            let cfg = shell_config.ok_or_else(|| {
                crate::error::AppError::Pty(format!(
                    "shell tab {id} has no launch config"
                ))
            })?;
            // The detection module verified the default binary; user-
            // supplied paths from the New Shell Tab dialog are validated
            // up-front in `create_shell_tab` (M2 Phase B), so we trust the
            // config here.
            let working_dir = cfg
                .cwd
                .clone()
                .unwrap_or_else(|| launch_cwd.to_path_buf());
            Ok(PtyLaunchSpec {
                tab,
                binary: cfg.spec.command.clone(),
                pre_args: Vec::new(),
                extra_args: cfg.spec.args.clone(),
                working_dir,
                env: cfg.env.clone(),
            })
        }
    }
}

fn build_pre_args(tab: &TabId, ts: &TabSettings) -> Vec<String> {
    if !ts.tts_injection.enabled || ts.tts_injection.instructions.is_empty() {
        return Vec::new();
    }
    match tab {
        TabId::Claude => vec![
            "--append-system-prompt".to_string(),
            ts.tts_injection.instructions.clone(),
        ],
        TabId::Aider => {
            // Aider has no equivalent CLI mechanism; the toggle exists in the
            // schema for forward-compat (see FUTURE-FEATURES.md) but the v2
            // milestone calls out injection as a no-op for the aider tab.
            tracing::info!(
                "aider tab: TTS injection requested but aider has no CLI mechanism; skipping"
            );
            Vec::new()
        }
        TabId::Shell(_) => Vec::new(),
    }
}

fn build_extra_args(tab: &TabId, ts: &TabSettings, invocation_args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // Built-in baseline flags. These come BEFORE the user's persistent
    // extra_cli_flags, so a user who really wants to override one (e.g.
    // point at a different metadata file) can add their own flag and
    // rely on aider's last-flag-wins parsing.
    out.extend(builtin_args(tab));

    out.extend(
        ts.extra_cli_flags
            .iter()
            .filter(|s| !s.is_empty())
            .cloned(),
    );

    // cctts is documented as a drop-in replacement for `claude`, so
    // invocation args (`cctts --resume <id>`, etc.) flow only into the
    // claude tab. The aider tab gets its persistent flags only.
    if matches!(tab, TabId::Claude) {
        for arg in invocation_args {
            if !arg.is_empty() {
                out.push(arg.clone());
            }
        }
    }
    out
}

/// Always-on flags per tab. Kept here (not in settings defaults) so the
/// flag set takes effect even on existing user settings files where
/// `extra_cli_flags` is already empty — i.e. nobody has to delete their
/// settings.json to pick up new defaults.
fn builtin_args(tab: &TabId) -> Vec<String> {
    match tab {
        TabId::Claude => Vec::new(),
        TabId::Aider => vec![
            // Aider's built-in model metadata is incomplete for newer
            // models (and lacks any project-specific tuning). Pointing
            // it at a project-local file lets each project ship its own
            // metadata; the path is relative to aider's cwd (= cctts
            // launch dir), so each project's `.aider.model.metadata.json`
            // is picked up automatically. If the file is absent, aider
            // logs a warning and falls back to its built-in defaults.
            "--model-metadata-file".to_string(),
            ".aider.model.metadata.json".to_string(),
        ],
        TabId::Shell(_) => Vec::new(),
    }
}

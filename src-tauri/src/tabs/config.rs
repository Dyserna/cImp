//! Per-tab launch configuration — translates `TabSettings` (binary, flags,
//! TTS injection) into a `PtyLaunchSpec`. Encapsulates Claude's
//! `--append-system-prompt` injection mechanism here so the PtyManager stays
//! generic.

use std::path::Path;

use crate::error::AppResult;
use crate::pty::{resolve_command, PtyLaunchSpec};
use crate::settings::{Settings, TabSettings};
use crate::state::TabId;

pub fn build_launch_spec(
    tab: TabId,
    settings: &Settings,
    launch_cwd: &Path,
    invocation_args: &[String],
) -> AppResult<PtyLaunchSpec> {
    let tab_settings = match tab {
        TabId::Claude => &settings.tabs.claude,
        TabId::Aider => &settings.tabs.aider,
    };
    let binary = resolve_command(&tab_settings.command)?;

    let pre_args = build_pre_args(tab, tab_settings);
    let extra_args = build_extra_args(tab, tab_settings, invocation_args);

    Ok(PtyLaunchSpec {
        tab,
        binary,
        pre_args,
        extra_args,
        working_dir: launch_cwd.to_path_buf(),
    })
}

fn build_pre_args(tab: TabId, ts: &TabSettings) -> Vec<String> {
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
    }
}

fn build_extra_args(tab: TabId, ts: &TabSettings, invocation_args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = ts
        .extra_cli_flags
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect();
    // cctts is documented as a drop-in replacement for `claude`, so
    // invocation args (`cctts --resume <id>`, etc.) flow only into the
    // claude tab. The aider tab gets its persistent flags only.
    if tab == TabId::Claude {
        for arg in invocation_args {
            if !arg.is_empty() {
                out.push(arg.clone());
            }
        }
    }
    out
}

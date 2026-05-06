//! JSON load/save with corruption recovery + v1→v2 migration.
//!
//! On-disk format is a single JSON object matching `Settings` (v2). v1 files
//! can be detected by the presence of the `claude_code` key (gone in v2):
//! when we see one, we route through `migrate_v1_to_v2` and rewrite the file
//! in v2 schema so subsequent loads are pure v2.
//!
//! Either way `load` always returns a usable `Settings` — missing/corrupt
//! files become defaults (and are written back).

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::settings::schema::{
    BehaviorSettings, ComposeSettings, DisplaySettings, ProcessingSettings, Settings,
    ShortcutSettings, TabSettings, TabsSettings, TtsSettings,
};

const FILE_NAME: &str = "settings.json";

/// `%APPDATA%\cctts\settings.json` on Windows; the analogous config dir on
/// other platforms.
pub fn config_path() -> AppResult<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| AppError::Settings("no config dir on this platform".into()))?;
    Ok(dir.join("cctts").join(FILE_NAME))
}

/// Always returns a `Settings`. Defaults are written to disk when the file
/// is absent or corrupt; v1 files are migrated and rewritten in v2 schema.
pub fn load() -> Settings {
    let path = match config_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "settings: cannot resolve config path; using defaults");
            return Settings::default();
        }
    };

    if !path.exists() {
        let s = Settings::default();
        if let Err(e) = save_to(&path, &s) {
            tracing::warn!(error = %e, path = %path.display(), "settings: write defaults failed");
        } else {
            tracing::info!(path = %path.display(), "settings: wrote defaults");
        }
        return s;
    }

    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "settings: read failed; using defaults");
            return Settings::default();
        }
    };

    // Branch on schema version. v1 had a top-level `claude_code` object that
    // no longer exists in v2; v2 has a top-level `tabs` object that didn't
    // exist in v1. We use the raw Value to make the discriminator explicit
    // — relying on serde fallthrough order would mis-route any v1 file
    // whose claude_code section happens to be missing.
    let raw: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "settings: parse failed; reverting to defaults"
            );
            let s = Settings::default();
            let _ = save_to(&path, &s);
            return s;
        }
    };

    let looks_v1 = raw.get("claude_code").is_some() && raw.get("tabs").is_none();
    if looks_v1 {
        match serde_json::from_value::<V1Settings>(raw) {
            Ok(v1) => {
                let migrated = migrate_v1_to_v2(v1);
                if let Err(e) = save_to(&path, &migrated) {
                    tracing::warn!(error = %e, "settings: writing migrated v2 file failed");
                } else {
                    tracing::info!(path = %path.display(), "settings: migrated v1 -> v2");
                }
                return migrated;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "settings: v1 parse failed; reverting to defaults"
                );
                let s = Settings::default();
                let _ = save_to(&path, &s);
                return s;
            }
        }
    }

    match serde_json::from_str::<Settings>(&text) {
        Ok(s) => {
            tracing::info!(path = %path.display(), "settings: loaded");
            s
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "settings: v2 parse failed; reverting to defaults"
            );
            let s = Settings::default();
            let _ = save_to(&path, &s);
            s
        }
    }
}

pub fn save(settings: &Settings) -> AppResult<()> {
    let path = config_path()?;
    save_to(&path, settings)
}

fn save_to(path: &Path, settings: &Settings) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    let text = serde_json::to_string_pretty(settings)
        .map_err(|e| AppError::Settings(format!("serialize: {e}")))?;
    fs::write(path, text).map_err(AppError::Io)
}

// --- v1 → v2 migration ------------------------------------------------------

/// Minimal mirror of the v1 schema, just enough to read a v1 file and copy
/// preserved fields forward. `#[serde(default)]` everywhere so a v1 file
/// missing some sections (or one we never knew it had) still parses.
#[derive(Deserialize, Default)]
#[serde(default)]
struct V1Settings {
    tts: TtsSettings,
    avatar: crate::settings::schema::AvatarSettings,
    display: DisplaySettings,
    behavior: V1BehaviorSettings,
    compose: ComposeSettings,
    shortcuts: V1ShortcutSettings,
    claude_code: V1ClaudeCodeSettings,
    processing: ProcessingSettings,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct V1BehaviorSettings {
    interrupt_on_input: Option<bool>,
    auto_speak: Option<bool>,
    fallback_silent: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct V1ShortcutSettings {
    open_compose: Option<String>,
    submit_compose: Option<String>,
    cancel_compose: Option<String>,
    open_settings: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct V1ClaudeCodeSettings {
    extra_cli_args: Vec<String>,
    /// Read but discarded — v2's TTS injection mechanism is `--append-system-
    /// prompt` driven by `tabs.claude.tts_injection.instructions`, not a
    /// CLAUDE.md file path. The user's per-tab instructions can be edited in
    /// the v2 settings UI (Milestone V2-02).
    #[allow(dead_code)]
    claude_md_path: Option<PathBuf>,
}

fn migrate_v1_to_v2(v1: V1Settings) -> Settings {
    let defaults = Settings::default();

    // Carry `claude_code.extra_cli_args` into `tabs.claude.extra_cli_flags`.
    // Everything else under `tabs` gets the v2 defaults (in particular,
    // `tts_injection.instructions` becomes the embedded RUNTIME_SYSTEM_PROMPT
    // — which is what the user had implicitly in v1 anyway).
    let claude_tab = TabSettings {
        extra_cli_flags: v1.claude_code.extra_cli_args,
        ..TabSettings::default_claude()
    };

    Settings {
        tts: v1.tts,
        avatar: v1.avatar,
        display: v1.display,
        behavior: BehaviorSettings {
            interrupt_on_input: v1
                .behavior
                .interrupt_on_input
                .unwrap_or(defaults.behavior.interrupt_on_input),
            auto_speak: v1.behavior.auto_speak.unwrap_or(defaults.behavior.auto_speak),
            fallback_silent: v1
                .behavior
                .fallback_silent
                .unwrap_or(defaults.behavior.fallback_silent),
            announcements_enabled: defaults.behavior.announcements_enabled,
        },
        compose: v1.compose,
        shortcuts: ShortcutSettings {
            open_compose: v1.shortcuts.open_compose.or(defaults.shortcuts.open_compose),
            submit_compose: v1
                .shortcuts
                .submit_compose
                .or(defaults.shortcuts.submit_compose),
            cancel_compose: v1
                .shortcuts
                .cancel_compose
                .or(defaults.shortcuts.cancel_compose),
            open_settings: v1
                .shortcuts
                .open_settings
                .or(defaults.shortcuts.open_settings),
            switch_to_tab_1: defaults.shortcuts.switch_to_tab_1,
            switch_to_tab_2: defaults.shortcuts.switch_to_tab_2,
            switch_to_tab_3: defaults.shortcuts.switch_to_tab_3,
            switch_to_tab_4: defaults.shortcuts.switch_to_tab_4,
            switch_to_tab_5: defaults.shortcuts.switch_to_tab_5,
            switch_to_tab_6: defaults.shortcuts.switch_to_tab_6,
            switch_to_tab_7: defaults.shortcuts.switch_to_tab_7,
            switch_to_tab_8: defaults.shortcuts.switch_to_tab_8,
            switch_to_tab_9: defaults.shortcuts.switch_to_tab_9,
            new_shell_tab: defaults.shortcuts.new_shell_tab,
            close_tab: defaults.shortcuts.close_tab,
        },
        tabs: TabsSettings {
            claude: claude_tab,
            aider: TabSettings::default_aider(),
        },
        processing: v1.processing,
        shell_1_tmp: defaults.shell_1_tmp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_v1_carries_extra_cli_args() {
        let v1_json = r#"{
            "claude_code": { "extra_cli_args": ["--foo", "--bar"], "claude_md_path": null },
            "tts": { "voice": "af_heart", "speed": 1.0, "volume": 1.0, "mute": false }
        }"#;
        let raw: serde_json::Value = serde_json::from_str(v1_json).unwrap();
        let v1: V1Settings = serde_json::from_value(raw).unwrap();
        let v2 = migrate_v1_to_v2(v1);
        assert_eq!(v2.tabs.claude.extra_cli_flags, vec!["--foo", "--bar"]);
        assert!(v2.tabs.claude.tts_injection.enabled);
        assert!(!v2.tabs.aider.tts_injection.enabled);
        assert_eq!(v2.tabs.aider.command, "aider");
        assert!(v2.behavior.announcements_enabled);
        assert_eq!(v2.shortcuts.switch_to_tab_1.as_deref(), Some("Ctrl+1"));
    }

    #[test]
    fn v2_file_round_trips() {
        let v2 = Settings::default();
        let text = serde_json::to_string(&v2).unwrap();
        let parsed: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.tabs.claude.command, "claude");
        assert_eq!(parsed.tabs.aider.command, "aider");
        assert!(!parsed.tabs.aider.tts_injection.enabled);
    }

    #[test]
    fn v1_detection_skips_files_with_tabs_already() {
        // A handcrafted file that has BOTH `claude_code` and `tabs` is treated
        // as v2 (already-migrated). `claude_code` is silently ignored by serde.
        let mixed = r#"{
            "claude_code": { "extra_cli_args": ["--legacy"] },
            "tabs": { "claude": { "command": "claude", "extra_cli_flags": ["--new"] } }
        }"#;
        let raw: serde_json::Value = serde_json::from_str(mixed).unwrap();
        let looks_v1 = raw.get("claude_code").is_some() && raw.get("tabs").is_none();
        assert!(!looks_v1);
    }
}

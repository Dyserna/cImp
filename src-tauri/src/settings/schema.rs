//! Settings schema. Every struct uses `#[serde(default)]` so loading a JSON
//! file written by a future or past version still succeeds: missing fields
//! get defaults, unknown fields are ignored. v2 schema; the v1 → v2 migration
//! lives in `persistence.rs` and runs once on first load of an old file.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct Settings {
    pub tts: TtsSettings,
    pub avatar: AvatarSettings,
    pub display: DisplaySettings,
    pub behavior: BehaviorSettings,
    pub compose: ComposeSettings,
    pub shortcuts: ShortcutSettings,
    pub tabs: TabsSettings,
    pub processing: ProcessingSettings,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TtsSettings {
    pub voice: String,
    pub speed: f32,
    pub volume: f32,
    pub mute: bool,
}

impl Default for TtsSettings {
    fn default() -> Self {
        Self {
            voice: "af_heart".to_string(),
            speed: 1.0,
            volume: 1.0,
            mute: false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AvatarSettings {
    pub visible: bool,
    pub size: AvatarSize,
    pub position: AvatarPosition,
    pub margin_px: u32,
    pub opacity: f32,
    pub images: AvatarImages,
    pub transition: TransitionSettings,
    pub waveform: WaveformSettings,
}

impl Default for AvatarSettings {
    fn default() -> Self {
        Self {
            visible: true,
            size: AvatarSize::default(),
            position: AvatarPosition::TopRight,
            margin_px: 16,
            opacity: 0.8,
            images: AvatarImages::default(),
            transition: TransitionSettings::default(),
            waveform: WaveformSettings::default(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AvatarSize {
    pub width_px: u32,
    pub height_px: u32,
}

impl Default for AvatarSize {
    fn default() -> Self {
        Self {
            width_px: 240,
            height_px: 240,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AvatarPosition {
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

impl Default for AvatarPosition {
    fn default() -> Self {
        Self::TopRight
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct AvatarImages {
    pub idle: Option<PathBuf>,
    pub listening: Option<PathBuf>,
    pub thinking: Option<PathBuf>,
    pub speaking: Option<PathBuf>,
    pub error: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TransitionSettings {
    pub path: Option<PathBuf>,
    pub duration_ms: u32,
}

impl Default for TransitionSettings {
    fn default() -> Self {
        // Bundled URL — frontend distinguishes vite-served paths (start with
        // `/`) from absolute disk paths picked via the file dialog. Empty
        // path on either side disables transitions.
        Self {
            path: Some(PathBuf::from("/avatar/Transition.mp4")),
            duration_ms: 400,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct WaveformSettings {
    pub color: String,
    pub line_width: f32,
    pub glow_intensity: f32,
    pub opacity: f32,
}

impl Default for WaveformSettings {
    fn default() -> Self {
        Self {
            color: "#bb55ff".to_string(),
            line_width: 2.0,
            glow_intensity: 0.6,
            opacity: 0.85,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct DisplaySettings {
    pub terminal_font_family: String,
    pub terminal_font_size: u32,
    pub theme: String,
    pub show_tts_markup: bool,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            terminal_font_family: "Consolas, Menlo, \"DejaVu Sans Mono\", monospace"
                .to_string(),
            terminal_font_size: 14,
            theme: "dark".to_string(),
            show_tts_markup: false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct BehaviorSettings {
    pub interrupt_on_input: bool,
    pub auto_speak: bool,
    pub fallback_silent: bool,
    pub announcements_enabled: bool,
}

impl Default for BehaviorSettings {
    fn default() -> Self {
        Self {
            interrupt_on_input: true,
            auto_speak: true,
            fallback_silent: true,
            announcements_enabled: true,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ComposeSettings {
    pub min_height_px: u32,
    pub max_height_px: u32,
}

impl Default for ComposeSettings {
    fn default() -> Self {
        Self {
            min_height_px: 80,
            max_height_px: 300,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ShortcutSettings {
    pub open_compose: Option<String>,
    pub submit_compose: Option<String>,
    pub cancel_compose: Option<String>,
    pub open_settings: Option<String>,
    pub switch_to_tab_1: Option<String>,
    pub switch_to_tab_2: Option<String>,
}

impl Default for ShortcutSettings {
    fn default() -> Self {
        Self {
            open_compose: Some("Ctrl+Shift+E".to_string()),
            submit_compose: Some("Ctrl+Enter".to_string()),
            cancel_compose: Some("Escape".to_string()),
            open_settings: Some("Ctrl+,".to_string()),
            switch_to_tab_1: Some("Ctrl+1".to_string()),
            switch_to_tab_2: Some("Ctrl+2".to_string()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TabsSettings {
    #[serde(default = "TabSettings::default_claude")]
    pub claude: TabSettings,
    #[serde(default = "TabSettings::default_aider")]
    pub aider: TabSettings,
}

impl Default for TabsSettings {
    fn default() -> Self {
        Self {
            claude: TabSettings::default_claude(),
            aider: TabSettings::default_aider(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TabSettings {
    pub command: String,
    pub extra_cli_flags: Vec<String>,
    pub tts_injection: TtsInjection,
    pub notifications: NotificationsSettings,
}

impl TabSettings {
    pub fn default_claude() -> Self {
        Self {
            command: "claude".to_string(),
            extra_cli_flags: Vec::new(),
            tts_injection: TtsInjection {
                enabled: true,
                instructions: crate::tts::RUNTIME_SYSTEM_PROMPT.to_string(),
            },
            notifications: NotificationsSettings {
                idle: "Claude is idle".to_string(),
                awaiting_permission: "Claude is awaiting permission".to_string(),
                error: "Claude encountered an error".to_string(),
            },
        }
    }

    pub fn default_aider() -> Self {
        Self {
            command: "aider".to_string(),
            extra_cli_flags: Vec::new(),
            tts_injection: TtsInjection {
                enabled: false,
                instructions: String::new(),
            },
            notifications: NotificationsSettings {
                idle: "Aider is idle".to_string(),
                awaiting_permission: "Aider is awaiting permission".to_string(),
                error: "Aider encountered an error".to_string(),
            },
        }
    }
}

// Neutral default for the field-level `#[serde(default)]` fallback when a
// caller manually edits the file and partially deletes a tab object. The
// per-tab defaults from `default_claude` / `default_aider` cover the
// missing-whole-object case.
impl Default for TabSettings {
    fn default() -> Self {
        Self {
            command: String::new(),
            extra_cli_flags: Vec::new(),
            tts_injection: TtsInjection::default(),
            notifications: NotificationsSettings::default(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct TtsInjection {
    pub enabled: bool,
    pub instructions: String,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct NotificationsSettings {
    pub idle: String,
    pub awaiting_permission: String,
    pub error: String,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ProcessingSettings {
    pub stability_timeout_ms: u32,
    pub max_hold_ms: u32,
}

impl Default for ProcessingSettings {
    fn default() -> Self {
        Self {
            stability_timeout_ms: 200,
            max_hold_ms: 500,
        }
    }
}

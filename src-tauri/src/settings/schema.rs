//! Settings schema. Every struct uses `#[serde(default)]` so loading a JSON
//! file written by a future or past version still succeeds: missing fields
//! get defaults, unknown fields are ignored. v1.2 schema; the v1 → v2 and
//! v1.1 → v1.2 migrations live in `migration.rs` and run once on first load.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::shell::ShellSpec;

/// Reserved IDs the integrity check protects. Hand-edited settings files
/// cannot make these disappear — they are restored with defaults if missing
/// and forced to `builtin: true` regardless of what the file claims.
pub const CLAUDE_TAB_ID: &str = "claude";
pub const AIDER_TAB_ID: &str = "aider";
pub const SHELL_DEFAULT_TAB_ID: &str = "shell-default-1";

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct Settings {
    pub tts: TtsSettings,
    pub avatar: AvatarSettings,
    pub display: DisplaySettings,
    pub behavior: BehaviorSettings,
    pub compose: ComposeSettings,
    pub shortcuts: ShortcutSettings,
    /// Ordered list of tabs. The first user-created shell tab appears after
    /// the two AI builtins (claude/aider) and the default shell tab. Order
    /// is user-visible (tab bar) and persisted across launches. The startup
    /// integrity check ensures the three reserved-id entries are present;
    /// hand-edits that delete them are repaired at load time.
    pub tabs: Vec<TabConfig>,
    pub processing: ProcessingSettings,
    /// Last-active tab pointer, restored on launch. None on a fresh install
    /// (falls back to the first tab); set whenever the user switches tabs.
    pub session: SessionState,
    /// Persisted layout tree + focused-pane id (V4-04). `None` on fresh
    /// installs and on the first launch after a v1.2 → v1.3 migration that
    /// detected the file but couldn't synthesize a layout (defensive — the
    /// migration normally always builds a single-pane layout). The frontend
    /// builds a default single-root-pane tree containing every tab when
    /// this is `None`.
    pub layout: Option<LayoutPersisted>,
    /// Named layout presets. Empty by default; populated via the Layouts
    /// menu's "Save current layout as..." entry. Restoring a preset
    /// replaces the live tree wholesale; the preset itself is unchanged.
    pub layout_presets: Vec<LayoutPreset>,
    /// UI chrome theme settings (V5). The `theme` field selects the
    /// design-token block applied to the cctts chrome (tab bar, status
    /// bar, dialogs, settings). Distinct from `display.theme`, which
    /// governs the xterm.js terminal palette inside each tab.
    pub ui: UiSettings,
}

impl Settings {
    /// Lookup a tab entry by id. Returns None for ids that don't exist —
    /// callers (launch flow, lifecycle commands) treat this as "tab gone"
    /// rather than constructing a default.
    pub fn find_tab(&self, id: &str) -> Option<&TabConfig> {
        self.tabs.iter().find(|t| t.id() == id)
    }

    pub fn find_tab_mut(&mut self, id: &str) -> Option<&mut TabConfig> {
        self.tabs.iter_mut().find(|t| t.id() == id)
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct SessionState {
    pub active_tab_id: Option<String>,
}

/// Persisted layout state. Mirrors the frontend's `LayoutState` 1:1 — the
/// `type` discriminator on `LayoutNodePersisted` matches the frontend's
/// `'split' | 'pane'` shape, so serialize/deserialize is identity work
/// across the IPC boundary.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct LayoutPersisted {
    pub tree: LayoutNodePersisted,
    pub focused_pane_id: String,
}

/// Recursive layout-tree node. Splits are internal (two children + ratio +
/// direction); panes are leaves (ordered tab id list + per-pane active tab
/// id).
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNodePersisted {
    Split {
        id: String,
        direction: SplitDirection,
        ratio: f32,
        first: Box<LayoutNodePersisted>,
        second: Box<LayoutNodePersisted>,
    },
    Pane {
        id: String,
        tab_ids: Vec<String>,
        active_tab_id: Option<String>,
    },
}

/// Direction of a Split node. Naming matches CSS flexbox: `Horizontal`
/// arranges children side-by-side (vertical splitter between them);
/// `Vertical` stacks them top-to-bottom. See DESIGN.md for the
/// rationale for this convention.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// A named layout preset. The tree is the layout-only payload — focus and
/// the live `focused_pane_id` are intentionally not persisted with the
/// preset, since restoring a preset is "set up panes this way" and focus
/// follows the user's next click.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct LayoutPreset {
    pub name: String,
    /// RFC 3339 / ISO 8601 timestamp (UTC, second precision). Used to
    /// order the popover's "Recent presets" list. Renames do not refresh
    /// this — it remains the original creation time.
    pub created_at: String,
    pub tree: LayoutNodePersisted,
}

/// Discriminated tab config. The `kind` field is the JSON discriminator
/// (`"ai_tool"` or `"shell"`), produced by serde's internally-tagged
/// representation. Each variant carries the fields specific to its kind —
/// AI tabs have `tts_injection` and three notification slots; Shell tabs
/// have two notification slots and no TTS hook.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabConfig {
    AiTool(AiToolTabConfig),
    Shell(ShellTabConfig),
}

impl TabConfig {
    pub fn id(&self) -> &str {
        match self {
            TabConfig::AiTool(c) => &c.id,
            TabConfig::Shell(c) => &c.id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            TabConfig::AiTool(c) => &c.name,
            TabConfig::Shell(c) => &c.name,
        }
    }

    pub fn set_name(&mut self, name: String) {
        match self {
            TabConfig::AiTool(c) => c.name = name,
            TabConfig::Shell(c) => c.name = name,
        }
    }

    pub fn builtin(&self) -> bool {
        match self {
            TabConfig::AiTool(c) => c.builtin,
            TabConfig::Shell(c) => c.builtin,
        }
    }

    pub fn set_builtin(&mut self, value: bool) {
        match self {
            TabConfig::AiTool(c) => c.builtin = value,
            TabConfig::Shell(c) => c.builtin = value,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AiToolTabConfig {
    pub id: String,
    pub ai_tool_kind: AiToolKindWire,
    pub builtin: bool,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub tts_injection: TtsInjection,
    pub notifications: AiNotificationConfig,
    /// Mirrors v1.1's per-tab `first_launch_notice_dismissed`. Carried
    /// through migration verbatim so the aider banner doesn't re-appear for
    /// existing users.
    pub first_launch_notice_dismissed: bool,
}

impl Default for AiToolTabConfig {
    fn default() -> Self {
        // Neutral default — only used when serde encounters a malformed
        // entry mid-array. Real defaults come from the constructors below.
        Self {
            id: String::new(),
            ai_tool_kind: AiToolKindWire::ClaudeCode,
            builtin: false,
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
            tts_injection: TtsInjection::default(),
            notifications: AiNotificationConfig::default(),
            first_launch_notice_dismissed: false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ShellTabConfig {
    pub id: String,
    pub builtin: bool,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub notifications: ShellNotificationConfig,
}

impl Default for ShellTabConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            builtin: false,
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
            notifications: ShellNotificationConfig::default(),
        }
    }
}

/// Wire-format mirror of `state::AiToolKind`. The state-side enum lives in
/// `state::manager` for use with the runtime state machine; this serde-aware
/// twin is the on-disk discriminator. The `From` impls below keep them in
/// lockstep.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AiToolKindWire {
    ClaudeCode,
    Aider,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct AiNotificationConfig {
    pub idle: String,
    pub awaiting_permission: String,
    pub error: String,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ShellNotificationConfig {
    pub error: String,
    /// `{code}` placeholder is interpolated with the actual exit code in M4.
    pub exited: String,
}

impl Default for ShellNotificationConfig {
    fn default() -> Self {
        Self {
            error: "Shell encountered an error".to_string(),
            exited: "Shell exited (code {code})".to_string(),
        }
    }
}

// --- Builtin defaults -------------------------------------------------------
//
// Used by:
//   1. The migration step to fill in missing entries (e.g. an aider entry
//      absent from a hand-edited v1.1 file).
//   2. The integrity check at load time to restore deleted builtins.
//   3. `Settings::default()` to seed a fresh-install file before the first
//      save.

pub fn default_claude_tab() -> TabConfig {
    TabConfig::AiTool(AiToolTabConfig {
        id: CLAUDE_TAB_ID.to_string(),
        ai_tool_kind: AiToolKindWire::ClaudeCode,
        builtin: true,
        name: "Claude".to_string(),
        command: "claude".to_string(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        tts_injection: TtsInjection {
            enabled: true,
            instructions: crate::tts::RUNTIME_SYSTEM_PROMPT.to_string(),
        },
        notifications: AiNotificationConfig {
            idle: "Claude is idle".to_string(),
            awaiting_permission: "Claude is awaiting permission".to_string(),
            error: "Claude encountered an error".to_string(),
        },
        // Claude has no first-launch notice; pre-dismissed so the overlay
        // code can use a single per-tab predicate.
        first_launch_notice_dismissed: true,
    })
}

pub fn default_aider_tab() -> TabConfig {
    TabConfig::AiTool(AiToolTabConfig {
        id: AIDER_TAB_ID.to_string(),
        ai_tool_kind: AiToolKindWire::Aider,
        builtin: true,
        name: "Aider".to_string(),
        command: "aider".to_string(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        tts_injection: TtsInjection {
            enabled: false,
            instructions: String::new(),
        },
        notifications: AiNotificationConfig {
            idle: "Aider is idle".to_string(),
            awaiting_permission: "Aider is awaiting permission".to_string(),
            error: "Aider encountered an error".to_string(),
        },
        first_launch_notice_dismissed: false,
    })
}

/// Default Shell-1 entry. Takes the resolved platform default shell so the
/// `command` and `args` fields land on the right binary for the host. The
/// reserved id keeps the integrity check able to identify "the original
/// Shell 1" across launches.
pub fn default_shell_1_tab(default_shell: &ShellSpec) -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: SHELL_DEFAULT_TAB_ID.to_string(),
        builtin: true,
        name: "Shell 1".to_string(),
        command: default_shell.command.to_string_lossy().into_owned(),
        args: default_shell.args.clone(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
    })
}

// --- Other settings sub-structs (unchanged from v2) -------------------------

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
pub struct UiSettings {
    /// Active UI chrome theme. V5-01 ships only `"modern-dark"`; future
    /// themes (light, high-contrast) plug in here as additional values.
    pub theme: String,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: "modern-dark".to_string(),
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
    pub switch_to_tab_3: Option<String>,
    pub switch_to_tab_4: Option<String>,
    pub switch_to_tab_5: Option<String>,
    pub switch_to_tab_6: Option<String>,
    pub switch_to_tab_7: Option<String>,
    pub switch_to_tab_8: Option<String>,
    pub switch_to_tab_9: Option<String>,
    /// Open the New Shell Tab dialog. Identical to clicking the `+`
    /// button on the tab bar.
    pub new_shell_tab: Option<String>,
    /// Close the active tab. No-op (with a transient toast) on builtins.
    pub close_tab: Option<String>,
    /// V4-03 pane shortcuts. Optional so v1.2 settings files round-trip
    /// without migration; the frontend defaults supply working bindings
    /// when these are missing.
    pub focus_pane_left: Option<String>,
    pub focus_pane_right: Option<String>,
    pub focus_pane_up: Option<String>,
    pub focus_pane_down: Option<String>,
    pub split_pane_horizontal: Option<String>,
    pub split_pane_vertical: Option<String>,
    pub close_pane: Option<String>,
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
            switch_to_tab_3: Some("Ctrl+3".to_string()),
            switch_to_tab_4: Some("Ctrl+4".to_string()),
            switch_to_tab_5: Some("Ctrl+5".to_string()),
            switch_to_tab_6: Some("Ctrl+6".to_string()),
            switch_to_tab_7: Some("Ctrl+7".to_string()),
            switch_to_tab_8: Some("Ctrl+8".to_string()),
            switch_to_tab_9: Some("Ctrl+9".to_string()),
            new_shell_tab: Some("Ctrl+T".to_string()),
            close_tab: Some("Ctrl+W".to_string()),
            focus_pane_left: Some("Ctrl+Alt+Left".to_string()),
            focus_pane_right: Some("Ctrl+Alt+Right".to_string()),
            focus_pane_up: Some("Ctrl+Alt+Up".to_string()),
            focus_pane_down: Some("Ctrl+Alt+Down".to_string()),
            split_pane_horizontal: Some("Ctrl+\\".to_string()),
            split_pane_vertical: Some("Ctrl+Shift+\\".to_string()),
            close_pane: Some("Ctrl+Shift+W".to_string()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct TtsInjection {
    pub enabled: bool,
    pub instructions: String,
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

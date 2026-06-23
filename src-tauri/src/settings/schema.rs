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
/// and forced to `builtin: true` regardless of what the file claims. Four
/// reserved AI ids today: subscription/local pairs for both Claude Code
/// and Aider.
pub const CLAUDE_TAB_ID: &str = "claude";
/// V1.4-07: second Claude tab preconfigured to use a local LLM via the
/// `claude_local` provider settings. Replaces the v1.7-and-earlier
/// `aider` reserved id (the v1.7 → v1.8 migration rewrites the aider
/// tab in place to this id).
pub const CLAUDE_LOCAL_TAB_ID: &str = "claude-local";
/// Aider AI-tool tab using whatever provider Aider's own config selects
/// (cloud/keys/etc). Reserved in V14 alongside `aider-local`.
pub const AIDER_TAB_ID: &str = "aider";
/// Aider AI-tool tab pointed at a local OpenAI-compatible endpoint via
/// the global `aider_local` provider settings.
pub const AIDER_LOCAL_TAB_ID: &str = "aider-local";
pub const SHELL_DEFAULT_TAB_ID: &str = "shell-default-1";
/// V8-03: the read-only, non-closable Offload Server tab. Materialized only
/// while `offload.enabled` (integrity check), it shows the local
/// `llama-server`'s live output (model-load progress + logs). Internally a
/// Shell-kind tab with `builtin: true` (so it can't be closed) and a reserved
/// id the frontend keys off to render read-only, log-fed content with no PTY.
pub const OFFLOAD_SERVER_TAB_ID: &str = "offload-server";
/// Legacy id of the V15 reserved broot tab. Retired in V16: broot is no
/// longer a persistent builtin — it (like rustnet) launches on demand from
/// the bottom-bar tool buttons into ordinary closable Shell tabs (uuid ids).
/// This constant survives only so the v15 → v16 migration can find and drop
/// the old auto-seeded `shell-broot` entry from existing settings files.
pub const SHELL_BROOT_TAB_ID: &str = "shell-broot";

/// The on-disk schema version. Bumped on every migration step. Detection
/// of legacy files prefers this field's integer value over the older
/// presence-of-key archaeology in the migration cascade — a future migration
/// only needs to compare `value.schema_version < N`.
///
/// Files that pre-date V1.10 lack the field entirely; the cascade still
/// uses the `looks_v1_X` predicates for those, falling through to a final
/// step that stamps the field with the current value.
pub const CURRENT_SCHEMA_VERSION: u8 = 17;

fn current_schema_version() -> u8 {
    CURRENT_SCHEMA_VERSION
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Settings {
    /// On-disk schema version. Stamped at `CURRENT_SCHEMA_VERSION`
    /// on fresh installs and by the v1.9→v1.10 migration. Defaulted via
    /// `current_schema_version` so legacy files (which lack the key)
    /// deserialize at the latest version after migration runs.
    #[serde(default = "current_schema_version")]
    pub schema_version: u8,
    pub tts: TtsSettings,
    /// V6-01 offline speech-to-text (dictation) config. Additive
    /// `#[serde(default)]` field — old settings files round-trip with
    /// the feature disabled by default.
    pub stt: SttSettings,
    pub avatar: AvatarSettings,
    pub display: DisplaySettings,
    pub behavior: BehaviorSettings,
    /// Bottom-bar Claude Code usage tracker config.
    pub usage: UsageSettings,
    /// Bottom-bar system-monitor panel config.
    pub system_stats: SystemStatsSettings,
    /// Claude Code context-window status line bar. Global (like the avatar
    /// and TTS voice) — applies to every ccImp-launched Claude tab rather
    /// than per-tab. Drives a `--settings` overlay injected at launch (see
    /// `tabs::config`) that points Claude Code's `statusLine` at our own
    /// `ccimp --statusline` renderer.
    pub statusline: StatuslineSettings,
    pub compose: ComposeSettings,
    pub shortcuts: ShortcutSettings,
    /// Ordered list of tabs. AI builtins occupy the canonical leading
    /// slots in the order claude → claude-local → aider → aider-local
    /// (only the ids in `enabled_ai_tabs` actually materialize); user
    /// shell tabs follow. The startup integrity check reconciles
    /// `tabs[]` with `enabled_ai_tabs` — hand-edits that delete an
    /// enabled AI builtin are repaired at load time.
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
    /// design-token block applied to the ccimp chrome (tab bar, status
    /// bar, dialogs, settings). Distinct from `terminal.theme`, which
    /// governs the xterm.js terminal palette inside each tab.
    pub ui: UiSettings,
    /// Terminal-pane settings (V1.4-01+): xterm.js theme today, plus
    /// the V1.4-02 background image/color group when that ships.
    /// Distinct from `ui`, which themes the ccimp chrome.
    pub terminal: TerminalSettings,
    /// V1.4-07: local-LLM provider config for AI tabs whose
    /// `use_local_provider` flag is `true`. The launch-time env
    /// composition reads `base_url`/`auth_token`/`model_alias` from
    /// here and synthesizes `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN`
    /// (and `ANTHROPIC_MODEL` if set) into the spawned process's env.
    /// Per-tab `env` entries take precedence over synthesized values.
    pub claude_local: ClaudeLocalSettings,
    /// V14: local-LLM provider config for Aider tabs whose
    /// `use_local_provider` flag is `true`. The launch-time env
    /// composition reads `base_url`/`auth_token`/`model` from here and
    /// synthesizes `OPENAI_API_BASE` / `OPENAI_API_KEY` (and a
    /// `--model <model>` CLI arg when `model` is non-empty) into the
    /// spawned aider process. Per-tab `env` entries take precedence
    /// over synthesized values.
    pub aider_local: AiderLocalSettings,
    /// V8-01: local task-offload config. ccImp runs a user-supplied
    /// `llama-server` and exposes an `offload_task` MCP tool into
    /// ccImp-launched Claude tabs so Opus can delegate token-heavy
    /// subtasks to the local model. Off by default. Additive — old
    /// settings files load with the feature disabled.
    pub offload: OffloadSettings,
    /// Which AI-tool tabs are enabled. Each id in this list corresponds
    /// to one of the four reserved AI builtins (`claude`, `claude-local`,
    /// `aider`, `aider-local`). Adding an id opens that tab; removing
    /// one closes it (kills its PTY, drops scrollback, removes the
    /// settings entry). The list is required to be non-empty — the
    /// integrity check forces `[claude]` if it deserializes empty, and
    /// the runtime IPC rejects an empty value. Default is `[claude]` so
    /// a fresh install starts with the subscription Claude tab only.
    pub enabled_ai_tabs: Vec<AiTabId>,
    /// File-logger configuration. The tracing subscriber writes daily
    /// rolling files into `<portable-root>/logs/`; the level field drives
    /// the EnvFilter via a reload handle so changes apply live.
    pub logging: LoggingSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            tts: TtsSettings::default(),
            stt: SttSettings::default(),
            avatar: AvatarSettings::default(),
            display: DisplaySettings::default(),
            behavior: BehaviorSettings::default(),
            usage: UsageSettings::default(),
            system_stats: SystemStatsSettings::default(),
            statusline: StatuslineSettings::default(),
            compose: ComposeSettings::default(),
            shortcuts: ShortcutSettings::default(),
            tabs: Vec::new(),
            processing: ProcessingSettings::default(),
            session: SessionState::default(),
            layout: None,
            layout_presets: Vec::new(),
            ui: UiSettings::default(),
            terminal: TerminalSettings::default(),
            claude_local: ClaudeLocalSettings::default(),
            aider_local: AiderLocalSettings::default(),
            offload: OffloadSettings::default(),
            enabled_ai_tabs: vec![AiTabId::Claude],
            logging: LoggingSettings::default(),
        }
    }
}

/// Logging configuration. The file path is fixed at
/// `<portable-root>/logs/ccimp.log.<YYYY-MM-DD>`; the `level` field drives
/// the live filter and `retention` drives the startup cleanup pass.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct LoggingSettings {
    pub level: LogLevel,
    pub retention: LogRetention,
    pub content_capture: ContentCaptureSettings,
}

/// Per-tab content (PTY raw output) capture configuration. Disabled by
/// default — when enabled, every byte read from each tab's PTY is also
/// appended to `<portable-root>/logs/content/<tab-id>.log.<YYYY-MM-DD>`,
/// rotated daily by `tracing-appender`. `retention` runs the same
/// max-age cleanup as the tracing logs but against the `content/`
/// subdirectory.
#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ContentCaptureSettings {
    pub enabled: bool,
    pub retention: LogRetention,
}

impl Default for ContentCaptureSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            retention: LogRetention::Weekly,
        }
    }
}

/// Per-process tracing-filter level. Mapped to an `EnvFilter` string by
/// `as_filter_str`; serialized as a lowercase string. The `RUST_LOG`
/// environment variable, when set, overrides this at startup.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_filter_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// How long to keep rolled log files before the startup cleanup pass
/// removes them. `Never` skips cleanup entirely. Computed as a max-age
/// against each file's mtime in `logging::run_cleanup`.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogRetention {
    Daily,
    #[default]
    Weekly,
    Monthly,
    Never,
}

impl LogRetention {
    /// Max-age threshold for cleanup. `None` means "keep everything".
    /// Approximate calendar values (1d / 7d / 30d) — exact day-boundary
    /// alignment isn't necessary for log retention.
    pub fn max_age(self) -> Option<std::time::Duration> {
        const DAY: u64 = 24 * 60 * 60;
        match self {
            Self::Daily => Some(std::time::Duration::from_secs(DAY)),
            Self::Weekly => Some(std::time::Duration::from_secs(7 * DAY)),
            Self::Monthly => Some(std::time::Duration::from_secs(30 * DAY)),
            Self::Never => None,
        }
    }
}

/// One of the four reserved AI-tool tab ids. Wire format is the kebab-
/// case tab-id string (`"claude"`, `"claude-local"`, `"aider"`,
/// `"aider-local"`); the type exists so `enabled_ai_tabs` can be a
/// strongly-typed `Vec<AiTabId>` instead of an untyped string list.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum AiTabId {
    Claude,
    ClaudeLocal,
    Aider,
    AiderLocal,
}

impl AiTabId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => CLAUDE_TAB_ID,
            Self::ClaudeLocal => CLAUDE_LOCAL_TAB_ID,
            Self::Aider => AIDER_TAB_ID,
            Self::AiderLocal => AIDER_LOCAL_TAB_ID,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            CLAUDE_TAB_ID => Some(Self::Claude),
            CLAUDE_LOCAL_TAB_ID => Some(Self::ClaudeLocal),
            AIDER_TAB_ID => Some(Self::Aider),
            AIDER_LOCAL_TAB_ID => Some(Self::AiderLocal),
            _ => None,
        }
    }

    /// True for the local-provider variants (`claude-local`,
    /// `aider-local`). The integrity check uses this as the canonical
    /// `use_local_provider` value for each reserved id.
    pub fn uses_local_provider(self) -> bool {
        matches!(self, Self::ClaudeLocal | Self::AiderLocal)
    }

    /// Canonical tab-bar position: claude (0) → claude-local → aider →
    /// aider-local, with shells trailing afterwards. Used by
    /// `add_ai_builtin_tab` and `integrity_check` so re-adding a
    /// previously-disabled AI tab lands in the same slot every time.
    pub fn canonical_order(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::ClaudeLocal => 1,
            Self::Aider => 2,
            Self::AiderLocal => 3,
        }
    }
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

    /// Whether the given tab is in "speak all output" mode — true only for
    /// an AI tab with `tts_all_output` set. Read live by the per-tab
    /// processor on each settings broadcast. Unknown / shell tabs are false.
    pub fn tab_speak_all_output(&self, id: &str) -> bool {
        matches!(self.find_tab(id), Some(TabConfig::AiTool(c)) if c.tts_all_output)
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

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct AiToolTabConfig {
    pub id: String,
    pub builtin: bool,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub tts_injection: TtsInjection,
    pub notifications: AiNotificationConfig,
    /// Carried over from earlier schemas where aider had a one-time
    /// first-launch banner. Aider is gone (V1.4-07) and Claude tabs
    /// pre-dismiss this; left in place to keep the wire format stable
    /// for users still loading older settings files.
    pub first_launch_notice_dismissed: bool,
    /// V1.4-01 per-tab terminal palette override. `None` means inherit
    /// the global `terminal.theme`; `Some(_)` replaces it with the
    /// override's bundled name (or Custom block) for this tab only.
    /// The override travels with the tab through drag-and-drop because
    /// it lives on the tab itself, not on a pane.
    pub theme_override: Option<TerminalThemeSettings>,
    /// V1.4-02 per-tab background override (three-state). `None` means
    /// inherit the global `terminal.background`; `Some(Disabled)` means
    /// opt out (theme bg only); `Some(Custom(cfg))` replaces the global
    /// background wholesale for this tab.
    pub background_override: Option<BackgroundOverride>,
    /// V1.4-07: when `true`, the launch-time env composition synthesizes
    /// `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` (and `ANTHROPIC_MODEL`
    /// if `claude_local.model_alias` is non-empty) from the global
    /// `claude_local` settings group. Per-tab `env` entries override
    /// synthesized values.
    pub use_local_provider: bool,
    /// When `true`, the processing layer speaks ALL new terminal output
    /// for this tab (sentence-segmented, deduped) and ignores
    /// `[[TTS]]…[[/TTS]]` markers entirely, rather than speaking only the
    /// marked segments. Toggled from the tab's right-click menu; persists
    /// in the per-folder overlay like any other per-tab field. Read live by
    /// the per-tab processor via the settings broadcast — no tab restart.
    pub tts_all_output: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
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
    /// V1.4-01 per-tab terminal palette override. `None` means inherit
    /// the global `terminal.theme`; `Some(_)` replaces it for this tab.
    /// See `AiToolTabConfig::theme_override` for the full rationale.
    pub theme_override: Option<TerminalThemeSettings>,
    /// V1.4-02 per-tab background override (three-state). See
    /// `AiToolTabConfig::background_override`.
    pub background_override: Option<BackgroundOverride>,
}


/// V1.4-07: local-LLM provider configuration. When an AI tab has
/// `use_local_provider: true`, the launch flow composes env vars from
/// these fields onto the spawn process, allowing Claude Code to talk
/// to a local proxy (typically LiteLLM bridging to Ollama / LM Studio /
/// other OpenAI-compatible endpoints) instead of api.anthropic.com.
/// Stored cleartext in settings.json — local proxies typically accept
/// dummy tokens. OS-keychain integration is documented as a future
/// upgrade in `docs/FUTURE-FEATURES-keyring.md`.
///
/// The `Debug` impl is hand-rolled to redact `auth_token` so any
/// accidental `?settings` / `?cfg` log line cannot leak the secret to
/// the rolling log file. This is defense-in-depth against future code
/// that adds such a log line; today no caller logs the struct.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeLocalSettings {
    pub base_url: String,
    pub auth_token: String,
    pub model_alias: String,
}

impl std::fmt::Debug for ClaudeLocalSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted = if self.auth_token.is_empty() {
            "<empty>"
        } else {
            "<redacted>"
        };
        f.debug_struct("ClaudeLocalSettings")
            .field("base_url", &self.base_url)
            .field("auth_token", &redacted)
            .field("model_alias", &self.model_alias)
            .finish()
    }
}

impl Default for ClaudeLocalSettings {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:4000".to_string(),
            auth_token: "sk-dummy".to_string(),
            model_alias: String::new(),
        }
    }
}

/// V14: local-LLM provider configuration for Aider tabs whose
/// `use_local_provider: true`. The launch flow synthesizes
/// `OPENAI_API_BASE` from `base_url`, `OPENAI_API_KEY` from
/// `auth_token`, and (when `model` is non-empty) appends
/// `--model <model>` to the spawn argv. Stored cleartext for the same
/// reasons as `ClaudeLocalSettings` (local proxies typically accept
/// dummy tokens; OS-keychain integration is a future upgrade).
///
/// The hand-rolled `Debug` impl redacts `auth_token` so a stray
/// `?settings` log line cannot leak the secret to the rolling log.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiderLocalSettings {
    pub base_url: String,
    pub auth_token: String,
    pub model: String,
}

impl std::fmt::Debug for AiderLocalSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted = if self.auth_token.is_empty() {
            "<empty>"
        } else {
            "<redacted>"
        };
        f.debug_struct("AiderLocalSettings")
            .field("base_url", &self.base_url)
            .field("auth_token", &redacted)
            .field("model", &self.model)
            .finish()
    }
}

impl Default for AiderLocalSettings {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434/v1".to_string(),
            auth_token: "ollama".to_string(),
            model: String::new(),
        }
    }
}

/// V8-01: local task-offload configuration. ccImp runs a user-supplied
/// `llama-server` (the single source of truth is `server_command`) and
/// exposes an `offload_task` MCP tool into ccImp-launched Claude tabs so
/// the main Opus session can delegate token-heavy subtasks (deep search,
/// large-file/log summarization, web research) to the local model and
/// receive only the synthesized result. Off by default (`enabled` and
/// `autostart` both false): the feature spawns a multi-GB server and is
/// useless without a configured command, so it is opt-in.
///
/// Additive `#[serde(default)]` block — old settings files round-trip
/// with the feature disabled. No schema-version bump.
///
/// The `Debug` impl is hand-rolled to redact any secrets carried in the
/// configured MCP servers' `env` maps, mirroring `ClaudeLocalSettings`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OffloadSettings {
    /// Master switch. Off = no server, no `--mcp-config` injection into
    /// Claude tabs, `offload_task` not exposed, no Offload Server tab.
    pub enabled: bool,
    /// When `enabled`, spawn `llama-server` at app launch and keep it
    /// warm. When off, the server starts lazily on the first
    /// `offload_task` (or via a manual Start) so a ~20 GB load never
    /// blocks launch.
    pub autostart: bool,
    /// Inject the system-prompt addendum that tells Opus *when* to
    /// offload. Default true — without the nudge Opus won't reach for
    /// the tool. Routed through the same `--append-system-prompt`
    /// mechanism as the TTS markup convention.
    pub inject_guidance: bool,
    /// The single source-of-truth `llama-server` command, e.g.
    /// `llama-server --model …\Qwen3.6-35B-A3B-Q4.gguf --port 8080
    /// --jinja -ngl 99 --ctx-size 150000 --flash-attn`. ccImp
    /// `shlex`-parses it to spawn, parses host/port + `-np` to know
    /// where to connect and how many slots exist, and validates
    /// `--jinja` is present (tool-calling needs it). ccImp never
    /// silently mutates the command. Empty on a fresh install.
    pub server_command: String,
    /// Native baseline tool on/off toggles (`read_file`, `code_search`,
    /// `run_command`).
    pub tools: OffloadToolToggles,
    /// Roots the native `code_search`/`read_file` tools and the
    /// `filesystem` MCP server are confined to. Empty default → the loop
    /// resolves it to the launch project root at call time.
    pub allowed_roots: Vec<PathBuf>,
    /// Commands the native `run_command` tool may execute. Deny by
    /// default (empty list = nothing runnable). Matched against the
    /// command's program name.
    pub command_allowlist: Vec<String>,
    /// User-installed MCP tool servers aggregated by ccImp's MCP host
    /// and exposed to the local model as OpenAI tools. Mirrors Claude's
    /// own `mcpServers` config shape so users can paste familiar config.
    pub mcp_servers: Vec<McpServerConfig>,
    /// V8-02: the offload backend pool. One entry per backend (Local
    /// `llama-server`, Remote-LAN, or Remote-cloud), each with its own
    /// capabilities, tier, and tool scope. The router picks one per
    /// `offload_task`. Empty on a fresh install / pre-V8-02 file — the
    /// v1.16→v1.17 migration (and [`Self::effective_backends`] at
    /// runtime) synthesizes one Local entry from the legacy
    /// `server_command`/`autostart` fields so single-local setups keep
    /// working unchanged.
    pub backends: Vec<OffloadBackend>,
    /// Fraction (0–100) of the per-slot window the loop works against,
    /// reserving the rest for reasoning + the final answer (~80%).
    pub budget_high_water_pct: u8,
    /// Hard cap on every tool result (native *and* MCP) in tokens; the
    /// loop appends a truncation marker past this so the model knows it
    /// was cut and paginates instead of assuming full coverage (~8k).
    pub per_tool_result_token_cap: u32,
    /// Maximum agent-loop iterations before a forced final-synthesis
    /// turn.
    pub max_steps: u32,
    /// Per-task wall-clock bound (seconds), *including* queue-wait for a
    /// concurrency slot. On expiry the loop is cancelled and a clear
    /// timeout result is returned to Claude rather than hanging the tool
    /// call.
    pub offload_timeout_secs: u64,
    /// V8-03: global cap on offloads in flight across the whole app (all
    /// Claude tabs, all backends). `None` (default) lets the
    /// [`OffloadService`] size it from config — the summed per-backend slot
    /// counts, clamped to a sane ceiling. A queue forms past this so a busy
    /// pool stays coherent and the router's spill/fail-over runs on honest
    /// `in_flight`.
    ///
    /// [`OffloadService`]: crate::offload::OffloadService
    pub global_concurrency: Option<u32>,
}

impl std::fmt::Debug for OffloadSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OffloadSettings")
            .field("enabled", &self.enabled)
            .field("autostart", &self.autostart)
            .field("inject_guidance", &self.inject_guidance)
            .field("server_command", &self.server_command)
            .field("tools", &self.tools)
            .field("allowed_roots", &self.allowed_roots)
            .field("command_allowlist", &self.command_allowlist)
            // McpServerConfig has its own redacted Debug.
            .field("mcp_servers", &self.mcp_servers)
            // OffloadBackend redacts the Remote `auth_token`.
            .field("backends", &self.backends)
            .field("budget_high_water_pct", &self.budget_high_water_pct)
            .field("per_tool_result_token_cap", &self.per_tool_result_token_cap)
            .field("max_steps", &self.max_steps)
            .field("offload_timeout_secs", &self.offload_timeout_secs)
            .field("global_concurrency", &self.global_concurrency)
            .finish()
    }
}

impl Default for OffloadSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            autostart: false,
            inject_guidance: true,
            server_command: String::new(),
            tools: OffloadToolToggles::default(),
            allowed_roots: Vec::new(),
            command_allowlist: Vec::new(),
            mcp_servers: Vec::new(),
            backends: Vec::new(),
            budget_high_water_pct: 80,
            per_tool_result_token_cap: 8000,
            max_steps: 16,
            offload_timeout_secs: 300,
            global_concurrency: None,
        }
    }
}

/// On/off toggles for the native baseline offload tools (built into
/// ccImp, zero external deps). All default on so offload works with no
/// MCP servers installed.
#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct OffloadToolToggles {
    /// Bounded line/byte reads within an `allowed_root`.
    pub read_file: bool,
    /// `grep`/`glob` across an `allowed_root`; matching paths + snippets.
    pub code_search: bool,
    /// Allowlisted, read-only command execution. Default true, but inert
    /// until `command_allowlist` is populated (deny-by-default).
    pub run_command: bool,
}

impl Default for OffloadToolToggles {
    fn default() -> Self {
        Self {
            read_file: true,
            code_search: true,
            run_command: true,
        }
    }
}

/// One user-installed MCP tool server, aggregated by ccImp's MCP host.
/// Mirrors Claude Code's own `mcpServers` entry shape: either a stdio
/// server (`command` + `args` + `env`) or an HTTP server (`url`). Only
/// read-class tools from each server are exposed this milestone.
///
/// The hand-rolled `Debug` redacts `env` values, which may carry API
/// keys, so a stray `?settings` log line cannot leak them.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// Display + namespacing prefix (e.g. `ddg`, `git`). Tools are
    /// exposed to the model as `<name>__<tool>`.
    pub name: String,
    /// Stdio transport: the server executable. Empty when `url` is set.
    pub command: String,
    /// Args for the stdio `command`.
    pub args: Vec<String>,
    /// Extra env for the stdio `command` (may carry secrets — redacted
    /// in `Debug`).
    pub env: HashMap<String, String>,
    /// HTTP transport: the server base URL. Empty when `command` is set.
    pub url: String,
    /// Per-server enable toggle.
    pub enabled: bool,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env_keys: Vec<&String> = self.env.keys().collect();
        f.debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("args", &self.args)
            // Redact values; show only which keys are present.
            .field("env_keys", &env_keys)
            .field("url", &self.url)
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            url: String::new(),
            enabled: true,
        }
    }
}

/// V8-02: native + MCP-server tool names treated as **local-data** tools —
/// they read the user's files / run commands / query the local repo, so
/// their output must never leave the machine. The router refuses to send a
/// task needing any of these to a cloud backend, and a cloud backend's
/// default [`ToolScope`] denies them. (MCP servers are matched by their
/// configured `name`; native tools by their fixed name.)
pub const LOCAL_DATA_TOOLS: &[&str] = &[
    "read_file",
    "code_search",
    "run_command",
    "filesystem",
    "git",
];

/// V8-02: web/docs tool names a cloud backend is allowed by default — they
/// reach out to the public internet, not the user's machine.
pub const WEB_DOCS_TOOLS: &[&str] = &["duckduckgo", "fetch", "context7"];

/// V8-02: which capability tier a backend serves. The router prefers a
/// `Fast` backend for trivial single-pass work and a `Quality` backend for
/// real reasoning; Claude's `tier` hint on `offload_task` biases the choice.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendTier {
    /// Small/fast backend (e.g. an 8 GB LAN box): trivial, single-pass,
    /// small-context offloads only.
    Fast,
    /// Large/capable backend (the main model): real reasoning, big context,
    /// multi-tool loops. The default when unspecified.
    #[default]
    Quality,
}

/// V8-02: a backend's allow-list over the global tool pool (native tools +
/// configured MCP-server names). Only allowed tools are placed in the
/// `tools` array sent to that backend's model — the privacy boundary for
/// cloud backends.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum ToolScope {
    /// Every tool in the pool (Local + trusted LAN default).
    All,
    /// Only the named tools.
    Only { tools: Vec<String> },
    /// Every tool except the named ones (cloud default = `AllExcept` the
    /// local-data set).
    AllExcept { tools: Vec<String> },
}

impl Default for ToolScope {
    fn default() -> Self {
        ToolScope::All
    }
}

impl ToolScope {
    /// Whether `tool` (a native tool name or an MCP-server name) is allowed
    /// under this scope. Matching is exact; MCP tools namespaced as
    /// `<server>__<tool>` are tested by their server prefix via
    /// [`Self::allows_namespaced`].
    pub fn allows(&self, tool: &str) -> bool {
        match self {
            ToolScope::All => true,
            ToolScope::Only { tools } => tools.iter().any(|t| t == tool),
            ToolScope::AllExcept { tools } => !tools.iter().any(|t| t == tool),
        }
    }

    /// Whether a (possibly namespaced) tool id is allowed. An MCP tool is
    /// exposed to the model as `<server>__<tool>`; scopes are written in
    /// terms of native tool names and MCP *server* names, so we test the
    /// server prefix for namespaced ids and the whole id otherwise.
    pub fn allows_namespaced(&self, tool_id: &str) -> bool {
        let key = tool_id.split("__").next().unwrap_or(tool_id);
        self.allows(key)
    }

    /// The default scope for a backend of the given kind: Local and trusted
    /// LAN get everything; a cloud backend gets web/docs only (the
    /// local-data set denied) until the user explicitly opts in. The
    /// Settings UI applies this when a backend's cloud flag is toggled; the
    /// router/tests exercise it as the canonical default.
    #[allow(dead_code)]
    pub fn default_for(kind_is_cloud: bool) -> Self {
        if kind_is_cloud {
            ToolScope::AllExcept {
                tools: LOCAL_DATA_TOOLS.iter().map(|s| s.to_string()).collect(),
            }
        } else {
            ToolScope::All
        }
    }
}

/// V8-02: kind-specific configuration for one offload backend.
///
/// `Local` mirrors V8-01's single-server config (the command ccImp owns +
/// spawns as a read-only tab). `Remote` is a `base_url` ccImp only
/// health-checks and connects to — no process, no tab. The hand-rolled
/// `Debug` on [`OffloadBackend`] redacts the Remote `auth_token`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OffloadBackendKind {
    /// ccImp owns the process: the V8-01 `server_command` + `autostart` +
    /// read-only Offload Server tab + Start/Stop/Reset.
    Local {
        /// The single source-of-truth `llama-server` command (shlex-parsed
        /// to spawn; host/port/`-np`/`--jinja` parsed from it).
        server_command: String,
        /// Spawn at app launch and keep warm (else lazy on first offload).
        autostart: bool,
    },
    /// ccImp holds a `base_url` (+ optional auth) and health-checks it; it
    /// cannot start/stop the process. A LAN box or a cloud API.
    Remote {
        /// HTTP origin of the OpenAI-compatible endpoint, e.g.
        /// `http://192.168.1.50:8080` or `https://api.example.com/v1`.
        base_url: String,
        /// Optional bearer token (cloud APIs). Redacted in `Debug`.
        auth_token: String,
        /// Marks a backend whose data leaves the user's machine/network.
        /// Gates the consent toggle, the distinct UI badge, and the
        /// default web/docs-only tool scope.
        is_cloud: bool,
        /// Explicit acknowledgement that offloading here sends task text
        /// (and any local-data tool results, if scoped in) to a third
        /// party. A cloud backend is unusable until this is `true`.
        cloud_consent: bool,
    },
}

/// V8-02: one backend in the offload pool.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OffloadBackend {
    /// Display + routing-log name (e.g. `main`, `lan-3070`, `cloud`).
    pub name: String,
    /// Per-backend enable toggle. Disabled backends are invisible to the
    /// router and the capability union.
    pub enabled: bool,
    /// Local (owned process) vs. Remote (health-checked URL).
    pub kind: OffloadBackendKind,
    /// Context window to assume when `/props` is unavailable (many cloud
    /// APIs don't expose it). Ignored for endpoints that report `n_ctx`.
    pub declared_context: Option<u32>,
    /// Model label to show when `/props` is unavailable (cosmetic).
    pub declared_model: String,
    /// Which tier this backend serves (router bias).
    pub tier: BackendTier,
    /// This backend's allow-list over the global tool pool.
    pub tool_scope: ToolScope,
}

impl std::fmt::Debug for OffloadBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the Remote auth_token so a stray `?settings` log can't leak it.
        let kind_dbg: String = match &self.kind {
            OffloadBackendKind::Local {
                server_command,
                autostart,
            } => format!("Local {{ server_command: {server_command:?}, autostart: {autostart} }}"),
            OffloadBackendKind::Remote {
                base_url,
                auth_token,
                is_cloud,
                cloud_consent,
            } => format!(
                "Remote {{ base_url: {base_url:?}, auth_token: {}, is_cloud: {is_cloud}, cloud_consent: {cloud_consent} }}",
                if auth_token.is_empty() { "<none>" } else { "<redacted>" }
            ),
        };
        f.debug_struct("OffloadBackend")
            .field("name", &self.name)
            .field("enabled", &self.enabled)
            .field("kind", &format_args!("{kind_dbg}"))
            .field("declared_context", &self.declared_context)
            .field("declared_model", &self.declared_model)
            .field("tier", &self.tier)
            .field("tool_scope", &self.tool_scope)
            .finish()
    }
}

impl Default for OffloadBackendKind {
    fn default() -> Self {
        OffloadBackendKind::Local {
            server_command: String::new(),
            autostart: false,
        }
    }
}

impl Default for OffloadBackend {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            kind: OffloadBackendKind::default(),
            declared_context: None,
            declared_model: String::new(),
            tier: BackendTier::Quality,
            tool_scope: ToolScope::All,
        }
    }
}

impl OffloadBackend {
    /// A cloud backend that hasn't been consented to is not usable: the
    /// router skips it and the UI flags it. Non-cloud backends are always
    /// "consented".
    pub fn cloud_blocked(&self) -> bool {
        matches!(
            self.kind,
            OffloadBackendKind::Remote {
                is_cloud: true,
                cloud_consent: false,
                ..
            }
        )
    }
}

impl OffloadSettings {
    /// The effective backend pool. Returns the configured [`backends`]
    /// when non-empty; otherwise synthesizes a single Local backend from
    /// the legacy `server_command`/`autostart` fields so a V8-01 config
    /// (or one whose migration hasn't run) keeps working as one
    /// quality-tier, all-tools local backend.
    ///
    /// [`backends`]: Self::backends
    pub fn effective_backends(&self) -> Vec<OffloadBackend> {
        if !self.backends.is_empty() {
            return self.backends.clone();
        }
        vec![OffloadBackend {
            name: "local".to_string(),
            enabled: true,
            kind: OffloadBackendKind::Local {
                server_command: self.server_command.clone(),
                autostart: self.autostart,
            },
            declared_context: None,
            declared_model: String::new(),
            tier: BackendTier::Quality,
            tool_scope: ToolScope::All,
        }]
    }
}

/// One notification slot: a per-event `{ enabled, text }` pair. The
/// firing path requires both `enabled == true` AND a non-empty `text`
/// to dispatch — the empty-text suppression matches the pre-v1.11
/// convention so users who hand-edit a slot to `""` still see it
/// disabled.
///
/// Custom `Deserialize` accepts either a bare string (the v1.10-and-
/// earlier shape — empty string maps to `enabled: false`, non-empty to
/// `enabled: true`) or the v1.11 object shape, so a legacy file loads
/// without losing the user's text. On next save the file is rewritten
/// in the new shape.
#[derive(Clone, Serialize, Debug, Default)]
pub struct NotificationSlot {
    pub enabled: bool,
    pub text: String,
}

impl NotificationSlot {
    /// A configured-and-enabled slot. Constructor for the builtin tab
    /// defaults so the call sites stay terse.
    pub fn enabled(text: impl Into<String>) -> Self {
        Self {
            enabled: true,
            text: text.into(),
        }
    }
}

impl<'de> Deserialize<'de> for NotificationSlot {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        match v {
            // v1.10-and-earlier shape: bare string. Empty string was the
            // documented "leave blank to disable" convention, so map it
            // to `enabled: false`. Non-empty maps to `enabled: true` so
            // the upgrade path preserves prior firing behavior.
            serde_json::Value::String(s) => Ok(Self {
                enabled: !s.is_empty(),
                text: s,
            }),
            serde_json::Value::Object(_) => {
                #[derive(Deserialize)]
                struct Inner {
                    #[serde(default = "default_true")]
                    enabled: bool,
                    #[serde(default)]
                    text: String,
                }
                fn default_true() -> bool {
                    true
                }
                let inner: Inner = serde_json::from_value(v).map_err(D::Error::custom)?;
                Ok(Self {
                    enabled: inner.enabled,
                    text: inner.text,
                })
            }
            serde_json::Value::Null => Ok(Self::default()),
            _ => Err(D::Error::custom(
                "notification slot: expected string or { enabled, text } object",
            )),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct AiNotificationConfig {
    pub idle: NotificationSlot,
    pub awaiting_permission: NotificationSlot,
    /// Spoken when a `kind: question` pattern fires (AskUserQuestion-style
    /// multi-option prompts). Older settings files that pre-date this
    /// field deserialize to a default-disabled slot via
    /// `#[serde(default)]`; the integrity check at load doesn't backfill
    /// it, so users on the two AI builtins get the configured-defaults
    /// experience only on fresh installs. (See `default_claude_tab` and
    /// `default_claude_local_tab`.)
    pub question: NotificationSlot,
    pub error: NotificationSlot,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ShellNotificationConfig {
    pub error: NotificationSlot,
    /// `{code}` placeholder is interpolated with the actual exit code in M4.
    pub exited: NotificationSlot,
}

impl Default for ShellNotificationConfig {
    fn default() -> Self {
        Self {
            error: NotificationSlot::enabled("Shell encountered an error"),
            exited: NotificationSlot::enabled("Shell exited (code {code})"),
        }
    }
}

// --- Builtin defaults -------------------------------------------------------
//
// Used by:
//   1. The migration step to fill in missing entries (e.g. a claude-local
//      tab absent from an upgraded settings file).
//   2. The integrity check at load time to restore deleted builtins.
//   3. `Settings::default()` to seed a fresh-install file before the first
//      save.

pub fn default_claude_tab() -> TabConfig {
    TabConfig::AiTool(AiToolTabConfig {
        id: CLAUDE_TAB_ID.to_string(),
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
            idle: NotificationSlot::enabled("Claude is idle"),
            awaiting_permission: NotificationSlot::enabled("Claude is awaiting permission"),
            question: NotificationSlot::enabled("Claude has a question"),
            error: NotificationSlot::enabled("Claude encountered an error"),
        },
        // Pre-dismissed so the overlay code can use a single per-tab
        // predicate. Aider used to fire a first-launch banner (V1.1)
        // but Claude tabs never did.
        first_launch_notice_dismissed: true,
        theme_override: None,
        background_override: None,
        use_local_provider: false,
        tts_all_output: false,
    })
}

/// V1.4-07: second Claude tab, preconfigured to talk to a local LLM
/// via the global `claude_local` provider settings. Replaces the
/// pre-V1.4-07 Aider builtin tab.
pub fn default_claude_local_tab() -> TabConfig {
    TabConfig::AiTool(AiToolTabConfig {
        id: CLAUDE_LOCAL_TAB_ID.to_string(),
        builtin: true,
        name: "Claude (local)".to_string(),
        command: "claude".to_string(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        tts_injection: TtsInjection {
            enabled: true,
            instructions: crate::tts::RUNTIME_SYSTEM_PROMPT.to_string(),
        },
        notifications: AiNotificationConfig {
            idle: NotificationSlot::enabled("Claude (local) is idle"),
            awaiting_permission: NotificationSlot::enabled(
                "Claude (local) is awaiting permission",
            ),
            question: NotificationSlot::enabled("Claude (local) has a question"),
            error: NotificationSlot::enabled("Claude (local) encountered an error"),
        },
        first_launch_notice_dismissed: true,
        theme_override: None,
        background_override: None,
        use_local_provider: true,
        tts_all_output: false,
    })
}

/// V14: Aider AI-tool tab using whatever provider Aider's own config
/// selects (cloud / API keys / per-project `.aider.conf.yml`). ccimp
/// does not synthesize provider env vars for this tab — the user's
/// existing aider configuration is in charge. TTS prompt injection is
/// disabled by default because Aider's CLI has no
/// `--append-system-prompt` equivalent (the spawn path enforces this
/// regardless of the toggle).
pub fn default_aider_tab() -> TabConfig {
    TabConfig::AiTool(AiToolTabConfig {
        id: AIDER_TAB_ID.to_string(),
        builtin: true,
        name: "Aider".to_string(),
        command: "aider".to_string(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        tts_injection: TtsInjection::default(),
        notifications: AiNotificationConfig {
            idle: NotificationSlot::enabled("Aider is idle"),
            awaiting_permission: NotificationSlot::enabled("Aider is awaiting permission"),
            question: NotificationSlot::enabled("Aider has a question"),
            error: NotificationSlot::enabled("Aider encountered an error"),
        },
        first_launch_notice_dismissed: true,
        theme_override: None,
        background_override: None,
        use_local_provider: false,
        tts_all_output: false,
    })
}

/// V14: second Aider tab preconfigured to use a local OpenAI-compatible
/// LLM via the global `aider_local` provider settings.
pub fn default_aider_local_tab() -> TabConfig {
    TabConfig::AiTool(AiToolTabConfig {
        id: AIDER_LOCAL_TAB_ID.to_string(),
        builtin: true,
        name: "Aider (local)".to_string(),
        command: "aider".to_string(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        tts_injection: TtsInjection::default(),
        notifications: AiNotificationConfig {
            idle: NotificationSlot::enabled("Aider (local) is idle"),
            awaiting_permission: NotificationSlot::enabled(
                "Aider (local) is awaiting permission",
            ),
            question: NotificationSlot::enabled("Aider (local) has a question"),
            error: NotificationSlot::enabled("Aider (local) encountered an error"),
        },
        first_launch_notice_dismissed: true,
        theme_override: None,
        background_override: None,
        use_local_provider: true,
        tts_all_output: false,
    })
}

/// Look up the default `TabConfig` for a reserved AI tab id. Used by
/// the integrity check and the lifecycle IPC when materializing a tab
/// the user just enabled.
pub fn default_ai_tab(id: AiTabId) -> TabConfig {
    match id {
        AiTabId::Claude => default_claude_tab(),
        AiTabId::ClaudeLocal => default_claude_local_tab(),
        AiTabId::Aider => default_aider_tab(),
        AiTabId::AiderLocal => default_aider_local_tab(),
    }
}

/// V8-03: the read-only Offload Server tab config. A Shell-kind tab with the
/// reserved id and `builtin: true` so the close `×` is suppressed and
/// `close_tab` refuses it. `command`/`args` carry the parsed `llama-server`
/// program for display only — the tab spawns no PTY (the frontend renders its
/// content from the live `offload-server-output` stream), so they are never
/// executed here. Materialized/removed by the integrity check per
/// `offload.enabled`.
pub fn default_offload_server_tab() -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: OFFLOAD_SERVER_TAB_ID.to_string(),
        builtin: true,
        name: "Offload Server".to_string(),
        command: String::new(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
    })
}

/// Default Shell-1 entry. Takes the resolved platform default shell so the
/// `command` and `args` fields land on the right binary for the host. The
/// reserved id is just the seed value for the first shell tab on a fresh
/// install — it's a regular closable shell, not a builtin.
pub fn default_shell_1_tab(default_shell: &ShellSpec) -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: SHELL_DEFAULT_TAB_ID.to_string(),
        builtin: false,
        name: "Shell 1".to_string(),
        command: default_shell.command.to_string_lossy().into_owned(),
        args: default_shell.args.clone(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
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
    /// Read-along highlight shown while a Ctrl+right-click selection is being
    /// spoken. Optional/back-compat via `#[serde(default)]`.
    pub selection_highlight: SelectionHighlightSettings,
    /// Show the play / pause / restart / stop transport for selection TTS in
    /// the bottom status bar.
    pub show_selection_controls: bool,
}

impl Default for TtsSettings {
    fn default() -> Self {
        Self {
            voice: "af_heart".to_string(),
            speed: 1.0,
            volume: 1.0,
            mute: false,
            selection_highlight: SelectionHighlightSettings::default(),
            show_selection_controls: true,
        }
    }
}

/// V6-01 offline speech-to-text (dictation). Captures microphone audio and
/// transcribes it with a bundled Whisper model (whisper.cpp), dropping the
/// transcript into the compose overlay. Enabled by default; the feature still
/// needs a `ggml-*.bin` model present under `models/` to actually transcribe.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct SttSettings {
    /// Master enable for the whole STT feature (record button + PTT).
    pub enabled: bool,
    /// GGML model filename under `models/` (e.g. "ggml-small.bin").
    pub model_file: String,
    /// Whisper language hint. "auto" = detect; "en", "he", … force a language.
    pub language: String,
    /// cpal input device name; empty = system default input device.
    pub input_device: String,
    /// Bottom-bar record button behavior (click-to-toggle vs press-and-hold).
    pub button_mode: SttButtonMode,
    /// Translate non-English speech to English instead of transcribing
    /// verbatim (Whisper's translate task).
    pub translate_to_english: bool,
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SttButtonMode {
    Toggle,
    Hold,
}

impl Default for SttSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            model_file: "ggml-small.bin".to_string(),
            language: "auto".to_string(),
            input_device: String::new(),
            button_mode: SttButtonMode::Toggle,
            translate_to_english: false,
        }
    }
}

/// Colors for the Ctrl+right-click read-along highlight. The whole selection
/// is painted with the `unread_*` colors when a read starts; the sentence
/// currently being spoken uses the `reading_*` accent; each sentence reverts
/// to its original terminal colors as it finishes. xterm decorations only
/// accept `#RRGGBB` hex, so these must be 6-digit hex strings.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct SelectionHighlightSettings {
    /// Master toggle. When false, the gesture still speaks the selection
    /// (chunked, so large/multi-line selections work) but paints no highlight.
    pub enabled: bool,
    /// Foreground/background for not-yet-read sentences. Each `*_custom`
    /// flag chooses between the custom color below (true) and leaving that
    /// channel as the terminal's own palette color (false) — so the user can
    /// tint just the background, just the text, both, or neither.
    pub unread_fg: String,
    pub unread_fg_custom: bool,
    pub unread_bg: String,
    pub unread_bg_custom: bool,
    /// Foreground/background for the sentence currently being spoken.
    pub reading_fg: String,
    pub reading_fg_custom: bool,
    pub reading_bg: String,
    pub reading_bg_custom: bool,
}

impl Default for SelectionHighlightSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            unread_fg: "#000000".to_string(),
            unread_fg_custom: true,
            unread_bg: "#ff5555".to_string(),
            unread_bg_custom: true,
            reading_fg: "#000000".to_string(),
            reading_fg_custom: true,
            reading_bg: "#f1fa8c".to_string(),
            reading_bg_custom: true,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AvatarSettings {
    pub visible: bool,
    /// Which renderer drives the avatar. `Media` (default) shows the
    /// per-state image/video files in `images`; `Sprite` ignores `images`
    /// and instead plays a manifest-driven frame animation from the sprite
    /// set named in `sprite`. The two are mutually exclusive render paths in
    /// the frontend — see `AvatarOverlay.svelte`.
    pub kind: AvatarKind,
    pub size: AvatarSize,
    pub position: AvatarPosition,
    pub margin: AvatarMargin,
    pub opacity: f32,
    /// Draw the 1px frame (in the waveform color) around the avatar box.
    /// Defaults off — the sprite mascot reads better borderless. Applies to
    /// both render kinds; toggled by the "Show border" checkbox in Settings.
    pub show_border: bool,
    pub images: AvatarImages,
    /// Sprite-renderer configuration. Only consulted when `kind == Sprite`.
    pub sprite: SpriteSettings,
    pub transition: TransitionSettings,
    pub waveform: WaveformSettings,
}

impl Default for AvatarSettings {
    fn default() -> Self {
        // Size/margin/opacity are set as literals here (rather than via the
        // field structs' own `Default`) so the v1.11→v1.12 margin migration —
        // which relies on `AvatarMargin::default()` staying 16/16 for legacy
        // files — is unaffected while fresh installs get the tuned sprite look.
        Self {
            visible: true,
            kind: AvatarKind::default(),
            size: AvatarSize {
                width_px: 140,
                height_px: 140,
            },
            position: AvatarPosition::TopRight,
            margin: AvatarMargin { x_px: 21, y_px: 0 },
            opacity: 1.0,
            show_border: false,
            images: AvatarImages::default(),
            sprite: SpriteSettings::default(),
            transition: TransitionSettings::default(),
            waveform: WaveformSettings::default(),
        }
    }
}

/// Avatar render mode. `Media` is the original image/video-per-state
/// renderer; `Sprite` is the pixel-art frame-animation renderer driven by a
/// `manifest.json` under `<root>/sprites/<set>/`.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AvatarKind {
    Media,
    /// Default render mode: the animated pixel-art mascot, paired with the
    /// default `tui-red` theme. The default set is `impSprites` (the imp).
    #[default]
    Sprite,
}

/// Sprite-renderer settings. `set` names a folder under the bundled
/// `<root>/sprites/` tree (served to the WebView at `/sprites/<set>/`) that
/// contains a `manifest.json` plus its frame subfolders. Kept as a plain
/// name (not a path) so new sets can be dropped in alongside `impSprites`
/// (default) and `claudeSprites` without a schema change; the frontend maps
/// the name to a URL.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct SpriteSettings {
    pub set: String,
}

impl Default for SpriteSettings {
    fn default() -> Self {
        Self {
            set: "impSprites".to_string(),
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

/// Per-axis offset from the screen edge defined by `AvatarPosition`. The
/// X component pushes the avatar inward from the left/right edge; the Y
/// component pushes it inward from the top/bottom. Replaces the pre-v1.12
/// scalar `margin_px` field, which applied a single value to both axes.
#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AvatarMargin {
    pub x_px: u32,
    pub y_px: u32,
}

impl Default for AvatarMargin {
    fn default() -> Self {
        Self { x_px: 16, y_px: 16 }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AvatarPosition {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
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
    /// Hex color the waveform stroke and the avatar's outline use. Empty
    /// string means "follow the active UI theme" — the frontend resolves
    /// it from the `--waveform-color` CSS variable in `theme.css`. A
    /// non-empty value is a user override that wins regardless of theme.
    /// Render the audio waveform over the avatar. When false the waveform
    /// canvas is hidden entirely (the avatar itself is unaffected). Toggled
    /// by the "Show waveform" checkbox in Settings → Waveform.
    pub visible: bool,
    pub color: String,
    pub line_width: f32,
    pub glow_intensity: f32,
    pub opacity: f32,
}

impl Default for WaveformSettings {
    fn default() -> Self {
        Self {
            visible: true,
            color: String::new(),
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
    pub show_tts_markup: bool,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            terminal_font_family: "Consolas, Menlo, \"DejaVu Sans Mono\", monospace"
                .to_string(),
            terminal_font_size: 14,
            show_tts_markup: false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct BehaviorSettings {
    pub auto_speak: bool,
    pub fallback_silent: bool,
    pub announcements_enabled: bool,
    /// When true, `tts.mute` follows `avatar.visible` — hiding the avatar
    /// auto-mutes, showing it auto-unmutes. The frontend handles the sync
    /// (App.svelte settings subscriber); the backend just persists the flag.
    pub follow_avatar: bool,
    /// When true, announcements (idle / awaiting-permission / error / exit)
    /// fire even for the tab the user is currently looking at. Default off
    /// preserves the historical "background-only" behavior — most users
    /// don't want to hear "awaiting permission" for the tab they're
    /// staring at.
    pub announce_focused_tab: bool,
    /// When true, tagged-content TTS (the `[[TTS]]…[[/TTS]]` segments
    /// produced by AI tabs) plays even when the originating tab is not
    /// the active one. Default off keeps the v2 behavior — only the
    /// foreground tab's TTS plays. Independent of announcement TTS,
    /// which is gated by `announce_focused_tab` and never dropped for
    /// background tabs.
    pub speak_background_tabs: bool,
    /// When true, text selected in any terminal is copied to the system
    /// clipboard automatically. Older settings files without this field
    /// deserialize to the default via serde-default — no migration bump.
    pub copy_on_select: bool,
    /// When true, a right-click inside any terminal pastes the system
    /// clipboard into the focused PTY (and suppresses the browser's
    /// default context menu). Backward-compatible via serde-default.
    pub paste_on_right_click: bool,
    /// When true, Ctrl+right-click inside any terminal reads the current
    /// selection aloud through TTS instead of pasting. The Ctrl modifier
    /// always suppresses the paste branch when this is on, so the gesture
    /// can never accidentally paste. Backward-compatible via serde-default.
    pub speak_selection_on_right_click: bool,
}

impl Default for BehaviorSettings {
    fn default() -> Self {
        Self {
            auto_speak: true,
            fallback_silent: true,
            announcements_enabled: true,
            follow_avatar: false,
            announce_focused_tab: false,
            speak_background_tabs: false,
            copy_on_select: true,
            paste_on_right_click: true,
            speak_selection_on_right_click: true,
        }
    }
}

/// Bottom-bar Claude Code usage tracker (session 5h + weekly 7d). The
/// per-element flags toggle individual pieces of each window's inline
/// readout; they apply to both windows. `enabled` gates the whole widget.
/// Backward-compatible via serde-default — no migration bump.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct UsageSettings {
    /// Overall on/off for the inline usage widget.
    pub enabled: bool,
    /// Show the proportional fill bar.
    pub show_bar: bool,
    /// Show the rounded utilization percentage.
    pub show_percentage: bool,
    /// Show the live countdown to reset.
    pub show_countdown: bool,
    /// Show the local reset clock time.
    pub show_reset_clock: bool,
    /// How often the frontend polls the usage endpoint, in seconds. The UI
    /// clamps this to a sane minimum so the undocumented endpoint isn't
    /// hammered.
    pub poll_interval_secs: u32,
}

impl Default for UsageSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            show_bar: true,
            show_percentage: true,
            show_countdown: true,
            show_reset_clock: true,
            poll_interval_secs: 60,
        }
    }
}

/// Bottom-bar system-monitor panel (CPU / memory / GPU / network), shown to
/// the right of the Claude usage meter. Backward-compatible via serde-default.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct SystemStatsSettings {
    /// Overall on/off for the system-monitor panel.
    pub enabled: bool,
    /// Poll cadence in seconds (the sparklines tick locally between polls).
    pub poll_interval_secs: u32,
    /// Per-component visibility. `show_gpu_temp` is a sub-toggle of `show_gpu`.
    pub show_cpu: bool,
    pub show_memory: bool,
    pub show_gpu: bool,
    pub show_gpu_temp: bool,
    pub show_network: bool,
}

impl Default for SystemStatsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_secs: 1,
            show_cpu: true,
            show_memory: true,
            show_gpu: true,
            show_gpu_temp: true,
            show_network: true,
        }
    }
}

/// Context-window status line config. When `enabled`, the AI launch path
/// injects a session-scoped `--settings` overlay into ccImp-launched
/// Claude Code tabs that points `statusLine.command` at `ccimp
/// --statusline` — our own renderer for a themed context-usage bar
/// (`Opus  ▓▓▓▓▓░░░░░ 50% (100k/200k)`). The overlay *merges* with the
/// user's own Claude Code settings (CLI flags outrank settings files and
/// only `statusLine` is set), so the user's global `~/.claude` config is
/// left untouched and the bar appears only inside ccImp.
///
/// Additive `#[serde(default)]` field — settings files written before this
/// landed round-trip with the bar enabled. Enabled by default, mirroring
/// the TTS/STT defaults; toggle off in Settings → Bottom bar.
#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct StatuslineSettings {
    pub enabled: bool,
}

impl Default for StatuslineSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct UiSettings {
    /// Active UI chrome theme. Three values currently ship, all ratatui-style
    /// (custom title bar, square borders): `"tui-red"` (the Imp Red palette +
    /// the imp's scarlet accent, #e23c3c), `"tui-orange"` (Gruvbox surfaces +
    /// Claude Code's accent orange, #d77757), and `"tui-green"` (the Aider
    /// Green palette + Aider's terminal green accent, #2eb82e). New installs
    /// land on `"tui-orange"` so the chrome accent matches Claude Code's
    /// orange. The avatar still defaults to the animated `impSprites` mascot
    /// independently (see [`AvatarKind`] / [`SpriteSettings`]).
    /// The pre-V1.13 `"tui"` value is rewritten to `"tui-orange"` by the
    /// v12 → v13 migration so existing users keep a Gruvbox look. Existing
    /// settings.json files otherwise keep whatever value they were
    /// persisted with.
    pub theme: String,
    /// Arrangement of the bottom status bar's movable left cluster. See
    /// [`StatusBarLayout`]. Added after the `theme` field; old files
    /// lacking the key deserialize to the default `[usage, system_stats]`
    /// via the struct-level `#[serde(default)]`.
    pub status_bar: StatusBarLayout,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: "tui-orange".to_string(),
            status_bar: StatusBarLayout::default(),
        }
    }
}

/// A display panel in the status bar's movable left cluster: `usage` =
/// Claude session meter, `system_stats` = CPU/GPU/network panel.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusBarComponent {
    Usage,
    SystemStats,
}

/// One slot in the movable cluster: a component plus the leading gap (in
/// px) before it. The gap is grown/shrunk by dragging the panel left or
/// right — it "stays where you drop it" — and is reset to 0 for every
/// slot whenever the component order changes.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct StatusBarSlot {
    pub component: StatusBarComponent,
    #[serde(default)]
    pub gap: u32,
}

/// Persisted left-to-right arrangement of the status bar's movable
/// cluster. The frontend normalizes on read so `usage` and
/// `system_stats` each appear exactly once regardless of what's on disk.
/// Reordered and spaced by dragging the panels in the bar; reset from
/// Settings → Bottom bar.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct StatusBarLayout {
    pub items: Vec<StatusBarSlot>,
}

impl Default for StatusBarLayout {
    fn default() -> Self {
        Self {
            items: vec![
                StatusBarSlot {
                    component: StatusBarComponent::Usage,
                    gap: 0,
                },
                StatusBarSlot {
                    component: StatusBarComponent::SystemStats,
                    gap: 0,
                },
            ],
        }
    }
}

/// Terminal-pane settings (V1.4-01+). Holds the xterm.js palette config
/// (V1.4-01) and the background image / solid-color sub-group (V1.4-02).
/// Distinct from `ui`, which themes the ccimp chrome.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct TerminalSettings {
    pub theme: TerminalThemeSettings,
    /// V1.4-02 background configuration. Image, color, and the
    /// opacity/blur/size/position controls that apply only when an
    /// image is set. See `MILESTONE-V1.4-02-terminal-background.md`
    /// for the four-cell rendering matrix and the three-state
    /// override semantics.
    pub background: TerminalBackgroundSettings,
    /// V1.4-04 D: cross-restart scrollback buffer. PTY output is
    /// captured into a per-tab ring buffer (`ring_bytes` cap, 256 KB
    /// default), persisted to disk on graceful exit, and replayed on
    /// next launch.
    pub scrollback: ScrollbackSettings,
}

/// Terminal palette setting. `name` is either a bundled theme name
/// (e.g., "Default", "Dracula") or "Custom" — in which case `custom`
/// carries the user's chosen 22-color override map.
///
/// The `custom` block is kept untyped on the Rust side because its
/// values are pure data the frontend forwards to xterm.js. Missing or
/// malformed keys are tolerated by the resolver, which merges the
/// custom map over the bundled "Default" theme.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TerminalThemeSettings {
    pub name: String,
    pub custom: Option<HashMap<String, String>>,
}

impl Default for TerminalThemeSettings {
    fn default() -> Self {
        Self {
            // Paired with the default `tui-orange` UI theme (whose theme.json
            // points at the GitHub Dark palette). The frontend's theme picker
            // re-pairs this when the user switches UI theme.
            name: "GitHub Dark".to_string(),
            custom: None,
        }
    }
}

/// V1.4-02 terminal background configuration. `image` and `color` are
/// independent (sibling fields, not a discriminated union) — both can be
/// `Some` simultaneously, in which case `color` becomes the dimming-overlay
/// tint atop `image`. `opacity`, `blur`, `size`, and `position` apply only
/// when `image` is `Some`; when only `color` is set, the resolver
/// rewrites the theme background and the renderer stays on the canvas
/// fast path with no transparency cost.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TerminalBackgroundSettings {
    /// Absolute path to the background image. `None` means "no image".
    /// Invalid paths surface a settings error and resolve to the
    /// no-image rendering path; the `color` field, if set, still applies.
    pub image: Option<PathBuf>,
    /// Hex color (e.g., `"#1a2b3c"`). When `image` is `None`, this
    /// replaces the resolved theme's background field. When `image` is
    /// `Some`, this drives the dimming-overlay tint (default black if
    /// `None`).
    pub color: Option<String>,
    /// Image-mode dimming-overlay alpha. 0.0-1.0. Ignored when `image`
    /// is `None`.
    pub opacity: f32,
    /// Image-mode CSS `backdrop-filter: blur(...)` radius in pixels.
    /// Ignored when `image` is `None`.
    pub blur: u32,
    /// Image-mode CSS `background-size` strategy. Ignored when `image`
    /// is `None`.
    pub size: BackgroundSize,
    /// Image-mode CSS `background-position` value. Ignored when `image`
    /// is `None`.
    pub position: String,
    /// V1.4-04 A.1 snapshot cap. Number of scrollback rows to capture
    /// when `serializeAddon.serialize({ scrollback })` runs on a
    /// renderer-category flip. Bounds JS-heap allocation under
    /// long-scrollback (50k+ lines) edge cases. Existing v1.5 files
    /// without this field deserialize to the default via serde-default
    /// — no migration version bump.
    pub snapshot_lines: u32,
    /// V1.4-04 B: named presets the user has saved. The recursion is
    /// blocked by the sister-struct `BackgroundPresetConfig` (presets
    /// can't contain presets). Migration v1.5 → v1.6 stamps `[]`; older
    /// files deserialize to `[]` via serde-default. When a per-tab
    /// `BackgroundOverride::Custom(...)` round-trips, this field rides
    /// along as `[]` — that's harmless wire-format growth and avoided
    /// the wire-format break that switching `Custom` to wrap
    /// `BackgroundPresetConfig` would have caused.
    pub presets: Vec<BackgroundPreset>,
    /// V1.4-04 C.4: when `false`, per-tab Configure dialog edits that
    /// would flip renderer category (image ↔ no-image) are deferred to
    /// Save. In-place changes (color / opacity / blur / size /
    /// position / tint) preview live regardless. Default `true`. Older
    /// files deserialize to `true` via serde-default; the v1.6 → v1.7
    /// migration (Phase D) stamps it explicitly.
    pub preview_category_flips: bool,
}

impl Default for TerminalBackgroundSettings {
    fn default() -> Self {
        Self {
            image: None,
            color: None,
            opacity: 0.4,
            blur: 0,
            size: BackgroundSize::Cover,
            position: "center".to_string(),
            snapshot_lines: 2000,
            presets: Vec::new(),
            preview_category_flips: true,
        }
    }
}

/// V1.4-04 B: a saved preset. `name` is the user-facing label;
/// `config` carries the same fields as `TerminalBackgroundSettings`
/// minus the `presets` field itself (the sister-struct
/// `BackgroundPresetConfig` makes the "presets don't contain presets"
/// invariant structural rather than runtime-enforced).
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct BackgroundPreset {
    pub name: String,
    pub config: BackgroundPresetConfig,
}

/// V1.4-04 B: the payload of a `BackgroundPreset` — same fields as
/// `TerminalBackgroundSettings` except for the recursive `presets`
/// field. `From`/`Into` impls bridge the two so the editor UI can hand
/// either shape into `composeTheme` etc. The `BackgroundOverride::Custom`
/// variant deliberately stays wrapped around `TerminalBackgroundSettings`
/// rather than this struct — see the doc note on
/// `TerminalBackgroundSettings.presets`.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct BackgroundPresetConfig {
    pub image: Option<PathBuf>,
    pub color: Option<String>,
    pub opacity: f32,
    pub blur: u32,
    pub size: BackgroundSize,
    pub position: String,
    pub snapshot_lines: u32,
}

impl Default for BackgroundPresetConfig {
    fn default() -> Self {
        Self {
            image: None,
            color: None,
            opacity: 0.4,
            blur: 0,
            size: BackgroundSize::Cover,
            position: "center".to_string(),
            snapshot_lines: 2000,
        }
    }
}

impl From<&TerminalBackgroundSettings> for BackgroundPresetConfig {
    fn from(s: &TerminalBackgroundSettings) -> Self {
        Self {
            image: s.image.clone(),
            color: s.color.clone(),
            opacity: s.opacity,
            blur: s.blur,
            size: s.size,
            position: s.position.clone(),
            snapshot_lines: s.snapshot_lines,
        }
    }
}

impl From<BackgroundPresetConfig> for TerminalBackgroundSettings {
    fn from(p: BackgroundPresetConfig) -> Self {
        Self {
            image: p.image,
            color: p.color,
            opacity: p.opacity,
            blur: p.blur,
            size: p.size,
            position: p.position,
            snapshot_lines: p.snapshot_lines,
            presets: Vec::new(),
            // V1.4-04 C.4: a preset doesn't carry the dialog
            // preview-opt-out flag (it's a global UI behavior, not a
            // background-config attribute). Lifting a preset back into
            // a `TerminalBackgroundSettings` defaults to the same
            // value as `Default`.
            preview_category_flips: true,
        }
    }
}

/// CSS `background-size` strategy. `Tile` is mapped to
/// `background-repeat: repeat` + `background-size: auto` on the
/// frontend; `Cover` and `Contain` map directly to their CSS values.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundSize {
    #[default]
    Cover,
    Contain,
    Tile,
}

/// V1.4-02 three-state per-tab override. Encodes:
///   - `None` (the field is `null` on disk, or the override variant is
///     `None`): inherit the global `terminal.background`.
///   - `Some(BackgroundOverride::Disabled)` (`"disabled"` on disk):
///     opt out of any background — render with theme bg only,
///     ignoring both global image and global color.
///   - `Some(BackgroundOverride::Custom(cfg))` (full object on disk):
///     this tab's background config replaces the global one wholesale.
///
/// Custom (de)serialize because `serde(untagged)` can't express the
/// literal-string `"disabled"` cleanly alongside an object variant.
#[derive(Clone, Debug)]
pub enum BackgroundOverride {
    Disabled,
    Custom(TerminalBackgroundSettings),
}

impl Serialize for BackgroundOverride {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Disabled => s.serialize_str("disabled"),
            Self::Custom(c) => c.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for BackgroundOverride {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::String(ref s) if s == "disabled" => Ok(Self::Disabled),
            serde_json::Value::Object(_) => serde_json::from_value(v)
                .map(Self::Custom)
                .map_err(D::Error::custom),
            _ => Err(D::Error::custom(
                "background_override: expected \"disabled\" string or background config object",
            )),
        }
    }
}

/// V1.4-04 D: cross-restart scrollback configuration. The ring
/// buffer is per-tab and capped at `ring_bytes`. `persist` toggles
/// disk persistence on graceful exit (`tauri::RunEvent::ExitRequested`);
/// `restore_on_launch` toggles the read-back at next `pty_start`.
/// Both default `true`. Defaults match the milestone doc — 256 KB per
/// tab is roughly 600 lines of dense ANSI, enough for "what was I
/// doing yesterday" continuity without ballooning disk usage.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ScrollbackSettings {
    pub ring_bytes: usize,
    pub persist: bool,
    pub restore_on_launch: bool,
}

impl Default for ScrollbackSettings {
    fn default() -> Self {
        Self {
            ring_bytes: 262_144,
            persist: true,
            restore_on_launch: true,
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
    /// V6-01 push-to-talk (hold) dictation trigger. Default bare
    /// `Ctrl+Shift` (modifiers-only) — held to record, released to
    /// transcribe. The dispatcher's arm/debounce + abort-on-other-key
    /// state machine keeps this from firing on ordinary `Ctrl+Shift+<key>`
    /// chords. Optional so older settings files round-trip; the default
    /// supplies the binding when the key is absent.
    pub push_to_talk: Option<String>,
}

impl Default for ShortcutSettings {
    fn default() -> Self {
        Self {
            // V6-01: open_compose, split_pane_vertical, and close_pane were
            // moved off `Ctrl+Shift+…` so the bare-`Ctrl+Shift` push-to-talk
            // chord doesn't visibly arm/abort when these fire. New installs
            // get these defaults; existing settings files keep their bindings.
            open_compose: Some("Alt+Enter".to_string()),
            // V6-01: Enter submits (one-handed send for dictation); Alt+Enter
            // (and Shift+Enter) insert a newline — see ComposeOverlay.svelte.
            submit_compose: Some("Enter".to_string()),
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
            split_pane_vertical: Some("Alt+\\".to_string()),
            close_pane: Some("Ctrl+Alt+W".to_string()),
            push_to_talk: Some("Ctrl+Shift".to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn background_override_null_round_trips_as_none() {
        // Field-level: serializing/deserializing the Option<BackgroundOverride>
        // with `None` produces a JSON `null`.
        let none: Option<BackgroundOverride> = None;
        let v = serde_json::to_value(&none).unwrap();
        assert_eq!(v, Value::Null);
        let parsed: Option<BackgroundOverride> = serde_json::from_value(Value::Null).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn background_override_disabled_string_round_trips() {
        let disabled = Some(BackgroundOverride::Disabled);
        let v = serde_json::to_value(&disabled).unwrap();
        assert_eq!(v, json!("disabled"));
        let parsed: Option<BackgroundOverride> = serde_json::from_value(json!("disabled")).unwrap();
        assert!(matches!(parsed, Some(BackgroundOverride::Disabled)));
    }

    #[test]
    fn stt_settings_default_round_trips() {
        let s = SttSettings::default();
        let v = serde_json::to_value(&s).unwrap();
        let back: SttSettings = serde_json::from_value(v).unwrap();
        assert!(back.enabled);
        assert_eq!(back.model_file, "ggml-small.bin");
        assert_eq!(back.language, "auto");
        assert!(back.input_device.is_empty());
        assert_eq!(back.button_mode, SttButtonMode::Toggle);
        assert!(!back.translate_to_english);
    }

    #[test]
    fn settings_without_stt_block_loads_defaults() {
        // An old settings file lacking the `stt` field deserializes with the
        // additive `#[serde(default)]` SttSettings — no migration needed.
        let json = json!({ "schema_version": CURRENT_SCHEMA_VERSION });
        let s: Settings = serde_json::from_value(json).unwrap();
        assert!(s.stt.enabled);
        assert_eq!(s.stt.model_file, "ggml-small.bin");
        // The push_to_talk shortcut default is present too.
        assert_eq!(s.shortcuts.push_to_talk.as_deref(), Some("Ctrl+Shift"));
    }

    #[test]
    fn background_override_custom_object_round_trips() {
        let cfg = TerminalBackgroundSettings {
            image: Some(PathBuf::from("/tmp/bg.png")),
            color: Some("#1a2b3c".to_string()),
            opacity: 0.7,
            blur: 12,
            size: BackgroundSize::Contain,
            position: "top left".to_string(),
            snapshot_lines: 2000,
            presets: Vec::new(),
            preview_category_flips: true,
        };
        let custom = Some(BackgroundOverride::Custom(cfg.clone()));
        let v = serde_json::to_value(&custom).unwrap();
        assert!(v.is_object(), "custom override should serialize as an object");
        let parsed: Option<BackgroundOverride> = serde_json::from_value(v).unwrap();
        match parsed {
            Some(BackgroundOverride::Custom(out)) => {
                assert_eq!(out.image, cfg.image);
                assert_eq!(out.color, cfg.color);
                assert!((out.opacity - cfg.opacity).abs() < f32::EPSILON);
                assert_eq!(out.blur, cfg.blur);
                assert_eq!(out.size, cfg.size);
                assert_eq!(out.position, cfg.position);
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn background_override_rejects_garbage() {
        // Numbers, arrays, and arbitrary strings all fail with the
        // explicit error message — the only valid string is "disabled".
        for v in [json!(42), json!([1, 2, 3]), json!("random")] {
            let result: Result<BackgroundOverride, _> = serde_json::from_value(v);
            assert!(result.is_err(), "expected error for invalid override input");
        }
    }

    #[test]
    fn background_settings_default_matches_milestone_doc() {
        // Sanity check on the values the migration writes — opacity 0.4,
        // blur 0, size cover, position center, no image, no color.
        let d = TerminalBackgroundSettings::default();
        assert!(d.image.is_none());
        assert!(d.color.is_none());
        assert!((d.opacity - 0.4).abs() < f32::EPSILON);
        assert_eq!(d.blur, 0);
        assert_eq!(d.size, BackgroundSize::Cover);
        assert_eq!(d.position, "center");
        assert_eq!(d.snapshot_lines, 2000);
        assert!(d.presets.is_empty());
    }

    #[test]
    fn background_preset_config_round_trips() {
        let p = BackgroundPresetConfig {
            image: Some(PathBuf::from("/tmp/frost.jpg")),
            color: Some("#0011aa".to_string()),
            opacity: 0.55,
            blur: 8,
            size: BackgroundSize::Tile,
            position: "top right".to_string(),
            snapshot_lines: 5000,
        };
        let v = serde_json::to_value(&p).unwrap();
        let parsed: BackgroundPresetConfig = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.image, p.image);
        assert_eq!(parsed.color, p.color);
        assert!((parsed.opacity - p.opacity).abs() < f32::EPSILON);
        assert_eq!(parsed.blur, p.blur);
        assert_eq!(parsed.size, p.size);
        assert_eq!(parsed.position, p.position);
        assert_eq!(parsed.snapshot_lines, p.snapshot_lines);
    }

    #[test]
    fn background_preset_config_from_settings_strips_presets() {
        // From<&TerminalBackgroundSettings> for BackgroundPresetConfig
        // copies the shared fields; presets has no analogue on the sister
        // struct, so it is dropped.
        let mut s = TerminalBackgroundSettings::default();
        s.color = Some("#101010".to_string());
        s.presets.push(BackgroundPreset {
            name: "noise".to_string(),
            config: BackgroundPresetConfig::default(),
        });
        let p: BackgroundPresetConfig = (&s).into();
        assert_eq!(p.color, s.color);
        // The round-trip back through Into<TerminalBackgroundSettings>
        // produces a fresh `presets: []` regardless of what `s` had.
        let s2: TerminalBackgroundSettings = p.into();
        assert!(s2.presets.is_empty());
    }

    #[test]
    fn background_preset_round_trips() {
        let preset = BackgroundPreset {
            name: "Frosted glass".to_string(),
            config: BackgroundPresetConfig {
                image: Some(PathBuf::from("C:\\images\\frost.jpg")),
                color: None,
                opacity: 0.4,
                blur: 12,
                size: BackgroundSize::Cover,
                position: "center".to_string(),
                snapshot_lines: 2000,
            },
        };
        let v = serde_json::to_value(&preset).unwrap();
        let parsed: BackgroundPreset = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.name, preset.name);
        assert_eq!(parsed.config.image, preset.config.image);
        assert_eq!(parsed.config.blur, preset.config.blur);
    }
}

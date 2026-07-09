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
/// The single OpenCode AI-tool tab. OpenCode picks its own provider/model
/// (global config + credentials, switchable in-session), so unlike Claude
/// there is no cloud/local pair. Reserved in V19, replacing BOTH the V14
/// `aider` and `aider-local` reserved ids (the v18 → v19 migration collapses
/// them into this one id).
pub const OPENCODE_TAB_ID: &str = "opencode";
pub const SHELL_DEFAULT_TAB_ID: &str = "shell-default-1";
/// V8-03: the read-only, non-closable Offload Server tab. Materialized only
/// while `offload.enabled` (integrity check), it shows the local
/// `llama-server`'s live output (model-load progress + logs). Internally a
/// Shell-kind tab with `builtin: true` (so it can't be closed) and a reserved
/// id the frontend keys off to render read-only, log-fed content with no PTY.
pub const OFFLOAD_SERVER_TAB_ID: &str = "offload-server";
/// V9-01: reserved id of the read-only, non-closable "Code Graph" monitor
/// tab. Materialized iff `graph.enabled` (reconciled by the integrity check,
/// like the Offload Server tab). Unlike the other reserved tabs it is
/// app-rendered, not PTY-backed.
pub const GRAPH_MONITOR_TAB_ID: &str = "graph-monitor";
/// V13 Phase A: reserved id of the read-only, app-rendered Workbench tab
/// (live diff / checkpoint timeline / worktrees). Materialized iff
/// `workbench.enabled` (reconciled by the integrity check, exactly like the
/// Code Graph monitor tab). App-rendered like the graph monitor — no PTY.
pub const WORKBENCH_TAB_ID: &str = "workbench-1";
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
pub const CURRENT_SCHEMA_VERSION: u8 = 20;

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
    /// and TTS voice) — applies to every cImp-launched Claude tab rather
    /// than per-tab. Drives a `--settings` overlay injected at launch (see
    /// `tabs::config`) that points Claude Code's `statusLine` at our own
    /// `cimp --statusline` renderer.
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
    ///
    /// Unlike the other settings structs (which tolerate missing/unknown
    /// fields via `#[serde(default)]`), the recursive `LayoutNodePersisted`
    /// has required fields per node, so a single malformed node (e.g. a
    /// `Split` missing `ratio`) would otherwise fail the *entire* `Settings`
    /// parse and silently discard the whole per-folder overlay. The lenient
    /// deserializer degrades a bad layout to `None` — the frontend rebuilds
    /// a default tree — keeping the rest of the config intact.
    #[serde(default, deserialize_with = "deserialize_lenient_layout")]
    pub layout: Option<LayoutPersisted>,
    /// Named layout presets. Empty by default; populated via the Layouts
    /// menu's "Save current layout as..." entry. Restoring a preset
    /// replaces the live tree wholesale; the preset itself is unchanged.
    /// Lenient like `layout`: a malformed preset is dropped individually
    /// rather than taking the whole settings load down with it.
    #[serde(default, deserialize_with = "deserialize_lenient_presets")]
    pub layout_presets: Vec<LayoutPreset>,
    /// UI chrome theme settings (V5). The `theme` field selects the
    /// design-token block applied to the cimp chrome (tab bar, status
    /// bar, dialogs, settings). Distinct from `terminal.theme`, which
    /// governs the xterm.js terminal palette inside each tab.
    pub ui: UiSettings,
    /// Terminal-pane settings (V1.4-01+): xterm.js theme today, plus
    /// the V1.4-02 background image/color group when that ships.
    /// Distinct from `ui`, which themes the cimp chrome.
    pub terminal: TerminalSettings,
    /// V1.4-07: local-LLM provider config for AI tabs whose
    /// `use_local_provider` flag is `true`. The launch-time env
    /// composition reads `base_url`/`auth_token`/`model_alias` from
    /// here and synthesizes `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN`
    /// (and `ANTHROPIC_MODEL` if set) into the spawned process's env.
    /// Per-tab `env` entries take precedence over synthesized values.
    pub claude_local: ClaudeLocalSettings,
    /// Optional explicit executable paths for the bundled quick-launch
    /// tools (rustnet / broot). A non-empty field overrides the normal
    /// `ebin/` → PATH resolution for that tool, letting the user point at
    /// an exe in any folder; empty means "resolve normally". Additive
    /// `#[serde(default)]` — old settings files load with both empty.
    pub external_tools: ExternalToolsSettings,
    /// V8-01: local task-offload config. cImp runs a user-supplied
    /// `llama-server` and exposes an `offload_task` MCP tool into
    /// cImp-launched Claude tabs so Opus can delegate token-heavy
    /// subtasks to the local model. Off by default. Additive — old
    /// settings files load with the feature disabled.
    pub offload: OffloadSettings,
    /// V9-01: per-project code knowledge graph config. cImp builds an
    /// on-disk graph of code + docs at `<project>/<db_subdir>/graph.db`
    /// and exposes `graph_*` query tools to Claude tabs (MCP) and the
    /// offload worker (native). Off by default. Additive — old settings
    /// files load with the feature disabled.
    pub graph: GraphSettings,
    /// V13 Phase A: the Workbench feature — live diff pane, checkpoints (a
    /// shadow git repo), and a worktree manager, hosted in one reserved tab.
    /// The master `enabled` flag defaults `true` (the tab itself is cheap;
    /// each sub-feature gates itself, and checkpoints stay off by default —
    /// see `WorkbenchSettings`). Additive — old settings files load with the
    /// tab present but checkpoints off.
    pub workbench: WorkbenchSettings,
    /// V12 Phase A: project checker commands (`cargo check`, `tsc`, `eslint`,
    /// `pytest`, …) the `run_check` MCP tool can run. Lives at the root, not
    /// inside `GraphSettings` — it's project tooling, independent of the code
    /// graph (`run_check` is advertised whenever this is non-empty, whether or
    /// not `graph.enabled`). Empty by default; rides the `.cimp/config.json`
    /// overlay, which is where users actually set it. A model-supplied
    /// `run_check` tool call only *selects* a `CheckDef` by name — the command
    /// itself is never model-supplied.
    pub checks: Vec<crate::checks::CheckDef>,
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
            external_tools: ExternalToolsSettings::default(),
            offload: OffloadSettings::default(),
            graph: GraphSettings::default(),
            workbench: WorkbenchSettings::default(),
            checks: Vec::new(),
            enabled_ai_tabs: vec![AiTabId::Claude],
            logging: LoggingSettings::default(),
        }
    }
}

/// Logging configuration. The file path is fixed at
/// `<portable-root>/logs/cimp.log.<YYYY-MM-DD>`; the `level` field drives
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

/// One of the three reserved AI-tool tab ids. Wire format is the kebab-
/// case tab-id string (`"claude"`, `"claude-local"`, `"opencode"`); the type
/// exists so `enabled_ai_tabs` can be a strongly-typed `Vec<AiTabId>` instead
/// of an untyped string list.
///
/// V19 ships a single OpenCode tab (not a cloud/local pair like Claude):
/// OpenCode addresses many providers as `provider/model`, switches between
/// them in-session, and reads global config/credentials, so a local variant
/// would be redundant — the one tab covers cloud and local.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum AiTabId {
    Claude,
    ClaudeLocal,
    // Explicit rename: serde's kebab-case would split the camelCase variant
    // name into `open-code`, but the wire format must be the single-word tab id
    // `opencode` (matching OPENCODE_TAB_ID, the migration output, and the
    // frontend literals). A mismatch quarantines settings on load — see the
    // round-trip test.
    #[serde(rename = "opencode")]
    OpenCode,
}

impl AiTabId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => CLAUDE_TAB_ID,
            Self::ClaudeLocal => CLAUDE_LOCAL_TAB_ID,
            Self::OpenCode => OPENCODE_TAB_ID,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            CLAUDE_TAB_ID => Some(Self::Claude),
            CLAUDE_LOCAL_TAB_ID => Some(Self::ClaudeLocal),
            OPENCODE_TAB_ID => Some(Self::OpenCode),
            _ => None,
        }
    }

    /// True for the local-provider variants (`claude-local`). The integrity
    /// check uses this as the canonical `use_local_provider` value for each
    /// reserved id. OpenCode picks its own provider, so it is never "local".
    pub fn uses_local_provider(self) -> bool {
        matches!(self, Self::ClaudeLocal)
    }

    /// Canonical tab-bar position: claude (0) → claude-local → opencode,
    /// with shells trailing afterwards. Used by `add_ai_builtin_tab` and
    /// `integrity_check` so re-adding a previously-disabled AI tab lands in
    /// the same slot every time.
    pub fn canonical_order(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::ClaudeLocal => 1,
            Self::OpenCode => 2,
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

/// Tolerant deserializer for `Settings::layout`. Parses to a generic
/// `Value` first, then attempts the typed conversion; any failure (a
/// malformed/partial node, a `Split` missing `ratio`, a hand-edit that
/// broke the tree) degrades to `None` with a warning instead of failing
/// the whole `Settings` parse. The frontend rebuilds a default single-pane
/// tree when the layout is `None`, so the user loses only the broken layout
/// — not their entire per-folder overlay.
fn deserialize_lenient_layout<'de, D>(d: D) -> Result<Option<LayoutPersisted>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(d)?;
    match raw {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(val) => match serde_json::from_value::<LayoutPersisted>(val) {
            Ok(layout) => Ok(Some(layout)),
            Err(e) => {
                tracing::warn!(error = %e, "settings: malformed layout dropped to None");
                Ok(None)
            }
        },
    }
}

/// Tolerant deserializer for `Settings::layout_presets`. Drops individual
/// malformed presets (keeping the valid ones) and tolerates the field not
/// being an array at all, rather than aborting the entire settings load.
fn deserialize_lenient_presets<'de, D>(d: D) -> Result<Vec<LayoutPreset>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = serde_json::Value::deserialize(d)?;
    let serde_json::Value::Array(items) = raw else {
        if !raw.is_null() {
            tracing::warn!("settings: layout_presets was not an array; ignoring");
        }
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match serde_json::from_value::<LayoutPreset>(item) {
            Ok(p) => out.push(p),
            Err(e) => tracing::warn!(error = %e, "settings: malformed layout preset dropped"),
        }
    }
    Ok(out)
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

/// Optional explicit executable paths for the bundled quick-launch tools
/// (the bottom-bar rustnet / broot buttons). Each field, when non-empty, is
/// used as the launch command verbatim — overriding the normal `ebin/` → PATH
/// resolution (see `pty::resolve`) — so a user who doesn't want the bundled
/// build and doesn't have the tool on PATH can select an exe from any folder.
/// Empty (the default) means "resolve normally". Additive `#[serde(default)]`
/// block — old settings files round-trip with both fields empty.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct ExternalToolsSettings {
    /// Override for the `rustnet` tool; empty = resolve via ebin → PATH.
    pub rustnet: String,
    /// Override for the `broot` tool; empty = resolve via ebin → PATH.
    pub broot: String,
}

/// V8-01: local task-offload configuration. cImp runs a user-supplied
/// `llama-server` (the single source of truth is `server_command`) and
/// exposes an `offload_task` MCP tool into cImp-launched Claude tabs so
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
    /// --jinja -ngl 99 --ctx-size 150000 --flash-attn`. cImp
    /// `shlex`-parses it to spawn, parses host/port + `-np` to know
    /// where to connect and how many slots exist, and validates
    /// `--jinja` is present (tool-calling needs it). cImp never
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
    /// Per-program command security policies layered on top of the
    /// allowlist by `run_command`: which argument flags and subcommands to
    /// refuse, and which environment variables to force at spawn (to
    /// neutralize config-driven hooks). Seeded with a default `git` policy
    /// (refuse config-injection / root-escape flags + the `config`
    /// subcommand, neutralize pager/ssh via env). Fully visible and editable
    /// in Settings → Offload → Tools. A program with no matching policy gets
    /// only the allowlist + bare-name/PATH guard.
    pub command_policies: Vec<CommandPolicy>,
    /// User-installed MCP tool servers aggregated by cImp's MCP host
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
    /// Saved, reusable `llama-server` launch commands the user can paste into a
    /// Local backend's `Server command` field via the Pool editor's
    /// Save/Load/Delete controls. Purely a convenience library — nothing reads
    /// these at runtime; they only populate the command field on demand. Empty
    /// on a fresh install. Additive `#[serde(default)]`, so old files load with
    /// an empty library.
    pub server_command_templates: Vec<ServerCommandTemplate>,
    /// Saved, reusable Remote-backend endpoints (base URL + auth token) the user
    /// can paste into a Remote backend's fields via the same Save/Load/Delete
    /// controls. Convenience library only; empty on a fresh install. Additive
    /// `#[serde(default)]`.
    pub remote_backend_templates: Vec<RemoteBackendTemplate>,
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
    /// Max tasks allowed to *wait* for a slot when the pool is saturated.
    /// `None` (default) = unbounded blocking queue (a new task waits up to
    /// `offload_timeout_secs` for a slot). `Some(n)` fast-rejects a task with
    /// a clear "queue full" error once `n` are already waiting and every slot
    /// is busy — backpressure that fails fast instead of stacking long waits.
    pub max_queue_depth: Option<u32>,
    /// The OpenCode `local-llama` custom provider, derived from a Local
    /// backend's `server_command` via the Offload settings "Add to OpenCode"
    /// button (or kept in sync when [`opencode_provider_auto`] is on). When
    /// `Some`, `build_opencode_config` injects a `provider.local-llama` block +
    /// selects it as the default `model`, so a freshly opened OpenCode tab
    /// talks to the local `llama-server` out of the box. `None` = never
    /// registered. Additive `#[serde(default)]`.
    ///
    /// [`opencode_provider_auto`]: Self::opencode_provider_auto
    pub opencode_provider: Option<OpencodeLocalProvider>,
    /// When `true` AND the local offload server is [`enabled`], keep
    /// [`opencode_provider`] in step with the primary Local backend's command:
    /// re-derived at each OpenCode launch and re-persisted on a settings save
    /// whenever the command changed. When the local server is disabled this
    /// does nothing (the last snapshot, if any, stands). Off = the provider is
    /// a manual snapshot the button wrote once.
    ///
    /// [`enabled`]: Self::enabled
    /// [`opencode_provider`]: Self::opencode_provider
    pub opencode_provider_auto: bool,
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
            .field("command_policies", &self.command_policies)
            // McpServerConfig has its own redacted Debug.
            .field("mcp_servers", &self.mcp_servers)
            // OffloadBackend redacts the Remote `auth_token`.
            .field("backends", &self.backends)
            // No secrets: plain saved command lines.
            .field("server_command_templates", &self.server_command_templates)
            // RemoteBackendTemplate redacts its `auth_token`.
            .field("remote_backend_templates", &self.remote_backend_templates)
            .field("budget_high_water_pct", &self.budget_high_water_pct)
            .field("per_tool_result_token_cap", &self.per_tool_result_token_cap)
            .field("max_steps", &self.max_steps)
            .field("offload_timeout_secs", &self.offload_timeout_secs)
            .field("global_concurrency", &self.global_concurrency)
            .field("max_queue_depth", &self.max_queue_depth)
            // No secrets beyond the (already-cleartext) `--api-key` the user
            // themselves put in the server command; `OpencodeLocalProvider`
            // derives Debug.
            .field("opencode_provider", &self.opencode_provider)
            .field("opencode_provider_auto", &self.opencode_provider_auto)
            .finish()
    }
}

/// A derived OpenCode custom-provider entry — always registered under the id
/// `local-llama` — pointing at the local `llama-server`'s OpenAI-compatible
/// endpoint. Built from a Local backend's `server_command` (see
/// [`crate::offload::server::derive_opencode_provider`]) and injected into the
/// session config OpenCode receives via `OPENCODE_CONFIG_CONTENT`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct OpencodeLocalProvider {
    /// OpenAI-compatible base URL, ending in `/v1`
    /// (e.g. `http://127.0.0.1:8080/v1`). Host + port come from `--host`
    /// (default `127.0.0.1`) and `--port` in the command.
    pub base_url: String,
    /// Model id OpenCode sends in the completion request and selects as the
    /// default (`local-llama/<model>`). `--alias`/`-a` if present, else the
    /// `--model`/`-m` file basename (directory + `.gguf` stripped).
    pub model: String,
    /// API key from `--api-key` in the command, if any. `llama-server` ignores
    /// auth unless it was launched with a key, so this is usually empty.
    pub api_key: String,
    /// The `server_command` this snapshot was derived from. Lets the auto-sync
    /// re-derive only when the command actually changed.
    pub source_command: String,
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
            command_policies: default_command_policies(),
            mcp_servers: Vec::new(),
            backends: Vec::new(),
            server_command_templates: Vec::new(),
            remote_backend_templates: Vec::new(),
            budget_high_water_pct: 80,
            per_tool_result_token_cap: 8000,
            max_steps: 16,
            offload_timeout_secs: 300,
            global_concurrency: None,
            max_queue_depth: None,
            opencode_provider: None,
            opencode_provider_auto: false,
        }
    }
}

/// V9-01: per-project code knowledge graph configuration. The structural
/// graph (symbols/refs/calls/imports/full-text docs) needs no embedding
/// model; the `semantic_*` fields drive the optional Phase-G semantic search
/// over a remote `/v1/embeddings` endpoint. No secrets here, so `Debug` is
/// derived (unlike [`OffloadSettings`]). Additive `#[serde(default)]` — old
/// settings files round-trip with the feature disabled.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct GraphSettings {
    /// Master switch. Off = no indexing, no `graph_*` tools, no monitor tab.
    pub enabled: bool,
    /// Languages to index. Each maps to a tree-sitter grammar (+ `tags.scm`,
    /// + optional stack-graphs `tsg`). Unsupported files are skipped.
    pub languages: Vec<String>,
    /// Extra ignore globs, additive to the project's `.gitignore`. Generated
    /// / vendored / minified dirs are excluded by default to keep the graph
    /// clean and the DB small.
    pub ignore: Vec<String>,
    /// Index markdown files + doc-comments as `doc_chunk` nodes linked to the
    /// code they describe (powers `graph_search_docs`).
    pub index_docs: bool,
    /// Skip files larger than this many bytes (minified bundles, blobs).
    pub max_file_bytes: u64,
    /// Debounce window for the fs watcher's re-index pass (milliseconds).
    pub watch_debounce_ms: u64,
    /// Hard cap on rows returned by any single `graph_*` query (results feed
    /// an LLM context, so they're bounded like V8's tool results).
    pub max_rows_per_query: u32,
    /// Hard cap on the snippet bytes attached to each result row.
    pub max_snippet_bytes: u32,
    /// Hard cap on the body bytes returned by `graph_snippet` (V11 Phase A).
    /// Larger than `max_snippet_bytes` because whole definition bodies are
    /// bigger than the one-line snippets attached to result rows.
    pub max_body_bytes: u32,
    /// Per-project subdirectory holding `graph.db`. Recommended git-ignored.
    pub db_subdir: String,
    /// Let the **offload worker** query the graph when it's running on a
    /// **remote** backend. The local worker always gets graph access; a remote
    /// backend (LAN *or* cloud) sends your project's code structure off this
    /// machine, so it's opt-in and off by default. The user decides per their
    /// trust in the remote (a private LAN box vs. a public cloud API). The
    /// cloud Opus session (via MCP) and a local worker are unaffected.
    pub allow_remote_worker_access: bool,

    // --- Semantic search (Phase G) ---
    /// Enable embedding-based semantic search. Default off — it needs a
    /// reachable embedding endpoint; the structural graph works without it.
    pub semantic_search: bool,
    /// OpenAI-compatible `/v1/embeddings` endpoint (e.g. a `llama-server
    /// --embedding` on a spare GPU box).
    pub embedding_endpoint: String,
    /// Embedding model id requested from the endpoint. Baked into the vector
    /// "epoch"; changing it forces a re-embed.
    pub embedding_model: String,
    /// Embedding vector dimension. `0` = auto-probe on the first embed. The
    /// HNSW index never mixes dimensions.
    pub embedding_dims: u32,
    /// Also embed full symbol bodies (not just docs + signatures) for
    /// semantic *code* search. Off by default — multiplies vector count.
    /// Requires `semantic_search` on (it shares the embedder + backfill pass);
    /// with `semantic_search` on, this enables the `graph_semantic_code` tool and
    /// its code-embedding pass.
    pub embed_code_bodies: bool,
    /// Number of chunks per `/v1/embeddings` request (amortizes round-trips).
    pub embedding_batch: usize,
    /// Project-wide cap on how many `code_chunk` rows a full rebuild keeps
    /// (a simple count cap for V1 — see `build_tree`). Bounds DB size and
    /// embedding cost on very large repos.
    pub semantic_code_max_chunks: u32,

    // --- Context injection (V10 Phase D) ---
    /// Automatically prepend a budget-bounded digest of the most relevant files
    /// to each user prompt (Claude via a UserPromptSubmit hook, OpenCode via a
    /// plugin). Off by default — it changes what the agent sees.
    pub context_injection: bool,
    /// Max characters of digest emitted per file (outline + best snippet).
    pub context_per_file_chars: u32,
    /// Total character budget for one turn's injected context across all files.
    pub context_turn_budget_chars: u32,
    /// Fold the current session's working set (Phase C memory) into the ranking
    /// so session-hot files rank first.
    pub context_include_session: bool,
    /// Minimum top-file relevance score below which nothing is injected (so
    /// meta/"hi" prompts inject nothing).
    pub context_min_score: u32,

    // --- V11 Phase B: repo map (session-start orientation) ---
    /// Character budget for the once-per-session project map (`graph_repo_map`
    /// tool, and the session-start injection when enabled).
    pub repo_map_budget_chars: u32,
    /// Prepend the project map to the first injected turn of each new session.
    /// Rides the `context_injection` master toggle AND this flag. Off by default.
    pub repo_map_on_session_start: bool,

    // --- V11 Phase C: injection dedup ---
    /// How many turns a dedup suppression lasts: a file injected in full is
    /// demoted to a one-line "unchanged" reminder on later turns until it changes
    /// or this many turns pass. `0` disables dedup (every turn re-injects).
    pub context_dedup_ttl_turns: u32,

    // --- V11 Phase D: compaction survival (Claude PreCompact) ---
    /// Feed the compactor the session's working set + pinned notes so they
    /// survive the summary (and clear dedup / mark post-compaction). Costs a few
    /// hundred chars once per compaction; still master-gated by `context_injection`.
    pub compaction_context: bool,

    // --- V11 Phase E: redundant-read advisor (opt-in; logic in Phase E) ---
    /// Intercept a `Read` of a file already read unchanged this session and
    /// answer with a cheap reminder (outline digest) instead of re-reading it.
    /// Strictly opt-in — it changes the agent's tool behaviour. Default off.
    pub read_advisor: bool,
    /// Files with fewer than this many lines always pass the advisor (a small
    /// file is cheap to re-read; the reminder isn't worth it).
    pub read_advisor_min_lines: u32,
    /// `"advise"` (remind with the outline) or `"substitute"` (also include the
    /// most relevant symbol body). Default `"advise"`.
    pub read_advisor_mode: String,

    // --- V11 Phase F: local-model context digests ---
    /// For files with no useful outline (docs/configs/long scripts), have the
    /// **local** offload backend write a 3-line semantic digest, cached in
    /// `graph.db`. Off by default; needs a ready local offload backend. Never
    /// leaves the machine (local-only path).
    pub context_llm_digests: bool,

    // --- V12 Phase E: memory distillation (durable project facts) ---
    /// Distill an idle session's working set + notes into at most 3 durable
    /// `project_fact` rows via the **local-only** offload path before/instead
    /// of letting that knowledge evaporate with the session. Off by default —
    /// needs a ready local offload backend and the prompt is model-dependent
    /// (milestone Decision 3: revisit after real-session validation).
    pub memory_distillation: bool,
    /// Append **pinned** project facts (only pinned — the human-curated tier)
    /// to the launch-time guidance payload (Claude `--append-system-prompt`,
    /// OpenCode's instructions file), so durable knowledge arrives with zero
    /// tool calls. Off by default. Launch-time only: a fact pinned mid-session
    /// applies on the tab's next launch.
    pub promote_pinned_facts: bool,

    // --- V12 Phase F: proactive automation ---
    /// Auto-run the project's configured checks after an edit (`PostToolUse`
    /// hook → `/context/post_edit`) and inject only NEW/worsened diagnostics
    /// as additional context — the agent learns it broke something in the
    /// same turn instead of three turns later. Strictly opt-in — it's a
    /// behavior hook, same posture as `read_advisor`. Off by default; needs
    /// `checks` non-empty to do anything.
    pub auto_check: bool,
    /// Debounce window (seconds): edits inside this window since the last
    /// triggered run are coalesced (no new run); the run then covers
    /// everything the burst touched, since checks run against the file system
    /// state, not a specific edit.
    pub auto_check_debounce_s: u32,
    /// Minimum DIRECT inbound call count (`graph_callers`'s count) an edited
    /// file's symbol must have before the same hook appends a two-line
    /// blast-radius note (6b) — the moments an agent most needs impact
    /// analysis are exactly the moments it doesn't think to ask for it.
    pub auto_impact_min_dependents: u32,
    /// Re-run `dead_exports`/`import_cycles` after every completed index pass
    /// (bounded, read-only on the warm index — cheap) and badge the Analyses
    /// section when the counts changed. On by default — unlike the other
    /// Phase F toggles this doesn't change agent behavior, only a UI badge.
    pub analyses_auto: bool,
}

impl GraphSettings {
    /// The per-project db subdirectory, falling back to `.cimp` when unset.
    /// Single source of truth so the service and the MCP child can't open
    /// different paths.
    pub fn effective_db_subdir(&self) -> String {
        let s = self.db_subdir.trim();
        if s.is_empty() {
            ".cimp".to_string()
        } else {
            s.to_string()
        }
    }
}

impl Default for GraphSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            // Tier-1 code languages are on by default; markup/data languages
            // (html/css/json) stay opt-in to keep a fresh index lean (V9-02).
            languages: [
                "rust", "typescript", "javascript", "python", "markdown",
                "go", "java", "c", "cpp", "csharp", "php", "bash", "scala",
                "ocaml", "ruby", "haskell", "kotlin", "swift", "sql", "erlang",
                "r", "perl", "ada",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            ignore: Vec::new(),
            index_docs: true,
            max_file_bytes: 1_048_576, // 1 MiB
            watch_debounce_ms: 300,
            max_rows_per_query: 100,
            max_snippet_bytes: 2_000,
            max_body_bytes: 16_384,
            db_subdir: ".cimp".to_string(),
            allow_remote_worker_access: false,
            semantic_search: false,
            embedding_endpoint: String::new(),
            embedding_model: String::new(),
            embedding_dims: 0,
            embed_code_bodies: false,
            embedding_batch: 32,
            semantic_code_max_chunks: 20_000,
            context_injection: false,
            context_per_file_chars: 800,
            context_turn_budget_chars: 6_000,
            context_include_session: true,
            context_min_score: 3,
            repo_map_budget_chars: 4_000,
            repo_map_on_session_start: false,
            context_dedup_ttl_turns: 10,
            compaction_context: true,
            read_advisor: false,
            read_advisor_min_lines: 300,
            read_advisor_mode: "advise".to_string(),
            context_llm_digests: false,
            memory_distillation: false,
            promote_pinned_facts: false,
            auto_check: false,
            auto_check_debounce_s: 5,
            auto_impact_min_dependents: 10,
            analyses_auto: true,
        }
    }
}

/// V13 §0.4: the Workbench feature's settings. `enabled` is the master
/// switch for the tab itself (default **on** — the tab is cheap and each
/// section gates its own behavior); `checkpoints` is the shadow-repo
/// snapshot feature (default **off** in V1 — proposed on-by-default once the
/// shadow-repo cost is validated on a large real repo, per the milestone's
/// open decision 2). The five `checkpoint_*` fields tune retention (`_max`,
/// `_max_age_days`) and the debounced burst trigger (`_burst_files`,
/// `_burst_window_s`, `_min_gap_s`) that Phase C's fallback-to-activity
/// snapshot trigger reads.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(default)]
pub struct WorkbenchSettings {
    /// Master switch: the reserved Workbench tab exists. Off = no tab, no
    /// fs-batch event/broadcast, no checkpoint scheduling.
    pub enabled: bool,
    /// Automatic checkpoint snapshots (Phase C's shadow git repo). Off by
    /// default in V1 — the tab's Diff/Worktrees sections work without it;
    /// Timeline needs this on.
    pub checkpoints: bool,
    /// Ring-buffer cap: the shadow repo keeps at most this many checkpoints
    /// (oldest pruned first by `shadow::gc`, subject to `checkpoint_max_age_days`).
    pub checkpoint_max: u32,
    /// Age cap in days: checkpoints older than this are pruned regardless of
    /// how far under `checkpoint_max` the ring is.
    pub checkpoint_max_age_days: u32,
    /// Burst trigger: at least this many distinct changed paths within
    /// `checkpoint_burst_window_s` (and at least `checkpoint_min_gap_s` since
    /// the last snapshot) fires an "activity" checkpoint — the fallback that
    /// covers shell-tab edits and any flow that doesn't go through the
    /// prompt-tap trigger.
    pub checkpoint_burst_files: u32,
    /// Time window (seconds) the burst-file count above is measured over.
    pub checkpoint_burst_window_s: u32,
    /// Minimum seconds between any two automatic snapshots (prompt-tap OR
    /// burst), so a rapid-fire prompt sequence or a noisy save loop can't
    /// spam the shadow repo with near-duplicate commits.
    pub checkpoint_min_gap_s: u32,
}

impl Default for WorkbenchSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            checkpoints: false,
            checkpoint_max: 100,
            checkpoint_max_age_days: 7,
            checkpoint_burst_files: 5,
            checkpoint_burst_window_s: 60,
            checkpoint_min_gap_s: 120,
        }
    }
}

/// A named, reusable `llama-server` launch command the user can save from a
/// Local backend's `Server command` field in the Offload → Pool editor and
/// paste back into that field later. Stored globally in
/// [`OffloadSettings::server_command_templates`] so a library of commands
/// survives across backends and app restarts.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ServerCommandTemplate {
    /// User-facing label, unique within the list (the UI enforces uniqueness).
    pub name: String,
    /// The saved `llama-server` command line, verbatim.
    pub command: String,
}

/// A named, reusable Remote-backend endpoint (base URL + auth token) the user
/// can save from a Remote backend's fields and paste back in later. Stored
/// globally in [`OffloadSettings::remote_backend_templates`]. The `auth_token`
/// may be a real bearer secret, so the hand-rolled `Debug` below redacts it.
#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct RemoteBackendTemplate {
    /// User-facing label, unique within the list (the UI enforces uniqueness).
    pub name: String,
    /// Saved backend base URL (e.g. `http://192.168.1.50:8080`).
    pub base_url: String,
    /// Saved bearer/auth token. Stored cleartext on disk (like the backend's
    /// own `auth_token`); redacted in `Debug`.
    pub auth_token: String,
}

impl std::fmt::Debug for RemoteBackendTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteBackendTemplate")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field(
                "auth_token",
                &if self.auth_token.is_empty() { "<empty>" } else { "<redacted>" },
            )
            .finish()
    }
}

/// One environment variable forced at spawn by a [`CommandPolicy`] (e.g.
/// `GIT_PAGER=cat`). An ordered list of these — rather than a map — keeps the
/// settings diff deterministic and maps cleanly to the UI's key/value rows.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
pub struct CommandEnvVar {
    pub key: String,
    pub value: String,
}

/// A per-program command security policy applied by the native `run_command`
/// tool on top of the allowlist. Generalizes what used to be a hardcoded `git`
/// guard so any allowlisted program can be hardened, and so the rules are
/// visible/editable in Settings rather than buried in code.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
pub struct CommandPolicy {
    /// Program this policy hardens, matched against the allowlisted command by
    /// file-stem, case-insensitively (so `git` covers `git` and `git.exe`).
    pub program: String,
    /// Argument tokens to refuse. An argument matches when it equals the entry
    /// OR starts with `<entry>=` — covering `-c`, `--config-env`, `--git-dir`,
    /// … in both `--flag value` and `--flag=value` forms.
    pub denied_flags: Vec<String>,
    /// Subcommands (the first non-flag argument) to refuse, e.g. `config`.
    pub denied_subcommands: Vec<String>,
    /// Environment variables forced at spawn to neutralize config-driven hooks
    /// (e.g. `GIT_PAGER=cat`, empty `GIT_SSH_COMMAND`).
    pub env: Vec<CommandEnvVar>,
}

/// The seeded default policies. The `git` policy reproduces exactly the
/// hardening that `run_command` previously applied in code, so behavior is
/// unchanged on a fresh install — it's just visible and editable now. Because
/// [`OffloadSettings`] is `#[serde(default)]`, existing config files missing
/// the `command_policies` key inherit this automatically (no migration).
pub fn default_command_policies() -> Vec<CommandPolicy> {
    fn env(key: &str, value: &str) -> CommandEnvVar {
        CommandEnvVar { key: key.to_string(), value: value.to_string() }
    }
    fn s(v: &str) -> String {
        v.to_string()
    }
    vec![CommandPolicy {
        program: s("git"),
        denied_flags: vec![
            s("-c"),
            s("-C"),
            s("--config-env"),
            s("--exec-path"),
            s("--git-dir"),
            s("--work-tree"),
            s("--upload-pack"),
            s("--receive-pack"),
            // The remaining value-consuming git globals. They aren't dangerous
            // themselves, but `dangerous_args` finds the subcommand as the first
            // non-flag token — so a non-denied value-taking global would shift
            // that token onto its own value and let `git --namespace x config …`
            // slip the `config` check. Denying every value-taking global keeps
            // that assumption sound (a read probe never needs these).
            s("--namespace"),
            s("--super-prefix"),
            s("--attr-source"),
        ],
        denied_subcommands: vec![s("config")],
        env: vec![
            env("GIT_PAGER", "cat"),
            env("PAGER", "cat"),
            env("GIT_SSH_COMMAND", ""),
            env("GIT_TERMINAL_PROMPT", "0"),
            env("GIT_CONFIG_NOSYSTEM", "1"),
            env("GIT_CONFIG_GLOBAL", "/dev/null"),
        ],
    }]
}

/// On/off toggles for the native baseline offload tools (built into
/// cImp, zero external deps). All default on so offload works with no
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

/// One user-installed MCP tool server, aggregated by cImp's MCP host.
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
    /// Expose this server's tools to **Claude Code** (proxied through the
    /// per-session `cimp-offload` child). Off by default — a deliberate opt-in.
    pub claude_access: bool,
    /// Expose this server's tools to the **offload worker** (the local model,
    /// via the warm `McpHost`). This is the legacy `enabled` behavior.
    pub offload_access: bool,
    /// V19: expose this server's tools to **OpenCode** (proxied through the
    /// per-session `cimp-offload --consumer opencode` child). Off by default;
    /// the v18 → v19 migration seeds it from `claude_access` so upgraders keep
    /// their web-research tools across both agents.
    pub opencode_access: bool,
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
            .field("claude_access", &self.claude_access)
            .field("offload_access", &self.offload_access)
            .field("opencode_access", &self.opencode_access)
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
            claude_access: false,
            offload_access: true,
            opencode_access: false,
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
/// reach out to the public internet, not the user's machine. Kept as the
/// documented counterpart to [`LOCAL_DATA_TOOLS`] (the cloud-allow taxonomy)
/// even though no code path currently reads it.
#[allow(dead_code)]
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
/// `Local` mirrors V8-01's single-server config (the command cImp owns +
/// spawns as a read-only tab). `Remote` is a `base_url` cImp only
/// health-checks and connects to — no process, no tab. The hand-rolled
/// `Debug` on [`OffloadBackend`] redacts the Remote `auth_token`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OffloadBackendKind {
    /// cImp owns the process: the V8-01 `server_command` + `autostart` +
    /// read-only Offload Server tab + Start/Stop/Reset.
    Local {
        /// The single source-of-truth `llama-server` command (shlex-parsed
        /// to spawn; host/port/`-np`/`--jinja` parsed from it).
        server_command: String,
        /// Spawn at app launch and keep warm (else lazy on first offload).
        autostart: bool,
    },
    /// cImp holds a `base_url` (+ optional auth) and health-checks it; it
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

    /// The `server_command` of the primary Local backend the OpenCode
    /// `local-llama` provider tracks: the first enabled Local backend with a
    /// non-blank command (from [`effective_backends`], so a legacy single-local
    /// config resolves too). `None` when there's no usable Local command.
    ///
    /// [`effective_backends`]: Self::effective_backends
    pub fn primary_local_command(&self) -> Option<String> {
        self.effective_backends().into_iter().find_map(|b| match b.kind {
            OffloadBackendKind::Local { server_command, .. }
                if b.enabled && !server_command.trim().is_empty() =>
            {
                Some(server_command)
            }
            _ => None,
        })
    }

    /// The effective `local-llama` provider to inject into an OpenCode session,
    /// or `None` to inject nothing. When auto-sync is on and the local server
    /// is enabled, re-derive from the current primary Local command so edits
    /// take effect at launch without re-clicking the button; if that command is
    /// missing/incomplete, fall back to the last persisted snapshot. Otherwise
    /// use the stored snapshot as-is.
    pub fn resolve_opencode_provider(&self) -> Option<OpencodeLocalProvider> {
        if self.opencode_provider_auto && self.enabled {
            if let Some(cmd) = self.primary_local_command() {
                if let Ok(p) = crate::offload::server::derive_opencode_provider(&cmd) {
                    return Some(p);
                }
            }
        }
        self.opencode_provider.clone()
    }

    /// Re-sync the persisted `local-llama` snapshot on a settings save. No-op
    /// unless auto-sync is on AND the local server is enabled (per the auto
    /// contract: disabled ⇒ do nothing). Re-derives only when the primary Local
    /// command differs from the snapshot's `source_command`, so unrelated saves
    /// don't churn. A derive failure (missing `--port`/model) leaves the prior
    /// snapshot untouched rather than clearing it.
    pub fn sync_opencode_provider_on_save(&mut self) {
        if !(self.opencode_provider_auto && self.enabled) {
            return;
        }
        let Some(cmd) = self.primary_local_command() else {
            return;
        };
        let unchanged = self
            .opencode_provider
            .as_ref()
            .is_some_and(|p| p.source_command == cmd);
        if unchanged {
            return;
        }
        if let Ok(p) = crate::offload::server::derive_opencode_provider(&cmd) {
            self.opencode_provider = Some(p);
        }
    }

    /// Whether at least one MCP server is exposed to Claude Code.
    pub fn any_claude_mcp(&self) -> bool {
        self.mcp_servers.iter().any(|m| m.claude_access)
    }

    /// V19: whether at least one MCP server is exposed to OpenCode.
    pub fn any_opencode_mcp(&self) -> bool {
        self.mcp_servers.iter().any(|m| m.opencode_access)
    }

    /// Whether the warm MCP host + loopback endpoint need to run. True when
    /// offload is enabled (the worker needs the host) OR any MCP server is
    /// exposed to Claude Code or OpenCode directly (each reaches it over the
    /// loopback, independent of offload). Drives runtime startup, the warm-host
    /// lifecycle, and the per-tab MCP injection.
    pub fn mcp_host_needed(&self) -> bool {
        self.enabled || self.any_claude_mcp() || self.any_opencode_mcp()
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
        tts_injection: TtsInjection { enabled: true },
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
        use_local_provider: false,    })
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
        tts_injection: TtsInjection { enabled: true },
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
        use_local_provider: true,    })
}

/// V19: OpenCode AI-tool tab using whatever provider OpenCode's own config
/// selects (cloud / API keys / project config) when `use_local_provider` is
/// off. TTS prompt injection is enabled by default: OpenCode accepts an
/// instructions file (injected via `OPENCODE_CONFIG_CONTENT`), so it honors
/// the TTS-markup convention and the tab can speak.
pub fn default_opencode_tab() -> TabConfig {
    TabConfig::AiTool(AiToolTabConfig {
        id: OPENCODE_TAB_ID.to_string(),
        builtin: true,
        name: "OpenCode".to_string(),
        command: "opencode".to_string(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        // V19: unlike Aider, OpenCode accepts an instructions file (injected
        // via OPENCODE_CONFIG_CONTENT), so the TTS-markup convention applies
        // and the tab can speak. Seeded with the same runtime prompt as Claude.
        tts_injection: TtsInjection { enabled: true },
        notifications: AiNotificationConfig {
            idle: NotificationSlot::enabled("OpenCode is idle"),
            awaiting_permission: NotificationSlot::enabled("OpenCode is awaiting permission"),
            question: NotificationSlot::enabled("OpenCode has a question"),
            error: NotificationSlot::enabled("OpenCode encountered an error"),
        },
        first_launch_notice_dismissed: true,
        theme_override: None,
        background_override: None,
        use_local_provider: false,    })
}

/// Look up the default `TabConfig` for a reserved AI tab id. Used by
/// the integrity check and the lifecycle IPC when materializing a tab
/// the user just enabled.
pub fn default_ai_tab(id: AiTabId) -> TabConfig {
    match id {
        AiTabId::Claude => default_claude_tab(),
        AiTabId::ClaudeLocal => default_claude_local_tab(),
        AiTabId::OpenCode => default_opencode_tab(),
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

/// V9-01: the reserved, non-closable Code Graph monitor tab. Like the Offload
/// Server tab it's a Shell-kind entry with no command (never PTY-backed — its
/// content is an app-rendered dashboard of the graph indexer/embedder).
/// Materialized/removed by the integrity check per `graph.enabled`.
pub fn default_graph_monitor_tab() -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: GRAPH_MONITOR_TAB_ID.to_string(),
        builtin: true,
        name: "Code Intelligence".to_string(),
        command: String::new(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
    })
}

/// V13 Phase A: the reserved, non-closable Workbench tab. Same shape as the
/// Code Graph monitor tab — Shell-kind with no command (app-rendered, no
/// PTY). Materialized/removed by the integrity check per `workbench.enabled`.
pub fn default_workbench_tab() -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: WORKBENCH_TAB_ID.to_string(),
        builtin: true,
        name: "Workbench".to_string(),
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

/// Where a TTS/STT model runs. `Gpu` prefers the compiled GPU backend and
/// **auto-falls-back to CPU** if no usable GPU is present (so it's a safe
/// default everywhere); `Cpu` forces CPU. Runtime-switchable per feature —
/// changing it reloads only that model, no app restart. On a CPU-only build
/// (no GPU Cargo feature) both values run on CPU. This setting is
/// authoritative: it supersedes the legacy `CIMP_GPU` env var, which is no
/// longer consulted for device selection.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingDevice {
    #[default]
    Gpu,
    Cpu,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TtsSettings {
    /// Master enable for the whole TTS feature. When false, the Kokoro ONNX
    /// model is **unloaded** (freeing CPU/GPU memory) and no synthesis runs;
    /// flipping it back on reloads the model. Distinct from `mute`, which
    /// keeps the model loaded and only silences playback. Back-compat via the
    /// struct-level `#[serde(default)]` — files predating the field load as
    /// `true`.
    pub enabled: bool,
    /// GPU vs CPU for Kokoro synthesis. Changing it live reloads the model on
    /// the newly-selected device (no restart). Additive/back-compat via the
    /// struct-level `#[serde(default)]` — older files load as `Gpu`.
    pub device: ProcessingDevice,
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
            enabled: true,
            device: ProcessingDevice::Gpu,
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
    /// GPU vs CPU for Whisper transcription. Changing it live reloads the
    /// model on the newly-selected device (on the next recording / preload).
    /// Additive/back-compat via the struct-level `#[serde(default)]` — older
    /// files load as `Gpu`.
    pub device: ProcessingDevice,
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
            device: ProcessingDevice::Gpu,
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
    /// Default render mode: the animated pixel-art mascot. The default set is
    /// `impSprites` (the imp), independent of the active UI theme.
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
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            terminal_font_family: "Consolas, Menlo, \"DejaVu Sans Mono\", monospace"
                .to_string(),
            terminal_font_size: 14,
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
    /// When true, an AI tab's spoken prose plays even when that tab is not
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
/// injects a session-scoped `--settings` overlay into cImp-launched
/// Claude Code tabs that points `statusLine.command` at `cimp
/// --statusline` — our own renderer for a themed context-usage bar
/// (`Opus  ▓▓▓▓▓░░░░░ 50% (100k/200k)`). The overlay *merges* with the
/// user's own Claude Code settings (CLI flags outrank settings files and
/// only `statusLine` is set), so the user's global `~/.claude` config is
/// left untouched and the bar appears only inside cImp.
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
    /// Active UI chrome theme. Two values currently ship, both ratatui-style
    /// (custom title bar, square borders): `"tui-orange"` (Gruvbox surfaces +
    /// Claude Code's accent orange, #d77757) and `"tui-grey"` (the OpenCode Grey
    /// palette + OpenCode's cool light-grey accent, #c8ccd0). New installs land
    /// on `"tui-orange"` so the chrome accent matches Claude Code's orange. The
    /// avatar still defaults to the animated `impSprites` mascot independently
    /// (see [`AvatarKind`] / [`SpriteSettings`]).
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
/// Distinct from `ui`, which themes the cimp chrome.
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
    /// Read the active terminal's current selection aloud through TTS —
    /// the keyboard equivalent of the Ctrl+right-click gesture. Toasts
    /// "No text selected" when nothing is selected. Optional so older
    /// settings files round-trip; the default supplies the binding.
    pub speak_selection: Option<String>,
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
            speak_selection: Some("Ctrl+Alt+S".to_string()),
        }
    }
}

/// Per-tab gate for whether cImp speaks this AI tab's assistant prose. V20
/// retired the `[[TTS]]` markup convention (out-of-band adapters now read the
/// tool's structured transcript/event stream and speak all prose), so this is
/// a plain on/off toggle — the former free-text `instructions` field is gone.
/// Kept as a struct (not a bare bool) so the serialized shape stays an object
/// and every historical settings file / migration that carries
/// `tts_injection` as `{ "enabled": … }` round-trips without a schema bump; an
/// old file's leftover `instructions` key is simply ignored on load.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct TtsInjection {
    pub enabled: bool,
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

    fn local_backend(cmd: &str) -> OffloadBackend {
        OffloadBackend {
            name: "local".to_string(),
            enabled: true,
            kind: OffloadBackendKind::Local {
                server_command: cmd.to_string(),
                autostart: false,
            },
            ..Default::default()
        }
    }

    #[test]
    fn sync_provider_noop_when_auto_off_or_server_disabled() {
        // Auto off ⇒ untouched even with offload enabled.
        let mut o = OffloadSettings {
            enabled: true,
            opencode_provider_auto: false,
            backends: vec![local_backend("llama-server -a m --port 8080")],
            ..Default::default()
        };
        o.sync_opencode_provider_on_save();
        assert!(o.opencode_provider.is_none(), "auto off ⇒ no sync");

        // Auto on but server disabled ⇒ do nothing (the auto contract).
        o.opencode_provider_auto = true;
        o.enabled = false;
        o.sync_opencode_provider_on_save();
        assert!(o.opencode_provider.is_none(), "disabled server ⇒ no sync");
    }

    #[test]
    fn sync_provider_derives_when_enabled_and_rederives_on_change() {
        let mut o = OffloadSettings {
            enabled: true,
            opencode_provider_auto: true,
            backends: vec![local_backend("llama-server -a first --port 8080")],
            ..Default::default()
        };
        o.sync_opencode_provider_on_save();
        assert_eq!(o.opencode_provider.as_ref().unwrap().model, "first");

        // Same command again ⇒ unchanged snapshot (source_command matches).
        let snap = o.opencode_provider.clone();
        o.sync_opencode_provider_on_save();
        assert_eq!(o.opencode_provider, snap, "no change ⇒ no churn");

        // Command edited ⇒ re-derived.
        o.backends = vec![local_backend("llama-server -a second --port 9099")];
        o.sync_opencode_provider_on_save();
        let p = o.opencode_provider.as_ref().unwrap();
        assert_eq!(p.model, "second");
        assert_eq!(p.base_url, "http://127.0.0.1:9099/v1");
    }

    #[test]
    fn ai_tab_id_serde_wire_format_matches_tab_ids() {
        // The serde wire string for each AiTabId MUST equal its tab-id constant
        // (and the frontend literal + the migration output). A mismatch
        // quarantines settings on load. Round-trip both directions.
        for (variant, id) in [
            (AiTabId::Claude, "claude"),
            (AiTabId::ClaudeLocal, "claude-local"),
            (AiTabId::OpenCode, "opencode"),
        ] {
            assert_eq!(serde_json::to_value(variant).unwrap(), json!(id), "serialize {id}");
            assert_eq!(
                serde_json::from_value::<AiTabId>(json!(id)).unwrap(),
                variant,
                "deserialize {id}"
            );
            assert_eq!(variant.as_str(), id, "as_str {id}");
        }
    }

    #[test]
    fn malformed_layout_drops_to_none_without_losing_rest_of_settings() {
        // A Split node missing `ratio` is invalid, but it must not take the
        // whole Settings parse down with it. The layout degrades to None and
        // a sibling field (session.active_tab_id) still loads.
        let v = json!({
            "session": { "active_tab_id": "claude" },
            "layout": {
                "tree": {
                    "type": "split",
                    "id": "s1",
                    "direction": "horizontal",
                    "first": { "type": "pane", "id": "p1", "tab_ids": ["claude"], "active_tab_id": "claude" },
                    "second": { "type": "pane", "id": "p2", "tab_ids": [], "active_tab_id": null }
                },
                "focused_pane_id": "p1"
            }
        });
        let parsed: Settings = serde_json::from_value(v).unwrap();
        assert!(parsed.layout.is_none(), "malformed layout should drop to None");
        assert_eq!(parsed.session.active_tab_id.as_deref(), Some("claude"));
    }

    #[test]
    fn valid_layout_still_parses() {
        let v = json!({
            "layout": {
                "tree": { "type": "pane", "id": "p1", "tab_ids": ["claude"], "active_tab_id": "claude" },
                "focused_pane_id": "p1"
            }
        });
        let parsed: Settings = serde_json::from_value(v).unwrap();
        assert!(parsed.layout.is_some(), "valid layout should parse");
    }

    #[test]
    fn malformed_preset_is_dropped_individually() {
        // First preset is valid; second is missing `tree`. Keep the good one.
        let v = json!({
            "layout_presets": [
                {
                    "name": "good",
                    "created_at": "2026-01-01T00:00:00Z",
                    "tree": { "type": "pane", "id": "p1", "tab_ids": [], "active_tab_id": null }
                },
                { "name": "bad", "created_at": "2026-01-01T00:00:00Z" }
            ]
        });
        let parsed: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.layout_presets.len(), 1);
        assert_eq!(parsed.layout_presets[0].name, "good");
    }

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
        assert_eq!(back.device, ProcessingDevice::Gpu);
        assert_eq!(back.model_file, "ggml-small.bin");
        assert_eq!(back.language, "auto");
        assert!(back.input_device.is_empty());
        assert_eq!(back.button_mode, SttButtonMode::Toggle);
        assert!(!back.translate_to_english);
    }

    #[test]
    fn processing_device_serializes_snake_case() {
        // The wire form must be lowercase to match the frontend union type
        // `'gpu' | 'cpu'`.
        assert_eq!(serde_json::to_value(ProcessingDevice::Gpu).unwrap(), json!("gpu"));
        assert_eq!(serde_json::to_value(ProcessingDevice::Cpu).unwrap(), json!("cpu"));
    }

    #[test]
    fn tts_stt_without_device_field_default_to_gpu() {
        // Pre-existing settings files predate the `device` field; the additive
        // struct-level `#[serde(default)]` must load them as GPU (preserving the
        // historical "prefer GPU, fall back to CPU" behavior) — no migration.
        let tts: TtsSettings =
            serde_json::from_value(json!({ "enabled": true, "voice": "af_heart" })).unwrap();
        assert_eq!(tts.device, ProcessingDevice::Gpu);
        let stt: SttSettings =
            serde_json::from_value(json!({ "enabled": true, "model_file": "ggml-small.bin" }))
                .unwrap();
        assert_eq!(stt.device, ProcessingDevice::Gpu);
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

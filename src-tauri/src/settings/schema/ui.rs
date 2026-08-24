//! UI chrome: behaviour, usage, system stats, status bar, terminal theme and
//! background, scrollback, compose, prompt templates and shortcuts.
//!
//! Split out of `schema.rs` by V42 R10; see the module docs in `mod.rs`.

use super::*;

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
    /// Minimum number of seconds a tab must have been *working* — the span
    /// from its avatar entering `Thinking` to it settling back — before an
    /// **idle** announcement is allowed to fire. Fast turns (a one-line
    /// answer, a single tool call) otherwise announce "… is idle" every few
    /// seconds, which is the noise this exists to kill.
    ///
    /// `0` announces every idle (the historical behavior). Only
    /// `NotificationEvent::Idle` is gated — permission, question, error and
    /// exit announcements are never suppressed by this. A settle into Idle
    /// with no preceding working span (e.g. a harness's startup banner) counts
    /// as "worked for nothing" and is suppressed whenever this is above 0.
    ///
    /// Backward-compatible via the struct's serde-default — an absent key
    /// reads as the default below, no migration needed.
    pub idle_announce_min_working_secs: u32,
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
            idle_announce_min_working_secs: 120,
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
    // `show_context` lived here until 2026-08-17. It gated NC-3's live
    // context/cache group, which was removed from the widget (the meter now
    // ends at the reset clock), leaving a toggle with no consumer. An
    // installed settings file may still carry the key; `Settings` does not set
    // `deny_unknown_fields`, so it is ignored on read and gone on the next
    // write. No migration, no schema-version bump — same treatment as
    // `detection_update_classifier_mode` above.
    //
    // Only the widget went: the statusline extractor, the `context_window` slot
    // in the push file, `harness_usage`'s wire shape and the terminal
    // status line all still carry and render the context reading.
    /// How often the frontend re-reads the status-line usage push (a local
    /// file — see `harness::claude::usage`), in seconds. The UI clamps this to a sane
    /// minimum as busy-poll hygiene.
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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

// `StatuslineSettings` moved to the Claude plugin's declared settings in V40
// Phase B (locked decision 6). It was one `bool` that only Claude's
// `--settings` overlay ever read, so it is the `statusline` `ext` row on
// `Settings::harness["claude"]` now — no core field, no core reader, and a
// harness without a status line declares nothing.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct UiSettings {
    /// Active UI chrome theme. `"tui"` is the built-in ratatui-style theme
    /// (custom title bar, square borders, Gruvbox surfaces) — hardcoded in
    /// the binary and always available; its accent color comes from
    /// `tui_accent` below. Any other value refers to an on-disk theme under
    /// `<exe-dir>/themes/` (`"nippon-dark"` / `"nippon-light"` ship today).
    /// New installs land on `"tui"` (paired with the OpenCode Grey terminal
    /// palette). The avatar still defaults to the animated `impSprites`
    /// mascot independently (see [`AvatarKind`] / [`SpriteSettings`]).
    ///
    /// History: the pre-V1.13 `"tui"` value was rewritten to `"tui-orange"`
    /// by the v12 → v13 migration when the four accent-variant tui themes
    /// shipped; the v27 → v28 migration collapses those four back into the
    /// single `"tui"` id, seeding `tui_accent` with each variant's old
    /// accent so users keep their look.
    pub theme: String,
    /// Accent color of the built-in `"tui"` theme, as a `#rrggbb` hex
    /// string. Injected by the frontend as the `--tui-accent` CSS variable;
    /// the theme derives its whole accent family (hover/bright/soft/muted)
    /// from it. Ignored by on-disk themes — the picker only shows for
    /// `"tui"`. Invalid values fall back to the default blue frontend-side.
    pub tui_accent: String,
    /// Arrangement of the bottom status bar's movable left cluster. See
    /// [`StatusBarLayout`]. Added after the `theme` field; old files
    /// lacking the key deserialize to the default `[usage, system_stats]`
    /// via the struct-level `#[serde(default)]`.
    pub status_bar: StatusBarLayout,
    /// Show the reserved Tool Activity tab (unified graph-call + offload
    /// request feed, plus the tool reference lists). Default true; old files
    /// lacking the key deserialize to true via the struct-level
    /// `#[serde(default)]`. Reconciled like the other reserved feature tabs.
    pub tool_activity_tab: bool,
    /// #51: show the reserved Events tab (the per-tab-attributed, filterable
    /// activity feed). Default true; an existing settings file lacking the key
    /// deserializes to true via the struct-level `#[serde(default)]` and the
    /// integrity check materializes the tab on the next load — which is why
    /// this needs no schema-version migration. Additive: `tool_activity_tab`
    /// is untouched and both tabs coexist.
    pub events_tab: bool,
    /// V32: the color (`#rrggbb`) the containment surfaces wear while a tab
    /// is LATCHED but not contaminated — the tab strip's taint badge and the
    /// frame the pane draws around that tab's content. Rendered and validated
    /// frontend-side only (invalid values fall back to this default there,
    /// like `tui_accent`, in `latch.ts`'s `taintColor`); the default mirrors
    /// the TUI theme's `--warning`,
    /// which is what the badge wore before the color became configurable.
    /// Additive with the struct-level `#[serde(default)]` — no migration.
    pub latched_color: String,
    /// V32: the same surfaces while the tab's conversation is CONTAMINATED —
    /// the stronger state (it outlives the latch). Default mirrors the TUI
    /// theme's `--danger`. Same validation/additive story as `latched_color`.
    pub contaminated_color: String,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: crate::theming::TUI_THEME_ID.to_string(),
            tui_accent: DEFAULT_TUI_ACCENT.to_string(),
            status_bar: StatusBarLayout::default(),
            tool_activity_tab: true,
            events_tab: true,
            latched_color: "#fabd2f".to_string(),
            contaminated_color: "#fb4934".to_string(),
        }
    }
}

/// Default accent for the built-in `tui` theme — the blue the pre-v28
/// `tui-blue` default theme used, so new installs keep the familiar look.
/// Mirrors `DEFAULT_TUI_ACCENT` in `src/lib/themes/accent.ts`.
pub const DEFAULT_TUI_ACCENT: &str = "#7aa2f7";

/// A display panel in the status bar's movable left cluster: `usage` =
/// Claude session meter, `system_stats` = CPU/GPU/network panel.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub enum StatusBarComponent {
    Usage,
    SystemStats,
}

/// One slot in the movable cluster: a component plus the leading gap (in
/// px) before it. The gap is grown/shrunk by dragging the panel left or
/// right — it "stays where you drop it" — and is reset to 0 for every
/// slot whenever the component order changes.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct TerminalThemeSettings {
    pub name: String,
    pub custom: Option<HashMap<String, String>>,
}

impl Default for TerminalThemeSettings {
    fn default() -> Self {
        Self {
            // Paired with the default built-in `tui` UI theme (whose metadata
            // points at the OpenCode Grey palette). The frontend's theme picker
            // re-pairs this when the user switches UI theme.
            name: "OpenCode Grey".to_string(),
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "BackgroundPresetWire", export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "BackgroundPresetConfigWire", export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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

/// V14 Phase A: one saved prompt-library entry. `body` may contain the two
/// immediately-resolvable variables `{selection}` / `{clipboard}` (substituted
/// by the frontend on insert — see `lib/compose/templates.ts`) plus any
/// number of free-form `{name}` placeholders, which stay literal as tab-stops
/// the user fills in. Lives at `Settings::prompt_templates` (global scope);
/// project-scope entries of the same shape live in the `.cimp/config.json`
/// overlay's own `prompt_templates` array, read directly by the
/// `compose_templates` resolver IPC rather than through the normal
/// deep-merged `Settings` (see `settings::persistence::read_project_prompt_templates`
/// — the merge would otherwise wholesale-replace the global list instead of
/// shadowing it by name).
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct PromptTemplate {
    pub name: String,
    pub body: String,
}

/// The 4 starter templates seeded into the global list exactly once (guarded
/// by `Settings::templates_seeded`, not `CURRENT_SCHEMA_VERSION` — a user who
/// deletes all 4 must not have them reappear on a later migration). Clearly
/// example-flavored bodies; deletable like anything else in the list.
pub fn starter_prompt_templates() -> Vec<PromptTemplate> {
    vec![
        PromptTemplate {
            name: "review-this-diff".to_string(),
            body: "Review the following diff for correctness, style, and missed edge cases:\n\n{selection}".to_string(),
        },
        PromptTemplate {
            name: "write-tests-for".to_string(),
            body: "Write tests covering {selection}. Include the obvious edge cases.".to_string(),
        },
        PromptTemplate {
            name: "explain-selection".to_string(),
            body: "Explain what this does and why:\n\n{selection}".to_string(),
        },
        PromptTemplate {
            name: "commit-message".to_string(),
            body: "Write a concise, conventional commit message for:\n\n{selection}".to_string(),
        },
    ]
}

/// A template resolved by name across the two scopes — what the compose
/// overlay's `/` picker actually renders. `scope` is `"global"` or
/// `"project"`, surfaced in the UI so a shadowed global entry can be shown
/// greyed with a note.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct ResolvedTemplate {
    pub name: String,
    pub body: String,
    pub scope: String,
}

/// Merge global + project template lists by name: a project entry shadows a
/// same-named global one (matching every other overlay's precedence rule),
/// and any project-only entry is appended after the (filtered) global list.
/// Pure and file-I/O-free so the shadowing rule is unit-testable without
/// touching disk; `ipc::commands::compose_templates` is the thin I/O wrapper
/// that feeds it the two raw lists (see the `PromptTemplate` doc comment for
/// why those are read directly rather than through the merged `Settings`).
pub fn resolve_prompt_templates(
    global: Vec<PromptTemplate>,
    project: Vec<PromptTemplate>,
) -> Vec<ResolvedTemplate> {
    let project_names: std::collections::HashSet<&str> =
        project.iter().map(|t| t.name.as_str()).collect();
    let mut out: Vec<ResolvedTemplate> = global
        .into_iter()
        .filter(|t| !project_names.contains(t.name.as_str()))
        .map(|t| ResolvedTemplate {
            name: t.name,
            body: t.body,
            scope: "global".to_string(),
        })
        .collect();
    out.extend(project.into_iter().map(|t| ResolvedTemplate {
        name: t.name,
        body: t.body,
        scope: "project".to_string(),
    }));
    out
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct ShortcutSettings {
    pub open_compose: Option<String>,
    /// V14 Phase A: open compose (like `open_compose`) AND immediately open
    /// the prompt-template picker popover — the discoverable keyboard path
    /// alongside the 📋 button beside the compose textarea. Default
    /// `Alt+/` (mirrors `open_compose`'s `Alt+Enter` and the picker's own
    /// `/` trigger); NOT a `Ctrl+Shift+…` chord — see the V6-01 note above
    /// on `push_to_talk`'s bare-`Ctrl+Shift` collision.
    pub open_compose_picker: Option<String>,
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
            open_compose_picker: Some("Alt+/".to_string()),
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
        assert!(
            v.is_object(),
            "custom override should serialize as an object"
        );
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
        let mut s = TerminalBackgroundSettings {
            color: Some("#101010".to_string()),
            ..TerminalBackgroundSettings::default()
        };
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

    // --- V14 Phase A: prompt library -----------------------------------

    #[test]
    fn starter_prompt_templates_has_the_four_named_entries() {
        let templates = starter_prompt_templates();
        let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "review-this-diff",
                "write-tests-for",
                "explain-selection",
                "commit-message",
            ]
        );
    }

    #[test]
    fn resolve_prompt_templates_project_shadows_global_by_name() {
        let global = vec![
            PromptTemplate {
                name: "a".to_string(),
                body: "global-a".to_string(),
            },
            PromptTemplate {
                name: "b".to_string(),
                body: "global-b".to_string(),
            },
        ];
        let project = vec![
            // Shadows global "a" — project body wins, global "a" is dropped.
            PromptTemplate {
                name: "a".to_string(),
                body: "project-a".to_string(),
            },
            // Project-only entry, appended after the (filtered) global list.
            PromptTemplate {
                name: "c".to_string(),
                body: "project-c".to_string(),
            },
        ];
        let resolved = resolve_prompt_templates(global, project);
        assert_eq!(
            resolved.len(),
            3,
            "shadowed global \"a\" must not appear twice"
        );

        let a = resolved.iter().find(|t| t.name == "a").unwrap();
        assert_eq!(a.body, "project-a");
        assert_eq!(a.scope, "project");

        let b = resolved.iter().find(|t| t.name == "b").unwrap();
        assert_eq!(b.body, "global-b");
        assert_eq!(b.scope, "global");

        let c = resolved.iter().find(|t| t.name == "c").unwrap();
        assert_eq!(c.body, "project-c");
        assert_eq!(c.scope, "project");
    }

    #[test]
    fn resolve_prompt_templates_empty_project_passes_global_through() {
        let global = vec![PromptTemplate {
            name: "a".to_string(),
            body: "x".to_string(),
        }];
        let resolved = resolve_prompt_templates(global, Vec::new());
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].scope, "global");
    }

    #[test]
    fn settings_default_starts_unseeded_with_no_templates() {
        let s = Settings::default();
        assert!(!s.templates_seeded);
        assert!(s.prompt_templates.is_empty());
    }
}

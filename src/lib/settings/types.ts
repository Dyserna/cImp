// Frontend mirror of the Rust Settings schema. Field names use snake_case to
// match the JSON serde output. Optional fields come through as `null` over
// JSON, never as `undefined`.

import type { LayoutNode } from '../layout/types';
import type { AiTabId } from '../tabs/types';

export type AvatarPosition = 'top-right' | 'top-left' | 'bottom-right' | 'bottom-left';

export interface AvatarSize {
  width_px: number;
  height_px: number;
}

/// Per-axis offset from the screen edge specified by `AvatarPosition`.
/// Replaces the pre-v1.12 scalar `margin_px` field; the migration copies
/// the legacy value into both axes.
export interface AvatarMargin {
  x_px: number;
  y_px: number;
}

export interface AvatarImages {
  idle: string | null;
  listening: string | null;
  thinking: string | null;
  speaking: string | null;
  error: string | null;
}

export interface TransitionSettings {
  path: string | null;
  duration_ms: number;
}

export interface WaveformSettings {
  /// Render the audio waveform over the avatar. Default true.
  visible: boolean;
  color: string;
  line_width: number;
  glow_intensity: number;
  opacity: number;
}

/// Avatar render mode. `media` plays the per-state image/video files in
/// `AvatarImages`; `sprite` ignores them and runs the manifest-driven
/// frame-animation renderer over the sprite set named in `SpriteSettings`.
export type AvatarKind = 'media' | 'sprite';

/// Sprite-renderer config. `set` is a folder name under the bundled
/// `sprites/` tree (resolved to `/sprites/<set>/manifest.json` at runtime).
export interface SpriteSettings {
  set: string;
}

export interface AvatarSettings {
  visible: boolean;
  kind: AvatarKind;
  size: AvatarSize;
  position: AvatarPosition;
  margin: AvatarMargin;
  opacity: number;
  /// Draw the 1px frame around the avatar box. Default off.
  show_border: boolean;
  images: AvatarImages;
  sprite: SpriteSettings;
  transition: TransitionSettings;
  waveform: WaveformSettings;
}

export interface TtsSettings {
  voice: string;
  speed: number;
  volume: number;
  mute: boolean;
  /// Read-along highlight shown while a Ctrl+right-click selection is spoken.
  selection_highlight: SelectionHighlightSettings;
  /// Show the play/pause/restart/stop selection-TTS transport in the bottom bar.
  show_selection_controls: boolean;
}

/// Bottom-bar record button behavior: click-to-toggle vs press-and-hold.
export type SttButtonMode = 'toggle' | 'hold';

/// V6-01 offline speech-to-text (dictation) config. Mirrors the Rust
/// `SttSettings`.
export interface SttSettings {
  /// Master enable for the whole STT feature (record button + PTT).
  enabled: boolean;
  /// GGML model filename under `models/` (e.g. "ggml-small.bin").
  model_file: string;
  /// Whisper language hint. "auto" = detect; "en", "he", … force a language.
  language: string;
  /// cpal input device name; empty = system default input device.
  input_device: string;
  /// Bottom-bar record button behavior.
  button_mode: SttButtonMode;
  /// Translate non-English speech to English instead of transcribing verbatim.
  translate_to_english: boolean;
}

/// Colors for the Ctrl+right-click read-along highlight. xterm decorations
/// only accept `#RRGGBB` hex, so all four colors must be 6-digit hex strings.
export interface SelectionHighlightSettings {
  /// When false the selection is still spoken (chunked) but not highlighted.
  enabled: boolean;
  /// Not-yet-read sentences. Each `*_custom` flag chooses between the custom
  /// color (true) and the terminal's own palette color (false) for that
  /// channel.
  unread_fg: string;
  unread_fg_custom: boolean;
  unread_bg: string;
  unread_bg_custom: boolean;
  /// The sentence currently being spoken.
  reading_fg: string;
  reading_fg_custom: boolean;
  reading_bg: string;
  reading_bg_custom: boolean;
}

export interface DisplaySettings {
  terminal_font_family: string;
  terminal_font_size: number;
  show_tts_markup: boolean;
}

export interface BehaviorSettings {
  auto_speak: boolean;
  fallback_silent: boolean;
  announcements_enabled: boolean;
  /// When true, the frontend keeps `tts.mute` in sync with the inverse of
  /// `avatar.visible` — hide → mute, show → unmute. Wired in App.svelte.
  follow_avatar: boolean;
  /// When true, tab announcements fire even for the currently-focused tab.
  /// Default false reproduces the historical background-only behavior.
  announce_focused_tab: boolean;
  /// When true, tagged-content TTS plays even for tabs that aren't the
  /// active one. Default false matches the v2 behavior — only the
  /// foreground tab speaks. Independent of `announce_focused_tab`, which
  /// only controls announcement TTS.
  speak_background_tabs: boolean;
  /// When true, text selected in any terminal is copied to the system
  /// clipboard automatically.
  copy_on_select: boolean;
  /// When true, a right-click inside any terminal pastes the system
  /// clipboard into the focused PTY and suppresses the browser's default
  /// context menu.
  paste_on_right_click: boolean;
  /// When true, Ctrl+right-click inside any terminal reads the current
  /// selection aloud through TTS instead of pasting.
  speak_selection_on_right_click: boolean;
}

export interface UsageSettings {
  /// Overall on/off for the inline bottom-bar usage widget.
  enabled: boolean;
  /// Show the proportional fill bar.
  show_bar: boolean;
  /// Show the rounded utilization percentage.
  show_percentage: boolean;
  /// Show the live countdown to reset.
  show_countdown: boolean;
  /// Show the local reset clock time.
  show_reset_clock: boolean;
  /// Poll cadence for the usage endpoint, in seconds. Clamped to a sane
  /// minimum in the UI so the undocumented endpoint isn't hammered.
  poll_interval_secs: number;
}

export interface SystemStatsSettings {
  /// Overall on/off for the bottom-bar system-monitor panel.
  enabled: boolean;
  /// Poll cadence in seconds (sparklines tick locally between polls).
  poll_interval_secs: number;
  /// Per-component visibility (show_gpu_temp is a sub-toggle of show_gpu).
  show_cpu: boolean;
  show_memory: boolean;
  show_gpu: boolean;
  show_gpu_temp: boolean;
  show_network: boolean;
}

/// Context-window status line for ccImp-launched Claude Code tabs.
/// When enabled, the backend injects a session-scoped `--settings`
/// overlay pointing Claude Code's `statusLine` at `ccimp --statusline`,
/// which renders a themed context-usage bar. Global (not per-tab) and
/// scoped to ccImp sessions only — the user's ~/.claude config is
/// untouched. Mirrors `StatuslineSettings` in the backend schema.
export interface StatuslineSettings {
  /// Overall on/off for the context bar.
  enabled: boolean;
}

export interface ComposeSettings {
  min_height_px: number;
  max_height_px: number;
}

export interface ShortcutSettings {
  open_compose: string | null;
  submit_compose: string | null;
  cancel_compose: string | null;
  open_settings: string | null;
  switch_to_tab_1: string | null;
  switch_to_tab_2: string | null;
  switch_to_tab_3: string | null;
  switch_to_tab_4: string | null;
  switch_to_tab_5: string | null;
  switch_to_tab_6: string | null;
  switch_to_tab_7: string | null;
  switch_to_tab_8: string | null;
  switch_to_tab_9: string | null;
  new_shell_tab: string | null;
  close_tab: string | null;
  /// V4-03 pane shortcuts. All optional/nullable so a hand-edited
  /// settings file from v1.2 still parses; missing fields fall back to
  /// the defaults below at runtime.
  focus_pane_left: string | null;
  focus_pane_right: string | null;
  focus_pane_up: string | null;
  focus_pane_down: string | null;
  split_pane_horizontal: string | null;
  split_pane_vertical: string | null;
  close_pane: string | null;
  /// V6-01 push-to-talk (hold) dictation trigger. Default bare `Ctrl+Shift`.
  push_to_talk: string | null;
}

export interface TtsInjection {
  enabled: boolean;
  instructions: string;
}

/// V1.11 per-event notification slot. Both `enabled === true` AND a
/// non-empty `text` are required for the slot to fire — the legacy
/// "leave blank to disable" convention still works alongside the
/// explicit checkbox. The Rust side ships a tolerant `Deserialize` so
/// pre-v1.11 settings files (bare strings) load without losing text.
export interface NotificationSlot {
  enabled: boolean;
  text: string;
}

export interface AiNotificationConfig {
  idle: NotificationSlot;
  awaiting_permission: NotificationSlot;
  /// Spoken when a `kind: question` pattern fires (AskUserQuestion-style
  /// multi-option prompts).
  question: NotificationSlot;
  error: NotificationSlot;
}

export interface ShellNotificationConfig {
  error: NotificationSlot;
  /// `{code}` placeholder is interpolated with the actual exit code in M4.
  exited: NotificationSlot;
}

export interface AiToolTabConfig {
  kind: 'ai_tool';
  id: string;
  builtin: boolean;
  name: string;
  command: string;
  args: string[];
  cwd: string | null;
  env: Record<string, string>;
  tts_injection: TtsInjection;
  notifications: AiNotificationConfig;
  first_launch_notice_dismissed: boolean;
  /// V1.4-01 per-tab terminal palette override. `null` inherits the
  /// global `terminal.theme`; non-null replaces it for this tab.
  theme_override: TerminalThemeSettings | null;
  /// V1.4-02 per-tab background override (three-state). `null` inherits
  /// the global `terminal.background`; `"disabled"` opts out (theme bg
  /// only); a full settings object replaces the global wholesale.
  background_override: BackgroundOverrideWire | null;
  /// V1.4-07: when true, the launch flow synthesizes
  /// `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` (and `ANTHROPIC_MODEL`
  /// if `claude_local.model_alias` is non-empty) from the global
  /// `claude_local` settings group. Per-tab `env` entries override
  /// synthesized values.
  use_local_provider: boolean;
  /// When true, the processing layer speaks ALL new terminal output for this
  /// tab (sentence-segmented, deduped) and ignores `[[TTS]]…[[/TTS]]` markers,
  /// rather than speaking only the marked segments. Toggled from the tab's
  /// right-click menu; surfaced as a speaker icon on the tab.
  tts_all_output: boolean;
}

export interface ShellTabConfig {
  kind: 'shell';
  id: string;
  builtin: boolean;
  name: string;
  command: string;
  args: string[];
  cwd: string | null;
  env: Record<string, string>;
  notifications: ShellNotificationConfig;
  /// V1.4-01 per-tab terminal palette override. `null` inherits the
  /// global `terminal.theme`; non-null replaces it for this tab.
  theme_override: TerminalThemeSettings | null;
  /// V1.4-02 per-tab background override (three-state). See
  /// `AiToolTabConfig.background_override`.
  background_override: BackgroundOverrideWire | null;
}

export type TabConfig = AiToolTabConfig | ShellTabConfig;

export interface SessionState {
  active_tab_id: string | null;
}

export interface ProcessingSettings {
  stability_timeout_ms: number;
  max_hold_ms: number;
}

export interface UiSettings {
  /// Active UI chrome theme. Three values currently ship, all ratatui-style
  /// (custom title bar, square borders): `"tui-red"` (Imp Red palette + the
  /// imp's scarlet accent), `"tui-orange"` (Gruvbox surfaces + Claude Code's
  /// accent orange), and `"tui-green"` (Aider Green palette + Aider's terminal
  /// green accent). New installs default to `"tui-orange"` so the chrome
  /// accent matches Claude Code's orange; the avatar still defaults to the
  /// animated `impSprites` mascot independently. Distinct from
  /// `terminal.theme`, which governs the xterm.js terminal palette inside
  /// each tab.
  theme: string;
  /// Arrangement of the bottom status bar's movable left cluster.
  status_bar: StatusBarLayout;
}

/// A display panel in the status bar's movable left cluster.
export type StatusBarComponent = 'usage' | 'system_stats';

/// One slot in the movable cluster: a component plus the leading gap (px)
/// before it. The gap is drag-adjustable ("stays where you drop it") and
/// resets to 0 for all slots when the component order changes.
export interface StatusBarSlot {
  component: StatusBarComponent;
  gap: number;
}

/// Persisted left-to-right arrangement of the status bar's movable
/// cluster. Normalized on read so `usage` and `system_stats` each appear
/// exactly once.
export interface StatusBarLayout {
  items: StatusBarSlot[];
}

/// On-wire shape of a custom palette block. Mirrors the Rust
/// `HashMap<String, String>`. The 22 valid keys match xterm.js's
/// `ITheme`; missing keys are filled in from the bundled "Default"
/// theme by `themeFromSetting` in `src/lib/themes/resolve.ts`.
export type ThemeColorsWire = Record<string, string>;

/// Terminal palette setting. `name` is either a bundled theme name
/// (Default, Dracula, Solarized Dark, …) or "Custom" — in which case
/// `custom` carries the user's chosen color overrides.
export interface TerminalThemeSettings {
  name: string;
  custom: ThemeColorsWire | null;
}

/// V1.4-02: CSS `background-size` strategy. `tile` is mapped on the
/// frontend to `background-repeat: repeat` + `background-size: auto`;
/// `cover` and `contain` map directly to their CSS values.
export type BackgroundSize = 'cover' | 'contain' | 'tile';

/// V1.4-02 terminal background config. `image` and `color` are
/// independent — both can be set, in which case `color` becomes the
/// dimming-overlay tint atop the image. The opacity / blur / size /
/// position fields apply only when `image` is set; the resolver in
/// `src/lib/terminal/background.ts` enforces this.
export interface TerminalBackgroundSettings {
  image: string | null;
  color: string | null;
  opacity: number;
  blur: number;
  size: BackgroundSize;
  position: string;
  /// V1.4-04 A.1: scrollback rows captured by `serializeAddon.serialize`
  /// on a renderer-category flip. Bounds JS-heap allocation; default
  /// 2000.
  snapshot_lines: number;
  /// V1.4-04 B: named presets the user has saved. The recursive shape
  /// is blocked by `BackgroundPresetConfigWire` (preset configs don't
  /// contain a `presets` field), so a preset can never reference
  /// presets-of-presets. Migration v1.5 → v1.6 stamps `[]`.
  presets: BackgroundPresetWire[];
  /// V1.4-04 C.4: when false, per-tab dialog edits that flip renderer
  /// category (image ↔ no-image) are deferred to Save. In-place
  /// changes preview live regardless. Default true. Phase D's
  /// v1.6 → v1.7 migration stamps this explicitly; older files
  /// serde-default to `true`.
  preview_category_flips: boolean;
}

/// V1.4-04 B: payload of a saved preset. Same fields as
/// `TerminalBackgroundSettings` minus the recursive `presets` field.
/// Mirrors Rust's `BackgroundPresetConfig`.
export interface BackgroundPresetConfigWire {
  image: string | null;
  color: string | null;
  opacity: number;
  blur: number;
  size: BackgroundSize;
  position: string;
  snapshot_lines: number;
}

/// V1.4-04 B: a named preset entry. `name` is the user-facing label
/// shown in the "Load preset…" dropdown and Manage modal. Mirrors
/// Rust's `BackgroundPreset`.
export interface BackgroundPresetWire {
  name: string;
  config: BackgroundPresetConfigWire;
}

/// Project the shared subset of a `TerminalBackgroundSettings` into a
/// `BackgroundPresetConfigWire`. The reverse is achieved by spreading
/// the preset config into a `TerminalBackgroundSettings` with a fresh
/// `presets: []`, which the editor's `loadPreset` does inline.
export function toPresetConfig(
  s: TerminalBackgroundSettings,
): BackgroundPresetConfigWire {
  return {
    image: s.image,
    color: s.color,
    opacity: s.opacity,
    blur: s.blur,
    size: s.size,
    position: s.position,
    snapshot_lines: s.snapshot_lines,
  };
}

/// V1.4-02 three-state per-tab override on the wire. The literal
/// `'disabled'` string opts the tab out of any background; an object
/// is a full per-tab config; `null` (handled at the field level on the
/// containing tab type) inherits the global setting.
export type BackgroundOverrideWire = 'disabled' | TerminalBackgroundSettings;

/// Type guard: distinguishes the `'disabled'` literal from the object
/// branch so callers can narrow safely without struct-vs-string runtime
/// checks scattered around the codebase.
export function isBackgroundDisabled(
  o: BackgroundOverrideWire | null,
): o is 'disabled' {
  return o === 'disabled';
}

/// V1.4-04 D: cross-restart scrollback config. The PTY ring buffer is
/// capped at `ring_bytes` per tab; on graceful exit each tab's ring is
/// written to `<config-dir>/scrollback/<tab-id>.bin`; on next launch
/// `pty_start` returns the persisted bytes for the new xterm to replay
/// before live PTY output resumes.
export interface ScrollbackSettings {
  ring_bytes: number;
  persist: boolean;
  restore_on_launch: boolean;
}

/// V1.4-01+: terminal-pane settings. Holds the xterm.js palette config
/// (V1.4-01), the V1.4-02 background sub-group, and the V1.4-04 D
/// cross-restart scrollback group.
export interface TerminalSettings {
  theme: TerminalThemeSettings;
  background: TerminalBackgroundSettings;
  scrollback: ScrollbackSettings;
}

/// Persisted layout state (V4-04). Mirrors the in-memory `LayoutState`
/// 1:1 — `LayoutNode` is the same `'split' | 'pane'`-discriminated tree
/// the frontend already uses, so serialize/deserialize is identity work.
export interface LayoutPersisted {
  tree: LayoutNode;
  focused_pane_id: string;
}

/// A named layout preset. The tree is the layout-only payload — focus
/// is intentionally not persisted with the preset, since restoring it
/// is "set up panes this way" and the user's next click decides focus.
export interface LayoutPreset {
  name: string;
  /// RFC 3339 / ISO 8601 timestamp (UTC). Used to order the popover's
  /// "Recent presets" list. Renames do not refresh this.
  created_at: string;
  tree: LayoutNode;
}

/// Latest on-disk schema version. Mirrors `CURRENT_SCHEMA_VERSION` in
/// `src-tauri/src/settings/schema.rs`. Bumped on every backend migration
/// step. Frontend doesn't read it for any logic — it round-trips as a
/// bare integer through the IPC bridge.
export const CURRENT_SCHEMA_VERSION = 14;

export interface Settings {
  /// On-disk schema version. The backend stamps `CURRENT_SCHEMA_VERSION`
  /// on fresh installs and via the v1.9 → v1.10 migration; the frontend
  /// receives it as a bare integer and includes it in `defaultSettings`
  /// so a manually-constructed Settings (test fixtures, fallback paths)
  /// matches the on-disk shape.
  schema_version: number;
  tts: TtsSettings;
  /// V6-01 offline speech-to-text. Additive; old files default it disabled.
  stt: SttSettings;
  avatar: AvatarSettings;
  display: DisplaySettings;
  behavior: BehaviorSettings;
  usage: UsageSettings;
  system_stats: SystemStatsSettings;
  /// Claude Code context-window status line bar (global, ccImp-scoped).
  statusline: StatuslineSettings;
  compose: ComposeSettings;
  shortcuts: ShortcutSettings;
  /// Ordered tab configs. Reserved ids (claude, claude-local, shell-default-1)
  /// are guaranteed to be present after the backend's startup integrity check.
  tabs: TabConfig[];
  processing: ProcessingSettings;
  session: SessionState;
  /// Persisted layout. `null` on fresh installs (frontend builds a
  /// single-root-pane default) or when the v1.2 → v1.3 migration was
  /// skipped for some reason. The hydration code handles both cases.
  layout: LayoutPersisted | null;
  /// Saved layout presets. Empty by default; populated via the Layouts
  /// menu. Order is insertion order; the popover sorts by `created_at`.
  layout_presets: LayoutPreset[];
  /// UI chrome theme settings (V5).
  ui: UiSettings;
  /// Terminal-pane settings (V1.4-01+): xterm.js palette today, plus
  /// the V1.4-02 background sub-group when that ships.
  terminal: TerminalSettings;
  /// V1.4-07: local-LLM provider config for AI tabs whose
  /// `use_local_provider` flag is `true`. Stored cleartext on disk —
  /// local proxies typically accept dummy tokens, so this is acceptable.
  claude_local: ClaudeLocalSettings;
  /// V14: local-LLM provider config for Aider tabs whose
  /// `use_local_provider: true`. Stored cleartext on disk for the same
  /// reasons as `claude_local` (local proxies typically accept dummy
  /// tokens; OS-keychain integration is a future upgrade).
  aider_local: AiderLocalSettings;
  /// V8-01: local task-offload config. ccImp runs a user-supplied
  /// `llama-server` and exposes an `offload_task` MCP tool into
  /// ccImp-launched Claude tabs. Off by default.
  offload: OffloadSettings;
  /// Which AI-tool tabs are enabled. The checkbox group in
  /// Settings → Tabs is the canonical way to flip this; the backend's
  /// `set_enabled_ai_tabs` IPC opens / closes the corresponding AI
  /// tabs in response. The list is required to be non-empty; the UI
  /// disables the last-checked checkbox to enforce that, and the IPC
  /// rejects an empty value as defense-in-depth. Default is
  /// `["claude"]` (subscription Claude only) on a fresh install.
  enabled_ai_tabs: AiTabId[];
  /// File-logger configuration. The backend writes daily rolling log
  /// files into `<portable-root>/logs/`; this field drives the live filter.
  logging: LoggingSettings;
}

/// One of the four reserved AI-tool tab ids. Wire format mirrors the
/// backend's `AiTabId` enum (kebab-case strings). Canonical definition
/// lives in `../tabs/types` (alongside `AI_TABS` and the type guards);
/// re-exported here so settings consumers keep a single source of truth.
export type { AiTabId };

/// Tracing-filter level. Lowercase to match Rust's serde rename.
export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error';

/// Log file retention. Drives the startup cleanup pass that removes
/// rolled files older than the chosen window. `never` keeps everything.
export type LogRetention = 'daily' | 'weekly' | 'monthly' | 'never';

/// Per-tab raw PTY content capture. Disabled by default. When on,
/// every tab's PTY output is also written to
/// `<portable-root>/logs/content/<tab-id>.log.<YYYY-MM-DD>`.
export interface ContentCaptureSettings {
  enabled: boolean;
  retention: LogRetention;
}

/// File-logger configuration. Mirrors Rust's `LoggingSettings`.
export interface LoggingSettings {
  level: LogLevel;
  retention: LogRetention;
  content_capture: ContentCaptureSettings;
}

/// V1.4-07: local-LLM provider configuration. `base_url` and
/// `auth_token` become `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` on
/// any AI tab with `use_local_provider: true`. `model_alias`, when
/// non-empty, becomes `ANTHROPIC_MODEL` (also passed through some
/// proxies as a model-mapping key).
export interface ClaudeLocalSettings {
  base_url: string;
  auth_token: string;
  model_alias: string;
}

/// V14: local-LLM provider configuration for Aider tabs whose
/// `use_local_provider: true`. `base_url` becomes `OPENAI_API_BASE`
/// and `auth_token` becomes `OPENAI_API_KEY` on launch; `model`,
/// when non-empty, is passed as `--model <model>` on the spawn argv.
export interface AiderLocalSettings {
  base_url: string;
  auth_token: string;
  model: string;
}

/// V8-01: native baseline offload tool toggles (mirror of Rust
/// `OffloadToolToggles`).
export interface OffloadToolToggles {
  read_file: boolean;
  code_search: boolean;
  run_command: boolean;
}

/// V8-01: one user-installed MCP tool server (mirror of Rust
/// `McpServerConfig`). Either stdio (`command` + `args` + `env`) or
/// HTTP (`url`). `env` values may carry secrets — redacted in the Rust
/// `Debug` impl, but stored cleartext on disk.
export interface McpServerConfig {
  name: string;
  command: string;
  args: string[];
  env: Record<string, string>;
  url: string;
  enabled: boolean;
}

/// V8-02: which capability tier a backend serves (mirror of Rust
/// `BackendTier`).
export type BackendTier = 'fast' | 'quality';

/// V8-02: a backend's allow-list over the global tool pool (mirror of Rust
/// `ToolScope`). `all` = every tool; `only` = just the named ones;
/// `allexcept` = everything but the named ones (the cloud default denies
/// the local-data set).
export type ToolScope =
  | { mode: 'all' }
  | { mode: 'only'; tools: string[] }
  | { mode: 'allexcept'; tools: string[] };

/// V8-02: native + MCP tool names treated as local-data (denied to cloud
/// backends by default). Mirrors Rust `LOCAL_DATA_TOOLS`.
export const LOCAL_DATA_TOOLS = ['read_file', 'code_search', 'run_command', 'filesystem', 'git'];

/// V8-02: kind-specific config for one backend (mirror of Rust
/// `OffloadBackendKind`). Local = ccImp owns the process; Remote = a
/// health-checked URL (LAN or cloud).
export type OffloadBackendKind =
  | { type: 'local'; server_command: string; autostart: boolean }
  | { type: 'remote'; base_url: string; auth_token: string; is_cloud: boolean; cloud_consent: boolean };

/// V8-02: one backend in the offload pool (mirror of Rust `OffloadBackend`).
export interface OffloadBackend {
  name: string;
  enabled: boolean;
  kind: OffloadBackendKind;
  declared_context: number | null;
  declared_model: string;
  tier: BackendTier;
  tool_scope: ToolScope;
}

/// V8-01/V8-02: local task-offload config (mirror of Rust `OffloadSettings`).
/// `server_command`/`autostart` are the legacy single-local fields; V8-02
/// uses `backends` (the pool). When `backends` is empty the legacy fields
/// synthesize one Local backend at runtime.
export interface OffloadSettings {
  enabled: boolean;
  autostart: boolean;
  inject_guidance: boolean;
  server_command: string;
  tools: OffloadToolToggles;
  allowed_roots: string[];
  command_allowlist: string[];
  mcp_servers: McpServerConfig[];
  backends: OffloadBackend[];
  budget_high_water_pct: number;
  per_tool_result_token_cap: number;
  max_steps: number;
  offload_timeout_secs: number;
  /// V8-03: global cap on offloads in flight across the whole app. `null`
  /// lets the service auto-size it from the summed per-backend slot counts.
  global_concurrency: number | null;
}

/// Reserved tab ids — mirror of `crate::settings::*_TAB_ID` constants.
/// User-created shell tabs use uuid-based ids that never collide with these.
export const CLAUDE_TAB_ID = 'claude';
/// V1.4-07: replaces the pre-V1.4-07 `AIDER_TAB_ID = 'aider'`. The
/// v1.7 → v1.8 migration rewrites the aider tab to this id in place.
export const CLAUDE_LOCAL_TAB_ID = 'claude-local';
/// V14: Aider AI-tool tab using whatever provider Aider's own config
/// selects (cloud / API keys / per-project `.aider.conf.yml`).
export const AIDER_TAB_ID = 'aider';
/// V14: Aider tab pointed at a local OpenAI-compatible endpoint via
/// the `aider_local` provider settings.
export const AIDER_LOCAL_TAB_ID = 'aider-local';
export const SHELL_DEFAULT_TAB_ID = 'shell-default-1';

/// Look up a tab entry by id. Returns undefined for unknown ids; callers
/// treat that as a transient state (tab gone).
export function findTab(settings: Settings, id: string): TabConfig | undefined {
  return settings.tabs.find((t) => t.id === id);
}

/// Index of the tab entry; useful when callers need to mutate via array
/// index (e.g. spreading the new entry into a new array for state setters).
export function findTabIndex(settings: Settings, id: string): number {
  return settings.tabs.findIndex((t) => t.id === id);
}

// Defaults must match `impl Default for Settings` + the integrity check on
// the backend. They're the initial store value so subscribers get sane
// shapes before the first `settings-changed` event arrives. The backend
// re-broadcasts on init so this value is short-lived in practice.
export function defaultSettings(): Settings {
  return {
    schema_version: CURRENT_SCHEMA_VERSION,
    tts: {
      voice: 'af_heart',
      speed: 1.0,
      volume: 1.0,
      mute: false,
      selection_highlight: {
        enabled: true,
        unread_fg: '#000000',
        unread_fg_custom: true,
        unread_bg: '#ff5555',
        unread_bg_custom: true,
        reading_fg: '#000000',
        reading_fg_custom: true,
        reading_bg: '#f1fa8c',
        reading_bg_custom: true,
      },
      show_selection_controls: true,
    },
    stt: {
      enabled: true,
      model_file: 'ggml-small.bin',
      language: 'auto',
      input_device: '',
      button_mode: 'toggle',
      translate_to_english: false,
    },
    avatar: {
      visible: true,
      kind: 'sprite',
      size: { width_px: 140, height_px: 140 },
      position: 'top-right',
      margin: { x_px: 21, y_px: 0 },
      opacity: 1.0,
      show_border: false,
      images: {
        idle: null,
        listening: null,
        thinking: null,
        speaking: null,
        error: null,
      },
      sprite: { set: 'impSprites' },
      transition: { path: '/avatar/Transition.mp4', duration_ms: 400 },
      waveform: {
        visible: true,
        color: '',
        line_width: 2.0,
        glow_intensity: 0.6,
        opacity: 0.85,
      },
    },
    display: {
      terminal_font_family: 'Consolas, Menlo, "DejaVu Sans Mono", monospace',
      terminal_font_size: 14,
      show_tts_markup: false,
    },
    behavior: {
      auto_speak: true,
      fallback_silent: true,
      announcements_enabled: true,
      follow_avatar: false,
      announce_focused_tab: false,
      speak_background_tabs: false,
      copy_on_select: true,
      paste_on_right_click: true,
      speak_selection_on_right_click: true,
    },
    usage: {
      enabled: true,
      show_bar: true,
      show_percentage: true,
      show_countdown: true,
      show_reset_clock: true,
      poll_interval_secs: 60,
    },
    system_stats: {
      enabled: true,
      poll_interval_secs: 1,
      show_cpu: true,
      show_memory: true,
      show_gpu: true,
      show_gpu_temp: true,
      show_network: true,
    },
    statusline: { enabled: true },
    compose: { min_height_px: 80, max_height_px: 300 },
    shortcuts: {
      open_compose: 'Alt+Enter',
      submit_compose: 'Enter',
      cancel_compose: 'Escape',
      open_settings: 'Ctrl+,',
      switch_to_tab_1: 'Ctrl+1',
      switch_to_tab_2: 'Ctrl+2',
      switch_to_tab_3: 'Ctrl+3',
      switch_to_tab_4: 'Ctrl+4',
      switch_to_tab_5: 'Ctrl+5',
      switch_to_tab_6: 'Ctrl+6',
      switch_to_tab_7: 'Ctrl+7',
      switch_to_tab_8: 'Ctrl+8',
      switch_to_tab_9: 'Ctrl+9',
      new_shell_tab: 'Ctrl+T',
      close_tab: 'Ctrl+W',
      focus_pane_left: 'Ctrl+Alt+Left',
      focus_pane_right: 'Ctrl+Alt+Right',
      focus_pane_up: 'Ctrl+Alt+Up',
      focus_pane_down: 'Ctrl+Alt+Down',
      split_pane_horizontal: 'Ctrl+\\',
      split_pane_vertical: 'Alt+\\',
      close_pane: 'Ctrl+Alt+W',
      push_to_talk: 'Ctrl+Shift',
    },
    tabs: [
      {
        kind: 'ai_tool',
        id: CLAUDE_TAB_ID,
        builtin: true,
        name: 'Claude',
        command: 'claude',
        args: [],
        cwd: null,
        env: {},
        tts_injection: { enabled: true, instructions: '' },
        notifications: {
          idle: { enabled: true, text: 'Claude is idle' },
          awaiting_permission: {
            enabled: true,
            text: 'Claude is awaiting permission',
          },
          question: { enabled: true, text: 'Claude has a question' },
          error: { enabled: true, text: 'Claude encountered an error' },
        },
        first_launch_notice_dismissed: true,
        theme_override: null,
        background_override: null,
        use_local_provider: false,
        tts_all_output: false,
      },
      {
        kind: 'ai_tool',
        id: CLAUDE_LOCAL_TAB_ID,
        builtin: true,
        name: 'Claude (local)',
        command: 'claude',
        args: [],
        cwd: null,
        env: {},
        tts_injection: { enabled: true, instructions: '' },
        notifications: {
          idle: { enabled: true, text: 'Claude (local) is idle' },
          awaiting_permission: {
            enabled: true,
            text: 'Claude (local) is awaiting permission',
          },
          question: { enabled: true, text: 'Claude (local) has a question' },
          error: {
            enabled: true,
            text: 'Claude (local) encountered an error',
          },
        },
        first_launch_notice_dismissed: true,
        theme_override: null,
        background_override: null,
        use_local_provider: true,
        tts_all_output: false,
      },
    ],
    processing: { stability_timeout_ms: 200, max_hold_ms: 500 },
    session: { active_tab_id: null },
    layout: null,
    layout_presets: [],
    ui: {
      theme: 'tui-orange',
      status_bar: {
        items: [
          { component: 'usage', gap: 0 },
          { component: 'system_stats', gap: 0 },
        ],
      },
    },
    // Default terminal palette is paired with the default UI theme
    // (tui-orange → GitHub Dark); the pairing comes from each theme's
    // `palette` metadata (theme.json), applied by SettingsApp on theme switch.
    terminal: {
      theme: { name: 'GitHub Dark', custom: null },
      background: {
        image: null,
        color: null,
        opacity: 0.4,
        blur: 0,
        size: 'cover',
        position: 'center',
        snapshot_lines: 2000,
        presets: [],
        preview_category_flips: true,
      },
      scrollback: {
        ring_bytes: 262144,
        persist: true,
        restore_on_launch: true,
      },
    },
    claude_local: {
      base_url: 'http://localhost:4000',
      auth_token: 'sk-dummy',
      model_alias: '',
    },
    aider_local: {
      base_url: 'http://localhost:11434/v1',
      auth_token: 'ollama',
      model: '',
    },
    offload: {
      enabled: false,
      autostart: false,
      inject_guidance: true,
      server_command: '',
      tools: {
        read_file: true,
        code_search: true,
        run_command: true,
      },
      allowed_roots: [],
      command_allowlist: [],
      mcp_servers: [],
      backends: [],
      budget_high_water_pct: 80,
      per_tool_result_token_cap: 8000,
      max_steps: 16,
      offload_timeout_secs: 300,
      global_concurrency: null,
    },
    enabled_ai_tabs: ['claude'],
    logging: {
      level: 'info',
      retention: 'weekly',
      content_capture: { enabled: false, retention: 'weekly' },
    },
  };
}

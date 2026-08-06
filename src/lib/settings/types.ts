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
  /// Render the audio waveform over the avatar. Default false.
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

/// Where a TTS/STT model runs. `gpu` prefers the compiled GPU backend and
/// auto-falls-back to CPU if none is usable; `cpu` forces CPU. Switching it
/// live reloads only that model (no restart). Mirrors the Rust
/// `ProcessingDevice` enum.
export type ProcessingDevice = 'gpu' | 'cpu';

export interface TtsSettings {
  /// Master enable for TTS. When false the Kokoro model is unloaded and no
  /// synthesis runs; distinct from `mute` (which keeps the model loaded).
  enabled: boolean;
  /// GPU vs CPU for Kokoro synthesis. Changing it reloads the model live.
  device: ProcessingDevice;
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
  /// GPU vs CPU for Whisper transcription. Changing it reloads the model on
  /// the next recording / preload.
  device: ProcessingDevice;
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
  /// Show the live context-window row (used %, tokens, cache read/creation
  /// split). Also data-gated: nothing shows until a Claude tab pushes one.
  show_context: boolean;
  /// Poll cadence for the usage push file, in seconds. Clamped to a sane
  /// minimum in the UI as busy-poll hygiene.
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

/// Context-window status line for cImp-launched Claude Code tabs.
/// When enabled, the backend injects a session-scoped `--settings`
/// overlay pointing Claude Code's `statusLine` at `cimp --statusline`,
/// which renders a themed context-usage bar. Global (not per-tab) and
/// scoped to cImp sessions only — the user's ~/.claude config is
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
  /// V14 Phase A: open compose AND immediately open the prompt-template
  /// picker popover. Default `Alt+/`.
  open_compose_picker: string | null;
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
  /// Read the active terminal's selection aloud through TTS — the keyboard
  /// equivalent of Ctrl+right-click. Default `Ctrl+Alt+S`.
  speak_selection: string | null;
}

// V20: plain per-tab speak gate. The `[[TTS]]` markup convention was retired
// (TTS is sourced out-of-band and speaks all assistant prose), so the former
// free-text `instructions` field is gone. Kept as an object (not a bare bool)
// to match the Rust `TtsInjection` wire shape.
export interface TtsInjection {
  enabled: boolean;
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

/// V14 Phase F: a user-created Preview tab — an embedded, localhost-scoped
/// child webview, not a subprocess. No `command`/`args`/`cwd`/`env`/PTY
/// fields (unlike the two configs above) since there's nothing to spawn.
export interface PreviewTabConfig {
  kind: 'preview';
  id: string;
  builtin: boolean;
  name: string;
  url: string;
  /// `null` ⇒ the "Desktop" preset (fill the pane, no letterboxing).
  /// Non-null ⇒ letterbox to a fixed CSS-pixel width (mobile/tablet
  /// presets) — see `lib/preview/policy.ts`'s device-preset table.
  device_width: number | null;
  auto_reload: boolean;
}

export type TabConfig = AiToolTabConfig | ShellTabConfig | PreviewTabConfig;

/// V14 Phase F: `PreviewTabConfig` has neither `theme_override` nor
/// `background_override` — it has no terminal to theme at all (no PTY, no
/// xterm). Call sites that read/write those two fields off a `TabConfig`
/// looked up by id (`ConfigureTabDialog.svelte`, `SettingsApp.svelte`,
/// `terminals.ts`) narrow through this helper so they type-check against
/// the now-3-member union. In practice a Preview tab never reaches any of
/// them (it offers no "Configure…" — see `TabContextMenu.svelte` — and gets
/// no terminal entry — see `terminals.ts`'s `createTerminal` guard), so this
/// is a type-level narrowing, not a runtime behavior change.
export type ThemedTabConfig = AiToolTabConfig | ShellTabConfig;

export function asThemedTabConfig(t: TabConfig | undefined): ThemedTabConfig | undefined {
  return t && t.kind !== 'preview' ? t : undefined;
}

export interface SessionState {
  active_tab_id: string | null;
}

export interface ProcessingSettings {
  stability_timeout_ms: number;
  max_hold_ms: number;
}

export interface UiSettings {
  /// Active UI chrome theme. `"tui"` is the built-in ratatui-style theme
  /// (custom title bar, square borders, Gruvbox surfaces) — hardcoded in
  /// the backend binary and always available; its accent comes from
  /// `tui_accent` below. Any other value refers to an on-disk theme under
  /// `<exe-dir>/themes/` (`"nippon-dark"` / `"nippon-light"` ship today).
  /// New installs default to `"tui"` (paired with the OpenCode Grey
  /// terminal palette); the avatar still defaults to the animated
  /// `impSprites` mascot independently. Distinct from `terminal.theme`,
  /// which governs the xterm.js terminal palette inside each tab.
  theme: string;
  /// Accent color of the built-in `"tui"` theme (`#rrggbb`). Injected as
  /// the `--tui-accent` CSS variable (see lib/themes/accent.ts); the theme
  /// CSS derives the whole accent family from it. Only meaningful — and
  /// only editable in Settings — while the `"tui"` theme is active.
  tui_accent: string;
  /// Arrangement of the bottom status bar's movable left cluster.
  status_bar: StatusBarLayout;
  /// Show the reserved Tool Activity tab (unified graph-call + offload
  /// request feed, plus the tool reference lists). Default true.
  tool_activity_tab: boolean;
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
export const CURRENT_SCHEMA_VERSION = 21;

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
  /// Claude Code context-window status line bar (global, cImp-scoped).
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
  /// Optional explicit executable paths for the bundled quick-launch tools
  /// (rustnet / broot); empty fields resolve normally (ebin → PATH).
  external_tools: ExternalToolsSettings;
  /// V8-01: local task-offload config. cImp runs a user-supplied
  /// `llama-server` and exposes an `offload_task` MCP tool into
  /// cImp-launched Claude tabs. Off by default.
  offload: OffloadSettings;
  /// V9-01: per-project code knowledge graph config. Off by default.
  graph: GraphSettings;
  /// V13 Phase A: the Workbench feature (live diff / checkpoints /
  /// worktrees). The tab itself defaults on; checkpoints default off.
  workbench: WorkbenchSettings;
  /// V12 Phase A: project checker commands the `run_check` MCP tool can run
  /// (mirror of Rust `Vec<CheckDef>`). Lives at the root, not inside
  /// `GraphSettings` — independent of the code graph. Empty by default. Edited
  /// in Settings → Checks (V22 Phase E ChecksEditor), which lands the change as
  /// a per-project `.cimp/config.json` overlay diff like any other setting.
  checks: CheckDef[];
  /// V22 Phase D: auto-apply validated detection proposals on first index for a
  /// project with empty `checks`. Default false (propose-then-approve is the
  /// default; this is the fleet opt-in). Rides the per-project overlay.
  checks_auto_configure: boolean;
  /// V22 Phase D: set once the user dismisses the "N suggested checks" nudge for
  /// this project. Per-project (overlay); written by `checks_dismiss_suggestion`.
  checks_suggestion_dismissed: boolean;
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
  /// V14 Phase A: the global-scope prompt-template library. Read/written
  /// through the dedicated `compose_templates_global_*` IPC (which targets
  /// the physical global `settings.json` directly), NOT through the normal
  /// `settingsUpdate` round-trip — see `lib/compose/templates.ts`. Kept here
  /// mainly for schema/round-trip fidelity with the backend struct.
  prompt_templates: PromptTemplate[];
  /// One-shot starter-seed gate; `true` once the backend has seeded the 4
  /// starter templates. See the Rust field's doc comment.
  templates_seeded: boolean;
  /// V14 Phase D2: budget-tuning advisor proposals the user has dismissed.
  /// See `DismissedRule`'s doc comment for the (rule_id, signature) matching
  /// semantics.
  advisor_dismissed: DismissedRule[];
  /// Advisor proposals the user has APPLIED — the Apply-cooldown's memory
  /// (one entry per rule + project root). Written only by the dedicated
  /// `advisor_mark_applied` IPC (`lib/graph.ts` `advisorMarkApplied`); kept
  /// here for schema/round-trip fidelity with the backend struct.
  advisor_applied: AppliedRule[];
  /// V14 Phase F: the last URL entered into any Preview tab in this project
  /// — a fresh "New Preview tab" starts here (falling back to
  /// `lib/preview/policy.ts`'s `DEFAULT_PREVIEW_URL` when `null`). Round-trips
  /// through the ordinary per-project `.cimp/config.json` overlay diff (a
  /// plain scalar, unlike `prompt_templates` — no bespoke read/write path
  /// needed).
  preview_last_url: string | null;
  /// V14 Phase F: global gate on the Preview tab's navigation policy. `false`
  /// (default) restricts navigation to localhost/127.0.0.1/RFC-1918 hosts;
  /// see `lib/preview/policy.ts`'s `isAllowedPreviewHost` (frontend mirror of
  /// the Rust `preview::is_allowed_preview_host`, used for the toolbar's own
  /// pre-flight check before even calling the backend).
  preview_allow_remote: boolean;
  /// Provider/model token-price table ($ per MTok) for the session-cost
  /// popup. Read/written through the dedicated `llm_pricing_*` IPC (physical
  /// global `settings.json` only — see `lib/settings/ipc.ts`), NOT through
  /// the normal `settingsUpdate` round-trip; kept here for schema fidelity
  /// with the backend struct, like `prompt_templates`.
  llm_pricing: LlmPricingModel[];
  /// V16 Feature 1: harness version tripwire + contract state. Out-of-band
  /// like `llm_pricing` (global-only; preserved against stale snapshots by
  /// `apply_incoming_settings`); kept here for schema fidelity.
  harness_versions: HarnessVersions;
  /// V23 Phase A: Code Audit (aggregated security scanning) config. Off by
  /// default; `enabled` gates the reserved Code Audit dashboard tab and the
  /// bottom-bar entry point.
  code_audit: CodeAuditSettings;
}

/// One provider/model price row: USD per million tokens for the four billing
/// categories a session's `UsageTotals` reports. Mirror of Rust
/// `settings::LlmPricingModel`. Fresh installs are seeded backend-side with
/// current Anthropic API + GitHub Copilot prices (`default_llm_pricing`).
export interface LlmPricingModel {
  provider: string;
  model: string;
  /// V16 Feature 8: transcript model-id prefix this row auto-matches (e.g.
  /// `"claude-opus-4-8"`). Longest matching prefix wins; empty = manual-pick
  /// only. See `usageMath.ts`'s `matchPricing`.
  model_prefix: string;
  /// $/MTok for uncached input tokens (`in_tok`).
  input: number;
  /// $/MTok for cache-write tokens (`cache_make`).
  cache_write: number;
  /// $/MTok for cache-read tokens (`cache_read`).
  cache_read: number;
  /// $/MTok for output tokens (`out_tok`).
  output: number;
}

/// V16 Feature 1: per-install harness version + contract-verification state.
/// Mirror of Rust `settings::HarnessVersions`. Out-of-band like
/// `llm_pricing`: written by the transcript tap / tab spawn /
/// `harness_mark_verified`, straight to the physical global `settings.json`.
export interface HarnessVersions {
  claude_last_seen: string;
  claude_last_verified: string;
  opencode_last_seen: string;
  /// E1 spike outcome (`"unverified" | "pass" | "fail"`): a blocking status
  /// (see `harnessStatusBlocks`) hard blocks the read advisor (Settings
  /// block disabled, hook not installed).
  e1_status: string;
  /// D0 spike outcome (informational — the feature degrades to a no-op).
  d0_status: string;
}

/// Whether a recorded spike status blocks its feature — the ONE comparison
/// allowed to interpret `e1_status`/`d0_status` (never compare a bare
/// `'fail'` literal at a call site). Mirror of Rust
/// `HarnessVersions::status_blocks` (settings/schema.rs) — keep in sync.
/// Normalizes (trim + case-fold) and fails CLOSED: the statuses are
/// hand-editable strings, so anything that isn't a recognized non-fail
/// value blocks rather than silently sailing past the gate.
export function harnessStatusBlocks(status: string): boolean {
  const s = status.trim().toLowerCase();
  return !(s === '' || s === 'unverified' || s === 'pass');
}

/// V14 Phase D2: one dismissed advisor proposal. Mirror of Rust
/// `settings::DismissedRule`. `rule_id` is a versioned rule constant (e.g.
/// `"advisor.raise_context_min_score.v1"`); `signature` is the coarse
/// (10%-bucketed) rate that triggered the dismissed proposal — a materially
/// changed rate (different bucket) re-fires the proposal even for the same
/// `rule_id`.
export interface DismissedRule {
  rule_id: string;
  signature: string;
}

/// One APPLIED advisor proposal — the Apply-cooldown's memory. Mirror of
/// Rust `settings::AppliedRule`: the rule stays quiet until `root` has seen
/// a few more sessions than `session_count` (the count at apply time), so
/// the advisor re-evaluates on fresh post-change data instead of instantly
/// re-proposing off the cumulative pre-apply rates.
export interface AppliedRule {
  rule_id: string;
  root: string;
  session_count: number;
}

/// V14 Phase A: one saved prompt-library entry (global or project scope —
/// scope is not part of the stored shape, only of the resolved picker view,
/// see `ResolvedTemplate` in `lib/compose/templates.ts`).
export interface PromptTemplate {
  name: string;
  body: string;
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

/// Which built-in parser decodes a check's output (mirror of Rust
/// `ParserKind`). Wire format is kebab-case.
export type ParserKind =
  | 'cargo-json'
  | 'tsc'
  | 'eslint-json'
  | 'pytest'
  | 'cargo-test'
  | 'jest-json'
  | 'sarif'
  | 'go'
  | 'go-test-json'
  | 'dotnet'
  | 'junit-xml'
  | 'regex-custom'
  | 'generic-gcc';

/// One configured project check the `run_check` MCP tool can run (mirror of
/// Rust `CheckDef`). `cmd` is the full shell command line (cwd = project
/// root); `name` is what a model-supplied `run_check` tool call selects by.
export interface CheckDef {
  name: string;
  cmd: string;
  parser: ParserKind;
  timeout_secs: number;
  /// V22 Phase B: run `cmd` in this directory instead of the project root — a
  /// path relative to the root, confined strictly beneath it (absolute/escaping
  /// paths rejected). Diagnostic file paths are re-rooted back to the project
  /// root, so the report stays root-relative. Always present on the wire
  /// (Rust serializes it unconditionally); `null` means "run at the root".
  cwd: string | null;
  /// V22 Phase B: environment variables forced on the spawned child, as ordered
  /// `[key, value]` pairs (mirror of Rust `Vec<(String, String)>`).
  env: [string, string][];
  /// V22 Phase B2: when set, the parser reads this file's content after the run
  /// instead of stdout — for junit-xml / sarif tools that write a report to
  /// disk. Resolved relative to the check's working directory (`cwd` if set,
  /// else the project root), confined strictly beneath the root — matching how
  /// tools document their output paths (e.g. `mvn` writes `target/surefire-reports`
  /// under its module dir). For back-compat, a `cwd`-set config whose path only
  /// exists at the old root-relative location falls back to that. `null` means
  /// "parse stdout".
  report_file: string | null;
  /// V22 Phase C: the regex for the `regex-custom` parser (ignored by every
  /// other parser). Named groups `file`/`line`/`message` are mandatory,
  /// `col`/`severity` optional; validated at save time (see the Rust
  /// `parsers::validate_pattern`). Always present on the wire; `null` when
  /// unused. Mirror of Rust `Option<String>`.
  pattern: string | null;
  /// V22 Phase D: `true` when this entry was created by language auto-detection
  /// (`checks/detect.rs`) rather than hand-authored. Re-detection may refresh
  /// `auto === true` entries but never touches a `false` one. The ChecksEditor
  /// (Phase E) MUST clear this flag (set `false`) whenever the user edits an
  /// auto entry, so a later re-detection stops fighting the manual change.
  /// Mirror of Rust `CheckDef::auto`; always present on the wire.
  auto: boolean;
}

/// V22 Phase D: one auto-detection proposal (mirror of Rust
/// `checks::detect::Proposal`). Returned by the `checks_detect` IPC; the Phase E
/// editor renders `check` with a checkbox, greying items where `valid === false`
/// and showing `reason`.
export interface ChecksProposal {
  check: CheckDef;
  /// Human ecosystem label (`"Rust"`, `"Go"`, `"TypeScript/JavaScript"`, … ).
  ecosystem: string;
  /// What triggered it — the marker file(s) and/or the code-graph stat.
  evidence: string;
  /// Whether the machine could validate it (marker present + binary on PATH).
  valid: boolean;
  /// Why an invalid proposal can't run; `null` when `valid`.
  reason: string | null;
}

/// V22 Phase D: the passive-nudge payload (mirror of Rust
/// `ChecksSuggestion`). `count` is how many VALID proposals detection found for
/// a project whose `checks` is empty; `dismissed` reflects the per-project
/// `checks_suggestion_dismissed` flag. The chip shows only when
/// `count > 0 && !dismissed`.
export interface ChecksSuggestion {
  count: number;
  dismissed: boolean;
  /// Mirror of `checks_auto_configure` — the chip notes when auto-apply is on.
  auto_configure: boolean;
}

/// V22 Phase D: the `checks_apply_proposals` result — the names actually written
/// (added or refreshed) after the `auto`-ownership merge. Mirror of Rust
/// `ApplySummary`.
export interface ChecksApplySummary {
  applied: string[];
}

/// V22 Phase E: the `checks_test` dry-run result the ChecksEditor renders inline
/// (mirror of Rust `checks::ChecksTestResult`). `diag_count` is the number of
/// deduplicated diagnostic groups; `diagnostics` is the first few of them.
/// `stdout_bytes`/`stderr_bytes` are the raw captured output sizes — the
/// "did the command produce output at all?" signal `classifyTestResult`
/// (`checksEditor.ts`) uses to flag a wrong-parser config (output produced, zero
/// diagnostics) apart from a genuinely clean run. `error` is set (and the rest
/// zeroed) when validation/spawn failed before a report was produced.
export interface ChecksTestResult {
  exit_code: number | null;
  duration_ms: number;
  timed_out: boolean;
  diag_count: number;
  stdout_bytes: number;
  stderr_bytes: number;
  diagnostics: ChecksTestDiag[];
  error: string | null;
}

/// V22 Phase E: one diagnostic group summarized for the Test-button preview
/// (mirror of Rust `checks::TestDiag`).
export interface ChecksTestDiag {
  severity: string;
  message: string;
  /// `"file:line"` sample locations; a location-less group has an empty list.
  sites: string[];
}

/// V9-01: per-project code-knowledge-graph config. Mirror of Rust
/// `GraphSettings`. Only `enabled` and `allow_remote_worker_access` are
/// surfaced in the UI today; the rest carry defaults until the full
/// settings panel (Phase F) lands.
export interface GraphSettings {
  enabled: boolean;
  languages: string[];
  ignore: string[];
  index_docs: boolean;
  max_file_bytes: number;
  watch_debounce_ms: number;
  max_rows_per_query: number;
  max_snippet_bytes: number;
  /// Hard cap on the body bytes returned by `graph_snippet` (V11 Phase A).
  max_body_bytes: number;
  db_subdir: string;
  /// Let the offload worker query the graph when running on a *remote*
  /// backend (LAN or cloud). The local worker always has access; a remote
  /// one would receive the project's code structure, so it's opt-in.
  allow_remote_worker_access: boolean;
  semantic_search: boolean;
  embedding_endpoint: string;
  embedding_model: string;
  embedding_dims: number;
  embed_code_bodies: boolean;
  embedding_batch: number;
  /// Per-input token budget for the embedding endpoint; 0 = auto-detect from
  /// the server's `/props`. Oversized texts are truncated before sending.
  embedding_max_tokens: number;
  /// Project-wide cap on `code_chunk` rows kept by a full rebuild (V11 Phase G).
  semantic_code_max_chunks: number;
  // V10 context injection.
  context_injection: boolean;
  context_per_file_chars: number;
  context_turn_budget_chars: number;
  context_include_session: boolean;
  context_min_score: number;
  // V11 Phase B: repo map (session-start orientation).
  repo_map_budget_chars: number;
  repo_map_on_session_start: boolean;
  // V11 Phase C: injection dedup TTL in turns (0 disables).
  context_dedup_ttl_turns: number;
  // V11 Phase D: feed the compactor the working set + pinned notes.
  compaction_context: boolean;
  // V11 Phase E: redundant-read advisor (opt-in).
  read_advisor: boolean;
  read_advisor_min_lines: number;
  read_advisor_mode: string;
  // V16 Feature 5: trust TTL in retrieve turns (0 = off) — after this many
  // turns since the advisor last observed a full read, a Read passes again.
  read_advisor_ttl_turns: number;
  // V17 Phase A: answer a changed-file re-read with a unified diff against the
  // last-read snapshot instead of the whole file. Default on.
  read_advisor_diffs: boolean;
  // V17 Phase B: also intercept a whole-file shell read (cat/Get-Content/type/gc)
  // of an already-read file via a second PreToolUse Bash matcher. Default on.
  read_advisor_shell: boolean;
  // V17 Phase C: first-read tier — KiB threshold at/above which a first
  // whole-file read of a non-code file is answered with a cached digest +
  // head/tail sample instead of the full content. 0 = off (default).
  read_advisor_first_read_kb: number;
  // V17 Phase E: hide the cold-tail graph tools (cycles, dead_exports,
  // struct_search, path, architecture) from the advertised tool surface.
  // Advertisement-only — they still answer if called by name. Default off.
  lean_tools: boolean;
  // V11 Phase F: local-model context digests (local-only).
  context_llm_digests: boolean;
  // V12 Phase E: memory distillation (durable project facts, local-only).
  memory_distillation: boolean;
  // V12 Phase E: promote PINNED facts into launch-time guidance.
  promote_pinned_facts: boolean;
  // V12 Phase F: proactive automation.
  /// Auto-run configured checks after an edit (`PostToolUse` hook) and inject
  /// only NEW/worsened diagnostics. Opt-in; needs `checks` non-empty.
  auto_check: boolean;
  /// Debounce window (seconds) coalescing a burst of edits into one run.
  auto_check_debounce_s: number;
  /// Minimum direct inbound call count before the auto-impact note appends.
  auto_impact_min_dependents: number;
  /// Re-run dead-exports/import-cycles after each index pass and badge the
  /// Analyses section when the counts changed. On by default (read-only).
  analyses_auto: boolean;
  /// V15 Feature 1: hop bound for `graph_path` shortest-path tracing (1–32).
  path_max_hops: number;
  /// V15 Feature 2: max subsystems `graph_architecture` reports.
  arch_max_communities: number;
  /// V15 Feature 2: ignore communities smaller than this in the report.
  arch_min_community_size: number;
  /// V15 Feature 4 (STRETCH): master toggle for the Graph view live
  /// force-graph (the Tool Activity tab's "Graph view" section — formerly
  /// its own reserved tab, retired in schema v26).
  graph_viz: boolean;
  /// V15 Feature 4: cap on the rendered subgraph node count.
  graph_viz_max_nodes: number;
  /// Graph View tuning — multipliers on the built-in behavior (1.0 =
  /// unchanged): file-node radius, directory-cluster size (leash radius),
  /// edge line width, file↔file spacing, cluster↔cluster spacing, and how
  /// tightly files hug their folder anchor.
  graph_viz_node_scale: number;
  graph_viz_dir_scale: number;
  graph_viz_edge_width: number;
  graph_viz_node_spacing: number;
  graph_viz_cluster_spacing: number;
  graph_viz_cluster_strength: number;
  /// Edge colors (`#rrggbb`) for call and import edges.
  graph_viz_color_call: string;
  graph_viz_color_import: string;
  /// "This session" stacked-bar chart segment colors (`#rrggbb`), edited by
  /// clicking the chart's legend swatches in the Code Intelligence tab.
  usage_color_in: string;
  usage_color_cache: string;
  usage_color_out: string;
  usage_color_tool: string;
  // V16 Feature 8: the cache-write segment's color.
  usage_color_write: string;
  // V24 Phase C follow-up: the S/A lane colors — main-session and sub-agent
  // segments under the chart (the agent color also tints the sub-agent
  // bars' outline).
  usage_color_session: string;
  usage_color_agent: string;
}

/// V13 §0.4: the Workbench feature's settings. Mirror of Rust
/// `WorkbenchSettings`. `enabled` is the master switch for the reserved tab
/// itself (default true — the tab is cheap; each section gates its own
/// behavior); `checkpoints` is the shadow-repo snapshot feature (default
/// false in V1). The five `checkpoint_*` fields tune retention and the
/// debounced burst trigger.
export interface WorkbenchSettings {
  enabled: boolean;
  checkpoints: boolean;
  checkpoint_max: number;
  checkpoint_max_age_days: number;
  checkpoint_burst_files: number;
  checkpoint_burst_window_s: number;
  checkpoint_min_gap_s: number;
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

/// Optional explicit executable paths for the bundled quick-launch tools
/// (rustnet / broot). A non-empty value overrides the normal `ebin/` → PATH
/// resolution for that tool, letting the user point at an exe in any folder.
/// Empty (the default) means "resolve normally". Mirrors the backend's
/// `ExternalToolsSettings`.
export interface ExternalToolsSettings {
  rustnet: string;
  broot: string;
}

/// V23 Phase A / V25 Phase B: closed set of built-in audit tools (mirror of
/// Rust `AuditToolId`). Wire format is kebab-case; an unknown id in a settings
/// file is dropped backend-side (forward compat), never an error. The first
/// three are the Security category; the rest are V25 Quality tools.
export type AuditToolId =
  | 'osv-scanner'
  | 'gitleaks'
  | 'semgrep'
  | 'oxlint'
  | 'golangci-lint'
  | 'ruff'
  | 'cppcheck'
  | 'typos'
  | 'eslint'
  | 'pmd'
  | 'dotnet-analyzers'
  | 'knip'
  | 'cargo-machete'
  | 'semgrep-quality';

/// V23 Phase A: one configured audit tool (mirror of Rust `AuditToolConfig`).
/// `path` empty = resolve `ebin` → PATH; non-empty = used as the command
/// verbatim. `extra_args` are appended after the adapter's fixed argv (a
/// semgrep ruleset swap belongs in `ruleset`, not here).
export interface AuditToolConfig {
  id: AuditToolId;
  enabled: boolean;
  path: string;
  extra_args: string[];
  /// Ruleset override for the tools with a ruleset selector: the two semgrep
  /// tools (`--config <slug>`) and PMD (`-R <ruleset>`). Empty uses the
  /// adapter's built-in value; exists so an upstream-owned default breaking
  /// (e.g. a slug vanishing from the semgrep registry) is a settings edit,
  /// not a rebuild. Ignored by every other tool.
  ruleset: string;
  /// V25 Phase C: per-tool wall-clock timeout override in seconds. `null` (the
  /// default) falls back to the global `CodeAuditSettings.timeout_secs`. A
  /// build-style tool wants a longer budget than a linter — `dotnet-analyzers`
  /// is the motivating case (≈1200 recommended).
  timeout_secs: number | null;
}

/// V23 Phase A: Code Audit (aggregated security scanning) config (mirror of
/// Rust `CodeAuditSettings`). Off by default; `enabled` gates the "Code audit"
/// section inside the Tool Activity tab (its reserved tab was retired in
/// schema v27) and the bottom-bar entry.
export interface CodeAuditSettings {
  enabled: boolean;
  tools: AuditToolConfig[];
  timeout_secs: number;
  /// Keep the QUALITY tools' `enabled` flags following the project's language
  /// census automatically (default true). Editing a quality checkbox flips
  /// this to false (manual mode); the Settings section's "Auto-select for this
  /// project" button turns it back on. Security tools are never touched.
  quality_auto_select: boolean;
  /// V26: advertise the `cimp-code-audit` MCP server (security_audit /
  /// quality_audit) to Claude Code tabs. ANDed with `enabled` at the injection
  /// site; default true. (Backend field mirror — the settings-UI checkboxes
  /// land in a later stage.)
  expose_claude: boolean;
  /// V26: advertise `cimp-code-audit` to OpenCode tabs. OpenCode caches
  /// tools/list at connect, so toggling needs a tab restart. Default true.
  expose_opencode: boolean;
  /// V26: advertise the code-audit native tools to the offload worker. Default
  /// true. A scan always runs in-process, so a remote worker only ever gets the
  /// free-text report.
  expose_offload: boolean;
}

/// V23 Phase A: the `audit_detect_tool` IPC result (mirror of Rust
/// `AuditDetectResult`). Display-only — the Detect button renders it inline and
/// never writes the resolved path back into the tool's `path` field.
export interface AuditDetectResult {
  found: boolean;
  path: string | null;
  version: string | null;
  error: string | null;
}

/// V8-01: native baseline offload tool toggles (mirror of Rust
/// `OffloadToolToggles`).
export interface OffloadToolToggles {
  read_file: boolean;
  /// V21: directory enumeration — the ground-truth "what files exist / how many".
  list_dir: boolean;
  code_search: boolean;
  run_command: boolean;
  /// V21: run a configured project check (build/typecheck/lint/test) and get
  /// back deduplicated diagnostics. Inert until the top-level `checks` array is
  /// non-empty (gated identically to the `run_check` MCP tool).
  run_check: boolean;
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
  /// Expose this server's tools to Claude Code (proxied through the
  /// `cimp-offload` child). Off by default — a deliberate opt-in.
  claude_access: boolean;
  /// Expose this server's tools to the offload worker (the legacy `enabled`).
  offload_access: boolean;
  /// V19: expose this server's tools to OpenCode (proxied through the
  /// `--consumer opencode` child). Off by default.
  opencode_access: boolean;
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
export const LOCAL_DATA_TOOLS = ['read_file', 'list_dir', 'code_search', 'run_command', 'filesystem', 'git'];

/// V8-02: kind-specific config for one backend (mirror of Rust
/// `OffloadBackendKind`). Local = cImp owns the process; Remote = a
/// health-checked URL (LAN or cloud).
export type OffloadBackendKind =
  | {
      type: 'local';
      server_command: string;
      autostart: boolean;
      /// When true, the Offload server dashboard's Start button opens an editable
      /// confirm popup with the command; edits apply to that launch only.
      show_command_on_start: boolean;
    }
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
/// One environment variable a command policy forces at spawn (mirror of Rust
/// `CommandEnvVar`). An ordered list of these, not a map, to match the backend.
export interface CommandEnvVar {
  key: string;
  value: string;
}

/// A per-program command security policy applied by `run_command` on top of the
/// allowlist (mirror of Rust `CommandPolicy`). `program` matches the allowlisted
/// command by file-stem (case-insensitive). A `denied_flags` entry refuses an
/// argument that equals it or starts with `<entry>=`; `denied_subcommands`
/// refuses the first non-flag token; `env` vars are forced at spawn.
export interface CommandPolicy {
  program: string;
  denied_flags: string[];
  denied_subcommands: string[];
  /// V21 F7: when non-empty, an allowlist over the first non-flag argument —
  /// only these subcommands may run, every other (and a bare invocation) is
  /// refused. The strict counterpart to `denied_subcommands`, used to pin a
  /// program to a few read-only verbs (e.g. `cargo` → `metadata`/`tree`).
  allowed_subcommands: string[];
  env: CommandEnvVar[];
}

/// A named, reusable `llama-server` launch command saved globally and pasted
/// back into a Local backend's `Server command` field via the Pool editor's
/// Save/Load/Delete controls (mirror of Rust `ServerCommandTemplate`).
export interface ServerCommandTemplate {
  name: string;
  command: string;
}

/// A named, reusable Remote-backend endpoint (base URL + auth token) saved
/// globally and pasted back into a Remote backend's fields via the same
/// Save/Load/Delete controls (mirror of Rust `RemoteBackendTemplate`). The
/// `auth_token` is stored cleartext on disk, like the backend's own token.
export interface RemoteBackendTemplate {
  name: string;
  base_url: string;
  auth_token: string;
}

export interface OffloadSettings {
  enabled: boolean;
  autostart: boolean;
  inject_guidance: boolean;
  server_command: string;
  tools: OffloadToolToggles;
  allowed_roots: string[];
  command_allowlist: string[];
  /// Per-program security policies layered on top of the allowlist. Seeded
  /// with a default `git` policy (see `defaultSettings`).
  command_policies: CommandPolicy[];
  mcp_servers: McpServerConfig[];
  backends: OffloadBackend[];
  /// Saved, reusable server-command templates (see `ServerCommandTemplate`).
  /// A convenience library only — nothing reads these at runtime.
  server_command_templates: ServerCommandTemplate[];
  /// Saved, reusable Remote-backend endpoints (see `RemoteBackendTemplate`).
  remote_backend_templates: RemoteBackendTemplate[];
  budget_high_water_pct: number;
  per_tool_result_token_cap: number;
  max_steps: number;
  offload_timeout_secs: number;
  /// V8-03: global cap on offloads in flight across the whole app. `null`
  /// lets the service auto-size it from the summed per-backend slot counts.
  global_concurrency: number | null;
  /// Max tasks allowed to wait for a slot when the pool is saturated. `null`
  /// = unbounded blocking queue; a number fast-rejects once that many are
  /// already waiting on busy slots.
  max_queue_depth: number | null;
  /// V21 F5: when true (default), a fast-tier offload that comes back only
  /// partially verified is re-run once on a distinct, ready quality backend
  /// (the better answer wins). Inert unless a second quality-tier backend
  /// exists, so zero-config setups are unaffected.
  escalate_partial: boolean;
  /// V21: the OpenCode `local-llama` custom provider, derived from a Local
  /// backend's server command via the Offload "Add to OpenCode" button (or
  /// auto-sync). When set, the OpenCode tab gets a `provider.local-llama` block
  /// + this as its default model. `null` = never registered.
  opencode_provider: OpencodeLocalProvider | null;
  /// V21: when true AND the local offload server is enabled, keep
  /// `opencode_provider` in sync with the primary Local backend's command
  /// (re-derived at launch + on save when it changed). Disabled server ⇒
  /// no-op.
  opencode_provider_auto: boolean;
  /// V30 Phase A: register the `cimp-offload` MCP child as a Claude Code
  /// *channel*, so it can push out-of-band notices (offload/audit/graph
  /// completion) straight into a live Claude tab. Claude-only, spawn-baked
  /// (flips raise the AI-tab restart hint), default off — the registration
  /// flag is a research preview and pushes are fire-and-forget.
  session_push: boolean;
  /// V32 (injection hardening, locked decision 11): cap on how many EXTERNAL
  /// (proxied MCP-server) tool calls one contaminated scope may make — a
  /// worker task, or an agent/tab session at the loopback proxy. `0` disables
  /// the count cap.
  external_fetch_max_calls: number;
  /// V32: cap on the cumulative bytes of EXTERNAL tool results one scope may
  /// pull. `0` disables the byte cap.
  external_fetch_max_bytes: number;
}

/// V21: a derived OpenCode custom-provider entry (always id `local-llama`)
/// pointing at the local `llama-server` (mirror of Rust `OpencodeLocalProvider`).
export interface OpencodeLocalProvider {
  /// OpenAI-compatible base URL, ending in `/v1`.
  base_url: string;
  /// Model id OpenCode requests + selects as default (`local-llama/<model>`).
  model: string;
  /// Optional `--api-key` from the command; usually empty.
  api_key: string;
  /// The server command this was derived from (drives auto-sync change checks).
  source_command: string;
}

/// Reserved tab ids — mirror of `crate::settings::*_TAB_ID` constants.
/// User-created shell tabs use uuid-based ids that never collide with these.
export const CLAUDE_TAB_ID = 'claude';
/// V1.4-07: second Claude tab preconfigured for a local LLM provider.
export const CLAUDE_LOCAL_TAB_ID = 'claude-local';
/// V19: the single OpenCode AI-tool tab. OpenCode picks its own provider/model
/// (global config + credentials, switchable in-session), so there is no
/// cloud/local pair. Replaces BOTH V14 aider ids (the v18 → v19 migration
/// collapses them into this one).
export const OPENCODE_TAB_ID = 'opencode';
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
      enabled: true,
      device: 'gpu',
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
      device: 'gpu',
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
      opacity: 0.5,
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
        visible: false,
        color: '',
        line_width: 2.0,
        glow_intensity: 0.6,
        opacity: 0.85,
      },
    },
    display: {
      terminal_font_family: 'Consolas, Menlo, "DejaVu Sans Mono", monospace',
      terminal_font_size: 14,
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
      show_context: true,
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
      open_compose_picker: 'Alt+/',
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
      speak_selection: 'Ctrl+Alt+S',
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
        tts_injection: { enabled: true },
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
        use_local_provider: false,      },
      {
        kind: 'ai_tool',
        id: CLAUDE_LOCAL_TAB_ID,
        builtin: true,
        name: 'Claude (local)',
        command: 'claude',
        args: [],
        cwd: null,
        env: {},
        tts_injection: { enabled: true },
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
        use_local_provider: true,      },
    ],
    processing: { stability_timeout_ms: 200, max_hold_ms: 500 },
    session: { active_tab_id: null },
    layout: null,
    layout_presets: [],
    ui: {
      theme: 'tui',
      tui_accent: '#7aa2f7',
      status_bar: {
        items: [
          { component: 'usage', gap: 0 },
          { component: 'system_stats', gap: 0 },
        ],
      },
      tool_activity_tab: true,
    },
    // Default terminal palette is paired with the default UI theme
    // (tui → OpenCode Grey); the pairing comes from each theme's
    // `palette` metadata, applied by SettingsApp on theme switch.
    terminal: {
      theme: { name: 'OpenCode Grey', custom: null },
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
    external_tools: {
      rustnet: '',
      broot: '',
    },
    offload: {
      enabled: false,
      autostart: false,
      inject_guidance: true,
      server_command: '',
      tools: {
        read_file: true,
        list_dir: true,
        code_search: true,
        run_command: true,
        run_check: true,
      },
      allowed_roots: [],
      command_allowlist: [],
      command_policies: [
        {
          program: 'git',
          denied_flags: [
            '-c',
            '-C',
            '--config-env',
            '--exec-path',
            '--git-dir',
            '--work-tree',
            '--upload-pack',
            '--receive-pack',
            '--namespace',
            '--super-prefix',
            '--attr-source',
          ],
          denied_subcommands: ['config'],
          allowed_subcommands: [],
          env: [
            { key: 'GIT_PAGER', value: 'cat' },
            { key: 'PAGER', value: 'cat' },
            { key: 'GIT_SSH_COMMAND', value: '' },
            { key: 'GIT_TERMINAL_PROMPT', value: '0' },
            { key: 'GIT_CONFIG_NOSYSTEM', value: '1' },
            { key: 'GIT_CONFIG_GLOBAL', value: '/dev/null' },
          ],
        },
      ],
      mcp_servers: [],
      backends: [],
      server_command_templates: [],
      remote_backend_templates: [],
      budget_high_water_pct: 80,
      per_tool_result_token_cap: 8000,
      max_steps: 16,
      offload_timeout_secs: 300,
      global_concurrency: null,
      max_queue_depth: null,
      escalate_partial: true,
      opencode_provider: null,
      opencode_provider_auto: false,
      session_push: false,
      external_fetch_max_calls: 40,
      external_fetch_max_bytes: 4 * 1024 * 1024,
    },
    graph: {
      enabled: false,
      languages: [
        'rust', 'typescript', 'javascript', 'python', 'markdown',
        'go', 'java', 'c', 'cpp', 'csharp', 'php', 'bash', 'scala',
        'ocaml', 'ruby', 'haskell', 'kotlin', 'swift', 'sql', 'erlang',
        'r', 'perl', 'ada',
      ],
      ignore: [],
      index_docs: true,
      max_file_bytes: 1_048_576,
      watch_debounce_ms: 300,
      max_rows_per_query: 100,
      max_snippet_bytes: 2_000,
      max_body_bytes: 16_384,
      db_subdir: '.cimp',
      allow_remote_worker_access: false,
      semantic_search: false,
      embedding_endpoint: '',
      embedding_model: '',
      embedding_dims: 0,
      embed_code_bodies: false,
      embedding_batch: 32,
      embedding_max_tokens: 0,
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
      read_advisor_mode: 'advise',
      read_advisor_ttl_turns: 0,
      read_advisor_diffs: true,
      read_advisor_shell: true,
      read_advisor_first_read_kb: 0,
      lean_tools: false,
      context_llm_digests: false,
      memory_distillation: false,
      promote_pinned_facts: false,
      auto_check: false,
      auto_check_debounce_s: 5,
      auto_impact_min_dependents: 10,
      analyses_auto: true,
      path_max_hops: 8,
      arch_max_communities: 12,
      arch_min_community_size: 3,
      graph_viz: false,
      graph_viz_max_nodes: 1500,
      graph_viz_node_scale: 1.0,
      graph_viz_dir_scale: 1.0,
      graph_viz_edge_width: 1.0,
      graph_viz_node_spacing: 1.0,
      graph_viz_cluster_spacing: 1.0,
      graph_viz_cluster_strength: 1.0,
      graph_viz_color_call: '#4fb3ff',
      graph_viz_color_import: '#ff8a3d',
      usage_color_in: '#58a6ff',
      usage_color_cache: '#d2a8ff',
      usage_color_out: '#3fb950',
      usage_color_tool: '#f0c674',
      usage_color_write: '#e3738d',
      usage_color_session: '#30363d',
      usage_color_agent: '#3b6ea5',
    },
    workbench: {
      enabled: true,
      checkpoints: false,
      checkpoint_max: 100,
      checkpoint_max_age_days: 7,
      checkpoint_burst_files: 5,
      checkpoint_burst_window_s: 60,
      checkpoint_min_gap_s: 120,
    },
    checks: [],
    checks_auto_configure: false,
    checks_suggestion_dismissed: false,
    enabled_ai_tabs: ['claude'],
    logging: {
      level: 'info',
      retention: 'weekly',
      content_capture: { enabled: false, retention: 'weekly' },
    },
    prompt_templates: [],
    templates_seeded: false,
    advisor_dismissed: [],
    advisor_applied: [],
    preview_last_url: null,
    preview_allow_remote: false,
    // Real defaults are seeded Rust-side (`default_llm_pricing`); this local
    // fallback is only pre-init UI state and is never written back (the
    // field is out-of-band in `settings_update`, like `prompt_templates`).
    llm_pricing: [],
    harness_versions: {
      claude_last_seen: '',
      claude_last_verified: '',
      opencode_last_seen: '',
      e1_status: 'unverified',
      d0_status: 'unverified',
    },
    code_audit: {
      enabled: false,
      quality_auto_select: true,
      expose_claude: true,
      expose_opencode: true,
      expose_offload: true,
      tools: [
        // Security (V23).
        { id: 'osv-scanner', enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        { id: 'gitleaks', enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        { id: 'semgrep', enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        // Quality (V25) — enabled by default.
        { id: 'oxlint', enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        { id: 'golangci-lint', enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        { id: 'ruff', enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        { id: 'cppcheck', enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        { id: 'typos', enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        { id: 'eslint', enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        { id: 'pmd', enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        { id: 'knip', enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        { id: 'cargo-machete', enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        // Quality — default-disabled.
        { id: 'dotnet-analyzers', enabled: false, path: '', extra_args: [], ruleset: '', timeout_secs: null },
        { id: 'semgrep-quality', enabled: false, path: '', extra_args: [], ruleset: '', timeout_secs: null },
      ],
      timeout_secs: 600,
    },
  };
}

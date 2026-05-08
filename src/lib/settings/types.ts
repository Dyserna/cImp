// Frontend mirror of the Rust Settings schema. Field names use snake_case to
// match the JSON serde output. Optional fields come through as `null` over
// JSON, never as `undefined`.

import type { LayoutNode } from '../layout/types';

export type AvatarPosition = 'top-right' | 'top-left' | 'bottom-right' | 'bottom-left';

export interface AvatarSize {
  width_px: number;
  height_px: number;
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
  color: string;
  line_width: number;
  glow_intensity: number;
  opacity: number;
}

export interface AvatarSettings {
  visible: boolean;
  size: AvatarSize;
  position: AvatarPosition;
  margin_px: number;
  opacity: number;
  images: AvatarImages;
  transition: TransitionSettings;
  waveform: WaveformSettings;
}

export interface TtsSettings {
  voice: string;
  speed: number;
  volume: number;
  mute: boolean;
}

export interface DisplaySettings {
  terminal_font_family: string;
  terminal_font_size: number;
  show_tts_markup: boolean;
}

export interface BehaviorSettings {
  interrupt_on_input: boolean;
  auto_speak: boolean;
  fallback_silent: boolean;
  announcements_enabled: boolean;
  /// When true, the frontend keeps `tts.mute` in sync with the inverse of
  /// `avatar.visible` — hide → mute, show → unmute. Wired in App.svelte.
  follow_avatar: boolean;
  /// When true, tab announcements fire even for the currently-focused tab.
  /// Default false reproduces the historical background-only behavior.
  announce_focused_tab: boolean;
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
}

export interface TtsInjection {
  enabled: boolean;
  instructions: string;
}

export interface AiNotificationConfig {
  idle: string;
  awaiting_permission: string;
  /// Spoken when a `kind: question` pattern fires (AskUserQuestion-style
  /// multi-option prompts). Empty string disables the announcement.
  question: string;
  error: string;
}

export interface ShellNotificationConfig {
  error: string;
  /// `{code}` placeholder is interpolated with the actual exit code in M4.
  exited: string;
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

export type TabConfig = AiToolTabConfig | ShellTabConfig;

export interface SessionState {
  active_tab_id: string | null;
}

export interface ProcessingSettings {
  stability_timeout_ms: number;
  max_hold_ms: number;
}

export interface UiSettings {
  /// Active UI chrome theme. V5-01 ships only `"modern-dark"`; the field
  /// exists so future themes (light, high-contrast) plug in without UI
  /// plumbing churn. Distinct from `terminal.theme`, which governs the
  /// xterm.js terminal palette inside each tab.
  theme: string;
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

export interface Settings {
  tts: TtsSettings;
  avatar: AvatarSettings;
  display: DisplaySettings;
  behavior: BehaviorSettings;
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

/// Reserved tab ids — mirror of `crate::settings::*_TAB_ID` constants.
/// User-created shell tabs use uuid-based ids that never collide with these.
export const CLAUDE_TAB_ID = 'claude';
/// V1.4-07: replaces the pre-V1.4-07 `AIDER_TAB_ID = 'aider'`. The
/// v1.7 → v1.8 migration rewrites the aider tab to this id in place.
export const CLAUDE_LOCAL_TAB_ID = 'claude-local';
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
    tts: { voice: 'af_heart', speed: 1.0, volume: 1.0, mute: false },
    avatar: {
      visible: true,
      size: { width_px: 240, height_px: 240 },
      position: 'top-right',
      margin_px: 16,
      opacity: 0.8,
      images: {
        idle: null,
        listening: null,
        thinking: null,
        speaking: null,
        error: null,
      },
      transition: { path: '/avatar/Transition.mp4', duration_ms: 400 },
      waveform: {
        color: '#bb55ff',
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
      interrupt_on_input: true,
      auto_speak: true,
      fallback_silent: true,
      announcements_enabled: true,
      follow_avatar: false,
      announce_focused_tab: false,
    },
    compose: { min_height_px: 80, max_height_px: 300 },
    shortcuts: {
      open_compose: 'Ctrl+Shift+E',
      submit_compose: 'Ctrl+Enter',
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
      split_pane_vertical: 'Ctrl+Shift+\\',
      close_pane: 'Ctrl+Shift+W',
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
          idle: 'Claude is idle',
          awaiting_permission: 'Claude is awaiting permission',
          question: 'Claude has a question',
          error: 'Claude encountered an error',
        },
        first_launch_notice_dismissed: true,
        theme_override: null,
        background_override: null,
        use_local_provider: false,
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
          idle: 'Claude (local) is idle',
          awaiting_permission: 'Claude (local) is awaiting permission',
          question: 'Claude (local) has a question',
          error: 'Claude (local) encountered an error',
        },
        first_launch_notice_dismissed: true,
        theme_override: null,
        background_override: null,
        use_local_provider: true,
      },
    ],
    processing: { stability_timeout_ms: 200, max_hold_ms: 500 },
    session: { active_tab_id: null },
    layout: null,
    layout_presets: [],
    ui: { theme: 'modern-dark' },
    terminal: {
      theme: { name: 'Default', custom: null },
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
  };
}

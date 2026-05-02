// Frontend mirror of the Rust Settings schema. Field names use snake_case to
// match the JSON serde output (no need for a serde rename pass on the
// backend). Optional fields come through as `null` over JSON, never as
// `undefined`.

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
  theme: string;
  show_tts_markup: boolean;
}

export interface BehaviorSettings {
  interrupt_on_input: boolean;
  auto_speak: boolean;
  fallback_silent: boolean;
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
}

export interface ClaudeCodeSettings {
  extra_cli_args: string[];
  claude_md_path: string | null;
}

export interface ProcessingSettings {
  stability_timeout_ms: number;
  max_hold_ms: number;
}

export interface Settings {
  tts: TtsSettings;
  avatar: AvatarSettings;
  display: DisplaySettings;
  behavior: BehaviorSettings;
  compose: ComposeSettings;
  shortcuts: ShortcutSettings;
  claude_code: ClaudeCodeSettings;
  processing: ProcessingSettings;
}

// Defaults must exactly match `impl Default for Settings` on the backend.
// They're used as the initial store value so subscribers get sane shapes
// before the first `settings-changed` event arrives.
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
      theme: 'dark',
      show_tts_markup: false,
    },
    behavior: {
      interrupt_on_input: true,
      auto_speak: true,
      fallback_silent: true,
    },
    compose: { min_height_px: 80, max_height_px: 300 },
    shortcuts: {
      open_compose: 'Ctrl+Shift+E',
      submit_compose: 'Ctrl+Enter',
      cancel_compose: 'Escape',
      open_settings: 'Ctrl+,',
    },
    claude_code: { extra_cli_args: [], claude_md_path: null },
    processing: { stability_timeout_ms: 200, max_hold_ms: 500 },
  };
}

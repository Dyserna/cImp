//! Avatar, TTS/STT, selection highlight, waveform and display settings.
//!
//! Split out of `schema.rs` by V42 R10; see the module docs in `mod.rs`.

use super::*;

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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub enum ProcessingDevice {
    #[default]
    Gpu,
    Cpu,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
            opacity: 0.5,
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub enum AvatarPosition {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct AvatarImages {
    pub idle: Option<PathBuf>,
    pub listening: Option<PathBuf>,
    pub thinking: Option<PathBuf>,
    pub speaking: Option<PathBuf>,
    pub error: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct WaveformSettings {
    /// Hex color the waveform stroke and the avatar's outline use. Empty
    /// string means "follow the active UI theme" — the frontend resolves
    /// it from the `--waveform-color` CSS variable in `theme.css`. A
    /// non-empty value is a user override that wins regardless of theme.
    /// Render the audio waveform over the avatar. When false the waveform
    /// canvas is hidden entirely (the avatar itself is unaffected). Toggled
    /// by the "Show waveform" checkbox in Settings → Avatar → Waveform.
    /// Defaults off.
    pub visible: bool,
    pub color: String,
    pub line_width: f32,
    pub glow_intensity: f32,
    pub opacity: f32,
}

impl Default for WaveformSettings {
    fn default() -> Self {
        Self {
            visible: false,
            color: String::new(),
            line_width: 2.0,
            glow_intensity: 0.6,
            opacity: 0.85,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct DisplaySettings {
    pub terminal_font_family: String,
    pub terminal_font_size: u32,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            terminal_font_family: "Consolas, Menlo, \"DejaVu Sans Mono\", monospace".to_string(),
            terminal_font_size: 14,
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct TtsInjection {
    pub enabled: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
    use serde_json::json;

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
        assert_eq!(
            serde_json::to_value(ProcessingDevice::Gpu).unwrap(),
            json!("gpu")
        );
        assert_eq!(
            serde_json::to_value(ProcessingDevice::Cpu).unwrap(),
            json!("cpu")
        );
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
}

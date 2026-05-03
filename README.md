# cctts

Claude Code wrapper with text-to-speech and a state-driven animated avatar.
A Tauri desktop app that wraps the `claude` CLI in an embedded terminal,
extracts `[[TTS]]…[[/TTS]]` markers from its output, synthesizes them with a
local Kokoro ONNX model, and drives an avatar overlay (Idle / Listening /
Thinking / Speaking / Error) plus a real-time waveform.

Local, offline-after-install, no audio leaves the machine.

## System Requirements

- **OS:** Windows 10/11 (primary). Linux is feasible but not part of the
  v1 validation matrix — see `docs/MILESTONE-08-polish.md`.
- **GPU:** optional. The app defaults to CPU inference (Kokoro is small
  enough for near-real-time CPU). NVIDIA CUDA 12.x can be opted into via
  `setx CCTTS_GPU cuda` and a restart — see `MAINTENANCE.md` for the
  current GPU support matrix and Blackwell caveat.
- **Claude Code:** the `claude` binary must be on `PATH`. cctts spawns it
  as a subprocess and passes `--append-system-prompt` so Claude knows to
  emit the TTS markers.
- **WebView2 (Windows):** preinstalled on updated Windows 10/11. Older
  systems may need the WebView2 runtime installed manually.

## Installing the Kokoro Model

cctts ships without the Kokoro model files because they're large
(hundreds of MB) and have their own license. You provide them.

Place these two files under `%APPDATA%\cctts\models\`:

```
%APPDATA%\cctts\models\kokoro-v1.0.onnx
%APPDATA%\cctts\models\voices\af_heart.bin
```

Download from
[onnx-community/Kokoro-82M-v1.0-ONNX](https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/tree/main):

- model: `onnx/model.onnx` → rename to `kokoro-v1.0.onnx`
- voices: `voices/<name>.bin` — `af_heart.bin` is the default; any voicepack in `voices/` shows up in the settings dropdown.

If the files are missing at startup the app launches with TTS silent and
prints the expected paths to the log.

## Configuring Claude Code

cctts embeds a runtime system prompt (`src-tauri/src/tts/runtime_prompt.md`)
that explains the `[[TTS]]…[[/TTS]]` markup convention to Claude on every
launch. You don't need to add anything to your project's `CLAUDE.md`.

To override the embedded prompt with your own text, set
**Settings → Claude Code → CLAUDE.md** to a markdown file. cctts reads it
on each Claude Code restart and passes it via `--append-system-prompt`.

## Running

```
npm install
npm run tauri dev
```

For a release build:

```
npm run tauri build
```

See `PACKAGING.md` for distribution considerations.

## Settings Overview

Open with `Ctrl+,` or the cog button on the avatar.

- **TTS:** voice picker (auto-discovered from `voices/`), speed, volume,
  mute.
- **Avatar:** visibility, position, size, opacity, per-state image / video
  overrides, transition video + duration. Empty transition path or
  `duration = 0` falls back to a 150 ms crossfade.
- **Waveform:** color, line width, glow, opacity.
- **Display:** terminal font family + size, theme, toggle to render TTS
  markup verbatim in the terminal (debug aid).
- **Behavior:** interrupt TTS on input, auto-speak detected segments.
- **Compose:** min/max sheet height for the slide-up compose overlay.
- **Shortcuts:** rebindable open/submit/cancel for compose, open settings.
- **Claude Code:** extra CLI flags, custom CLAUDE.md path.
- **Processing:** stability and max-hold timers for the byte-burst /
  segment-detection pipeline.

Settings persist to `%APPDATA%\cctts\settings.json` (debounced save).

## Troubleshooting

- **TTS silent.** Check the log for `TTS disabled: Kokoro model files not found.`
  Place the model + voicepack under `%APPDATA%\cctts\models\` as documented above.
- **`claude` not found.** cctts looks up `claude` via `PATH`. Either install
  Claude Code so it's on `PATH` or add its install dir.
- **CUDA EP errors per segment (silent output).** You're on a GPU not yet
  covered by the bundled ORT 1.20 prebuilt (Blackwell / RTX 5090). Unset
  `CCTTS_GPU` to fall back to CPU. See `MAINTENANCE.md`.
- **Audio interrupted by typing.** That's `Behavior → Interrupt TTS when typing`.
  Disable it if you'd rather keep playback rolling.
- **Avatar doesn't move.** Confirm `Settings → Avatar → Visible` is on
  and the per-state image paths point to readable files (or are blank to use
  the bundled defaults).

## Known Limitations (v1)

- Single audio output device — no UI selector.
- No conversation/session UI on top of the terminal.
- No STT input.
- No voice mixing.
- Linux is not part of v1 validation; behavior is best-effort.
- Kokoro model is user-provided, not bundled.

See `docs/MILESTONE-08-polish.md` "After Milestone 8"
for the post-v1 parking lot.

## License

Apache-2.0. See `LICENSE`.

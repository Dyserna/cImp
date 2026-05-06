# cctts

Multi-tab AI assistant wrapper with text-to-speech and a state-driven
animated avatar. A Tauri desktop app that hosts two CLI tabs in v2 —
**Claude Code** and **Aider** — extracts `[[TTS]]…[[/TTS]]` markers from
output, synthesizes them with a local Kokoro ONNX model, and drives a
per-tab avatar overlay (Idle / Listening / Thinking / Speaking / Error)
plus a real-time waveform.

Local, offline-after-install, no audio leaves the machine.

## v2: Multi-Tab Support

- Two tabs ship in v2: **Claude Code** (default) and **Aider**.
- Switch with `Ctrl+1` (Claude Code) and `Ctrl+2` (Aider), or click the tab.
- Both subprocesses spawn at app launch in the directory cctts was started
  in. They run independently — switching tabs doesn't stop either one.
- The compose overlay submits to whichever tab is currently active.

## Shell Tabs (v1.2+)

In addition to the two AI builtins, cctts hosts **Shell tabs** — plain
configurable terminal sessions running alongside Claude and Aider, with no
TTS, no permission detection, and a reduced notification set (`error` and
`exited` only).

### Creating and managing Shell tabs

- **Create:** click the `+` button at the right end of the tab bar, or press
  `Ctrl+T`. The New Shell Tab dialog pre-fills the platform default shell.
- **Rename:** right-click a tab → *Rename*, or double-click the tab name.
- **Configure:** right-click a Shell tab → *Configure…* to change command,
  args, working directory, or notification text. Spawn-affecting changes
  apply on the next shell restart.
- **Restart shell:** right-click → *Restart shell* kills the running
  subprocess and respawns it with the current configuration. Useful after
  changing the command in Configure.
- **Close:** click the `×` on the tab, or press `Ctrl+W` while the tab is
  active. Builtin tabs (Claude, Aider) cannot be closed.
- **Switch by position:** `Ctrl+1`..`Ctrl+9` switch to the tab at that
  ordinal position. `Ctrl+9` with fewer than 9 tabs is a silent no-op.

### Default shell per platform

| Platform | Default                                                       |
|----------|---------------------------------------------------------------|
| Windows  | Git Bash (auto-detected) — falls back to PowerShell           |
| Linux    | `$SHELL` env var — falls back to `/bin/bash`, then `/bin/sh`  |

Git Bash auto-detection on Windows probes, in order:

1. `C:\Program Files\Git\bin\bash.exe`
2. `C:\Program Files (x86)\Git\bin\bash.exe`
3. `HKLM\SOFTWARE\GitForWindows\InstallPath` (registry)
4. `bash.exe` resolvable on `PATH`

If none match, the new-tab dialog shows a banner explaining the fallback to
PowerShell and how to enable Git Bash by default.

### Using an alternative shell

Common alternatives, paste into Configure → command + arguments:

| Shell             | Command                | Arguments      |
|-------------------|------------------------|----------------|
| WSL (Windows)     | `wsl.exe`              | `-d Ubuntu`    |
| PowerShell Core   | `pwsh.exe`             | `-NoLogo`      |
| Windows cmd       | `cmd.exe`              | `/K`           |
| zsh on Linux      | `/usr/bin/zsh`         | `-i`           |

### Shell tab troubleshooting

- **My Shell tab on Windows opens PowerShell, not Git Bash.** Git for
  Windows isn't installed at a standard location and isn't on `PATH`.
  Install Git for Windows from gitforwindows.org, or set the command
  manually in *Configure*.
- **The shell exits immediately when I create a new tab.** Usually a
  quoting issue in the args field, or a missing dependency. Use double
  quotes around args containing spaces (`--config "C:\My Folder\x"`), and
  verify the command runs from a normal terminal first.
- **Linux tools (grep, nano) aren't found in my Shell tab.** The shell is
  PowerShell or cmd, which don't ship with these. Switch to Git Bash or
  WSL via *Configure*.
- **My Shell tab's config changes don't take effect.** Spawn-affecting
  changes apply on next shell restart. Right-click → *Restart shell*, or
  type `exit` and press Enter on the closed-shell overlay.
- **How do I delete a Shell tab?** Hover the tab and click `×`, or press
  `Ctrl+W` while the tab is active. Builtin tabs can't be closed.
- **My settings.json got corrupted.** v1.2 backs up the v1.1 file as
  `config.json.v1.1.bak` on first migration. For other corruption,
  delete `settings.json` and the app writes a fresh default on next launch.

### Aider tab and the TTS limitation

Aider runs as a fully functional second tab — input, output, status,
notifications, and the avatar all work normally. **Spoken TTS is silent
in the aider tab** because aider does not currently expose a CLI
mechanism (`--append-system-prompt` or equivalent) for cctts to inject
the `[[TTS]]…[[/TTS]]` markup convention. The setting is preserved in
the schema and the toggle is visible in *Settings → Tabs → Aider*; when
upstream aider lands the feature, cctts will start using it.

See `docs/FUTURE-FEATURES.md` for the action plan and the upstream
aider issue we're tracking.

## System Requirements

- **OS:** Windows 10/11 (primary). Linux is feasible but not part of the
  validation matrix — see `docs/MILESTONE-08-polish.md`.
- **GPU:** optional. The app defaults to CPU inference (Kokoro is small
  enough for near-real-time CPU). NVIDIA CUDA 12.x can be opted into via
  `setx CCTTS_GPU cuda` and a restart — see `MAINTENANCE.md` for the
  current GPU support matrix and Blackwell caveat.
- **Claude Code:** the `claude` binary must be on `PATH`. cctts spawns it
  as a subprocess and passes `--append-system-prompt` so Claude knows to
  emit the TTS markers.
- **Aider (optional):** if `aider` is on `PATH`, the Aider tab will spawn
  it on launch. If it isn't, the Aider tab shows a clear error with a
  Retry button — the rest of the app, including the Claude Code tab,
  works regardless. Install instructions: <https://aider.chat>.
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

## Configuring Tabs

Per-tab subprocess configuration lives under **Settings → Tabs**. Each
tab has its own sub-section with:

- **Command** (read-only): the binary cctts spawns — `claude` or `aider`.
- **Persistent CLI flags:** flags appended to every spawn of that tab.
- **TTS markup injection:** toggle plus an editable instructions block.
  For Claude, the instructions are passed via `--append-system-prompt` on
  each spawn. The Reset button restores cctts's built-in runtime prompt
  (`src-tauri/src/tts/runtime_prompt.md`). For aider, the toggle is a
  no-op pending upstream support — see the section above.
- **Notifications:** text used for inactive-tab notifications (firing
  itself ships in V2-04; configuration is wired now).
- **Restart Tab:** apply changes that require respawning the subprocess
  (command, CLI flags, TTS injection). Notification text changes apply
  live — no restart needed.

Settings persist to `%APPDATA%\cctts\settings.json` (debounced save).

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
- **Shortcuts:** rebindable open/submit/cancel for compose, open settings,
  switch to tab 1 / tab 2 (default `Ctrl+1` / `Ctrl+2`).
- **Tabs:** per-tab command, CLI flags, TTS injection, and notification
  text — see *Configuring Tabs* above.
- **Processing:** stability and max-hold timers for the byte-burst /
  segment-detection pipeline.

## Troubleshooting

- **TTS silent.** Check the log for `TTS disabled: Kokoro model files not found.`
  Place the model + voicepack under `%APPDATA%\cctts\models\` as documented above.
- **`claude` not found.** cctts looks up `claude` via `PATH`. Either install
  Claude Code so it's on `PATH` or add its install dir.
- **`aider` not found.** Same deal: ensure `aider` is on `PATH`. If you
  don't intend to use the aider tab, you can ignore the in-tab error —
  the Claude tab is unaffected. Install instructions: <https://aider.chat>.
- **CUDA EP errors per segment (silent output).** You're on a GPU not yet
  covered by the bundled ORT 1.20 prebuilt (Blackwell / RTX 5090). Unset
  `CCTTS_GPU` to fall back to CPU. See `MAINTENANCE.md`.
- **Audio interrupted by typing.** That's `Behavior → Interrupt TTS when typing`.
  Disable it if you'd rather keep playback rolling.
- **Avatar doesn't move.** Confirm `Settings → Avatar → Visible` is on
  and the per-state image paths point to readable files (or are blank to use
  the bundled defaults).

## Known Limitations

- The aider tab spawns aider but cannot inject the TTS markup convention
  via CLI today, so its prose output is silent. See `docs/FUTURE-FEATURES.md`.
- Single audio output device — no UI selector.
- No conversation/session UI on top of the terminal.
- No STT input.
- No voice mixing.
- Linux is not part of the validation matrix; behavior is best-effort.
- Kokoro model is user-provided, not bundled.

See `docs/MILESTONE-08-polish.md` "After Milestone 8" and
`docs/FUTURE-FEATURES.md` for the post-v1 parking lot.

## License

Apache-2.0. See `LICENSE`.

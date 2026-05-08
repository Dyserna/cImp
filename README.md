# cctts

Multi-tab AI assistant wrapper with text-to-speech and a state-driven
animated avatar. A Tauri desktop app that hosts two **Claude Code** tabs
out of the box — one for your subscription / API account and one
preconfigured to talk to a local LLM via a translation proxy — extracts
`[[TTS]]…[[/TTS]]` markers from output, synthesizes them with a local
Kokoro ONNX model, and drives a per-tab avatar overlay (Idle / Listening /
Thinking / Speaking / Error) plus a real-time waveform.

Local, offline-after-install, no audio leaves the machine.

## Two Claude Code tabs

- **Claude** — your normal Claude Code, running with whatever auth flow
  you configured (Pro/Max subscription via OAuth, or `ANTHROPIC_API_KEY`).
- **Claude (local)** — a second `claude` instance with
  `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` injected at spawn time
  so it talks to a local LLM via a translation proxy (LiteLLM / similar)
  instead of `api.anthropic.com`. Configured in
  *Settings → Local LLM provider*.
- Switch with `Ctrl+1` (Claude) and `Ctrl+2` (Claude (local)), or click the tab.
- Both subprocesses spawn at app launch in the directory cctts was started
  in. They run independently — switching tabs doesn't stop either one.
- The compose overlay submits to whichever tab is currently active.

## Shell Tabs (v1.2+)

In addition to the two AI builtins, cctts hosts **Shell tabs** — plain
configurable terminal sessions running alongside the Claude tabs, with
no TTS, no permission detection, and a reduced notification set (`error`
and `exited` only).

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
  active. Builtin tabs (Claude, Claude (local)) cannot be closed.
- **Switch by position:** `Ctrl+1`..`Ctrl+9` switch to the tab at that
  ordinal position **within the focused pane** (v1.3 change — see
  *Multi-pane Layout* below). `Ctrl+9` with fewer than 9 tabs in the focused
  pane is a silent no-op.

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

## Multi-pane Layout (v1.3+)

The terminal area is a recursive tree of panes — split horizontally or
vertically, drag tabs between them, save named layouts. The avatar, audio
playback, and the compose overlay all follow the **focused pane's active
tab**; switching pane focus retargets all three.

### Splitting

- **Drag a tab to a pane edge** (left / right 25 %, top / bottom 25 %, below
  the tab bar) to tear it into a new sibling pane in that direction. The new
  pane gets focus.
- **`Ctrl+\`** splits the focused pane horizontally (side-by-side) with a
  fresh Shell tab on the right.
- **`Ctrl+Shift+\`** splits vertically (stacked) with a fresh Shell tab below.
- **Right-click the tab bar background** (not on a tab) for a context menu
  with Split horizontally / Split vertically and other pane operations.

### Moving tabs

- **Drag a tab to a different pane's tab bar** (or its center) to move it
  there. The tab keeps its xterm.js state, scrollback, and PTY connection —
  the underlying DOM element is just re-parented.
- **Drag within the same tab bar** to reorder.
- **Pane context menu → Move all tabs to →** moves every tab from one pane
  into another and collapses the source.

### Closing panes

- **Drag the last tab out** of a pane and the pane auto-collapses (its
  parent split is replaced by the surviving sibling).
- **`Ctrl+Shift+W`** closes the focused pane; its tabs migrate to the
  surviving sibling subtree's leftmost leaf, then the empty pane collapses.
  No-op when the focused pane is the root.

### Resizing

Drag the 4 px line between any split's two children. Min sizes apply
(200 px wide, 100 px tall) — neither pane can shrink past them. Window
shrink re-clamps visually but doesn't overwrite your stored ratio; window
re-grow restores it.

### Focus

- **Click any pane** to focus it. The avatar / audio / compose follow.
- **`Ctrl+Alt+Arrow`** moves focus to the geometrically-adjacent pane in
  that direction. Hits the closest one whose perpendicular axis overlaps
  the focused pane's; no-op if no pane lies in that direction.
- The focused pane shows a 2 px accent line along the top of its tab bar.

### Layout presets

Save the current pane arrangement under a name from the **Layouts** popover
in the bottom-left of the status bar.

- **Save current layout as…** prompts for a name. Same-name save replaces.
- **Recent presets** lists the five most recent by save time; click to restore.
- **Manage presets…** opens a dialog with inline rename and confirm-delete.
- Presets store the tree only — focus follows your next click after restore.
- Restoring a preset adapts to the current tab list: tabs created since the
  preset was saved land in the focused pane; tabs deleted since are skipped.

### Keyboard shortcuts

| Action                       | Default              |
|------------------------------|----------------------|
| Switch to tab N in focused pane | `Ctrl+1` … `Ctrl+9`  |
| New shell tab in focused pane   | `Ctrl+T`             |
| Close active tab in focused pane| `Ctrl+W`             |
| Split focused pane horizontally | `Ctrl+\`             |
| Split focused pane vertically   | `Ctrl+Shift+\`       |
| Close focused pane              | `Ctrl+Shift+W`       |
| Focus pane left / right / up / down | `Ctrl+Alt+Arrow` |
| Open compose                    | `Ctrl+Shift+E`       |
| Submit compose                  | `Ctrl+Enter`         |
| Open settings                   | `Ctrl+,`             |

All shortcuts are rebindable in *Settings → Shortcuts*.

### Known shortcut conflicts

- **`Ctrl+Shift+W` may collide with WebView2's "close window"** on some
  Windows configurations — if the press closes the app instead of the pane,
  remap `close_pane` to e.g. `Ctrl+Q` or `Ctrl+Alt+W`.
- **`Ctrl+Alt+Arrow` may collide with GNOME / KDE workspace switching** on
  Linux. Remap `focus_pane_*` to `Ctrl+Shift+Arrow` if needed.

Defaults aren't changed for either — different setups have different
conflicts; the rebind path covers them.

### Migrating from v1.2

On first v1.3 launch, an existing v1.2 settings file gets migrated to a
single root pane containing every tab in order, with the previously-active
tab focused. A `settings.json.v1.2.bak` backup is written alongside before
the rewrite. All v1.2 features (tabs, settings, notifications, presets,
TTS, avatar) carry over unchanged. The only behavior change is that
`Ctrl+1`..`Ctrl+9` are now **scoped to the focused pane** rather than the
global tab list.

## Local LLM provider (the second Claude tab)

The **Claude (local)** tab runs the same `claude` binary as the first
tab but with two environment variables injected at spawn time:

```
ANTHROPIC_BASE_URL=<your local proxy URL>
ANTHROPIC_AUTH_TOKEN=<token your proxy expects>
```

Configure both under *Settings → Local LLM provider*. cctts does **not**
start the proxy itself — you run a separate translator like
[LiteLLM](https://docs.litellm.ai/docs/proxy/quick_start) that speaks
the Anthropic Messages API on its public side and forwards to Ollama /
LM Studio / OpenAI / etc. on the back end.

A typical local-only setup:

1. Run Ollama (or LM Studio) with whichever model you want.
2. Run LiteLLM as a proxy in front of it, mapping `claude-*` model names
   to your local model. The LiteLLM docs cover the config shape.
3. In cctts: *Settings → Local LLM provider* → set Proxy URL to your
   LiteLLM URL (e.g. `http://localhost:4000`) and Auth token to whatever
   your proxy expects (often a dummy string like `sk-dummy`).
4. Restart the Claude (local) tab.

Per-tab `env` entries always take precedence over the synthesized
values, so you can also point a single tab at a different proxy by
setting `ANTHROPIC_BASE_URL` directly in *Settings → Tabs → Claude (local)
→ Environment*.

**Caveats:**

- Smaller local models often don't follow tool-use protocols reliably
  (Edit / Write / Bash). Test with the specific model you want before
  committing.
- Anthropic-server features (prompt caching, extended thinking, vision)
  are unavailable on local models.
- The auth token sits cleartext in `%APPDATA%\cctts\settings.json` —
  fine for local dummies; don't put a real Anthropic API key there.

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
- **Local LLM proxy (optional, for the Claude (local) tab):** if the
  Claude (local) tab's *Use local LLM provider* flag is on, you also
  need a running translator process at the URL configured under
  *Settings → Local LLM provider*. cctts does not start the proxy. If
  the proxy isn't reachable, the tab will fail on first message — you
  can disable the flag, or just stop using that tab; the subscription
  Claude tab is unaffected.
- **WebView2 (Windows):** preinstalled on updated Windows 10/11. Older
  systems may need the WebView2 runtime installed manually.

## Installing the Kokoro Model

The **portable Windows zip** (downloadable from the GitHub Releases page)
ships `kokoro-v1.0.onnx` and `af_heart.bin` next to the executable —
unzip, add `bin/` to PATH, run, hear TTS. Nothing else to install.

For **source builds** (or if you delete the bundled files), cctts looks
for the Kokoro model in two places, in order:

1. `%APPDATA%\cctts\models\` — the user data dir, always writable.
2. `<exe-dir>/../models/` — the portable layout's bundled location.

Place these two files in either:

```
kokoro-v1.0.onnx
voices/af_heart.bin
```

Download from
[onnx-community/Kokoro-82M-v1.0-ONNX](https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/tree/main):

- model: `onnx/model.onnx` → rename to `kokoro-v1.0.onnx`
- voices: `voices/<name>.bin` — `af_heart.bin` is the default; any
  voicepack in either `voices/` folder shows up in the settings dropdown.
  APPDATA wins over portable on duplicate filenames so a user-installed
  voice can override a bundled one.

If both lookups fail at startup the app launches with TTS silent and
prints the expected paths to the log.

## Configuring Tabs

Per-tab subprocess configuration lives under **Settings → Tabs**. Each
tab has its own sub-section with:

- **Command** (read-only): the binary cctts spawns — `claude` for both
  AI tabs.
- **Persistent CLI flags:** flags appended to every spawn of that tab.
- **Use local LLM provider:** toggle that gates env synthesis from the
  global *Local LLM provider* settings (off by default for the
  subscription Claude tab; on by default for the Claude (local) tab).
- **TTS markup injection:** toggle plus an editable instructions block.
  Instructions are passed via `--append-system-prompt` on each spawn.
  The Reset button restores cctts's built-in runtime prompt
  (`src-tauri/src/tts/runtime_prompt.md`).
- **Notifications:** text used for inactive-tab notifications (firing
  itself ships in V2-04; configuration is wired now).
- **Restart Tab:** apply changes that require respawning the subprocess
  (command, CLI flags, TTS injection). Notification text changes apply
  live — no restart needed.

Settings persist to `%APPDATA%\cctts\settings.json` (debounced save).

## Running

**End users (Windows):** download the latest portable zip from the
[Releases page](https://github.com/Dyserna/cctts/releases), unzip it, add
`bin/` to your PATH, and run `cctts`. The zip ships with the Kokoro
model and the default voice — no extra setup beyond Claude Code itself
being on PATH.

**Developers:**

```
npm install
npm run tauri dev
```

For a local release build:

```
npm run tauri build
```

See `docs/PACKAGING.md` for the distribution shape and `docs/RELEASE.md`
for the tag-driven release workflow.

## Settings Overview

Open with `Ctrl+,` or the cog button on the avatar.

- **TTS:** voice picker (auto-discovered from `voices/`), speed, volume,
  mute.
- **Avatar:** visibility, position, size, opacity, per-state image / video
  overrides, transition video + duration. Empty transition path or
  `duration = 0` falls back to a 150 ms crossfade.
- **Waveform:** color, line width, glow, opacity.
- **Display:** terminal font family + size, toggle to render TTS markup
  verbatim in the terminal (debug aid).
- **Appearance:** UI chrome theme (Modern Dark) and **terminal palette** —
  12 bundled palettes (Default, Dracula, Solarized Dark/Light, Nord,
  Tomorrow Night, Gruvbox Dark/Light, One Dark, Monokai, Tokyo Night,
  GitHub Dark) plus a 22-color Custom editor for foreground, background,
  cursor, selection, ANSI 8, and bright 8. Each tab can override the
  global palette via Configure Tab → Appearance — useful for color-coding
  Claude (subscription) vs. Claude (local) vs. shells. Per-tab overrides travel with the tab
  through drag-and-drop. Plus **terminal background** — a solid color or
  user-supplied image rendered beneath the terminal text. Solid color
  has no performance cost; image mode forces the slower DOM renderer
  (2-5× slower for high-throughput output like `tail -F`). Toggling the
  image switches xterm.js renderers cleanly — your shell session,
  scrollback, and running processes all survive the switch (V1.4-03).
  Image mode adds opacity, blur, size, position, and an optional tint
  color for the dimming overlay. **Per-tab Background row** in Configure
  Tab gives each tab its own image/color or a "Disabled" opt-out that
  forces plain theme background regardless of the global setting.
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
- **Claude (local) tab errors.** Most often: the local proxy isn't running
  or the URL in *Settings → Local LLM provider* is wrong. Confirm the
  proxy is reachable (e.g. `curl http://localhost:4000/v1/models` or
  similar). Until you fix the proxy you can simply use the subscription
  Claude tab, which is unaffected.
- **CUDA EP errors per segment (silent output).** You're on a GPU not yet
  covered by the bundled ORT 1.20 prebuilt (Blackwell / RTX 5090). Unset
  `CCTTS_GPU` to fall back to CPU. See `MAINTENANCE.md`.
- **Audio interrupted by typing.** That's `Behavior → Interrupt TTS when typing`.
  Disable it if you'd rather keep playback rolling.
- **Avatar doesn't move.** Confirm `Settings → Avatar → Visible` is on
  and the per-state image paths point to readable files (or are blank to use
  the bundled defaults).

## Known Limitations

- TTS markup compliance for the **Claude (local)** tab depends on the
  underlying model. Smaller local models may not wrap content in
  `[[TTS]]…[[/TTS]]` reliably even when the system prompt asks them to.
  cctts will be silent for those segments — this is fallback behavior,
  not an error.
- Tool-use (Edit / Write / Bash / etc.) on the Claude (local) tab depends
  on the local model supporting Anthropic-style tool calling. Test before
  committing to a particular model.
- cctts does not bundle or auto-spawn the local LLM proxy — you run it
  yourself.
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

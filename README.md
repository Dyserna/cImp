# cctts

Multi-tab AI assistant wrapper with text-to-speech and a state-driven
animated avatar. A Tauri desktop app that hosts **Claude Code** — your
subscription tab, a local-LLM tab, or both — extracts `[[TTS]]…[[/TTS]]`
markers from output, synthesizes them with a local Kokoro ONNX model,
and drives a per-tab avatar overlay (Idle / Listening / Thinking /
Speaking / Error) plus a real-time waveform.

Local, offline-after-install, no audio leaves the machine.

## Claude Code tabs

cctts can host two flavors of Claude Code:

- **Claude** — your normal Claude Code, running with whatever auth flow
  you configured (Pro/Max subscription via OAuth, or `ANTHROPIC_API_KEY`).
- **Claude (local)** — a second `claude` instance with
  `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` injected at spawn time
  so it talks to a local backend (LM Studio, Ollama, vLLM, llama-server)
  instead of `api.anthropic.com`. Configured in
  *Settings → Local LLM provider*.

Which tabs exist is controlled by the **Claude tabs enabled** radio in
*Settings → Tabs* (Cloud / Local / Both). Default on a fresh install is
**Cloud** (subscription tab only). Switching the radio at runtime closes
the affected tab (kills its PTY, drops scrollback) or re-creates it from
the built-in defaults.

- Switch between tabs with `Ctrl+1`..`Ctrl+9` (within the focused pane), or click the tab.
- Both subprocesses, when present, spawn at app launch in the directory
  cctts was started in. They run independently — switching tabs doesn't
  stop either one.
- The compose overlay submits to whichever tab is currently active.

### Multiple tabs of the same type (v0.6.7+)

Each builtin Claude tab carries a **`+` button** (revealed on hover, or
while the tab is active). Click it to spawn **another tab of the same
type** — `+` on **Claude** opens a second subscription Claude, `+` on
**Claude (local)** opens a second local-LLM Claude. A duplicate:

- clones the origin tab's live config (CLI flags, environment, TTS
  injection, *Use local LLM provider*), so a Claude (local) duplicate gets
  the same `ANTHROPIC_*` injection as the tab it came from;
- is auto-named `Claude 2`, `Claude 3`, … — rename via double-click;
- is **closable** — it shows a `×` and accepts `Ctrl+W`, unlike the builtin
  it was spawned from;
- persists across restarts, reopening with your saved layout.

Each duplicate is an independent subprocess with its own PTY, scrollback,
and avatar / TTS state, launched in the directory cctts was started in.
Duplicates aren't listed in *Settings → Tabs* — they inherit the origin
tab's configuration.

## Shell Tabs (v1.2+)

In addition to the two AI builtins, cctts hosts **Shell tabs** — plain
configurable terminal sessions running alongside the Claude tabs, with
no TTS, no permission detection, and a reduced notification set (`error`
and `exited` only).

### Creating and managing Shell tabs

- **Create:** click the `+` button at the **right end of the tab bar**
  (distinct from the per-tab `+` on Claude tabs, which spawns AI
  duplicates), or press `Ctrl+T`. The New Shell Tab dialog pre-fills the
  platform default shell.
- **Rename:** right-click a tab → *Rename*, or double-click the tab name.
- **Configure:** right-click a Shell tab → *Configure…* to change command,
  args, working directory, or notification text. Spawn-affecting changes
  apply on the next shell restart.
- **Restart shell:** right-click → *Restart shell* kills the running
  subprocess and respawns it with the current configuration. Useful after
  changing the command in Configure.
- **Close:** click the `×` on the tab, or press `Ctrl+W` while the tab is
  active. The **builtin** AI tabs (Claude, Claude (local)) cannot be closed
  via the `×` — toggle them via the **Claude tabs enabled** radio in
  Settings instead — but **spawned duplicates** (see *Multiple tabs of the
  same type*) are closable like any shell. The default first shell tab
  (`shell-default-1`) is closable like any other shell.
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
  `Ctrl+W` while the tab is active. The *builtin* AI tabs (Claude, Claude
  (local)) aren't closable from the tab bar — use the *Claude tabs enabled*
  radio in *Settings → Tabs*; *spawned* AI duplicates are closable with `×`
  like any shell.
- **My settings.json got corrupted.** Each migration writes a backup
  alongside the source file (e.g. `settings.json.v1.7.bak.<ts>`). For
  other corruption, delete the global `<exe-dir>/settings.json` (and the
  per-folder `.cctts.custom.config.json` overlay if present) and the app
  writes fresh defaults on next launch.

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

## Local LLM provider (the Claude (local) tab)

The **Claude (local)** tab runs the same `claude` binary as the
subscription tab but with two environment variables injected at spawn
time:

```
ANTHROPIC_BASE_URL=<your local backend URL>
ANTHROPIC_AUTH_TOKEN=<token your backend expects>
```

Configure both under *Settings → Local LLM provider*. cctts does **not**
start the backend itself — you run it separately. The supported backends
all expose a native Anthropic Messages API (`/v1/messages`), so no
translation proxy is needed:

| Backend                                                                                  | Default port | Notes                                            |
|------------------------------------------------------------------------------------------|--------------|--------------------------------------------------|
| [LM Studio](https://lmstudio.ai/docs/developer/anthropic-compat) (≥ 0.4.1)               | `1234`       | Easiest path: load a model, enable the server.   |
| [llama-server](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md) | `8080`       | Run with `--jinja` for tool use.                 |
| Ollama                                                                                   | `11434`      | Native Anthropic compatibility.                  |
| vLLM                                                                                     | `8000`       | Native Anthropic compatibility.                  |

A typical setup:

1. Start your backend with whichever model you want (e.g. LM Studio's
   "Start server" toggle on a loaded model).
2. In cctts: *Settings → Tabs → Claude tabs enabled* → switch to **Local**
   or **Both**.
3. *Settings → Local LLM provider* → set Endpoint URL to the backend's
   base URL (e.g. `http://localhost:1234`) and Auth token to whatever
   the backend expects (a dummy string like `sk-dummy` works for all
   four).
4. Restart the Claude (local) tab.

For OpenAI-only backends (no native Anthropic endpoint) run a translator
like [`anthropic-proxy-rs`](https://github.com/m0n0x41d/anthropic-proxy-rs)
in front of it and point cctts at the translator.

Per-tab `env` entries always take precedence over the synthesized
values, so you can also point a single tab at a different endpoint by
setting `ANTHROPIC_BASE_URL` directly in *Settings → Tabs → Claude (local)
→ Environment*.

**Caveats:**

- Smaller local models often don't follow tool-use protocols reliably
  (Edit / Write / Bash). Test with the specific model you want before
  committing.
- Anthropic-server features (prompt caching, extended thinking, vision)
  are unavailable on local models.
- The auth token sits cleartext in `<exe-dir>/settings.json` (or the
  per-folder `.cctts.custom.config.json` overlay) — fine for local
  dummies; don't put a real Anthropic API key there.

## System Requirements

- **OS:** Windows 10/11 (primary). Linux is feasible but not part of the
  validation matrix — see `docs/completedMilestones/MILESTONE-V1-08-polish.md`.
- **GPU:** optional. The app defaults to CPU inference (Kokoro is small
  enough for near-real-time CPU). NVIDIA CUDA 12.x can be opted into via
  `setx CCTTS_GPU cuda` and a restart — see `MAINTENANCE.md` for the
  current GPU support matrix and Blackwell caveat.
- **Claude Code:** the `claude` binary must be on `PATH`. cctts spawns it
  as a subprocess and passes `--append-system-prompt` so Claude knows to
  emit the TTS markers.
- **Local backend (optional, for the Claude (local) tab):** if the
  Claude (local) tab's *Use local LLM provider* flag is on, you need a
  running Anthropic-compatible backend (LM Studio, Ollama, vLLM, or
  llama-server) at the URL configured under *Settings → Local LLM
  provider*. cctts does not start the backend. If it isn't reachable,
  the tab will fail on first message — disable the flag, or just stop
  using that tab; the subscription Claude tab is unaffected.
- **WebView2 (Windows):** preinstalled on updated Windows 10/11. Older
  systems may need the WebView2 runtime installed manually.

## Installing the Kokoro Model

The **portable Windows zip** (downloadable from the GitHub Releases page)
ships `kokoro-v1.0.onnx` and `af_heart.bin` next to the executable —
unzip, add `bin/` to PATH, run, hear TTS. Nothing else to install.

For **source builds** (or if you delete the bundled files), cctts looks
for the Kokoro model in exactly one place, relative to the executable:

```
<exe-dir>/../models/kokoro-v1.0.onnx
<exe-dir>/../models/voices/af_heart.bin
```

In a portable install that resolves to the `models/` folder sitting next
to `bin/`. Drop new voicepacks into `models/voices/` and they appear in
the settings dropdown.

Download from
[onnx-community/Kokoro-82M-v1.0-ONNX](https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/tree/main):

- model: `onnx/model.onnx` → rename to `kokoro-v1.0.onnx`
- voices: `voices/<name>.bin` — `af_heart.bin` is the default.

If the lookup fails at startup the app launches with TTS silent and
prints the expected path to the log.

## Configuring Tabs

Per-tab subprocess configuration lives under **Settings → Tabs**, split
into three sub-sections: **Claude**, **Claude (local)**, and **Shells**.
Each tab exposes:

- **Command** (read-only on AI tabs): the binary cctts spawns — `claude`
  for both AI tabs.
- **Persistent CLI flags:** flags appended to every spawn of that tab.
- **Use local LLM provider** (AI tabs): toggle that gates env synthesis
  from the global *Local LLM provider* settings (off by default for the
  subscription Claude tab; on by default for the Claude (local) tab).
- **TTS markup injection** (AI tabs): toggle plus an editable
  instructions block. Instructions are passed via
  `--append-system-prompt` on each spawn. The Reset button restores
  cctts's built-in runtime prompt
  (`src-tauri/src/tts/runtime_prompt.md`).
- **Notifications:** text spoken when the tab transitions to a notable
  state and the user is focused elsewhere. AI tabs have four slots
  (`idle`, `awaiting_permission`, `question`, `error`); shell tabs have
  two (`error`, `exited`). Empty string disables that specific
  notification while leaving the others active.
- **Appearance:** per-tab terminal-palette and background overrides
  (V1.4-01 / V1.4-02); each travels with the tab through drag-and-drop.
- **Restart Tab:** apply changes that require respawning the subprocess
  (command, CLI flags, TTS injection, `use_local_provider`). Notification
  text and appearance changes apply live — no restart needed.

Settings are persisted to two files: a **portable global baseline** at
`<exe-dir>/settings.json`, and a **per-folder overlay** at
`<launch_cwd>/.cctts.custom.config.json` containing only the keys that
differ from the baseline. Saves are debounced (500 ms) and the overlay
file is deleted automatically when the diff is empty.

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
  Claude (subscription) vs. Claude (local) vs. shells. Per-tab overrides
  travel with the tab through drag-and-drop. Plus **terminal background**
  — a solid color or user-supplied image rendered beneath the terminal
  text, with named **global presets** you can save and apply across tabs.
  Solid color has no performance cost; image mode forces the slower DOM
  renderer (2-5× slower for high-throughput output like `tail -F`).
  Toggling the image switches xterm.js renderers cleanly — your shell
  session, scrollback, and running processes all survive the switch
  (V1.4-03), and the visible scrollback also survives an app restart via
  a per-tab on-disk ring buffer (V1.4-04 D). Image mode adds opacity,
  blur, size, position, and an optional tint color for the dimming
  overlay. **Per-tab Background row** in Configure Tab gives each tab
  its own image/color or a "Disabled" opt-out that forces plain theme
  background regardless of the global setting.
- **Audio / Behavior:** interrupt TTS on input, auto-speak detected
  segments, fall back to silence on responses with no TTS markup,
  enable/disable announcements globally, **announce focused tab** (let
  notifications fire even for the tab you're looking at; default off),
  **follow avatar** (auto-mute when the avatar is hidden; default off).
- **Compose:** min/max sheet height for the slide-up compose overlay.
- **Shortcuts:** rebindable bindings for compose open/submit/cancel,
  open settings, switch-to-tab-N (within focused pane), pane focus and
  splits, new shell tab, close active tab, and close pane. The full
  default set is in *Multi-pane Layout → Keyboard shortcuts* above.
- **Tabs:** the **Claude tabs enabled** radio (Cloud / Local / Both)
  plus per-tab command, CLI flags, TTS injection, notification text,
  and appearance overrides — see *Configuring Tabs* above.
- **Processing:** stability and max-hold timers for the byte-burst /
  segment-detection pipeline.

## Troubleshooting

- **TTS silent.** Check the log for `TTS disabled: Kokoro model files not found.`
  Place the model + voicepack under `<exe-dir>/../models/` as documented above.
- **`claude` not found.** cctts looks up `claude` via `PATH`. Either install
  Claude Code so it's on `PATH` or add its install dir.
- **Claude (local) tab errors.** Most often: the local backend isn't
  running or the URL in *Settings → Local LLM provider* is wrong.
  Confirm the backend is reachable (e.g.
  `curl http://localhost:1234/v1/models` for LM Studio). Until you fix
  the backend you can simply use the subscription Claude tab, which is
  unaffected.
- **CUDA EP errors per segment (silent output).** You're on a GPU not yet
  covered by the bundled ORT 1.20 prebuilt (Blackwell / RTX 5090). Unset
  `CCTTS_GPU` to fall back to CPU. See `MAINTENANCE.md`.
- **Audio interrupted by typing.** That's `Behavior → Interrupt TTS when typing`.
  Disable it if you'd rather keep playback rolling.
- **Avatar doesn't move.** Confirm `Settings → Avatar → Visible` is on
  and the per-state image paths point to readable files (or are blank to use
  the bundled defaults).
- **`Ctrl+R` / `Ctrl+Shift+R` / `F5` don't reload the app.** Intentional
  (v0.6.6+) — a reload would tear down every tab's session and lose
  scrollback. The keystrokes still reach the terminal, so `Ctrl+R` works
  as your shell's reverse-history search as usual.

## Known Limitations

- TTS markup compliance for the **Claude (local)** tab depends on the
  underlying model. Smaller local models may not wrap content in
  `[[TTS]]…[[/TTS]]` reliably even when the system prompt asks them to.
  cctts will be silent for those segments — this is fallback behavior,
  not an error.
- Tool-use (Edit / Write / Bash / etc.) on the Claude (local) tab depends
  on the local model supporting Anthropic-style tool calling. Test before
  committing to a particular model.
- cctts does not bundle or auto-spawn the local backend — you run
  LM Studio / Ollama / vLLM / llama-server yourself.
- Single audio output device — no UI selector.
- No conversation/session UI on top of the terminal.
- No STT input.
- No voice mixing.
- Linux is not part of the validation matrix; behavior is best-effort.
- Kokoro model is user-provided, not bundled.

See `docs/completedMilestones/MILESTONE-V1-08-polish.md` "After Milestone
8" and `docs/FUTURE-FEATURES.md` for the post-v1 parking lot.

## License

Apache-2.0. See `LICENSE`.

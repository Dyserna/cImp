# ccImp

Multi-tab AI assistant wrapper with text-to-speech and a state-driven
animated avatar. A Tauri desktop app that hosts **Claude Code** — your
subscription tab, a local-LLM tab, or both — extracts `[[TTS]]…[[/TTS]]`
markers from output, synthesizes them with a local Kokoro ONNX model,
and drives a per-tab avatar overlay (Idle / Listening / Thinking /
Speaking / Error) plus a real-time waveform.

Local, offline-after-install, no audio leaves the machine.

## Claude Code tabs

ccImp can host two flavors of Claude Code:

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
  ccImp was started in. They run independently — switching tabs doesn't
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
and avatar / TTS state, launched in the directory ccImp was started in.
Duplicates aren't listed in *Settings → Tabs* — they inherit the origin
tab's configuration.

## Shell Tabs (v1.2+)

In addition to the two AI builtins, ccImp hosts **Shell tabs** — plain
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
  per-folder `.ccimp.custom.config.json` overlay if present) and the app
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
| Split focused pane vertically   | `Alt+\`              |
| Close focused pane              | `Ctrl+Alt+W`         |
| Focus pane left / right / up / down | `Ctrl+Alt+Arrow` |
| Open compose                    | `Alt+Enter`          |
| Submit compose                  | `Ctrl+Enter`         |
| Open settings                   | `Ctrl+,`             |
| Push-to-talk (dictate)          | `Ctrl+Shift` (hold)  |

All shortcuts are rebindable in *Settings → Shortcuts*.

> **v6 note:** Open compose, Split-vertical, and Close-pane moved off
> `Ctrl+Shift+…` (they were `Ctrl+Shift+E` / `Ctrl+Shift+\` / `Ctrl+Shift+W`)
> so they no longer collide with the bare-`Ctrl+Shift` push-to-talk chord.
> These are new-install defaults — upgrading keeps your existing bindings; the
> table above reflects fresh installs.

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

Configure both under *Settings → Local LLM provider*. ccImp does **not**
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
2. In ccImp: *Settings → Tabs → Claude tabs enabled* → switch to **Local**
   or **Both**.
3. *Settings → Local LLM provider* → set Endpoint URL to the backend's
   base URL (e.g. `http://localhost:1234`) and Auth token to whatever
   the backend expects (a dummy string like `sk-dummy` works for all
   four).
4. Restart the Claude (local) tab.

For OpenAI-only backends (no native Anthropic endpoint) run a translator
like [`anthropic-proxy-rs`](https://github.com/m0n0x41d/anthropic-proxy-rs)
in front of it and point ccImp at the translator.

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
  per-folder `.ccimp.custom.config.json` overlay) — fine for local
  dummies; don't put a real Anthropic API key there.

## System Requirements

- **OS:** Windows 10/11 (primary). Linux is feasible but not part of the
  validation matrix — see `docs/completedMilestones/MILESTONE-V1-08-polish.md`.
- **GPU:** optional. The app defaults to CPU inference (Kokoro is small
  enough for near-real-time CPU). NVIDIA CUDA 12.x can be opted into via
  `setx CCIMP_GPU cuda` and a restart — see `MAINTENANCE.md` for the
  current GPU support matrix and Blackwell caveat.
- **Claude Code:** the `claude` binary must be on `PATH`. ccImp spawns it
  as a subprocess and passes `--append-system-prompt` so Claude knows to
  emit the TTS markers.
- **Local backend (optional, for the Claude (local) tab):** if the
  Claude (local) tab's *Use local LLM provider* flag is on, you need a
  running Anthropic-compatible backend (LM Studio, Ollama, vLLM, or
  llama-server) at the URL configured under *Settings → Local LLM
  provider*. ccImp does not start the backend. If it isn't reachable,
  the tab will fail on first message — disable the flag, or just stop
  using that tab; the subscription Claude tab is unaffected.
- **WebView2 (Windows):** preinstalled on updated Windows 10/11. Older
  systems may need the WebView2 runtime installed manually.

## Installing the Kokoro Model

The **portable Windows zip** (downloadable from the GitHub Releases page)
ships `kokoro-v1.0.onnx` and `af_heart.bin` next to the executable —
unzip, add `bin/` to PATH, run, hear TTS. Nothing else to install.

For **source builds** (or if you delete the bundled files), ccImp looks
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

## Speech-to-text (dictation)

ccImp can transcribe your voice into the compose overlay, fully offline. It
is **off by default** — enable it under *Settings → Speech-to-text*.

**Usage**

- **Record button** — a microphone button appears in the bottom bar when STT
  is enabled. In *toggle* mode (default) click to start, click again to stop.
  In *hold* mode press and hold while you speak.
- **Push-to-talk** — hold `Ctrl+Shift` (rebindable) to record, release to
  transcribe. A quick tap or a `Ctrl+Shift+<key>` chord won't start a
  recording, so it coexists with terminal copy/paste and OS shortcuts.
- The transcript appends into the compose overlay (opening it if needed) so
  you can edit before sending with `Ctrl+Enter`. Silence or a too-short clip
  shows a brief "Didn't catch that" toast instead.

**Models**

The **full** portable zip ships the default `ggml-small.bin` (multilingual,
~466 MB) next to the executable. Like Kokoro, the model is resolved at:

```
<exe-dir>/../models/ggml-small.bin
```

Drop additional `ggml-*.bin` files into `models/` and pick them in
*Settings → Speech-to-text → Model*. The **slim / no-models** zip omits the
model — download one from
[ggerganov/whisper.cpp](https://huggingface.co/ggerganov/whisper.cpp/tree/main)
(e.g. `ggml-base.bin`, `ggml-small.bin`, `ggml-medium.bin`) and drop it in.
A missing model launches normally; the record button reports "model not
found" on first use and logs the expected path.

**GPU.** The released portable zip is **GPU-accelerated and portable at the
same time**: it's built with whisper.cpp's Vulkan backend, so it automatically
uses any GPU (NVIDIA, AMD, or Intel) and falls back to CPU on machines without
one — the only requirement is `vulkan-1.dll`, which ships with Windows. Nothing
to install. (`CCIMP_GPU=cpu` forces CPU; a short utterance on `small` is
~1–3 s on CPU, well under a second on a GPU.)

Building from source, the **default is CPU-only** (no GPU SDK needed). To build
the GPU variant locally, compile `--features stt-vulkan` from a VS x64 Native
Tools prompt with the Vulkan SDK installed — see `docs/MAINTENANCE.md`. An
optional NVIDIA-only `stt-cuda` feature exists for maximum speed but isn't
portable and isn't shipped.

## Local task offload (V8-01)

Offloading lets the main Claude (Opus) session hand a self-contained, token-heavy
subtask — a broad codebase search, summarizing a large file or log, web research —
to a **local model**, and get back only the synthesized result. The local model
does the searching/reading/summarizing; Opus's context grows by a paragraph
instead of a megabyte. Everything stays local.

This is **not** the *Claude (local)* tab — that swaps the whole session's brain to
a local model. Offload keeps Opus in charge and delegates a bounded subtask to a
subordinate local worker, exposed as an `offload_task` MCP tool.

**Setup** (*Settings → Offload*):

1. Install and be able to run [`llama.cpp`](https://github.com/ggml-org/llama.cpp)'s
   `llama-server` and a GGUF model (e.g. Qwen3.6-35B-A3B). ccImp does **not**
   bundle or download the model.
2. Turn on **Enable offload** and paste a **Server command**, for example:

   ```
   llama-server --model C:\models\Qwen3.6-35B-A3B-Q4.gguf --port 8080 --jinja -ngl 99 --ctx-size 150000 --flash-attn
   ```

   `--jinja` is required for tool-calling (ccImp warns if it's missing). This
   command is the single source of truth for the model, GPU layers, context, and
   host/port; ccImp parses the host/port and `-np` and discovers the context
   window from the running server.
3. Optionally enable **Start the server on launch** (otherwise click **Start**, or
   it starts on the first offload). Use **Test offload** to confirm it works.
4. **Re-launch a Claude tab** so it picks up the injected `offload_task` tool. With
   *Inject offload guidance* on, ccImp also nudges Opus on when to offload.

**Tools the local worker can use** are native and read-only: `read_file` (bounded
reads), `code_search` (literal search), and `run_command` (allowlisted, deny by
default). File access is confined to **Allowed roots** (the launch project root by
default). The richer MCP tool servers (web search, fetch, docs, git) are a
follow-up.

**Safety:** offload is read-only this milestone — no write/edit tools.
`run_command` runs nothing unless its program is on your allowlist, and all file
access is confined to the configured roots. The model and `llama-server` are
yours; ccImp only spawns the command you give it and connects over localhost.

### A pool of backends + routing (V8-02)

Beyond the single local server, you can configure a **pool of backends** and let
ccImp route each `offload_task` to the right one. The motivating setup: your main
PC running the big model, plus a second LAN machine with a small GPU running a
small/fast model for trivial offloads — so the big backend stays free for heavy
work. A cloud OpenAI-compatible endpoint can join the pool too.

In *Settings → Offload → **Backend pool*** add backends:

- **Local** — ccImp owns the process (a `llama-server` command, as above), with
  per-backend Start/Stop/Reset and autostart.
- **Remote (LAN or cloud)** — a **Base URL** (+ optional auth token) ccImp only
  health-checks and connects to; it can't start/stop it. A remote `llama-server`
  exposes its context window via `/props`; for endpoints that don't (many cloud
  APIs), set a **Declared context**.

Each backend has a **tier** (`fast`/`quality`) and a **tool scope**. The router
picks **one** backend per task by, in order: (1) **tool need** — a task that
must read local files is never sent to a backend that lacks file tools;
(2) **required context** — a 100k-token ingest can't go to a small-window box;
(3) **tier/complexity** — trivial single-pass work → the fast backend, real
reasoning → the large one; (4) **availability** — spill to another eligible
backend when the preferred one is busy, fail over when it's down. Claude can bias
the choice with a `tier: "auto" | "fast" | "quality"` argument on `offload_task`;
with one enabled backend the router is a no-op. The `offload_task` description
reports the pool to Opus so it knows a fast tier exists and which backends can
read local files.

**Per-backend tool scoping is the privacy boundary.** Local and trusted-LAN
backends get all tools by default. A **cloud** backend defaults to **web/docs
only** — `read_file`, `code_search`, `run_command`, `filesystem`, and `git` are
denied so local file contents and command output never leave your machine, at
*both* the routing layer (a local-data task is not routed to cloud) and the tool
layer (those tools aren't placed in the cloud model's `tools` array). A cloud
backend is **unusable until you grant explicit consent** (a checkbox, because the
task text itself is sent to a third party) and is badged distinctly from LAN
backends. The LAN case keeps data on your own network.

> **Cloud = data leaves your machine.** Offloading to a cloud backend sends the
> task instructions (and any `context` Opus passes, plus any tool results if you
> widen its scope) to that provider. It still protects Opus's context window, but
> breaks the local/offline property. Use a LAN backend to keep everything on your
> network.

### MCP tool servers + warm pool (V8-03)

Beyond the built-in native tools (`read_file`, `code_search`, `run_command`), an
offload worker can use your own **MCP tool servers** — web search, fetch, docs,
git, filesystem. ccImp is the **MCP host**: it keeps warm connections to each
server so an offload reaches real tools without paying an `npx`/`uvx` cold-start
per call, and it surfaces per-server health in *Settings → Offload → MCP tool
servers*.

Configure them in `settings.json` under `offload.mcp_servers` (the same shape as
Claude's own `mcpServers`):

```jsonc
"offload": {
  "mcp_servers": [
    { "name": "ddg",        "command": "uvx", "args": ["duckduckgo-mcp-server"], "enabled": true },
    { "name": "fetch",      "command": "uvx", "args": ["mcp-server-fetch"],      "enabled": true },
    { "name": "git",        "command": "uvx", "args": ["mcp-server-git"],        "enabled": true },
    { "name": "filesystem", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem"], "enabled": true }
  ]
}
```

Tools are exposed to the worker namespaced as `<name>__<tool>` (e.g. `ddg__search`,
`git__log`). Two safety rules hold automatically:

- **Read-class only.** Write/destructive tools (`filesystem` write, `git` commit,
  anything whose verb mutates) are filtered out — an offload can search/read/query
  but never modify.
- **`filesystem` is confined.** A server named `filesystem` is restricted to your
  configured `allowed_roots` (ccImp appends them as the server's allowed
  directories), so it can't read outside them.

Each server is still **scoped per backend** by the same tool-scope rules above —
a cloud backend never sees a local-data MCP server (`git`, `filesystem`).

**How the warm pool runs.** When the ccImp app is running it owns the loop, the
backend pool, the global concurrency gate, and the MCP host; the hidden
`ccimp --offload-mcp` server in each Claude tab is a thin proxy to it over a
**token-authenticated loopback endpoint** (`127.0.0.1`, ephemeral port). If the
app isn't running (e.g. a headless `claude -p` run), the child falls back to a
self-contained path so offload still works — just without the warm tool pool,
global concurrency, or live health. Changes to a server's config take effect when
the app next warms the pool; toggling **Enable offload** itself takes effect on
the next app launch.

## Speak all output (per-tab)

By default ccImp only speaks the text Claude wraps in `[[TTS]]…[[/TTS]]`
markers. **Right-click any Claude tab → "TTS all output"** to flip that tab
into a mode where it speaks **all** new terminal output and **ignores the
`[[TTS]]` markers** entirely. A small **speaker icon** appears on the tab so
you can see at a glance which tabs have it on.

- The toggle is **per tab** and **persists in the per-folder overlay**
  (`.ccimp.custom.config.json`) like any other per-tab setting. It applies
  live — no tab restart — and only affects output produced *after* you turn
  it on (the existing backlog isn't replayed).
- Output is **sentence-segmented and deduped**: ANSI is stripped, complete
  sentences are spoken, and an in-progress trailing sentence is held until it
  finishes. Unterminated chrome (the spinner / "esc to interrupt" status
  line, box-drawing, the input prompt) is held and dropped rather than
  spoken, so the synthesizer stays focused on prose.
- **Your own input isn't read back.** Text you type (or submit from the
  compose overlay) is registered as you go, so when the TUI echoes your
  prompt it's recognized and skipped — even behind the `> ` prompt prefix.
  (Pasted input via bracketed paste is the one gap; it isn't captured.)
- **`Esc` still stops playback until new output** — the same interrupt that
  works for marked segments. Speaking resumes on the tab's next output burst.

This mode is cleanest for plain, line-oriented output — e.g. a **Claude
(local)** tab whose model doesn't emit `[[TTS]]` markers, or any model you
just want fully read aloud. **It reads everything Claude prints** — including
"thinking"/reasoning shown above the answer — because raw terminal output has
no reliable marker that separates the answer from thinking or status chrome.
If you want **only Claude's answer** spoken (no question, no thinking), that's
exactly what the default `[[TTS]]` marker mode does — leave "TTS all output"
**off** on the subscription Claude tab and let Claude mark its answer.

## Configuring Tabs

Per-tab subprocess configuration lives under **Settings → Tabs**, split
into three sub-sections: **Claude**, **Claude (local)**, and **Shells**.
Each tab exposes:

- **Command** (read-only on AI tabs): the binary ccImp spawns — `claude`
  for both AI tabs.
- **Persistent CLI flags:** flags appended to every spawn of that tab.
- **Use local LLM provider** (AI tabs): toggle that gates env synthesis
  from the global *Local LLM provider* settings (off by default for the
  subscription Claude tab; on by default for the Claude (local) tab).
- **TTS markup injection** (AI tabs): toggle plus an editable
  instructions block. Instructions are passed via
  `--append-system-prompt` on each spawn. The Reset button restores
  ccImp's built-in runtime prompt
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
`<launch_cwd>/.ccimp.custom.config.json` containing only the keys that
differ from the baseline. Saves are debounced (500 ms) and the overlay
file is deleted automatically when the diff is empty.

## Running

**End users (Windows):** download the latest portable zip from the
[Releases page](https://github.com/Dyserna/ccImp/releases), unzip it, add
`bin/` to your PATH, and run `ccimp`. The zip ships with the Kokoro
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
- **Avatar:** a **Type** picker — *Picture / Video* (the default) or
  *Animated sprites* — plus visibility, position, size, opacity. In
  Picture / Video mode: per-state image / video overrides and a transition
  video + duration (empty transition path or `duration = 0` falls back to a
  150 ms crossfade). In Animated sprites mode: a **Sprite set** picker that
  drives a manifest-based pixel-art mascot whose frames are timed per-frame
  and rotate per state; the image/video and transition options don't apply.
  See *Animated sprite avatars* below.
- **Waveform:** color, line width, glow, opacity.
- **Display:** terminal font family + size, toggle to render TTS markup
  verbatim in the terminal (debug aid).
- **Appearance:** UI chrome theme — Modern Dark plus the TUI variants
  (Yellow, Purple, Orange); new installs default to **TUI Orange**, which
  also defaults the avatar to the animated `claudeSprites` mascot — and the
  **terminal palette** —
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

## Animated sprite avatars

ccImp can render a **frame-animated pixel-art mascot** instead of the
image/video avatar. It is the **default** on new installs (paired with the
default TUI Orange theme); switch between it and the image/video avatar in
*Settings → Avatar → Type*.

How it works:

- A **sprite set** is a folder under `sprites/<set>/` containing a
  `manifest.json` plus one subfolder of PNG frames per animation. The
  manifest lists each animation's frames with a per-frame `hold_ms`, so
  timing is expressive rather than a fixed frame rate.
- Per-state behaviour is **manifest-driven**: the manifest's `groups` array
  maps each of the five avatar states to an animation rotation list (the
  player rotates a list with more than one entry). In the bundled sets
  **Idle** drifts between breathing, blinking, looking around and the
  occasional **dance**; **Listening** looks around; **Thinking** and
  **Speaking** show the work animations; **Error** shows a surprise. A state
  with no group falls back to the set's `Idle` group. (This behaviour used
  to be hardcoded in `SPRITE_STATE_ANIMS`; it now lives entirely in each
  set's manifest, so a sprite set fully defines its own behaviour.)
- Frames are drawn on a canvas with nearest-neighbor scaling, so the small
  source art (20×20) stays crisp pixel art at any avatar size.

The bundled `claudeSprites` set is sourced from the **Clawdmeter** project
(see *Credits* below); an `impSprites` set scaffolds the project's own imp
mascot. To add another set, drop a `sprites/<name>/` folder in (with a
`manifest.json` defining its `groups`) and register `<name>` in
`KNOWN_SPRITE_SETS` in `src/lib/avatarConfig.ts` — no other app-code changes
are needed, since the per-state behaviour comes from the manifest.

## Troubleshooting

- **TTS silent.** Check the log for `TTS disabled: Kokoro model files not found.`
  Place the model + voicepack under `<exe-dir>/../models/` as documented above.
- **`claude` not found.** ccImp looks up `claude` via `PATH`. Either install
  Claude Code so it's on `PATH` or add its install dir.
- **Claude (local) tab errors.** Most often: the local backend isn't
  running or the URL in *Settings → Local LLM provider* is wrong.
  Confirm the backend is reachable (e.g.
  `curl http://localhost:1234/v1/models` for LM Studio). Until you fix
  the backend you can simply use the subscription Claude tab, which is
  unaffected.
- **CUDA EP errors per segment (silent output).** You're on a GPU not yet
  covered by the bundled ORT 1.20 prebuilt (Blackwell / RTX 5090). Unset
  `CCIMP_GPU` to fall back to CPU. See `MAINTENANCE.md`.
- **Audio interrupted by typing.** That's `Behavior → Interrupt TTS when typing`.
  Disable it if you'd rather keep playback rolling.
- **Avatar doesn't move.** Confirm `Settings → Avatar → Visible` is on
  and the per-state image paths point to readable files (or are blank to use
  the bundled defaults). In *Animated sprites* mode, confirm the chosen
  sprite set exists under `sprites/<set>/` with a valid `manifest.json`.
- **`Ctrl+R` / `Ctrl+Shift+R` / `F5` don't reload the app.** Intentional
  (v0.6.6+) — a reload would tear down every tab's session and lose
  scrollback. The keystrokes still reach the terminal, so `Ctrl+R` works
  as your shell's reverse-history search as usual.

## Known Limitations

- TTS markup compliance for the **Claude (local)** tab depends on the
  underlying model. Smaller local models may not wrap content in
  `[[TTS]]…[[/TTS]]` reliably even when the system prompt asks them to.
  ccImp will be silent for those segments — this is fallback behavior,
  not an error.
- Tool-use (Edit / Write / Bash / etc.) on the Claude (local) tab depends
  on the local model supporting Anthropic-style tool calling. Test before
  committing to a particular model.
- ccImp does not bundle or auto-spawn the local backend — you run
  LM Studio / Ollama / vLLM / llama-server yourself.
- Single audio output device — no UI selector.
- No conversation/session UI on top of the terminal.
- No STT input.
- No voice mixing.
- Linux is not part of the validation matrix; behavior is best-effort.
- Kokoro model is user-provided, not bundled.

See `docs/completedMilestones/MILESTONE-V1-08-polish.md` "After Milestone
8" and `docs/FUTURE-FEATURES.md` for the post-v1 parking lot.

## Credits

- **Animated sprite avatar (`claudeSprites`)** — pixel-art Clawd mascot
  animations from the [Clawdmeter](https://github.com/HermannBjorgvin/Clawdmeter)
  project (MIT-licensed code; pixel art from
  [claudepix](https://claudepix.vercel.app)). The Clawd character is property
  of Anthropic PBC. Full attribution in `NOTICE`.
- **TTS** — Kokoro-82M ONNX model and `af_heart` voicepack (Apache-2.0);
  espeak-ng phonemizer via misaki-rs (GPLv3+). See `NOTICE`.

## License

Apache-2.0. See `LICENSE`.

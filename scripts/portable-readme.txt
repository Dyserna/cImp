cImp portable (Windows x64)
============================

This zip is a self-contained build of cImp. No installer; no registry
entries; nothing written outside this folder. The global settings file
is written next to cimp.exe; per-launch-directory overlays go in
whatever folder you start cImp from. Transient runtime state
(scrollback) is kept under bin\scrollback\.


Quick start
-----------

  1. Unzip somewhere stable (e.g. C:\Tools\cImp\).
  2. Add the `bin\` subfolder to your PATH:
       - Press Win, type "environment variables", open the editor.
       - Edit the User PATH variable, add: <unzip-folder>\bin
       - Open a fresh terminal so the new PATH is loaded.
  3. Run `cimp` from any terminal, or double-click `bin\cimp.exe`.


Prerequisites
-------------

  * A supported harness CLI: cImp does not ship an AI agent — it spawns
    the harness binary as a subprocess, so install at least one
    separately and make sure it is on your PATH (or drop its .exe into
    the `ebin\` folder). The two supported today:

      Claude Code   binary `claude`   https://docs.anthropic.com/en/docs/claude-code/setup
      OpenCode      binary `opencode` https://opencode.ai/docs

    From a new terminal, `claude --version` (or `opencode --version`)
    should print a version. A fresh install enables the Claude tab only;
    turn the others on under Settings -> Tabs -> AI tabs enabled.

  * WebView2 runtime: preinstalled on updated Windows 10/11. If cImp
    fails to launch with a missing-WebView2 error, install the Evergreen
    Bootstrapper from
    https://developer.microsoft.com/en-us/microsoft-edge/webview2/.


What is bundled
---------------

  bin\cimp.exe                      the app (ONNX Runtime 1.23 for TTS is
                                     STATIC-LINKED into the exe — there is no
                                     onnxruntime.dll to look for)
  bin\webgpu_dawn.dll                WebGPU (Dawn) runtime for GPU TTS, with
  bin\dxcompiler.dll                 its two shader-compiler DLLs
  bin\dxil.dll
  bin\patterns.json                  editable prompt-detection patterns
                                     (see "Customizing prompt detection")
  bin\themes\<id>\                   UI chrome themes (theme.json + theme.css);
                                     edit a theme or drop in a new folder,
                                     then restart to pick it up
  bin\palettes\<name>.json           terminal color palettes (one file each);
                                     edit or add files, then restart
  ebin\                              drop-in folder for external CLI tools
                                     (see "External tools"); empty by default
  models\kokoro-v1.0.onnx            Kokoro 82M TTS model
  models\voices\af_heart.bin         default voice
  avatars\Idle.mp4 / Listening.mp4 / Thinking.mp4 / Speaking.mp4 / Error.mp4
                                     bundled avatar state videos (also
                                     embedded inside cimp.exe — see
                                     "Custom avatars" below)
  avatars\Transition.mp4             optional crossfade between states
  LICENSE                            Apache 2.0 (cImp source)
  NOTICE                             attributions for bundled assets


Custom avatars
--------------

The same avatar videos are also embedded inside cimp.exe, so the app
runs with its built-in defaults out of the box. The standalone copies
under `avatars\` are there if you want to swap one out:

  1. Drop a replacement file (mp4 / webm / mov / png / jpg) into
     `avatars\` (or anywhere else on disk).
  2. Open Settings -> Avatar.
  3. For each state (Idle / Listening / Thinking / Speaking / Error /
     Transition) point the picker at the file you want to use.

Until a state is overridden it keeps using the embedded default. Clear
an override to fall back to the bundled video.


External tools (ebin\)
----------------------

The `ebin\` folder ("external binaries") holds CLI tools cImp can launch
from its bottom-bar quick-launch buttons and shell tabs. When cImp
resolves a command it looks in `ebin\` FIRST, then falls back to your
PATH — so you can make a tool available to cImp just by dropping its
executable in here, without touching your PATH.

No tools are bundled. The quick-launch buttons cover:

  broot.exe     broot — a file browser with git info
                (https://github.com/Canop/broot)
  rustnet.exe   rustnet — a terminal network monitor
                (https://github.com/domcyrus/rustnet)
                NOTE: to actually capture traffic, rustnet needs
                Npcap installed with "WinPcap API-compatible Mode"
                enabled — https://npcap.com/. Without it rustnet
                launches but can't see packets.

Install a tool yourself (drop it in `ebin\` or put it on PATH), or use
Settings -> Bottom bar -> External tools to point rustnet or broot at a
specific exe in any folder; that path overrides the ebin\ / PATH lookup.
Leave it blank to resolve normally.


Customizing prompt detection
----------------------------

cImp watches the terminal for prompts it should react to — tool-use
approvals and "pick one" questions. The substrings it matches are the
harness's own terminal wording, one set per supported harness, and they
all live in:

  bin\patterns.json

Each entry lists one or more substrings under `all_of` that must ALL be
present in the on-screen text for the pattern to match, plus a `kind`
("permission" or "question") that decides how cImp reacts. Set
`disabled: true` to keep an entry without using it. Edit the file and
restart cImp to apply changes.

Note: the OpenCode entries ship `disabled` on purpose — its prompt
wording has not been captured yet, and a wrong guess would fire the
badge and the announcement on every OpenCode tab. Use the recipe below
to capture the real text, then flip `disabled` to false.

If a harness update changes the prompt wording and detection stops
firing, capture the live text by launching with

    set RUST_LOG=perm_capture=debug

(the rendered prompt is then written to the log under logs\), pick a
distinctive substring, and add it as a new pattern. If you delete or
corrupt the file, cImp falls back to built-in defaults so detection
keeps working.


Adding more voicepacks
----------------------

cImp auto-discovers `.bin` files in one location:

  models\voices\   (next to this README)

Drop any voicepack `.bin` from
https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/tree/main/voices
into that folder; it appears in Settings -> TTS -> Voice on next launch
(or the next time the dropdown is opened, depending on the build).


GPU acceleration
----------------

This build is GPU-accelerated out of the box (Kokoro TTS on WebGPU,
Whisper STT on Vulkan) and falls back to CPU automatically on machines
without a usable GPU. Nothing to install.

To force CPU (or switch back to GPU) for either feature, use the
"Process on" dropdown in Settings -> Audio -> TTS and Settings ->
Speech-to-text. The change takes effect immediately -- no restart.
See docs/MAINTENANCE.md in the repo for the GPU support matrix and the
Blackwell (RTX 5090) caveat.


Updating
--------

Download the next release zip and unzip over the top of this folder.
The zip ships the exe, DLLs, models, docs, and a default
bin\patterns.json — your existing settings.json (next to the exe) and
any per-folder .cimp\config.json overlays are not in the zip and
stay where they are.

Note: the full zip DOES contain bin\patterns.json, so unzipping it over
the top overwrites any edits you made to that file. If you've customized
your prompt-detection patterns, back the file up first, or use the
no-models update zip below (which omits patterns.json and leaves your
copy untouched).

For an exe-only update that preserves your existing model files and
patterns.json, grab the matching `*-no-models.zip` from the same
release.


Uninstall
---------

  1. Delete the unzipped folder. (This removes cimp.exe, the bundled
     models, the global settings.json, scrollback files, and logs —
     everything cImp writes lives inside this folder.)
  2. Remove the PATH entry you added.
  3. Optionally delete the `.cimp\` folder from any folder you used to
     start cImp to drop those per-folder overlays (and the code graph).


Troubleshooting
---------------

  * "claude not found" / "opencode not found" -> install that harness
    and ensure its binary is on PATH (or drop it in `ebin\`); restart
    the terminal after installing.
  * TTS silent, log shows "Kokoro model files not found" -> someone
    deleted models\kokoro-v1.0.onnx. Re-extract the zip or download the
    model from the HuggingFace link above.
  * Tab errors mention "permission detection" -> check
    docs/MAINTENANCE.md in the source repo; the patterns used to detect
    a harness's permission prompt occasionally need an update after that
    harness releases a new version.

Source code, issue tracker, full documentation:
  https://github.com/Dyserna/cImp

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

  * Claude Code: cImp spawns the `claude` binary as a subprocess. Install
    Claude Code separately and make sure `claude` is on your PATH. From a
    new terminal, `claude --version` should print a version.

  * WebView2 runtime: preinstalled on updated Windows 10/11. If cImp
    fails to launch with a missing-WebView2 error, install the Evergreen
    Bootstrapper from
    https://developer.microsoft.com/en-us/microsoft-edge/webview2/.


What is bundled
---------------

  bin\cimp.exe                      the app
  bin\onnxruntime.dll                CPU TTS inference (ORT 1.20)
  bin\onnxruntime_providers_shared.dll
  bin\patterns.json                  editable prompt-detection patterns
                                     (see "Customizing prompt detection")
  bin\themes\<id>\                   UI chrome themes (theme.json + theme.css);
                                     edit a theme or drop in a new folder,
                                     then restart to pick it up
  bin\palettes\<name>.json           terminal color palettes (one file each);
                                     edit or add files, then restart
  ebin\broot.exe                     bundled CLI tools (see "Bundled tools")
  ebin\rustnet.exe
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


Bundled tools (ebin\)
---------------------

The `ebin\` folder ("external binaries") holds CLI tools cImp can launch
from its bottom-bar quick-launch buttons and shell tabs. When cImp
resolves a command it looks in `ebin\` FIRST, then falls back to your
PATH — so a copy you install yourself can be overridden by the bundled
one, and you can add new tools just by dropping an executable in here.

  ebin\broot.exe     broot — a file browser with git info (MIT licensed).
  ebin\rustnet.exe   rustnet — a terminal network monitor (Apache-2.0).
                     NOTE: to actually capture traffic, rustnet needs
                     Npcap installed with "WinPcap API-compatible Mode"
                     enabled — https://npcap.com/. Without it rustnet
                     launches but can't see packets.

Prefer your own build of a tool? Settings -> Bottom bar -> External tools
lets you point rustnet or broot at a specific exe in any folder; that
path overrides the ebin\ / PATH lookup. Leave it blank to resolve
normally.

Aider is NOT bundled: its Windows launcher hardcodes the install
machine's Python path and isn't portable. If you want the Aider tab,
install it yourself (`pip install aider-chat`) and make sure `aider` is
on your PATH — cImp checks for it before letting you enable the tab, and
will refuse (with a message) if it can't find it. You can also drop a
working `aider` into `ebin\` and cImp will pick it up.


Customizing prompt detection
----------------------------

cImp watches the terminal for prompts it should react to — Claude Code
tool-use approvals, AskUserQuestion-style questions, and a few Aider
prompts. The substrings it matches live in:

  bin\patterns.json

Each entry lists one or more substrings under `all_of` that must ALL be
present in the on-screen text for the pattern to match, plus a `kind`
("permission" or "question") that decides how cImp reacts. Set
`disabled: true` to keep an entry without using it. Edit the file and
restart cImp to apply changes.

If a Claude Code update changes the prompt wording and detection stops
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

  * "claude not found" -> install Claude Code and ensure `claude` is on
    PATH; restart the terminal after installing.
  * TTS silent, log shows "Kokoro model files not found" -> someone
    deleted models\kokoro-v1.0.onnx. Re-extract the zip or download the
    model from the HuggingFace link above.
  * Tab errors mention "permission detection" -> check
    docs/MAINTENANCE.md in the source repo; the regex used to detect the
    Claude permission prompt occasionally needs an update after a Claude
    Code release.

Source code, issue tracker, full documentation:
  https://github.com/Dyserna/cImp

cctts portable (Windows x64)
============================

This zip is a self-contained build of cctts. No installer; no registry
entries. The global settings file is written next to cctts.exe; per-
launch-directory overlays go in whatever folder you start cctts from.
Transient runtime state (scrollback) lives under %APPDATA%\cctts\.


Quick start
-----------

  1. Unzip somewhere stable (e.g. C:\Tools\cctts\).
  2. Add the `bin\` subfolder to your PATH:
       - Press Win, type "environment variables", open the editor.
       - Edit the User PATH variable, add: <unzip-folder>\bin
       - Open a fresh terminal so the new PATH is loaded.
  3. Run `cctts` from any terminal, or double-click `bin\cctts.exe`.


Prerequisites
-------------

  * Claude Code: cctts spawns the `claude` binary as a subprocess. Install
    Claude Code separately and make sure `claude` is on your PATH. From a
    new terminal, `claude --version` should print a version.

  * WebView2 runtime: preinstalled on updated Windows 10/11. If cctts
    fails to launch with a missing-WebView2 error, install the Evergreen
    Bootstrapper from
    https://developer.microsoft.com/en-us/microsoft-edge/webview2/.


What is bundled
---------------

  bin\cctts.exe                      the app
  bin\onnxruntime.dll                CPU TTS inference (ORT 1.20)
  bin\onnxruntime_providers_shared.dll
  models\kokoro-v1.0.onnx            Kokoro 82M TTS model
  models\voices\af_heart.bin         default voice
  LICENSE                            Apache 2.0 (cctts source)
  NOTICE                             attributions for bundled assets


Adding more voicepacks
----------------------

cctts auto-discovers `.bin` files in two locations:

  1. Next to this README:  models\voices\
  2. In your user config:  %APPDATA%\cctts\models\voices\

Drop any voicepack `.bin` from
https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/tree/main/voices
into either folder; it appears in Settings -> TTS -> Voice on next launch
(or on next time the dropdown is opened, depending on the build).

If the same filename exists in both locations, the %APPDATA% copy wins —
so you can override a bundled voice by saving an edited version into
%APPDATA%\cctts\models\voices\ without touching this folder.


Optional: GPU acceleration
--------------------------

CPU is the default and works near-real-time for Kokoro. Opt into NVIDIA
CUDA with:

    setx CCTTS_GPU cuda

then restart cctts. Requires CUDA 12.x runtime + cuDNN 9 installed
separately. Known broken on Blackwell (RTX 5090); see docs/MAINTENANCE.md
in the repo for the GPU support matrix.


Updating
--------

Download the next release zip and unzip over the top of this folder.
The zip ships only the exe, DLLs, models, and docs — your existing
settings.json (next to the exe) and any per-folder
.cctts.custom.config.json overlays are not in the zip and stay where
they are.

For an exe-only update that preserves your existing model files, grab
the matching `*-no-models.zip` from the same release.


Uninstall
---------

  1. Delete the unzipped folder. (This removes cctts.exe, the bundled
     models, and the global settings.json that lives next to the exe.)
  2. Remove the PATH entry you added.
  3. Optionally delete `%APPDATA%\cctts\` to drop scrollback and any
     extra voicepacks you saved there.
  4. Optionally delete `.cctts.custom.config.json` from any folder you
     used to start cctts to drop those per-folder overlays.


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
  https://github.com/Dyserna/cctts

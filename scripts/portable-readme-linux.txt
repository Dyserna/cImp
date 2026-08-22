cImp portable (Linux x86-64)
============================

This tarball is a self-contained build of cImp. No installer; nothing
written outside this folder. The global settings file is written next to
the `cimp` binary; per-launch-directory overlays go in whatever folder
you start cImp from. Transient runtime state (scrollback) is kept under
bin/scrollback/.


Requirements
------------

  * Ubuntu 24.04+ (or an equivalent glibc >= 2.39 / libstdc++ >= GCC 13
    distro). The binary is built against that floor because ort's WebGPU
    runtime requires it — it will NOT start on Ubuntu 22.04.

  * System libraries (install via your package manager if missing):
      sudo apt-get install libwebkit2gtk-4.1-0 libasound2t64 \
                           libvulkan1 mesa-vulkan-drivers
    - libwebkit2gtk-4.1-0  : the webview cImp renders its UI in
    - libasound2(t64)      : ALSA, for TTS playback and mic capture
    - libvulkan1 + a Vulkan driver : GPU TTS/STT (any vendor: NVIDIA,
      AMD, Intel). Absent/unusable -> automatic CPU fallback.

  * A supported harness CLI: cImp does not ship an AI agent — it spawns
    the harness binary as a subprocess, so install at least one
    separately and make sure it is on your PATH (or drop it into the
    `ebin/` folder). The two supported today:

      Claude Code  binary `claude`   https://docs.anthropic.com/en/docs/claude-code/setup
      OpenCode     binary `opencode` https://opencode.ai/docs

    `claude --version` (or `opencode --version`) should print a version
    from a fresh shell. A fresh install enables the Claude tab only;
    turn the others on under Settings -> Tabs -> AI tabs enabled.


Quick start
-----------

  1. Extract somewhere stable:  tar -xzf cimp-portable-linux-x64-*.tar.gz
  2. Run it:                    ./cimp-portable-linux-x64-*/bin/cimp
     (optionally add the bin/ folder to your PATH so `cimp` works from
     any directory).


What is bundled
---------------

  bin/cimp                  the app (self-contained; onnxruntime is
                            statically linked in)
  bin/libwebgpu_dawn.so     GPU TTS runtime (ort's WebGPU/Dawn backend);
                            found via an $ORIGIN rpath next to cimp
  bin/espeak-ng-data/       compiled phoneme data for the espeak OOV
                            fallback (primary G2P is built in)
  bin/patterns.json         editable prompt-detection patterns
  bin/themes/<id>/          UI chrome themes (edit / add, then restart)
  bin/palettes/<name>.json  terminal color palettes (edit / add, restart)
  ebin/                     drop-in folder for external CLI tools (see
                            "External tools"); empty by default
  models/kokoro-v1.0.onnx   Kokoro 82M TTS model
  models/voices/*.bin       voicepacks (default: af_heart)
  models/ggml-small.bin     Whisper STT model (if present)
  avatars/ , sprites/       avatar state videos + pixel-art sets (also
                            embedded in the binary; these on-disk copies
                            are for customizing via Settings -> Avatar)
  LICENSE / NOTICE          Apache-2.0 + attributions for bundled assets


GPU acceleration
----------------

GPU-accelerated out of the box: Kokoro TTS on ort's WebGPU EP (Dawn ->
Vulkan) and Whisper STT on whisper.cpp's Vulkan backend. Runs on any
vendor's GPU and falls back to CPU automatically when none is usable.
Force CPU (or GPU) per feature with the "Process on" dropdown in
Settings -> Audio -> TTS and Settings -> Speech-to-text; effective
immediately, no restart.


External tools (ebin/)
----------------------

The `ebin/` folder ("external binaries") holds CLI tools cImp launches
from its quick-launch buttons and shell tabs. cImp resolves a command
from `ebin/` FIRST, then your PATH — drop an executable in here to make
it available without touching your PATH.

No tools are bundled. The quick-launch buttons cover:

  broot     a file browser with git info
            (https://github.com/Canop/broot)
  rustnet   a terminal network monitor
            (https://github.com/domcyrus/rustnet)
            NOTE: capturing packets needs raw-socket capability.
            Grant it once with:
                sudo setcap cap_net_raw,cap_net_admin+ep ebin/rustnet
            Without it rustnet launches but sees no packets.


Updating
--------

Extract the next release tarball over the top of this folder. For a
binary-only update that preserves your models and your edited
patterns.json, grab the matching `*-no-models.tar.gz` (it omits
models/, avatars/, sprites/ and patterns.json).


Troubleshooting
---------------

  * Won't start, "libwebgpu_dawn.so: cannot open shared object file" ->
    keep libwebgpu_dawn.so in the same bin/ folder as cimp (the rpath is
    relative to the binary).
  * Won't start on Ubuntu 22.04 -> unsupported; needs 24.04+ (glibc 2.39).
  * "claude not found" / "opencode not found" -> install that harness
    and put its binary on PATH (or drop it in `ebin/`).
  * TTS silent, log shows "Kokoro model files not found" -> restore
    models/kokoro-v1.0.onnx (re-extract the full tarball).

Source, issues, full docs:
  https://github.com/Dyserna/cImp

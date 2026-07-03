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

  * Claude Code: cImp spawns the `claude` binary as a subprocess. Install
    it separately and make sure `claude` is on your PATH. `claude
    --version` should print a version from a fresh shell.


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
  ebin/broot                bundled CLI tools (see "Bundled tools")
  ebin/rustnet
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


Bundled tools (ebin/)
---------------------

The `ebin/` folder ("external binaries") holds CLI tools cImp launches
from its quick-launch buttons and shell tabs. cImp resolves a command
from `ebin/` FIRST, then your PATH.

  ebin/broot     broot — a file browser with git info (MIT).
  ebin/rustnet   rustnet — a terminal network monitor (Apache-2.0).
                 NOTE: capturing packets needs raw-socket capability.
                 Grant it once with:
                     sudo setcap cap_net_raw,cap_net_admin+ep ebin/rustnet
                 Without it rustnet launches but sees no packets.

Aider is NOT bundled. Install it yourself (`pip install aider-chat`,
ensure `aider` is on PATH) or drop a working `aider` into `ebin/`.


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
  * "claude not found" -> install Claude Code and put `claude` on PATH.
  * TTS silent, log shows "Kokoro model files not found" -> restore
    models/kokoro-v1.0.onnx (re-extract the full tarball).

Source, issues, full docs:
  https://github.com/Dyserna/cImp

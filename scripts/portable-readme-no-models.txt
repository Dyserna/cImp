cImp portable (Windows x64) — no-models update zip
=====================================================

This zip is the same self-contained build of cImp as the full portable
zip, with one difference: it does NOT contain the Kokoro TTS model file
or the default voicepack. Use this zip when you already have a working
cImp install and just want to update the executable.

Compared to the full zip:

  bin\cimp.exe                      <- updated
  bin\onnxruntime*.dll               <- updated (matched to the new exe)
  LICENSE / NOTICE / README.txt      <- updated
  ebin\                              <- drop-in folder for external CLI
                                        tools (see the full README's
                                        "External tools"). No tools are
                                        bundled; anything you put here is
                                        left untouched by this update.
  avatars\ / sprites\                <- NOT INCLUDED (the canonical avatar
                                        videos and sprite sets are embedded
                                        in cimp.exe, so the app still shows
                                        them; the on-disk copies are only for
                                        customization and are left out of this
                                        update zip. The full zip ships them.)
  bin\patterns.json                  <- NOT INCLUDED (your edited prompt
                                        patterns are preserved)
  models\kokoro-v1.0.onnx            <- NOT INCLUDED (preserved from your existing install)
  models\voices\af_heart.bin         <- NOT INCLUDED (preserved from your existing install)


How to use (updating an existing install)
------------------------------------------

  1. Close any running cImp windows.
  2. Unzip OVER your existing cImp folder (the one that contains the
     `bin\` and `models\` directories from a previous full release).
     Windows will overwrite the exe + DLLs and leave your `models\`
     folder untouched.
  3. Launch cImp as usual.

Your settings file (settings.json next to cimp.exe), your edited
prompt-detection patterns (bin\patterns.json), and any per-folder
overlay files (.cimp\config.json) are not affected — the zip
never touches them.


Don't have an existing install?
-------------------------------

You probably want the full zip instead:

  cimp-portable-win-x64-<version>.zip

…which bundles the model + default voice. If you'd rather keep this
no-models zip and supply the model files yourself, drop them at:

  models\kokoro-v1.0.onnx
  models\voices\af_heart.bin

Sources:
  https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/onnx/model.onnx
    (rename to kokoro-v1.0.onnx)
  https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/voices/af_heart.bin


Everything else
---------------

For PATH setup, prerequisites (a supported harness CLI — Claude Code or
OpenCode — plus WebView2), voicepack
discovery, GPU acceleration, uninstall, and troubleshooting — see the
full README that ships with the standard portable zip, or the project
docs:

  https://github.com/Dyserna/cImp

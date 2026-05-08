cctts portable (Windows x64) — no-models update zip
=====================================================

This zip is the same self-contained build of cctts as the full portable
zip, with one difference: it does NOT contain the Kokoro TTS model file
or the default voicepack. Use this zip when you already have a working
cctts install and just want to update the executable.

Compared to the full zip:

  bin\cctts.exe                      <- updated
  bin\onnxruntime*.dll               <- updated (matched to the new exe)
  LICENSE / NOTICE / README.txt      <- updated
  models\kokoro-v1.0.onnx            <- NOT INCLUDED (preserved from your existing install)
  models\voices\af_heart.bin         <- NOT INCLUDED (preserved from your existing install)


How to use (updating an existing install)
------------------------------------------

  1. Close any running cctts windows.
  2. Unzip OVER your existing cctts folder (the one that contains the
     `bin\` and `models\` directories from a previous full release).
     Windows will overwrite the exe + DLLs and leave your `models\`
     folder untouched.
  3. Launch cctts as usual.

Your settings file (settings.json next to cctts.exe) and any per-folder
overlay files (.cctts.custom.config.json) are not affected — they live
outside `bin\` and the zip never touches them.


Don't have an existing install?
-------------------------------

You probably want the full zip instead:

  cctts-portable-win-x64-<version>.zip

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

For PATH setup, prerequisites (Claude Code, WebView2), voicepack
discovery, GPU acceleration, uninstall, and troubleshooting — see the
full README that ships with the standard portable zip, or the project
docs:

  https://github.com/Dyserna/cctts

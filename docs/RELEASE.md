# Cutting a release

The release pipeline is **tag-driven**. Pushing a `v*.*.*` tag triggers
`.github/workflows/release.yml` on a Windows runner; the workflow asserts
that the version in `package.json`, `src-tauri/Cargo.toml`, and
`src-tauri/tauri.conf.json` all equal the tag, then builds the portable
zip and attaches it to a GitHub release.

## The four commands

```sh
node scripts/bump-version.mjs 0.2.0
git commit -am "Release v0.2.0"
git tag v0.2.0
git push && git push --tags
```

The bump script rewrites the three version files and refreshes
`Cargo.lock`. The CI workflow does the rest.

## What the workflow does

1. Resolves the tag (from `push` or `workflow_dispatch.inputs.tag`).
2. Asserts the three version fields equal the tag's `X.Y.Z`. Fails fast
   with a "run scripts/bump-version.mjs" hint if they don't.
3. Sets up Node 20, Rust stable, the cargo cache, and an HTTP cache for
   the Kokoro download.
4. Downloads `kokoro-v1.0.onnx` and `af_heart.bin` from
   `https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/<KOKORO_REF>/...`
   (default `KOKORO_REF` is `main`; pin to a commit SHA in the workflow
   for bit-identical reproductions).
5. Verifies SHA-256 against `models/CHECKSUMS.txt` if the file has
   non-comment entries. If not, logs the computed SHAs so you can pin
   them on the next release.
6. `npm ci && npm run tauri build -- --no-bundle`.
7. Stages a portable layout under `cctts-portable-win-x64-vX.Y.Z/`:

   ```
   bin/
     cctts.exe
     onnxruntime*.dll
   models/
     kokoro-v1.0.onnx
     voices/af_heart.bin
   LICENSE
   NOTICE
   README.txt        (from scripts/portable-readme.txt)
   ```

8. Zips it.
9. Builds release notes: pulls the matching `## [X.Y.Z]` section from
   `CHANGELOG.md`. Falls back to GitHub's auto-generated notes
   (`POST /releases/generate-notes`) if the section is absent.
10. `gh release create vX.Y.Z` with the zip attached. Idempotent — re-running
    against an existing tag refreshes notes and re-uploads the zip.

## Pinning the Kokoro download

The first release is acceptable to ship from `main` on HuggingFace. After
that, replace `KOKORO_REF: main` in `release.yml` with a commit SHA from
the model repo, and copy the SHA-256 lines the workflow logged into
`models/CHECKSUMS.txt` so future builds fail-fast on any silent change.

## Manual re-build

Use the `workflow_dispatch` trigger from the Actions tab with the tag
name as input — useful if a transient HuggingFace download failed or you
want to refresh a release zip without retagging.

## Local sanity check before tagging

```sh
node scripts/bump-version.mjs 0.2.0
cd src-tauri && cargo check        # compiles
cd ..
npm run check                       # svelte-check
npm run test                        # vitest
```

You can also exercise the portable layout locally without the workflow:

```sh
npm run tauri build -- --no-bundle
mkdir -p staging/cctts-portable/bin staging/cctts-portable/models/voices
cp src-tauri/target/release/cctts.exe staging/cctts-portable/bin/
cp src-tauri/target/release/onnxruntime*.dll staging/cctts-portable/bin/
# Drop kokoro-v1.0.onnx into staging/cctts-portable/models/ and the
# voicepacks under staging/cctts-portable/models/voices/, then run
# staging/cctts-portable/bin/cctts.exe — `model_dir()` in
# src-tauri/src/tts/mod.rs resolves to `<exe-dir>/../models/`.
```

## Code signing

Currently unsigned. SmartScreen will warn first-run users. To add signing
later: configure `tauri.conf.json -> bundle.windows.signCommand` and add a
`signtool` invocation step to the workflow with a cert from
`secrets.WINDOWS_CERT_PFX_BASE64` and `secrets.WINDOWS_CERT_PASSWORD`. Out
of scope for personal-use builds.

## Linux builds

Deferred — Windows is the validated target. Adding a Linux job is a small
matrix change: the runner key (`ubuntu-latest`), the staging step (paths
and `.tar.gz` instead of `.zip`), and an `apt-get install` for the
WebKitGTK and libsoup deps. The version-assertion step and the release
publication are platform-agnostic and can be reused.

# cImp detection channel (`detection-v1`)

This is an **orphan branch**. It shares no history with `main`/`develop`; it
exists only to serve the signature-rule update channel that cImp's V32
injection-detection layer reads. Cloning it gets you these files and nothing
else.

## What is here

| file | what it is |
|---|---|
| `manifest.json` | the index cImp fetches. Pinned as `DEFAULT_MANIFEST_URL` in `src-tauri/src/offload/detection/updater/manifest.rs`. |
| `<version>-<name>.yar` | the YARA rule files a bundle is made of. Names carry the version because every version's files live side by side on this branch forever. |

The ref is **fixed and its contents change over time** — it is never a moving
"latest" pointer to somewhere else. That pin is what makes the channel curated:
cImp only ever reads rule content from a location this repo controls, which is
locked decision 13 of `docs/MILESTONE-V32-injection-hardening.md`. The corpora
these rules derive from (Vigil, garak) live in repositories we do not control,
and an update channel for a defense layer is itself attack surface.

## What is NOT here

The Prompt Guard 2 classifier weights. They ship as `models-v1` release assets
with `CHECKSUMS.txt`, the same pipeline as the TTS and STT blobs (locked
decision 7). The `classifier` component was removed from the updater on
2026-08-08: a released checkpoint has no update stream to poll.

## Publishing a new bundle

Full recipe in `detection/manifest.example.json` on `develop`. The two rules
that matter:

1. **Push the `.yar` files first, `manifest.json` last.** A manifest that
   references files not yet on the branch makes every install fail a check it
   would otherwise have skipped.
2. **Version is a DATE** (`2026.08.10`), not semver. `compare_versions` in
   `manifest.rs` splits on non-alphanumeric boundaries and compares segments
   numerically, so dates order correctly without being forced into semver.

Set `min_app_version` only when a bundle genuinely needs a newer app — e.g. a
rule using a yara-x feature older builds cannot compile.

## Serving contract

Artifacts must resolve under the manifest's own directory (`AssetAnchor` in
`manifest.rs`), and the fetcher **refuses redirects** by design. That is why
this is a branch on `raw.githubusercontent.com` and not a GitHub release: every
release-asset path answers with a 302 to a signed CDN host, so a release-hosted
channel would have silently never updated on any install.

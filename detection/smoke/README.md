# Detection smoke corpus (`detection/smoke/`)

The control documents the **V32 Phase C3 auto-updater** validates every
candidate bundle against before it is allowed to replace the live one
(`src-tauri/src/offload/detection/updater/validate.rs`). They ship next to
`cimp.exe` exactly like the rules themselves — `build.rs` mirrors this folder
into the dev build's `target/{profile}/detection/`, and `release.yml` copies
the whole `detection/` tree into both zips.

| Folder | Contract |
|---|---|
| `benign/*.txt` | **Must NOT match.** The false-positive control. A bundle that flags ordinary content trains the reader to ignore the warning header, which is worse than no detection at all. |
| `hostile/*.txt` | **MUST match.** The positive control. Without it, a bundle of syntactically valid rules that matches nothing would pass every other gate and silently turn the signature layer off — the exact failure locked decision 13 forbids. |

Both halves are also the classifier's smoke set: staged weights must score the
`hostile/` documents high, the `benign/` ones low, and the two populations must
actually separate (`validate::classifier_smoke_verdict`).

**An absent or empty corpus rejects the update.** A validator that silently
passes everything when its fixtures go missing is a quality signal with no
consumer.

## Growing it

The maintenance run curates the upstream bundle *and* this corpus together: a
new rule family should arrive with a hostile document that proves it fires, and
a false positive found in the field should arrive as a benign document that
would have caught it. Files are plain UTF-8 text, read non-recursively, sorted;
an empty file is ignored.

Two constraints worth knowing before adding one:

- Each document is scanned under a per-document time budget
  (`validate::SCAN_BUDGET`, the same 750 ms the live scanner enforces), so a
  multi-megabyte sample would fail bundles for the wrong reason.
- These documents gate *every future update*. A benign sample that is arguably
  hostile — or vice versa — becomes a permanent obstacle rather than a test
  failure someone can reason about, so keep each one unambiguous.

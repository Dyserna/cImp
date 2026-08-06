# cImp detection rules (`detection/rules.d/`)

YARA rules for the **signature screen** of the V32 injection-hardening
detection layer (`src-tauri/src/offload/detection/signature.rs`). They are
matched against the **raw text of every EXTERNAL tool result** — a fetched web
page, a docs lookup, anything a proxied MCP server returns — at the two
boundaries where that text enters an LLM's conversation.

## What a match does — and does not do

A match **only warns**. Locked decision 5 of
`docs/MILESTONE-V32-injection-hardening.md` makes detection a *surface* signal:

- the result gets a one-line warning header prepended **outside** the
  spotlighting envelope, and
- an `injection_flag` row (screen `signature`) lands in the Tool Activity feed
  naming the rules that matched.

The content itself is never modified, never truncated, never blocked, and the
call never fails. So a false positive costs a line of noise, not a broken
research task — but a rule that fires on every third page trains the reader to
ignore the header, which is why the rules below are written to be *specific*
rather than *sensitive*.

## Layout

| Path | Owner | Updater |
|---|---|---|
| `<exe-dir>/detection/rules.d/*.yar` | cImp (shipped bundle) | **replaced** by the C3 auto-updater |
| `<exe-dir>/detection/rules.d/local/*.yar` | you | **never touched** — hand-written rules survive every update |

Both directories are scanned non-recursively for `*.yar` / `*.yara`. In a dev
build `src-tauri/build.rs` copies this repo-root folder next to the built
binary, exactly as it does for `themes/` and `palettes/`; the release zip gets
its copy from `.github/workflows/release.yml`.

A file that fails to compile is **skipped with a WARN log, and the remaining
files still load** — a typo in one hand-written local rule must never take the
whole signature layer offline. The Settings → Tools → Detection block shows the
loaded/failed file counts.

## Writing your own rules

Drop a `.yar` file in `rules.d/local/`. Rule identifiers must be unique across
*all* loaded files (YARA requirement), so prefix yours — `My_...` — to avoid
colliding with a future shipped rule. Useful `meta` keys (all optional, all
purely informational today):

```yara
rule My_Vendor_Injection_Phrasing {
    meta:
        family   = "instruction-override"
        severity = "high"
        author   = "me"
    strings:
        $a = /some\s+phrase/ nocase
    condition:
        any of them
}
```

Scanning is bounded: only the first 256 KiB of a result is scanned and the
scanner runs under a wall-clock timeout, so a pathological rule degrades to
"no verdict", never to a stalled fetch.

## Provenance

The shipped rules are **cImp-authored**, but the signature *families* and much
of the phrasing vocabulary are derived from public prior art:

- **Vigil** (`deadbits/vigil-llm`, Apache-2.0) — its `data/yara/` ruleset
  (`instruction_bypass.yar`, `system_instructions.yar`, `mdexfil.yar`,
  `react.yar`) is the origin of the instruction-bypass, system-prompt-claim and
  markdown-image-exfiltration families. `CImp_Exfil_MarkdownImageQuery` is a
  widened re-derivation of Vigil's `MarkdownExfiltration`, which in turn cites
  Embrace The Red's Bing Chat image-exfiltration PoC.
- **garak** (`NVIDIA/garak`, Apache-2.0) — the `promptinject`, `dan` and
  `encoding` probe families supplied the hijack phrasings ("ignore … and say",
  uppercase forceful variants), the DAN / "developer mode" role-reassignment
  vocabulary, and the base64-then-execute shape.
- **Embrace The Red** (embracethered.com) — markdown/link exfiltration and
  Unicode-tag smuggling write-ups behind `CImp_Obfuscation_UnicodeTagSmuggling`.

No upstream rule text is copied verbatim; the phrasings were re-derived and
re-tuned against this codebase's false-positive control (a benign expository
page about prompt engineering — see the `signature` module's tests). Upstream
attribution is kept here because the C3 updater's curated bundle will keep
pulling from the same corpora.

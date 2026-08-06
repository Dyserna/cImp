//! V32 Phase D — the **channel-content tripwire** (locked decision 9).
//!
//! # The invariant
//!
//! Push content ([`PushNotice`](crate::offload::service::PushNotice)) may only ever
//! carry **text this application composed itself**. Never LLM output, never a
//! scanner finding message, never fetched page content, never a tool result.
//!
//! # Why it is not "just another injection surface"
//!
//! Every other path by which untrusted text reaches a model is *pull*: the
//! model asked for a page, a snippet, a search result, and the answer lands as
//! tool output inside a turn the user started. A push is different — V30's
//! session-channel path delivers a `<channel source="cimp-offload">` message
//! that STARTS a turn on an idle session, with no user in the loop. Untrusted
//! text on that path stops being ordinary indirect injection and becomes
//! *autonomous, turn-starting* injection: an attacker who can influence a push
//! string can make an idle agent act, unprompted, on their instructions. That
//! is the V30 contract this test exists to keep.
//!
//! `offload.session_push` is OFF by decision (2026-08-06) and the V30 code is
//! released but dormant — which is exactly why an automated tripwire is worth
//! more than a comment. Nothing exercises these producers today, so a future
//! one that starts interpolating a tool result would break no test, produce no
//! symptom, and ship.
//!
//! # Why a source scan and not a type
//!
//! The obvious alternative is to make the API refuse violations: have
//! `PushNotice::new` take a `&'static str` template plus parameters. It was
//! rejected for two reasons.
//!
//! 1. **It does not actually enforce the invariant.** A `&'static str` template
//!    with `String` parameters is exactly as violable — `"cImp says: {}"` with
//!    an LLM answer in the slot type-checks perfectly. Enforcement would need
//!    the parameters restricted to numbers and enums, and neither live producer
//!    qualifies: the graph notice interpolates a project directory name, the
//!    audit notice a project root and a tool name. So the type-level version
//!    would have to be weakened to the point where it no longer catches the
//!    thing it exists to catch.
//! 2. **The real requirement is human review, not a compile error.** Whether a
//!    given string is app-composed is a semantic judgement about where its
//!    parts came from — a reviewer's call. What automation can do is guarantee
//!    the call is *made*: fail the build whenever a producer is added or a
//!    template is edited, until someone re-reads it and updates the allowlist
//!    below. That is a strictly stronger tripwire than a type that can be
//!    satisfied without thought.
//!
//! # What the scan checks
//!
//! Every `PushNotice::new(` call site in **production** code — every occurrence
//! in `src/**.rs` that is not inside a comment and not inside a `#[cfg(test)]`
//! item ([`test_spans`]) — must appear in [`ALLOWED_CALL_SITES`] with a
//! matching fingerprint of its content argument. Every named helper that
//! composes push content on a call site's behalf must appear in
//! [`ALLOWED_CONTENT_HELPERS`] with a matching fingerprint of its whole body;
//! that second list is what stops the indirection loophole, since
//! `audit/runner.rs` passes `audit_push_content(snap, category)` and
//! fingerprinting the argument alone would leave the template inside that
//! function free to be rewritten.
//!
//! The scan also guards *itself*: it fails if it finds no producers at all, and
//! if either known producer file drops out of the results — a scan that has
//! quietly stopped watching is indistinguishable from a green suite otherwise.

use std::path::{Path, PathBuf};

/// One reviewed push producer: the call site's file, a stable fingerprint of
/// its content argument, and the human note recording *why* the reviewer
/// concluded the string is app-composed.
struct AllowedSite {
    file: &'static str,
    fingerprint: u64,
    note: &'static str,
}

/// The reviewed set of `PushNotice::new` call sites in production code.
///
/// Adding an entry is a deliberate act: read the composed string, confirm every
/// interpolated value is app-owned (a count, a duration, a path the user
/// configured, a fixed tool name) and NOT model output, a finding message or
/// fetched content, then record the fingerprint the failure message prints.
const ALLOWED_CALL_SITES: &[AllowedSite] = &[
    AllowedSite {
        file: "graph/service.rs",
        fingerprint: 0x0e5e_6569_9908_7eb0,
        note: "V30 graph-index-complete notice: a literal template interpolating the project \
               directory name, the indexed file/symbol/edge counts and the elapsed seconds — all \
               produced by cImp's own indexer.",
    },
    AllowedSite {
        file: "audit/runner.rs",
        fingerprint: 0x1193_0c73_1e79_13fc,
        note: "V30 audit-scan-complete notice: delegates composition to `audit_push_content`, \
               pinned separately in ALLOWED_CONTENT_HELPERS. Carries counts and the scan root — \
               never a finding's message text.",
    },
];

/// Named helpers that compose push content on a call site's behalf. Fingerprint
/// covers the WHOLE function body, so editing the template inside one trips the
/// same review gate as editing a call site.
const ALLOWED_CONTENT_HELPERS: &[AllowedSite] = &[AllowedSite {
    file: "audit/runner.rs",
    fingerprint: 0xbbb6_5359_bf25_150b,
    note: "`audit_push_content`: counts of done/failed tools, the category word, the scan root \
           and the pull-twin tool name. Deliberately does NOT quote any finding message — a \
           finding's text is scanner output about attacker-influenced source.",
}];

/// The one helper name the scan pins, by definition site. Kept beside the
/// allowlist so adding an indirection means adding it here too.
const CONTENT_HELPER_FNS: &[(&str, &str)] = &[("audit/runner.rs", "audit_push_content")];

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// FNV-1a over the whitespace-normalized text. Stable across rustfmt reflows
/// and CRLF/LF differences (which would otherwise make the allowlist churn on
/// every formatting pass) while still changing on any real edit.
fn fingerprint(text: &str) -> u64 {
    let normalized: String = {
        let mut out = String::with_capacity(text.len());
        let mut space = false;
        for c in text.chars() {
            if c.is_whitespace() {
                space = true;
            } else {
                if space && !out.is_empty() {
                    out.push(' ');
                }
                space = false;
                out.push(c);
            }
        }
        out
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in normalized.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Every `.rs` file under `src/`, as `(relative-slash-path, contents)`.
fn source_files() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    let rel = p
                        .strip_prefix(root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((rel, text));
                }
            }
        }
    }
    let root = src_root();
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out.sort();
    out
}

/// This file's own path, excluded from the scan: it names the constructor in
/// prose and in the scan's own search literal, neither of which is a producer.
const SELF: &str = "push_tripwire.rs";

/// Is the match at `at` inside a comment? Cheap line-based test — a `//` (which
/// covers `///` and `//!`) earlier on the same line, or a line that continues a
/// block comment. Enough for this repo, where the constructor is named in doc
/// comments (`oob/opencode.rs`, `offload/service.rs`) but never in a string
/// literal outside [`SELF`].
fn in_comment(text: &str, at: usize) -> bool {
    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let before = &text[line_start..at];
    before.contains("//") || before.trim_start().starts_with('*')
}

/// Byte ranges of every `#[cfg(test)]` item in `text`, so a match inside one
/// can be recognized as test code. Test-only constructions (the wire
/// round-trip fixtures in `loopback.rs`, `mcp.rs`, `offload/service.rs`,
/// `oob/opencode.rs`) are not producers — no channel ever carries them.
///
/// Deliberately NOT "everything before the first `#[cfg(test)]`": that
/// heuristic looked right and was silently wrong. `graph/service.rs` carries a
/// `#[cfg(test)] fn` at line ~504 and its real push producer at ~3449, so the
/// truncating version stopped watching the very producer locked decision 9
/// names — and the suite still went green because the other producer matched.
/// Every `#[cfg(test)]` item in this crate is brace-delimited (`mod`, `fn`,
/// `impl`), so taking each attribute's item span is both exact and general.
fn test_spans(text: &str) -> Vec<(usize, usize)> {
    const ATTR: &str = "#[cfg(test)]";
    let mut spans = Vec::new();
    let mut from = 0usize;
    while let Some(hit) = text[from..].find(ATTR) {
        let at = from + hit;
        from = at + ATTR.len();
        let Some(brace) = text[from..].find('{').map(|i| from + i) else {
            break;
        };
        if let Some(body) = balanced(text, brace, '{', '}') {
            spans.push((at, brace + body.len()));
            from = brace + body.len();
        }
    }
    spans
}

fn in_test_code(spans: &[(usize, usize)], at: usize) -> bool {
    spans.iter().any(|(s, e)| at >= *s && at < *e)
}

/// The source text of a balanced-delimiter run starting at `open_idx` (the
/// index OF the opening delimiter), including both delimiters. Ignores
/// delimiters inside `"…"` string literals so a template containing a brace or
/// paren cannot end the scan early.
fn balanced(text: &str, open_idx: usize, open: char, close: char) -> Option<&str> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in text[open_idx..].char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
        } else if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(&text[open_idx..open_idx + i + c.len_utf8()]);
            }
        }
    }
    None
}

/// The first argument of a call whose argument list source is `args` (the text
/// including the outer parens), split at the top-level comma.
fn first_argument(args: &str) -> &str {
    let inner = &args[1..args.len() - 1];
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in inner.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => return inner[..i].trim(),
            _ => {}
        }
    }
    inner.trim()
}

/// The failure message every assertion in this module shares — the invariant,
/// the reason it is not negotiable, and what a reviewer must do.
fn review_demand(what: &str, file: &str, actual: u64, text: &str) -> String {
    format!(
        "CHANNEL-CONTENT INVARIANT (V32 locked decision 9) — {what} in `{file}` is not in the \
         reviewed allowlist, or its text changed.\n\n\
         Push content may carry ONLY app-composed template text. Never LLM output, never a \
         scanner finding message, never fetched or tool-returned content. A push STARTS A TURN on \
         an idle session with no user in the loop, so untrusted text here is not ordinary \
         injection — it is autonomous, turn-starting injection (the V30 contract).\n\n\
         This test cannot judge provenance; a human must. Read the composed string, confirm every \
         interpolated value is app-owned (counts, durations, a configured path, a fixed tool \
         name), then record it in `src/push_tripwire.rs`:\n\
         \x20   fingerprint: {actual:#018x},\n\n\
         Current text:\n{text}\n"
    )
}

#[test]
fn every_production_push_producer_is_reviewed() {
    let mut found: Vec<(String, u64, String)> = Vec::new();
    for (rel, text) in source_files() {
        if rel == SELF {
            continue;
        }
        let spans = test_spans(&text);
        let mut from = 0usize;
        while let Some(hit) = text[from..].find("PushNotice::new") {
            let at = from + hit;
            let open = at + "PushNotice::new".len();
            if in_comment(&text, at) || in_test_code(&spans, at) {
                from = open;
                continue;
            }
            let paren = text[open..]
                .find('(')
                .map(|i| open + i)
                .expect("a call always opens its argument list");
            let args = balanced(&text, paren, '(', ')')
                .unwrap_or_else(|| panic!("unbalanced PushNotice::new argument list in {rel}"));
            let arg = first_argument(args);
            found.push((rel.clone(), fingerprint(arg), arg.to_string()));
            from = paren + args.len();
        }
    }

    assert!(
        !found.is_empty(),
        "the scan found no `PushNotice::new` call sites at all — the tripwire has stopped \
         watching anything (was the type renamed, or the production/test split heuristic broken?)"
    );
    // A per-producer floor as well as a global one: a heuristic that silently
    // stops seeing ONE file is the failure mode this test almost shipped with
    // (see `test_spans`), and it leaves the suite green while the invariant
    // goes unguarded.
    for file in ["graph/service.rs", "audit/runner.rs"] {
        assert!(
            found.iter().any(|(f, _, _)| f == file),
            "the scan no longer sees the known push producer in `{file}` — if the producer really \
             was removed, delete it from this list AND from ALLOWED_CALL_SITES; otherwise the scan \
             heuristic is broken and the invariant is unguarded."
        );
    }

    for (file, fp, text) in &found {
        let ok = ALLOWED_CALL_SITES
            .iter()
            .any(|a| a.file == file && a.fingerprint == *fp);
        assert!(ok, "{}", review_demand("a push producer", file, *fp, text));
    }
    // The allowlist must not rot the other way either: an entry whose call site
    // was deleted would quietly widen what a future edit can slip past review.
    for allowed in ALLOWED_CALL_SITES {
        assert!(
            found
                .iter()
                .any(|(f, fp, _)| f == allowed.file && fp == &allowed.fingerprint),
            "stale allowlist entry for `{}` ({}) — the call site it covers is gone. Remove it.",
            allowed.file,
            allowed.note,
        );
    }
}

#[test]
fn every_push_content_helper_is_reviewed() {
    for (rel, fn_name) in CONTENT_HELPER_FNS {
        let path = src_root().join(rel);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let prod: &str = &text;
        let sig = format!("fn {fn_name}(");
        let at = prod.find(&sig).unwrap_or_else(|| {
            panic!(
                "`{fn_name}` is gone from `{rel}` — if the indirection was removed, drop it from \
                 CONTENT_HELPER_FNS and ALLOWED_CONTENT_HELPERS; if it was renamed, re-review it."
            )
        });
        let brace = prod[at..]
            .find('{')
            .map(|i| at + i)
            .unwrap_or_else(|| panic!("no body for `{fn_name}` in {rel}"));
        let body = balanced(prod, brace, '{', '}')
            .unwrap_or_else(|| panic!("unbalanced body for `{fn_name}` in {rel}"));
        let fp = fingerprint(body);
        let ok = ALLOWED_CONTENT_HELPERS
            .iter()
            .any(|a| a.file == *rel && a.fingerprint == fp);
        assert!(
            ok,
            "{}",
            review_demand(&format!("push-content helper `{fn_name}`"), rel, fp, body)
        );
    }
}

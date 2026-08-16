//! V32 Phase D — **terminal escape hygiene** for external/model-derived text.
//!
//! # Why this exists
//!
//! Spotlighting ([`offload::spotlight`](crate::offload::spotlight)) tells the
//! reading *model* that fetched bytes are data. This module addresses the other
//! reader: a fetched page — or an assistant message quoting one — can carry raw
//! ANSI/OSC control sequences, and cImp re-composes that text into strings that
//! leave the webview's HTML sandbox. Svelte's auto-escaping already neutralises
//! HTML in the display paths, but it does nothing about escape sequences, which
//! are *not* markup: they are instructions to a terminal-shaped sink.
//!
//! The concrete hijacks a control sequence buys an attacker:
//!
//! - **OSC 52** (`ESC ] 52 ; c ; <base64> BEL`) — writes the system clipboard.
//!   A page echoed into a terminal that honours it can plant a command the user
//!   later pastes into a shell. (cImp's own terminal does not honour it — see
//!   the audit note in `src/lib/terminals.ts` — but text we compose can be
//!   copied, logged, or piped anywhere.)
//! - **CSI / cursor motion / SGR** — repaint or overwrite already-printed
//!   lines, so a benign-looking transcript can hide what actually happened.
//! - **DCS / APC / PM / SOS strings** — terminal-specific command channels.
//! - **C0 controls** (`\r`, `\x08`, `\x0b`) — line overwrite and backspace
//!   tricks that spoof text without any escape at all.
//!
//! # What it does
//!
//! [`strip_terminal_escapes`] removes ESC-initiated sequences **whole**
//! (introducer *and* body, so the payload cannot survive as visible text once
//! its introducer is gone), plus the 8-bit C1 forms of the same sequences, plus
//! bare C0/DEL controls. `\n` and `\t` are preserved — they are the only two
//! control characters that carry meaning in the text cImp composes (multi-line
//! prose, tabular tool output).
//!
//! It is deliberately a *stripper*, not an escaper: the sinks (a TTS
//! synthesizer, a toast, a notification) have no use for the sequence in any
//! form, and rendering `^[[31m` as literal text would only relocate the noise.
//!
//! # Where it is called
//!
//! At the composition sites where EXTERNAL or model-derived text enters a
//! non-HTML sink — today the out-of-band TTS path
//! ([`OobContext::speak`](crate::oob::OobContext::speak)). Display-only webview
//! paths (Tool Activity payload popups, which do carry raw external text) are
//! **not** sanitized: Svelte escapes them, they are shown as inert text, and
//! stripping there would falsify the forensic record the user opened the popup
//! to inspect.
//!
//! # V35 Phase H — the second sink: text that lands on DISK
//!
//! [`scrub_payload`] is the capture path's scrubber
//! ([`crate::harness::capture`]). It is in this module because the milestone's
//! locked decision 4 names this file as the one scrubber, and because the first
//! thing a captured payload needs is exactly what [`strip_terminal_escapes`]
//! does. But it needs a **second** thing this module did not have, and the gap
//! is worth stating plainly rather than leaving implied:
//!
//! > Everything above this section is **terminal-escape hygiene**. It removes
//! > control sequences. It has never removed a credential, and reading
//! > "scrubbed through `processing/sanitize.rs`" as "redacted" would have been
//! > wrong before this function existed.
//!
//! So [`scrub_payload`] composes the strip with a **credential redaction** —
//! and it deliberately does not invent patterns for it. The rule set is the one
//! already compiled into this process for the memory secret screen
//! ([`crate::graph::secrets`], `graph/secrets.yar`), reached through
//! [`crate::graph::secrets::credential_rules`]. One curated corpus, two
//! consumers with different actions: memory *quarantines* a hit for review, a
//! capture *removes* it, because a capture has no reviewer and no user waiting
//! on it.
//!
//! ## The redaction unit is a container, never a byte range
//!
//! yara can report where a pattern matched, and redacting exactly those bytes
//! is the obvious implementation — and it is wrong here.
//! `secret_private_key_block` matches the `-----BEGIN … PRIVATE KEY-----`
//! header; the key itself is in the *following* bytes, which no pattern covers.
//! Byte-range redaction would therefore delete the label and keep the secret.
//!
//! Instead the unit is the smallest **structural container** that holds the
//! match: a JSON string value where the text parses as JSON, otherwise a whole
//! line. The whole container is replaced by a marker naming the rules that
//! fired. That keeps the shape a capture exists to record (every key, every
//! type, every nesting level survives) while making a partial match impossible
//! by construction — and where the match cannot be localized to a container at
//! all (a secret in an object *key*, a JSON shape too deep to walk), the whole
//! line goes. Every path out of here is fail-closed.

use std::borrow::Cow;

use serde_json::Value;

use crate::offload::detection::signature;

/// C0: bell, doubles as an OSC string terminator.
const BEL: char = '\u{07}';
/// The 7-bit escape introducer.
const ESC: char = '\u{1b}';
/// C1 single-shift/introducer range (8-bit forms of `ESC <char>`).
const C1_START: char = '\u{80}';
const C1_END: char = '\u{9f}';
/// C1 `CSI` — the 8-bit form of `ESC [`.
const C1_CSI: char = '\u{9b}';
/// C1 `ST` (string terminator) — the 8-bit form of `ESC \`.
const C1_ST: char = '\u{9c}';

/// Strip terminal control sequences from text that came from outside cImp.
///
/// Removes, whole:
/// - `ESC [ … <final>` (CSI: SGR colors, cursor motion, erase, DEC private
///   modes) and its 8-bit form `\u{9b} … <final>`;
/// - `ESC ] … (BEL | ESC \ | \u{9c})` (OSC — **including OSC 52 clipboard
///   writes**) and its 8-bit form `\u{9d} …`;
/// - `ESC P` / `ESC X` / `ESC ^` / `ESC _` string sequences (DCS/SOS/PM/APC)
///   and their 8-bit forms, to the same terminators;
/// - two-character escapes (charset designation `ESC ( B`, `ESC # 8`, …) and
///   single-character ones (`ESC c` full reset, `ESC 7`/`ESC 8` cursor save);
/// - a lone/trailing `ESC` with nothing after it;
/// - every remaining C0 control except `\n` and `\t`, `DEL`, and every C1.
///
/// An unterminated string sequence consumes the rest of the input — the same
/// choice a real terminal makes, and the safe one: the alternative (emit the
/// body as text) would let a truncated payload reappear.
///
/// Returns [`Cow::Borrowed`] unchanged when the input is already clean, so the
/// overwhelmingly common case allocates nothing.
pub fn strip_terminal_escapes(s: &str) -> Cow<'_, str> {
    if !s.chars().any(needs_stripping) {
        return Cow::Borrowed(s);
    }
    // Indexed over a char buffer rather than an iterator: the string scanner
    // needs one character of lookahead that it may decline to consume (an
    // escape opening *inside* a string body is handed back to the main loop
    // instead of being eaten, so the nested sequence is stripped too and can
    // never fall out as visible text).
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ESC => {
                i += 1;
                let Some(&next) = chars.get(i) else {
                    break; // trailing lone ESC — nothing follows, nothing to keep
                };
                match next {
                    // CSI: params 0x30–0x3F, intermediates 0x20–0x2F,
                    // final 0x40–0x7E.
                    '[' => {
                        i += 1;
                        skip_csi(&chars, &mut i);
                    }
                    // OSC / DCS / SOS / PM / APC: a string body up to ST or BEL.
                    ']' | 'P' | 'X' | '^' | '_' => {
                        i += 1;
                        skip_string(&chars, &mut i);
                    }
                    // Escapes with exactly one following byte: charset
                    // designation (`ESC ( B`), DEC line size (`ESC # 8`),
                    // character-set select (`ESC % G`). Drop the pair.
                    '(' | ')' | '*' | '+' | '-' | '.' | '/' | '#' | '%' => i += 2,
                    // Any other single-byte escape (`ESC c` full reset,
                    // `ESC 7`/`ESC 8`, `ESC =`), plus a stray `ESC \`.
                    _ => i += 1,
                }
            }
            C1_CSI => {
                i += 1;
                skip_csi(&chars, &mut i);
            }
            // 8-bit DCS / SOS / OSC / PM / APC introducers.
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => {
                i += 1;
                skip_string(&chars, &mut i);
            }
            '\n' | '\t' => {
                out.push(c);
                i += 1;
            }
            _ if is_bare_control(c) => i += 1,
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Cow::Owned(out)
}

/// Would `c` (or a sequence it introduces) be removed? The cheap pre-scan that
/// keeps the clean path allocation-free.
fn needs_stripping(c: char) -> bool {
    c == ESC || is_bare_control(c)
}

/// A control character with no meaning in composed text: C0 except `\n`/`\t`,
/// `DEL`, and the whole C1 block. `\r` is included deliberately — a carriage
/// return overwrites the current line in a terminal, which is a spoofing
/// primitive on its own, and `\r\n` still leaves its `\n`.
fn is_bare_control(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t') || (C1_START..=C1_END).contains(&c)
}

/// Skip a CSI body: zero or more parameter bytes (`0x30..=0x3F`), zero or more
/// intermediates (`0x20..=0x2F`), then one final byte (`0x40..=0x7E`).
/// A malformed run (cut short by end-of-input, or a byte outside all three
/// classes) simply stops — the offending character is left for the main loop,
/// which drops it if it is a control and keeps it otherwise.
fn skip_csi(chars: &[char], i: &mut usize) {
    while let Some(&c) = chars.get(*i) {
        match c {
            '\u{20}'..='\u{3f}' => *i += 1,
            '\u{40}'..='\u{7e}' => {
                *i += 1;
                return;
            }
            _ => return,
        }
    }
}

/// Skip a string-sequence body (OSC/DCS/SOS/PM/APC) up to and including its
/// terminator: `BEL`, `ESC \` (7-bit ST) or `\u{9c}` (8-bit ST). Unterminated ⇒
/// runs to end of input, matching what a terminal does and denying a truncated
/// payload any way back into the visible text.
fn skip_string(chars: &[char], i: &mut usize) {
    while let Some(&c) = chars.get(*i) {
        match c {
            BEL | C1_ST => {
                *i += 1;
                return;
            }
            ESC => {
                if chars.get(*i + 1) == Some(&'\\') {
                    *i += 2; // proper 7-bit ST
                    return;
                }
                // A non-ST escape inside the body aborts the string (as a real
                // terminal's parser does). Return WITHOUT consuming the ESC so
                // the main loop re-enters escape handling: the nested sequence
                // is stripped in turn rather than spilling out as text.
                return;
            }
            _ => *i += 1,
        }
    }
}

// ── V35 Phase H: credential redaction for text that lands on disk ───────────

/// The largest single line [`scrub_payload`] will scrub. A longer one is
/// **omitted wholesale**, never truncated and never written.
///
/// The number is not a size preference, it is the M-20 invariant from
/// [`crate::graph::secrets`] restated at a second boundary: the screen only
/// protects what it *reads*, and [`signature::scan_outcome_with`] stops at
/// [`signature::SCAN_PREFIX_BYTES`]. A line past that cap would be scanned in
/// part and written in full, so a credential could be pushed out of the
/// scanner's reach with padding — exactly the bypass M-20 found. 64 KiB is the
/// same figure `graph::secrets::MAX_NOTE_BYTES` picked, for the same reason
/// (a wide margin against `SCAN_PASS_TIMEOUT` as well as against the prefix),
/// and the static assert below is what keeps the two from drifting apart
/// silently.
pub const MAX_SCRUBBABLE_LINE_BYTES: usize = 64 * 1024;

/// The bound above is only real while it sits at or below what the scanner
/// reads. A compile-time check rather than a test, because the failure it
/// guards ships quietly.
const _: () = assert!(MAX_SCRUBBABLE_LINE_BYTES <= signature::SCAN_PREFIX_BYTES);

/// How deep [`scrub_payload`] will walk a JSON line looking for the string
/// value a credential landed in. Past this the match is treated as
/// unlocalizable and the whole line is replaced — the fail-closed answer.
const MAX_JSON_DEPTH: usize = 48;

/// The result of scrubbing one payload. The counts exist so the capture path
/// can log what it did at debug level without logging *what was removed*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scrubbed {
    /// The text to write. Terminal-escape-free and credential-free.
    pub text: String,
    /// How many containers (JSON string values, or whole lines) were replaced.
    pub redactions: usize,
    /// How many lines were dropped for exceeding [`MAX_SCRUBBABLE_LINE_BYTES`].
    pub omitted: usize,
}

/// Scrub `text` for writing to disk: strip terminal escapes, then redact every
/// container carrying something the credential rule set recognizes.
///
/// # `None` means "do not write"
///
/// The screen is fail-**closed** here, which is the opposite of every other
/// caller of the same rule set — and the difference is deliberate.
/// [`crate::graph::secrets`] degrades to "no hits" when the baked rules do not
/// compile, because *a screen that cannot run must not become a refusal path*
/// for a user's own memory note. A capture has no user waiting on it: the only
/// thing lost by refusing is one file in a diagnostic corpus, and the thing
/// risked by proceeding is a credential written to disk unscreened. So an
/// unavailable screen returns `None` and the caller writes nothing at all.
pub fn scrub_payload(text: &str) -> Option<Scrubbed> {
    let rules = crate::graph::secrets::credential_rules()?;
    let stripped = strip_terminal_escapes(text);
    let mut out = String::with_capacity(stripped.len());
    let mut redactions = 0usize;
    let mut omitted = 0usize;

    for (n, line) in stripped.lines().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        if line.len() > MAX_SCRUBBABLE_LINE_BYTES {
            out.push_str(&format!(
                "[OMITTED: a {}-byte line exceeds the {}-byte scrub limit, so it could not be \
                 screened in full and was not written]",
                line.len(),
                MAX_SCRUBBABLE_LINE_BYTES
            ));
            omitted += 1;
            continue;
        }
        // One scan for the overwhelmingly common clean line. Descending into a
        // JSON tree costs one scan per string leaf, so it only happens for a
        // line that already answered "yes, something in here matched".
        let hits = signature::scan_with(&rules, line);
        if hits.is_empty() {
            out.push_str(line);
            continue;
        }
        match redact_json_line(line, &rules) {
            Some((redacted, count)) => {
                out.push_str(&redacted);
                redactions += count;
            }
            None => {
                out.push_str(&format!("[REDACTED LINE: matched {}]", hits.join(", ")));
                redactions += 1;
            }
        }
    }
    // `str::lines` drops a trailing newline; a capture file that gained or lost
    // one between two versions would be a diff hunk that means nothing.
    if text.ends_with('\n') {
        out.push('\n');
    }
    Some(Scrubbed {
        text: out,
        redactions,
        omitted,
    })
}

/// Try to localize the hits inside one JSON line to individual string values.
///
/// `None` — the line is not JSON, the match did not land in any string value
/// (an object *key*, or a shape too deep to walk), or re-serialization failed.
/// Every one of those is the caller's cue to drop the whole line, so a match
/// this cannot place can never be written.
fn redact_json_line(line: &str, rules: &yara_x::Rules) -> Option<(String, usize)> {
    let mut value: Value = serde_json::from_str(line).ok()?;
    let mut redacted = 0usize;
    if !redact_value(&mut value, rules, 0, &mut redacted) {
        return None;
    }
    if redacted == 0 {
        // The line matched but no string value did: whatever fired is spread
        // across the serialized form (a key, or punctuation between fields).
        return None;
    }
    Some((serde_json::to_string(&value).ok()?, redacted))
}

/// Walk `value`, replacing every string leaf the rules match. Returns `false`
/// when the walk hit something it cannot vouch for — a key that matched, or a
/// tree deeper than [`MAX_JSON_DEPTH`] — which makes the caller drop the line.
fn redact_value(value: &mut Value, rules: &yara_x::Rules, depth: usize, redacted: &mut usize) -> bool {
    if depth > MAX_JSON_DEPTH {
        return false;
    }
    match value {
        Value::String(s) => {
            let hits = signature::scan_with(rules, s);
            if !hits.is_empty() {
                *s = format!("[REDACTED: {}]", hits.join(", "));
                *redacted += 1;
            }
            true
        }
        Value::Array(items) => items
            .iter_mut()
            .all(|v| redact_value(v, rules, depth + 1, redacted)),
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                // A credential used as an object KEY cannot be replaced without
                // changing the shape the capture exists to record, so it is not
                // localized at all — the line goes.
                if !signature::scan_with(rules, key).is_empty() {
                    return false;
                }
                if !redact_value(v, rules, depth + 1, redacted) {
                    return false;
                }
            }
            true
        }
        // Numbers, booleans and nulls cannot carry a credential.
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The headline case (locked decision / Phase D live-verify 8): a page
    /// carrying an OSC 52 clipboard write must not reach a sink with the
    /// sequence intact — introducer *and* base64 payload go.
    #[test]
    fn strips_osc_52_clipboard_writes_with_both_terminators() {
        // BEL-terminated (the common form).
        let bel = "before\u{1b}]52;c;bWFsaWNpb3VzIGNvbW1hbmQ=\u{07}after";
        assert_eq!(strip_terminal_escapes(bel), "beforeafter");
        // ST-terminated (`ESC \`).
        let st = "before\u{1b}]52;c;bWFsaWNpb3Vz\u{1b}\\after";
        assert_eq!(strip_terminal_escapes(st), "beforeafter");
        // 8-bit ST.
        let c1_st = "before\u{1b}]52;c;bWFsaWNpb3Vz\u{9c}after";
        assert_eq!(strip_terminal_escapes(c1_st), "beforeafter");
        // 8-bit OSC introducer.
        let c1_osc = "before\u{9d}52;c;bWFsaWNpb3Vz\u{07}after";
        assert_eq!(strip_terminal_escapes(c1_osc), "beforeafter");
        // No fragment of the payload survives — the whole point of removing the
        // sequence rather than just its introducer.
        for input in [bel, st, c1_st, c1_osc] {
            let out = strip_terminal_escapes(input);
            assert!(!out.contains("52;"), "{out:?}");
            assert!(!out.contains("bWFs"), "{out:?}");
        }
    }

    #[test]
    fn strips_csi_color_and_cursor_sequences() {
        assert_eq!(
            strip_terminal_escapes("\u{1b}[31mred\u{1b}[0m plain"),
            "red plain"
        );
        // Cursor motion, erase-line, DEC private mode (`?` parameter prefix),
        // and a multi-parameter SGR.
        assert_eq!(
            strip_terminal_escapes("a\u{1b}[2Jb\u{1b}[1;2Hc\u{1b}[?25ld\u{1b}[1;38;5;196me"),
            "abcde"
        );
        // 8-bit CSI.
        assert_eq!(strip_terminal_escapes("a\u{9b}31mb"), "ab");
    }

    /// The non-regression half: ordinary composed text — prose, tabs, blank
    /// lines, and the bracket characters that merely *look* like CSI — must
    /// come back byte-identical, and without allocating.
    #[test]
    fn plain_multiline_text_with_tabs_is_untouched_and_borrowed() {
        let text = "Line one.\n\tIndented [not a CSI] value\n\nLast line — 100% fine.\n";
        let out = strip_terminal_escapes(text);
        assert!(
            matches!(out, Cow::Borrowed(_)),
            "clean input must not allocate"
        );
        assert_eq!(out, text);
        // Including the empty string.
        assert!(matches!(strip_terminal_escapes(""), Cow::Borrowed("")));
    }

    #[test]
    fn strips_dcs_sos_pm_apc_string_sequences() {
        for intro in ['P', 'X', '^', '_'] {
            let s = format!("a\u{1b}{intro}payload;1;2\u{1b}\\b");
            assert_eq!(strip_terminal_escapes(&s), "ab", "ESC {intro}");
        }
        // 8-bit APC/PM/DCS introducers.
        assert_eq!(strip_terminal_escapes("a\u{9f}payload\u{9c}b"), "ab");
        assert_eq!(strip_terminal_escapes("a\u{9e}payload\u{07}b"), "ab");
        assert_eq!(strip_terminal_escapes("a\u{90}q\u{9c}b"), "ab");
    }

    /// An unterminated string sequence swallows the rest of the input — a real
    /// terminal does the same, and emitting the body as text would let a
    /// truncated payload reappear as visible content.
    #[test]
    fn unterminated_string_sequence_consumes_to_end() {
        assert_eq!(
            strip_terminal_escapes("keep\u{1b}]52;c;dGFpbA=="),
            "keep",
            "no terminator ⇒ nothing after the introducer survives"
        );
    }

    #[test]
    fn drops_a_lone_or_trailing_escape() {
        assert_eq!(strip_terminal_escapes("text\u{1b}"), "text");
        // ESC followed by a plain letter is a single-char escape (`ESC c` =
        // full reset): both characters go, and only those two.
        assert_eq!(strip_terminal_escapes("a\u{1b}cb"), "ab");
        // Two-byte charset designation.
        assert_eq!(strip_terminal_escapes("a\u{1b}(Bb"), "ab");
        assert_eq!(strip_terminal_escapes("a\u{1b}#8b"), "ab");
    }

    /// Bare C0/C1/DEL controls are spoofing primitives with no escape at all
    /// (`\r` overwrites the line, `\x08` backspaces over what was printed).
    #[test]
    fn strips_bare_controls_but_keeps_newline_and_tab() {
        assert_eq!(
            strip_terminal_escapes("ok\r\nnext\u{08}\u{0b}\u{0c}\u{7f}\u{85}\ttail"),
            "ok\nnext\ttail"
        );
        assert_eq!(strip_terminal_escapes("nul\0byte"), "nulbyte");
    }

    /// A hostile page cannot reintroduce a sequence by nesting introducers:
    /// whatever the body contains, the strip runs to the terminator.
    #[test]
    fn nested_introducers_inside_a_string_body_do_not_escape_the_strip() {
        let hostile = "x\u{1b}]0;\u{1b}]52;c;cHduZWQ=\u{07}\u{07}y";
        let out = strip_terminal_escapes(hostile);
        // The inner BEL closes the (single) string body; the trailing BEL is a
        // bare control and is dropped too. Nothing executable is left.
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(!out.contains('\u{07}'), "{out:?}");
        assert!(!out.contains("52;"), "{out:?}");
        assert_eq!(out, "xy");
    }

    // ── V35 Phase H: the disk-bound scrubber ──
    //
    // Every sample below is synthetic. The credential-shaped ones are
    // assembled at runtime (`concat!` / `repeat`) so no literal token appears
    // contiguously in this file — same discipline `graph::secrets`' own samples
    // follow, because a well-shaped fake still trips repo-side push protection.

    /// A credential inside a JSON string value is replaced **in place**: the
    /// secret is gone and the shape — every key, every type, every nesting
    /// level — survives. That combination is the whole reason the redaction
    /// unit is a container rather than a byte range.
    #[test]
    fn a_secret_in_a_json_value_is_replaced_and_the_shape_survives() {
        let key = format!("sk-{}", "A".repeat(36));
        let line = serde_json::json!({
            "type": "assistant",
            "message": { "id": "m1", "usage": { "input_tokens": 10 },
                         "content": [{ "type": "text", "text": format!("token {key}") }] }
        })
        .to_string();

        let got = scrub_payload(&line).expect("the baked rule set compiles");
        assert!(!got.text.contains(&key), "{}", got.text);
        assert_eq!(got.redactions, 1);
        assert_eq!(got.omitted, 0);

        let v: Value = serde_json::from_str(&got.text).expect("still one JSON object");
        assert_eq!(v["type"], "assistant");
        assert_eq!(v["message"]["id"], "m1");
        assert_eq!(v["message"]["usage"]["input_tokens"], 10);
        assert_eq!(v["message"]["content"][0]["type"], "text");
        let text = v["message"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("[REDACTED:"), "{text}");
        assert!(text.contains("secret_"), "the marker names the rule: {text}");
    }

    /// Non-JSON text redacts by line, and only the offending line — the rest of
    /// a `claude --help` capture has to stay diffable.
    #[test]
    fn plain_text_redacts_only_the_offending_line() {
        let aws = "AKIAIOSFODNN7EXAMPLE";
        let text = format!("Options:\n  --settings <file>\nkey={aws}\n  --session-id\n");
        let got = scrub_payload(&text).expect("compiles");

        assert!(!got.text.contains(aws), "{}", got.text);
        assert_eq!(got.redactions, 1);
        let lines: Vec<&str> = got.text.lines().collect();
        assert_eq!(lines[0], "Options:");
        assert_eq!(lines[1], "  --settings <file>");
        assert!(lines[2].starts_with("[REDACTED LINE:"), "{:?}", lines[2]);
        assert!(lines[2].contains("secret_aws_access_key_id"), "{:?}", lines[2]);
        assert_eq!(lines[3], "  --session-id");
        assert!(got.text.ends_with('\n'), "a trailing newline must survive");
    }

    /// Clean text comes back **byte-identical**, including its JSON formatting.
    /// A scrubber that re-serialized every line would make the corpus diff on
    /// its own normalization rather than on upstream's shape.
    #[test]
    fn clean_text_is_returned_verbatim() {
        for sample in [
            "{\"type\":\"user\",\"sessionId\":\"s-1\",\"version\":\"2.1.232\"}\n",
            "Options:\n  -c, --continue    Continue the most recent conversation\n",
            "[\"bash\",\"read\",\"webfetch\"]",
            "",
        ] {
            let got = scrub_payload(sample).expect("compiles");
            assert_eq!(got.text, sample, "{sample:?}");
            assert_eq!(got.redactions, 0);
            assert_eq!(got.omitted, 0);
        }
    }

    /// The strip still runs — a capture is text that lands in an editor, a
    /// terminal and an issue comment, and an OSC 52 in a transcript line would
    /// reach all three.
    #[test]
    fn terminal_escapes_are_stripped_on_the_capture_path_too() {
        let got = scrub_payload("a\u{1b}]52;c;cHduZWQ=\u{07}b\u{1b}[31mc").expect("compiles");
        assert_eq!(got.text, "abc");
    }

    /// The two fail-CLOSED paths, which are the ones worth pinning: a match
    /// this cannot localize to a string value takes the whole line, and a line
    /// too large for the screen to read in full is never written at all.
    ///
    /// The second is the M-20 invariant restated — a credential must not be
    /// pushable past the scanner's prefix with padding.
    #[test]
    fn an_unlocalizable_or_unscreenable_line_is_dropped_whole() {
        // The credential is an object KEY, so no string value carries it.
        let aws = "AKIAIOSFODNN7EXAMPLE";
        let line = format!("{{\"{aws}\":\"value\",\"other\":1}}");
        let got = scrub_payload(&line).expect("compiles");
        assert!(!got.text.contains(aws), "{}", got.text);
        assert!(got.text.starts_with("[REDACTED LINE:"), "{}", got.text);
        assert_eq!(got.redactions, 1);

        // …and a line past the scrub cap, with the credential in its final
        // bytes — where a prefix-limited scan would never have reached it.
        let mut huge = "x".repeat(MAX_SCRUBBABLE_LINE_BYTES);
        huge.push_str(aws);
        let got = scrub_payload(&huge).expect("compiles");
        assert!(!got.text.contains(aws), "the padded credential survived");
        assert!(got.text.starts_with("[OMITTED:"), "{}", got.text);
        assert!(
            got.text.len() < 200,
            "the marker replaces the line; none of the payload is written"
        );
        assert_eq!(got.omitted, 1);
    }

    /// Idempotent: sanitizing already-sanitized text is a no-op (so a value can
    /// pass more than one boundary without being progressively mangled).
    #[test]
    fn is_idempotent() {
        for input in [
            "\u{1b}[31mred\u{1b}[0m",
            "a\u{1b}]52;c;eA==\u{07}b",
            "plain\ttext\nhere",
        ] {
            let once = strip_terminal_escapes(input).into_owned();
            assert_eq!(strip_terminal_escapes(&once), once, "{input:?}");
        }
    }
}

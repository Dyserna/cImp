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

use std::borrow::Cow;

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

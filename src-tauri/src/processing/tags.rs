//! Tag detection over the raw PTY byte stream.
//!
//! The scanner reads append-only `raw_view()` from `Screen` rather than the
//! cell-derived rendered text. Cells can carry stale spinner/status content
//! in cursor-skipped positions, which fuses adjacent words into one bogus
//! token that misaki-rs falls back to spelling letter by letter. The raw
//! byte stream is exactly what Claude emitted, so a well-defined ANSI strip
//! gives clean, well-separated prose.
//!
//! `[[TTS]]` and `[[/TTS]]` markers are pure ASCII even when surrounded by
//! ANSI styling, so byte-pattern scanning is safe.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::processing::screen::Screen;

const OPEN: &[u8] = b"[[TTS]]";
const CLOSE: &[u8] = b"[[/TTS]]";

pub struct TagScanner {
    /// Whitespace-normalized tag contents already emitted as TTS segments.
    /// Normalizing the dedup key (collapsing runs of whitespace including
    /// newlines into a single space) makes the cache wrap-tolerant: when
    /// a column-count change forces a TUI redraw, the rewrapped content
    /// produces different `\n` placement in the stripped byte stream but
    /// the same key here, so the segment is correctly recognized as a
    /// repeat instead of replaying.
    spoken: HashSet<String>,
    /// Was the last scan unable to match an opener with a closer?
    open_tag: bool,
    /// Byte offset into `raw_view` past which scanning starts on the next
    /// call. Advances past closed pairs and (on max-hold) past recovered
    /// literal openers. Append-only, so this is monotone in practice.
    scan_offset: usize,
    /// Whitespace-normalized tag contents the user typed or pasted into
    /// the input box. The scanner suppresses TTS emission for matching
    /// content (the user echoed their own markers; we don't want to read
    /// them back). Shared with the `pty_write` IPC command so this list
    /// is filled BEFORE the echo bytes arrive — content-based, no timing
    /// window required. Normalization mirrors `spoken`'s, since echoed
    /// content can also be reflowed by terminal width changes.
    user_typed: Arc<Mutex<HashSet<String>>>,
}

impl TagScanner {
    pub fn new() -> Self {
        Self::with_user_typed_filter(Arc::new(Mutex::new(HashSet::new())))
    }

    pub fn with_user_typed_filter(user_typed: Arc<Mutex<HashSet<String>>>) -> Self {
        Self {
            spoken: HashSet::new(),
            open_tag: false,
            scan_offset: 0,
            user_typed,
        }
    }

    pub fn has_open_tag(&self) -> bool {
        self.open_tag
    }

    /// Walk the raw byte stream from `scan_offset` looking for closed
    /// `[[TTS]]...[[/TTS]]` pairs. Content between markers is ANSI-stripped
    /// (with `\x1b[<n>C` cursor-skips converted to N spaces, since Claude
    /// Code's TUI uses them as inter-word separators), then deduped against
    /// `spoken` and the user-typed set.
    pub fn scan_for_new_tags(&mut self, raw: &[u8]) -> Vec<String> {
        if self.scan_offset > raw.len() {
            self.scan_offset = raw.len();
        }
        let scan = &raw[self.scan_offset..];

        let mut new_speech = Vec::new();
        let mut i: usize = 0;
        let mut local_open = false;
        let mut last_consumed: usize = 0;

        loop {
            let rel_open = match find_bytes(&scan[i..], OPEN) {
                Some(p) => p,
                None => break,
            };
            let abs_open = i + rel_open;
            let content_start = abs_open + OPEN.len();

            match find_bytes(&scan[content_start..], CLOSE) {
                Some(rel_close) => {
                    let content_end = content_start + rel_close;
                    let content_bytes = &scan[content_start..content_end];
                    let content = strip_ansi(content_bytes).trim().to_string();
                    let key = normalize_for_dedup(&content);
                    if key.is_empty() {
                        self.spoken.insert(String::new());
                    } else if !self.spoken.contains(&key) {
                        let user_echo = self
                            .user_typed
                            .lock()
                            .map(|s| s.contains(&key))
                            .unwrap_or(false);
                        if !user_echo {
                            new_speech.push(content.clone());
                        }
                        self.spoken.insert(key);
                    }
                    i = content_end + CLOSE.len();
                    last_consumed = i;
                }
                None => {
                    local_open = true;
                    break;
                }
            }
        }

        self.open_tag = local_open;
        // Advance only past closed pairs; an open opener stays in scan range
        // so the next call (with more data) can complete the match.
        if !local_open {
            self.scan_offset += last_consumed;
        }
        new_speech
    }

    /// Called when the max-hold timer has expired with an open tag still
    /// pending. Skips past the unclosed opener so subsequent scans don't keep
    /// re-firing on it. The marker stays visible in the cell buffer (and thus
    /// in raw bytes) so the user sees the literal `[[TTS]]` text — exactly
    /// what milestone test #8 asks for.
    pub fn recover_unclosed(&mut self, raw: &[u8]) {
        if self.scan_offset > raw.len() {
            self.scan_offset = raw.len();
        }
        let scan = &raw[self.scan_offset..];
        if let Some(rel_open) = find_bytes(scan, OPEN) {
            self.scan_offset += rel_open + OPEN.len();
        }
        self.open_tag = false;
    }
}

impl Default for TagScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// Collapse runs of whitespace (spaces, tabs, newlines) in `s` into a
/// single ASCII space and trim. The resulting string is the dedup key
/// for `spoken` and the lookup key for `user_typed`. Two pieces of
/// content that differ only in line-wrap whitespace produce the same
/// key, so a column-driven TUI redraw doesn't bypass the cache.
pub fn normalize_for_dedup(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Walk all rows from row 0 up to (and including) the last non-empty row,
/// concatenating their rendered cell content with `\n` separators. Used by
/// the smart-flush partial-opener-prefix check, which needs to know what
/// the current visible terminal state ends with.
pub(crate) fn build_rendered(screen: &Screen) -> String {
    let rows = screen.rows();
    let last_filled = rows.iter().rposition(|r| !r.cells.is_empty());
    let stop = match last_filled {
        Some(idx) => idx + 1,
        None => return String::new(),
    };
    let mut out = String::new();
    for (i, row) in rows.iter().take(stop).enumerate() {
        out.push_str(&row.rendered());
        if i + 1 < stop {
            out.push('\n');
        }
    }
    out
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let last = haystack.len() - needle.len();
    for start in 0..=last {
        if &haystack[start..start + needle.len()] == needle {
            return Some(start);
        }
    }
    None
}

/// Convert raw PTY bytes (which may contain ANSI control sequences) into
/// plain text suitable for the TTS phonemizer.
///
/// - SGR / OSC / unknown CSI sequences → discarded entirely.
/// - `\x1b[<n>C` (cursor forward) → `n` spaces. Claude Code's TUI uses CUF
///   between words instead of literal spaces, so without this every CUF
///   between words would disappear and the tokenizer would see one giant
///   compound word.
/// - Other cursor-move CSIs (CUP, CUU, CUD, CUB, CHA, etc.) → newline,
///   so multi-row content stays separable.
/// - Erase / mode CSIs → discarded.
/// - Control bytes other than `\n` and `\t` → discarded.
/// - Printable bytes → preserved.
fn strip_ansi(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        let b = bytes[i];
        if b == 0x1b && i + 1 < n {
            let next = bytes[i + 1];
            if next == b'[' {
                i += 2;
                let params_start = i;
                while i < n && !((0x40..=0x7e).contains(&bytes[i])) {
                    i += 1;
                }
                if i < n {
                    let final_byte = bytes[i];
                    let params = &bytes[params_start..i];
                    match final_byte {
                        b'C' | b'a' => {
                            // Cursor forward: emit N spaces (capped to keep
                            // unrelated jumps from blowing up the buffer).
                            let n_skip = std::str::from_utf8(params)
                                .ok()
                                .and_then(|s| s.parse::<usize>().ok())
                                .unwrap_or(1)
                                .max(1)
                                .min(64);
                            for _ in 0..n_skip {
                                out.push(b' ');
                            }
                        }
                        b'A' | b'B' | b'D' | b'E' | b'F' | b'G' | b'H' | b'd' | b'f' | b'`' => {
                            // Other cursor-move sequences → break the line.
                            if !out.last().map(|&c| c == b'\n').unwrap_or(true) {
                                out.push(b'\n');
                            }
                        }
                        _ => { /* SGR, erase, etc. — discard */ }
                    }
                    i += 1;
                }
            } else if next == b']' {
                // OSC: skip until BEL or ST (ESC \).
                i += 2;
                while i < n {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < n && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            } else {
                // Other ESC sequence — skip ESC and the immediate next byte.
                i += 2;
            }
        } else if b < 0x20 {
            // Control byte: keep newline/tab, drop the rest.
            if b == b'\n' || b == b'\t' {
                out.push(b);
            }
            i += 1;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

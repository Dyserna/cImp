//! Tag detection over the cell-based rendered text. The scanner is stateful:
//! it remembers which tag contents have already been emitted so a redrawn
//! region with identical TTS doesn't get spoken twice (the cell model
//! collapses rewrites into a single final state, but we still need to
//! dedupe across multiple `scan_for_new_tags()` calls during streaming).
//!
//! The scanner does NOT touch the raw byte buffer. Marker stripping in raw
//! bytes is done by `Screen` via direct byte-pattern matching — we rely on
//! the fact that `[[TTS]]` and `[[/TTS]]` are pure ASCII even when Claude
//! styles them with surrounding ANSI.

use std::collections::HashSet;

use crate::processing::screen::Screen;

const OPEN: &str = "[[TTS]]";
const CLOSE: &str = "[[/TTS]]";

pub struct TagScanner {
    /// Tag contents already emitted as TTS segments. Used to dedupe identical
    /// content that may reappear if upstream redraws the same region.
    spoken: HashSet<String>,
    /// Was the last scan unable to match an opener with a closer?
    open_tag: bool,
    /// Byte offset (in the rendered text) past which scanning starts. Advances
    /// past openers that have been declared "literal" (max-hold recovery).
    scan_offset: usize,
}

impl TagScanner {
    pub fn new() -> Self {
        Self {
            spoken: HashSet::new(),
            open_tag: false,
            scan_offset: 0,
        }
    }

    pub fn has_open_tag(&self) -> bool {
        self.open_tag
    }

    /// Walk the screen's rendered text from `scan_offset`, find all complete
    /// `[[TTS]]...[[/TTS]]` pairs, and return their inner content (deduped).
    /// If the scan ends with an unmatched opener, set `open_tag = true`.
    pub fn scan_for_new_tags(&mut self, screen: &Screen) -> Vec<String> {
        let rendered = build_rendered(screen);
        if self.scan_offset > rendered.len() {
            self.scan_offset = rendered.len();
        }
        let scan = &rendered[self.scan_offset..];

        let mut new_speech = Vec::new();
        let mut i: usize = 0;
        let mut local_open = false;

        loop {
            let rel_open = match scan[i..].find(OPEN) {
                Some(p) => p,
                None => break,
            };
            let abs_open = i + rel_open;
            let content_start = abs_open + OPEN.len();

            match scan[content_start..].find(CLOSE) {
                Some(rel_close) => {
                    let content_end = content_start + rel_close;
                    let content = scan[content_start..content_end].to_string();
                    if !self.spoken.contains(&content) && !content.is_empty() {
                        new_speech.push(content.clone());
                        self.spoken.insert(content);
                    } else if content.is_empty() {
                        // Empty TTS block — still a closed pair; mark as spoken
                        // (with empty content) so we don't re-trigger.
                        self.spoken.insert(String::new());
                    }
                    i = content_end + CLOSE.len();
                }
                None => {
                    local_open = true;
                    break;
                }
            }
        }

        self.open_tag = local_open;
        new_speech
    }

    /// Called when the max-hold timer has expired with an open tag still
    /// pending. Skips past the unclosed opener so subsequent scans don't keep
    /// re-firing on it. The marker stays visible in the cell buffer (and thus
    /// in raw bytes) so the user sees the literal `[[TTS]]` text — exactly
    /// what milestone test #8 asks for.
    pub fn recover_unclosed(&mut self, screen: &Screen) {
        let rendered = build_rendered(screen);
        let scan = &rendered[self.scan_offset.min(rendered.len())..];
        if let Some(rel_open) = scan.find(OPEN) {
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

/// Walk all rows from row 0 up to (and including) the last non-empty row,
/// concatenating their rendered cell content with `\n` separators. We stop
/// at the last non-empty row to avoid trailing-empty-row noise; if every row
/// has been erased we still produce an empty string.
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

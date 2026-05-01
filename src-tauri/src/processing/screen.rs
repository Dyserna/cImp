//! Cell-based screen model. The `vte::Perform` impl mutates cells in place
//! so rewrites collapse: a spinner cycling through 100 frames produces 100
//! overwrites of the same cell, not 100 entries in any kind of history.
//!
//! Two parallel views are maintained:
//!
//! 1. **Cell rows** — the *final visual state*, used by the tag scanner to
//!    detect `[[TTS]]` markers and dedupe by content+position so a redrawn
//!    region with identical TTS isn't spoken twice.
//!
//! 2. **Raw byte buffer** — every input byte the parser saw, in order,
//!    forwarded to xterm.js verbatim once flushed. Tag *markers* are stripped
//!    via direct byte-pattern match (markers are pure ASCII even when Claude
//!    surrounds them with ANSI styling). Cell rewrites do NOT prune raw
//!    bytes — xterm renders the same cursor-moves-and-overwrites the original
//!    PTY would, so visual fidelity is preserved.

use std::time::Instant;

use vte::{Params, Perform};

use crate::processing::tags::{build_rendered, TagScanner};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellAttrs {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub fg: Option<u32>,
    pub bg: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Cell {
    pub ch: char,
    /// Reserved: SGR attributes captured at print time. Not yet inspected by
    /// the tag scanner (which works on plain text), but kept on the cell so
    /// future re-rendering or styled-aware features can use it without a
    /// data-structure change.
    #[allow(dead_code)]
    pub attrs: CellAttrs,
}

#[derive(Debug, Default)]
pub struct Row {
    pub cells: Vec<Option<Cell>>,
    pub last_modified: Option<Instant>,
}

impl Row {
    fn ensure_col(&mut self, col: usize) {
        while self.cells.len() <= col {
            self.cells.push(None);
        }
    }

    fn put(&mut self, col: usize, cell: Cell, now: Instant) {
        self.ensure_col(col);
        self.cells[col] = Some(cell);
        self.last_modified = Some(now);
    }

    fn erase_from(&mut self, from_col: usize, now: Instant) {
        if from_col < self.cells.len() {
            for col in from_col..self.cells.len() {
                self.cells[col] = None;
            }
            self.last_modified = Some(now);
        }
    }

    fn erase_to(&mut self, to_col_inclusive: usize, now: Instant) {
        let stop = to_col_inclusive.min(self.cells.len().saturating_sub(1));
        if !self.cells.is_empty() {
            for col in 0..=stop {
                self.cells[col] = None;
            }
            self.last_modified = Some(now);
        }
    }

    fn erase_all(&mut self, now: Instant) {
        if !self.cells.is_empty() {
            self.cells.clear();
            self.last_modified = Some(now);
        }
    }

    /// Build the rendered text of this row by walking cells column-by-column.
    /// Empty cells become spaces; trailing empty cells are trimmed.
    pub fn rendered(&self) -> String {
        let last_filled = self
            .cells
            .iter()
            .rposition(|c| c.is_some())
            .map(|i| i + 1)
            .unwrap_or(0);
        let mut s = String::with_capacity(last_filled);
        for cell in self.cells.iter().take(last_filled) {
            match cell {
                Some(c) => s.push(c.ch),
                None => s.push(' '),
            }
        }
        s
    }

}

pub struct Screen {
    rows: Vec<Row>,
    cursor_row: usize,
    cursor_col: usize,
    current_attrs: CellAttrs,
    saved_cursor: Option<(usize, usize)>,

    /// Raw byte buffer, mirrored from every byte fed to the parser.
    raw_buffer: Vec<u8>,

    /// Number of leading raw bytes already emitted as `TerminalBytes`.
    /// On flush, only bytes after this offset are eligible.
    raw_emitted_offset: usize,

    /// "Now" passed by the owning layer. Updated before each ingest/flush.
    now: Instant,
}

impl Screen {
    pub fn new() -> Self {
        Self {
            rows: vec![Row::default()],
            cursor_row: 0,
            cursor_col: 0,
            current_attrs: CellAttrs::default(),
            saved_cursor: None,
            raw_buffer: Vec::new(),
            raw_emitted_offset: 0,
            now: Instant::now(),
        }
    }

    pub fn set_now(&mut self, now: Instant) {
        self.now = now;
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    #[allow(dead_code)]
    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    pub fn has_pending(&self) -> bool {
        self.raw_emitted_offset < self.raw_buffer.len()
    }

    /// Push raw bytes into the parser. Each byte is mirrored into `raw_buffer`
    /// before being advanced through `vte::Parser` so the Performer callbacks
    /// observe a consistent view of "bytes-so-far."
    pub fn feed(&mut self, parser: &mut vte::Parser, bytes: &[u8]) {
        for &b in bytes {
            self.raw_buffer.push(b);
            parser.advance(self, b);
        }
    }

    fn ensure_row(&mut self, row: usize) {
        while self.rows.len() <= row {
            self.rows.push(Row::default());
        }
    }

    fn move_cursor(&mut self, row: usize, col: usize) {
        self.ensure_row(row);
        self.cursor_row = row;
        self.cursor_col = col;
    }

    fn handle_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.current_attrs = CellAttrs::default();
            return;
        }
        for p in params.iter() {
            let code = p.first().copied().unwrap_or(0);
            match code {
                0 => self.current_attrs = CellAttrs::default(),
                1 => self.current_attrs.bold = true,
                3 => self.current_attrs.italic = true,
                4 => self.current_attrs.underline = true,
                22 => self.current_attrs.bold = false,
                23 => self.current_attrs.italic = false,
                24 => self.current_attrs.underline = false,
                30..=37 => self.current_attrs.fg = Some(u32::from(code) - 30),
                38 => { /* extended color; we don't decode subparams in M2 */ }
                39 => self.current_attrs.fg = None,
                40..=47 => self.current_attrs.bg = Some(u32::from(code) - 40),
                48 => {}
                49 => self.current_attrs.bg = None,
                90..=97 => self.current_attrs.fg = Some(u32::from(code) - 90 + 8),
                100..=107 => self.current_attrs.bg = Some(u32::from(code) - 100 + 8),
                _ => {}
            }
        }
    }

    /// Try to drain `raw_buffer` to a `Vec<u8>` of bytes safe to forward to
    /// xterm.
    ///
    /// Smart flush strategy:
    ///
    /// - Hold while the scanner has an open tag (an `[[TTS]]` with no matching
    ///   closer yet). Otherwise the user would briefly see literal marker text.
    /// - Hold while the rendered tail is a *partial* opener prefix (`[`, `[[`,
    ///   `[[T`, … `[[TTS]`). One more byte could turn that into a real opener.
    /// - Otherwise — including the typing-feedback path, where Claude is just
    ///   echoing keystrokes back through TUI redraws — flush immediately. This
    ///   is the difference between visible-on-keystroke and
    ///   visible-after-stability-timeout.
    ///
    /// On force (max-hold expired with an open tag pending) we emit raw bytes
    /// without stripping markers, so the user sees the literal `[[TTS]]` per
    /// the unclosed-tag recovery rule. The caller resets the scanner's open
    /// state after this call.
    pub fn drain_flushable(&mut self, scanner: &TagScanner, force: bool) -> Vec<u8> {
        if !self.has_pending() {
            return Vec::new();
        }

        if !force {
            if scanner.has_open_tag() {
                return Vec::new();
            }
            let rendered = build_rendered(self);
            if tail_might_be_opener_prefix(&rendered) {
                return Vec::new();
            }
        }

        let start = self.raw_emitted_offset;
        let end = self.raw_buffer.len();
        let slice = &self.raw_buffer[start..end];

        // When force-flushing with an unclosed tag, emit the bytes as-is so the
        // user sees the literal `[[TTS]]`. Otherwise strip every marker
        // occurrence in the range — markers are ASCII even when Claude wraps
        // them in ANSI styling.
        let strip_markers = !scanner.has_open_tag();
        let out = if strip_markers {
            strip_marker_bytes(slice)
        } else {
            slice.to_vec()
        };

        self.raw_emitted_offset = end;
        out
    }
}

/// Returns true if `rendered` ends with any non-empty proper prefix of the
/// opener marker `[[TTS]]`. The full opener itself flips `has_open_tag`, so
/// it's handled separately and excluded here.
fn tail_might_be_opener_prefix(rendered: &str) -> bool {
    const PREFIXES: &[&str] = &["[", "[[", "[[T", "[[TT", "[[TTS", "[[TTS]"];
    PREFIXES.iter().any(|p| rendered.ends_with(p))
}

fn strip_marker_bytes(slice: &[u8]) -> Vec<u8> {
    const OPEN: &[u8] = b"[[TTS]]";
    const CLOSE: &[u8] = b"[[/TTS]]";
    let n = slice.len();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if i + OPEN.len() <= n && &slice[i..i + OPEN.len()] == OPEN {
            ranges.push((i, i + OPEN.len()));
            i += OPEN.len();
        } else if i + CLOSE.len() <= n && &slice[i..i + CLOSE.len()] == CLOSE {
            ranges.push((i, i + CLOSE.len()));
            i += CLOSE.len();
        } else {
            i += 1;
        }
    }
    if ranges.is_empty() {
        return slice.to_vec();
    }
    let mut out = Vec::with_capacity(n);
    let mut cursor = 0;
    for (s, e) in &ranges {
        if *s > cursor {
            out.extend_from_slice(&slice[cursor..*s]);
        }
        cursor = *e;
    }
    if cursor < n {
        out.extend_from_slice(&slice[cursor..]);
    }
    out
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.ensure_row(self.cursor_row);
        let cell = Cell {
            ch: c,
            attrs: self.current_attrs,
        };
        let row = self.cursor_row;
        let col = self.cursor_col;
        self.rows[row].put(col, cell, self.now);
        self.cursor_col = col + 1;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                let new_row = self.cursor_row + 1;
                self.ensure_row(new_row);
                self.cursor_row = new_row;
                // \n alone: cursor down, column unchanged. TUIs commonly emit \r\n.
            }
            b'\r' => {
                self.cursor_col = 0;
            }
            0x08 => {
                // BS: cursor back one column, no erase.
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                }
            }
            0x07 => {
                // BEL: ignore (terminals beep, we don't track).
            }
            b'\t' => {
                // HT: advance to next tab stop (every 8 cols).
                self.cursor_col = (self.cursor_col / 8 + 1) * 8;
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        let p = |i: usize, default: u16| -> u16 {
            params
                .iter()
                .nth(i)
                .and_then(|p| p.first().copied())
                .filter(|&v| v != 0)
                .unwrap_or(default)
        };

        match action {
            'A' => {
                let n = p(0, 1) as usize;
                let row = self.cursor_row.saturating_sub(n);
                self.move_cursor(row, self.cursor_col);
            }
            'B' | 'e' => {
                let n = p(0, 1) as usize;
                let row = self.cursor_row + n;
                self.move_cursor(row, self.cursor_col);
            }
            'C' | 'a' => {
                let n = p(0, 1) as usize;
                self.cursor_col += n;
            }
            'D' => {
                let n = p(0, 1) as usize;
                self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            'E' => {
                let n = p(0, 1) as usize;
                let row = self.cursor_row + n;
                self.move_cursor(row, 0);
            }
            'F' => {
                let n = p(0, 1) as usize;
                let row = self.cursor_row.saturating_sub(n);
                self.move_cursor(row, 0);
            }
            'G' | '`' => {
                let col = (p(0, 1).saturating_sub(1)) as usize;
                self.move_cursor(self.cursor_row, col);
            }
            'H' | 'f' => {
                let row = (p(0, 1).saturating_sub(1)) as usize;
                let col = (p(1, 1).saturating_sub(1)) as usize;
                self.move_cursor(row, col);
            }
            'd' => {
                let row = (p(0, 1).saturating_sub(1)) as usize;
                self.move_cursor(row, self.cursor_col);
            }
            'K' => {
                let mode = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                let row = self.cursor_row;
                let col = self.cursor_col;
                self.ensure_row(row);
                match mode {
                    0 => self.rows[row].erase_from(col, self.now),
                    1 => self.rows[row].erase_to(col, self.now),
                    2 => self.rows[row].erase_all(self.now),
                    _ => {}
                }
            }
            'J' => {
                let mode = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                let row = self.cursor_row;
                let col = self.cursor_col;
                self.ensure_row(row);
                match mode {
                    0 => {
                        self.rows[row].erase_from(col, self.now);
                        for r in (row + 1)..self.rows.len() {
                            self.rows[r].erase_all(self.now);
                        }
                    }
                    1 => {
                        self.rows[row].erase_to(col, self.now);
                        for r in 0..row {
                            self.rows[r].erase_all(self.now);
                        }
                    }
                    2 | 3 => {
                        for r in 0..self.rows.len() {
                            self.rows[r].erase_all(self.now);
                        }
                    }
                    _ => {}
                }
            }
            'm' => self.handle_sgr(params),
            's' => self.saved_cursor = Some((self.cursor_row, self.cursor_col)),
            'u' => {
                if let Some((r, c)) = self.saved_cursor {
                    self.move_cursor(r, c);
                }
            }
            _ => { /* unknown CSI: ignore screen-state effect; raw bytes still pass through */ }
        }
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // OSCs (window title, hyperlinks, etc.) are passthrough — the bytes are
        // already in raw_buffer; we simply don't update cell state.
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        // ESCs we don't model (alt-screen, etc.). Same passthrough rationale.
    }
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

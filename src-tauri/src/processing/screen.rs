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

use vte::{Params, Perform};

/// Hard ceiling on the cell-row buffer. Rows beyond this scroll off the top
/// (oldest first), mirroring a terminal's scrollback limit. Bounds memory for
/// long sessions and caps a single oversized cursor-down (`\x1b[65535B`) to one
/// bounded allocation instead of 65 536 rows. Tail-based readers
/// (`recent_rendered`, `build_rendered`) only ever look near the bottom, so
/// dropping old rows is invisible to them.
const MAX_ROWS: usize = 5_000;

/// Hard ceiling on columns per row. Real terminals are a few hundred columns
/// wide; this is far beyond any genuine width but small enough that a hostile
/// `\x1b[<huge>C` (cursor-forward) followed by a printable char can't make a
/// row's cell `Vec` balloon to billions of entries and OOM the process.
const MAX_COLS: usize = 2_000;

#[derive(Debug, Clone)]
pub struct Cell {
    pub ch: char,
}

#[derive(Debug, Default)]
pub struct Row {
    pub cells: Vec<Option<Cell>>,
}

impl Row {
    fn ensure_col(&mut self, col: usize) {
        let target = col.min(MAX_COLS - 1);
        while self.cells.len() <= target {
            self.cells.push(None);
        }
    }

    fn put(&mut self, col: usize, cell: Cell) {
        // Backstop against unbounded column growth: clamp the index so the
        // cells `Vec` can never exceed MAX_COLS regardless of how far the
        // cursor was advanced.
        let col = col.min(MAX_COLS - 1);
        self.ensure_col(col);
        self.cells[col] = Some(cell);
    }

    fn erase_from(&mut self, from_col: usize) {
        if from_col < self.cells.len() {
            for col in from_col..self.cells.len() {
                self.cells[col] = None;
            }
        }
    }

    fn erase_to(&mut self, to_col_inclusive: usize) {
        let stop = to_col_inclusive.min(self.cells.len().saturating_sub(1));
        if !self.cells.is_empty() {
            for col in 0..=stop {
                self.cells[col] = None;
            }
        }
    }

    fn erase_all(&mut self) {
        if !self.cells.is_empty() {
            self.cells.clear();
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
    saved_cursor: Option<(usize, usize)>,

    /// Raw byte buffer, mirrored from every byte fed to the parser.
    raw_buffer: Vec<u8>,

    /// Number of leading raw bytes already emitted as `TerminalBytes`.
    /// On flush, only bytes after this offset are eligible.
    raw_emitted_offset: usize,
}

impl Screen {
    pub fn new() -> Self {
        Self {
            rows: vec![Row::default()],
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: None,
            raw_buffer: Vec::new(),
            raw_emitted_offset: 0,
        }
    }

    #[cfg(test)]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Build a rendered tail of up to `max_chars` characters, walking rows
    /// from the bottom up. Used by the permission detector — it scans the
    /// recently-visible text for known prompt substrings without paying for
    /// the full scrollback.
    pub fn recent_rendered(&self, max_chars: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut total: usize = 0;
        for row in self.rows.iter().rev() {
            let line = row.rendered();
            // Count characters, not bytes. `String::len()` (bytes) would
            // overcount a multibyte glyph by up to 3x, shrinking the captured
            // tail to far fewer visible rows than `max_chars` implies and
            // pushing a multi-row prompt's marker out of the window. WHOSE
            // chrome is full of such glyphs is a fact about a harness's TUI and
            // is recorded beside the markers it protects
            // (`harness::claude::prompts`, V40 Phase C, locked decision 21);
            // this window stays neutral machinery, and counting characters is
            // the right rule for any of them.
            total += line.chars().count() + 1;
            parts.push(line);
            if total >= max_chars {
                break;
            }
        }
        parts.reverse();
        parts.join("\n")
    }

    pub fn has_pending(&self) -> bool {
        self.raw_emitted_offset < self.raw_buffer.len()
    }

    /// Number of leading raw bytes already emitted to the terminal. The
    /// processing layer uses this as the compaction watermark — bytes below it
    /// have been forwarded and can be dropped.
    pub fn emitted_offset(&self) -> usize {
        self.raw_emitted_offset
    }

    /// Drop the leading `watermark` raw bytes (clamped to the emit cursor) and
    /// rebase `raw_emitted_offset`. Returns the number of bytes actually
    /// dropped.
    pub fn compact_raw(&mut self, watermark: usize) -> usize {
        let drop = watermark.min(self.raw_emitted_offset);
        if drop == 0 {
            return 0;
        }
        self.raw_buffer.drain(..drop);
        self.raw_emitted_offset -= drop;
        drop
    }

    /// Push raw bytes into the parser. The whole chunk is mirrored into
    /// `raw_buffer` before it is advanced through `vte::Parser` (0.15's
    /// `advance` is slice-at-a-time, not byte-at-a-time). That ordering is
    /// safe because no `Perform` callback reads `raw_buffer` — it is only ever
    /// drained by `drain_flushable`/`compact_raw` between feeds — so the
    /// observable result is identical to the old per-byte interleave.
    pub fn feed(&mut self, parser: &mut vte::Parser, bytes: &[u8]) {
        self.raw_buffer.extend_from_slice(bytes);
        parser.advance(self, bytes);
    }

    /// Ensure row `row` exists, scrolling the oldest rows off the top if `row`
    /// would push the buffer past [`MAX_ROWS`]. Returns the (possibly rebased)
    /// row index to actually use — callers must use the return value, since a
    /// scroll shifts every absolute row index down by the number dropped.
    fn ensure_row(&mut self, row: usize) -> usize {
        if row >= MAX_ROWS {
            // Scroll: drop the oldest rows so the target lands at MAX_ROWS-1.
            let drop = (row + 1 - MAX_ROWS).min(self.rows.len());
            self.rows.drain(..drop);
            self.cursor_row = self.cursor_row.saturating_sub(drop);
            if let Some((r, c)) = self.saved_cursor {
                self.saved_cursor = Some((r.saturating_sub(drop), c));
            }
            while self.rows.len() < MAX_ROWS {
                self.rows.push(Row::default());
            }
            return MAX_ROWS - 1;
        }
        while self.rows.len() <= row {
            self.rows.push(Row::default());
        }
        row
    }

    fn move_cursor(&mut self, row: usize, col: usize) {
        self.cursor_row = self.ensure_row(row);
        // Rest on a VALID column. Clamping to MAX_COLS (one past the last
        // column) parks the cursor out of bounds after a huge CSI column param,
        // so every subsequent write clamps back onto the last column — a
        // persistent one-column write error.
        self.cursor_col = col.min(MAX_COLS - 1);
    }

    /// Drain all pending `raw_buffer` bytes to forward to xterm.
    ///
    /// V20: the identity forward of the PTY stream. With the `[[TTS]]` marker
    /// convention retired (TTS is sourced out-of-band), there is nothing to
    /// hold for or strip — every byte the child emitted is forwarded verbatim,
    /// in order, exactly once. This is also zero-lag: bytes are visible the
    /// instant they arrive.
    pub fn drain_flushable(&mut self) -> Vec<u8> {
        if !self.has_pending() {
            return Vec::new();
        }
        let start = self.raw_emitted_offset;
        let end = self.raw_buffer.len();
        let out = self.raw_buffer[start..end].to_vec();
        self.raw_emitted_offset = end;
        out
    }
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.ensure_row(self.cursor_row);
        let cell = Cell { ch: c };
        let row = self.cursor_row;
        let col = self.cursor_col;
        self.rows[row].put(col, cell);
        self.cursor_col = (col + 1).min(MAX_COLS);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                // Use the row ensure_row returns: past MAX_ROWS it scrolls and
                // rebases indices, so assigning the raw `cursor_row + 1` would
                // leave the cursor one past the end and panic the next erase CSI
                // (`'K'`/`'J'` index `rows[cursor_row]` directly).
                self.cursor_row = self.ensure_row(self.cursor_row + 1);
                // \n alone: cursor down, column unchanged. TUIs commonly emit \r\n.
            }
            b'\r' => {
                self.cursor_col = 0;
            }
            0x08
                // BS: cursor back one column, no erase.
                if self.cursor_col > 0 => {
                    self.cursor_col -= 1;
                }
            0x07 => {
                // BEL: ignore (terminals beep, we don't track).
            }
            b'\t' => {
                // HT: advance to next tab stop (every 8 cols). Rest on a valid
                // column (see `move_cursor`).
                self.cursor_col = ((self.cursor_col / 8 + 1) * 8).min(MAX_COLS - 1);
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
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
                // CUF: rest on a valid column (see `move_cursor`).
                self.cursor_col = self.cursor_col.saturating_add(n).min(MAX_COLS - 1);
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
                let mode = params
                    .iter()
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(0);
                let row = self.cursor_row;
                let col = self.cursor_col;
                self.ensure_row(row);
                match mode {
                    0 => self.rows[row].erase_from(col),
                    1 => self.rows[row].erase_to(col),
                    2 => self.rows[row].erase_all(),
                    _ => {}
                }
            }
            'J' => {
                let mode = params
                    .iter()
                    .next()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(0);
                let row = self.cursor_row;
                let col = self.cursor_col;
                self.ensure_row(row);
                match mode {
                    0 => {
                        self.rows[row].erase_from(col);
                        for r in (row + 1)..self.rows.len() {
                            self.rows[r].erase_all();
                        }
                    }
                    1 => {
                        self.rows[row].erase_to(col);
                        for r in 0..row {
                            self.rows[r].erase_all();
                        }
                    }
                    2 | 3 => {
                        for r in 0..self.rows.len() {
                            self.rows[r].erase_all();
                        }
                    }
                    _ => {}
                }
            }
            'm' => { /* SGR styling discarded — the cell model is text-only. */ }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_cursor_down_allocates_bounded_rows() {
        let mut s = Screen::new();
        let mut p = vte::Parser::new();
        // A single huge CUD must not allocate 65k rows.
        s.feed(&mut p, b"\x1b[65535B");
        assert!(s.rows().len() <= MAX_ROWS, "rows={}", s.rows().len());
        assert!(s.cursor_row < MAX_ROWS);
    }

    #[test]
    fn scrolling_bounds_rows_and_keeps_recent_content() {
        let mut s = Screen::new();
        let mut p = vte::Parser::new();
        let total = MAX_ROWS + 500;
        for i in 0..total {
            s.feed(&mut p, format!("line{i}\r\n").as_bytes());
        }
        // Buffer stayed bounded despite far more lines than the cap.
        assert!(s.rows().len() <= MAX_ROWS, "rows={}", s.rows().len());
        // The most recently printed line survived the scroll.
        let tail = s.recent_rendered(4096);
        assert!(
            tail.contains(&format!("line{}", total - 1)),
            "recent tail missing newest line"
        );
        // The oldest line scrolled off the top.
        assert!(!tail.contains("line0\n") && !tail.ends_with("line0"));
    }

    #[test]
    fn erase_csi_after_scroll_overflow_does_not_panic() {
        let mut s = Screen::new();
        let mut p = vte::Parser::new();
        // Push the cursor well past MAX_ROWS with bare newlines, then issue
        // erase-line / erase-display / a print. These index rows[cursor_row]
        // directly, so an un-rebased cursor would panic out of bounds.
        for _ in 0..(MAX_ROWS + 50) {
            s.feed(&mut p, b"\n");
        }
        s.feed(&mut p, b"\x1b[K"); // erase to end of line at cursor
        s.feed(&mut p, b"\x1b[2J"); // erase whole display
        s.feed(&mut p, b"text after overflow\r\n");
        assert!(s.cursor_row < MAX_ROWS);
        assert!(s.rows().len() <= MAX_ROWS);
    }
}

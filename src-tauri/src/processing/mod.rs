//! Processing layer between the PTY reader and the frontend forwarder.
//!
//! Architecture:
//!
//! - [`Screen`] is a `vte::Perform` implementation that maintains a per-cell
//!   row buffer with SGR attribute tracking. It is the single source of truth
//!   for "what's on screen" — any in-place rewrite, cursor jump, or line
//!   erase mutates cells in place rather than appending. Cells therefore
//!   reflect *final* visual state, not the temporal history of writes. This
//!   is what makes the layer robust to TUI rewrites: a spinner that cycles
//!   100 frames produces 100 cell-overwrites at the same position, not 100
//!   distinct entries in the rendered scan.
//!
//! - [`Segmenter`] splits assistant prose into sentence-bounded chunks for the
//!   downstream synthesizer, with disambiguation for decimals, common
//!   abbreviations, and ellipses. V20 calls it from the out-of-band TTS sources
//!   (`crate::harness::reader`) rather than from the terminal stream.
//!
//! - [`ProcessingLayer`] owns the parser + cell screen. V20: it no longer
//!   extracts TTS from the terminal — AI tabs are fullscreen and speak from
//!   structured side channels (`crate::harness::reader`), so the layer simply forwards the
//!   raw PTY stream to xterm verbatim and maintains the cell model used by
//!   permission detection ([`recent_rendered`](ProcessingLayer::recent_rendered)).
//!
//! - [`strip_terminal_escapes`] (V32 Phase D) is the inverse direction: text
//!   arriving from OUTSIDE cImp (fetched pages, model prose quoting them) is
//!   stripped of ANSI/OSC/C0 control sequences before it is composed into a
//!   non-HTML sink such as TTS. See `sanitize.rs` for the threat model.

pub mod patterns_file;
pub mod permission;
mod prose;
mod sanitize;
mod screen;
mod segmenter;
mod tags;

pub use prose::to_speakable;
pub use sanitize::strip_terminal_escapes;
// V35 Phase H: the disk-bound scrubber (strip + credential redaction), used by
// `harness::capture`. Its `Scrubbed` result is reached through the return type
// rather than re-exported — nothing outside that one call site names it.
pub use sanitize::scrub_payload;
pub use segmenter::segment_sentences;
pub use tags::normalize_for_dedup;

#[cfg(test)]
mod tests;

use crate::processing::screen::Screen;

/// Compact `raw_buffer` once the emitted prefix grows past this. Below it we
/// leave the buffer alone to avoid churning `Vec::drain` on every small burst.
const RAW_COMPACT_THRESHOLD: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingEvent {
    /// Bytes ready for the terminal display (the raw PTY stream, verbatim).
    TerminalBytes(Vec<u8>),
}

/// V20: a thin conduit between the PTY reader and the frontend. It forwards the
/// raw byte stream to xterm and keeps a cell model of the screen so permission
/// detection can scan the rendered tail. TTS is sourced out-of-band now, so
/// there is no tag scanning, marker stripping, or speak-all mode here.
pub struct ProcessingLayer {
    parser: vte::Parser,
    screen: Screen,
}

impl ProcessingLayer {
    pub fn new() -> Self {
        Self {
            parser: vte::Parser::new(),
            screen: Screen::new(),
        }
    }

    /// Rendered tail of the screen, capped to roughly `max_chars`. Permission
    /// detection consumes this — small enough to scan cheaply, large enough to
    /// catch multi-line prompts. Works in both inline and fullscreen
    /// (alternate-screen) renderers, since it reads the final cell state.
    pub fn recent_rendered(&self, max_chars: usize) -> String {
        self.screen.recent_rendered(max_chars)
    }

    pub fn ingest(&mut self, bytes: &[u8]) -> Vec<ProcessingEvent> {
        if bytes.is_empty() {
            return Vec::new();
        }
        self.screen.feed(&mut self.parser, bytes);
        self.collect_events()
    }

    pub fn flush_pending(&mut self) -> Vec<ProcessingEvent> {
        self.collect_events()
    }

    fn collect_events(&mut self) -> Vec<ProcessingEvent> {
        let mut events = Vec::new();

        // Forward every new raw byte to xterm in order, exactly once. With the
        // `[[TTS]]` marker convention retired there is nothing to strip or hold
        // — this is the identity forward of the PTY stream.
        let drained_bytes = self.screen.drain_flushable();
        if !drained_bytes.is_empty() {
            events.push(ProcessingEvent::TerminalBytes(drained_bytes));
        }

        // Bound `raw_buffer`: bytes before the emit cursor have been forwarded
        // and will never be re-read, so drop them and rebase the cursor.
        let watermark = self.screen.emitted_offset();
        if watermark >= RAW_COMPACT_THRESHOLD {
            self.screen.compact_raw(watermark);
        }

        events
    }
}

impl Default for ProcessingLayer {
    fn default() -> Self {
        Self::new()
    }
}

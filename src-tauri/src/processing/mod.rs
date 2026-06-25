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
//! - [`TagScanner`] runs over the row-based rendered text, locates complete
//!   `[[TTS]]...[[/TTS]]` tag pairs, and tracks per-tag byte ranges so they
//!   can be stripped from the outgoing raw byte stream. It also dedupes by
//!   content+position so a redrawn region with identical TTS content is
//!   only spoken once.
//!
//! - [`Segmenter`] splits TTS content into sentence-bounded chunks for the
//!   downstream synthesizer, with disambiguation for decimals, common
//!   abbreviations, and ellipses.
//!
//! - [`ProcessingLayer`] owns the parser + screen and coordinates the hybrid
//!   flush: bytes are held until either the stability timeout elapses for a
//!   row (no further edits in 200ms), the global max-hold expires (500ms),
//!   or a complete TTS tag is detected. Returns events as a `Vec` so the
//!   layer is easy to unit-test without async machinery.

pub mod patterns_file;
pub mod permission;
mod screen;
mod segmenter;
mod tags;

pub use segmenter::segment_sentences;
pub use tags::normalize_for_dedup;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::processing::screen::Screen;
use crate::processing::tags::TagScanner;

/// Default max-hold for an open `[[TTS]]` tag — used until the layer is
/// reconfigured via [`ProcessingLayer::set_max_hold`]. Beyond this, the
/// buffered content is emitted as literal terminal text and the open-tag
/// state is reset.
pub const DEFAULT_MAX_HOLD: Duration = Duration::from_millis(500);

/// Compact `raw_buffer` once the prefix consumed by both the emit cursor and
/// the scanner read cursor grows past this. Below it we leave the buffer alone
/// to avoid churning `Vec::drain` on every small burst.
const RAW_COMPACT_THRESHOLD: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingEvent {
    /// Bytes ready for the terminal display (tags stripped).
    TerminalBytes(Vec<u8>),
    /// A single sentence extracted from a `[[TTS]]...[[/TTS]]` block.
    TtsSegment(String),
}

pub struct ProcessingLayer {
    parser: vte::Parser,
    screen: Screen,
    scanner: TagScanner,
    /// Earliest unflushed byte timestamp; reset after each flush.
    oldest_pending_at: Option<Instant>,
    /// Tunable max-hold; reconfigured live from settings.
    max_hold: Duration,
    /// Per-tab "speak all output" mode. When set, the layer speaks every new
    /// sentence of terminal output and ignores `[[TTS]]…[[/TTS]]` markers
    /// (see [`TagScanner::scan_all`]) instead of extracting only marked
    /// segments. Reconfigured live from settings, like `max_hold`.
    speak_all: bool,
}

impl ProcessingLayer {
    pub fn new() -> Self {
        Self::with_user_typed_filter(Arc::new(Mutex::new(HashSet::new())))
    }

    pub fn with_user_typed_filter(user_typed: Arc<Mutex<HashSet<String>>>) -> Self {
        Self {
            parser: vte::Parser::new(),
            screen: Screen::new(),
            scanner: TagScanner::with_user_typed_filter(user_typed),
            oldest_pending_at: None,
            max_hold: DEFAULT_MAX_HOLD,
            speak_all: false,
        }
    }

    pub fn set_max_hold(&mut self, max_hold: Duration) {
        self.max_hold = max_hold;
    }

    /// Toggle "speak all output" mode for this tab (see the `speak_all`
    /// field). Cheap and idempotent; safe to call on every settings change.
    /// On the off→on edge it skips the current backlog so only output produced
    /// after the toggle is spoken.
    pub fn set_speak_all(&mut self, speak_all: bool) {
        if speak_all && !self.speak_all {
            self.scanner.begin_speak_all(self.screen.raw_view().len());
        }
        self.speak_all = speak_all;
    }

    /// Rendered tail of the screen, capped to roughly `max_chars`. Permission
    /// detection consumes this — small enough to scan cheaply, large enough to
    /// catch multi-line prompts.
    pub fn recent_rendered(&self, max_chars: usize) -> String {
        self.screen.recent_rendered(max_chars)
    }

    pub fn ingest(&mut self, bytes: &[u8]) -> Vec<ProcessingEvent> {
        self.ingest_at(bytes, Instant::now())
    }

    pub fn flush_pending(&mut self) -> Vec<ProcessingEvent> {
        self.flush_pending_at(Instant::now())
    }

    /// Test/internal hook: drive the layer with an explicit clock.
    pub(crate) fn ingest_at(&mut self, bytes: &[u8], now: Instant) -> Vec<ProcessingEvent> {
        if bytes.is_empty() {
            return Vec::new();
        }
        if self.oldest_pending_at.is_none() {
            self.oldest_pending_at = Some(now);
        }
        self.screen.feed(&mut self.parser, bytes);
        self.collect_events(/*allow_close_emit=*/ true, /*force=*/ false)
    }

    pub(crate) fn flush_pending_at(&mut self, now: Instant) -> Vec<ProcessingEvent> {
        let force = self
            .oldest_pending_at
            .map(|t| now.saturating_duration_since(t) >= self.max_hold)
            .unwrap_or(false);
        self.collect_events(/*allow_close_emit=*/ true, force)
    }

    fn collect_events(&mut self, allow_close_emit: bool, force: bool) -> Vec<ProcessingEvent> {
        let mut events = Vec::new();

        if allow_close_emit {
            if self.speak_all {
                // Speak-all mode: ignore `[[TTS]]` markers and speak every new
                // sentence of output. `scan_all` already returns deduped,
                // sentence-segmented text, so push each straight through.
                for sentence in self.scanner.scan_all(self.screen.raw_view(), force) {
                    events.push(ProcessingEvent::TtsSegment(sentence));
                }
            } else {
                // Scan the raw byte stream for newly-closed tags. We use raw
                // bytes (not the cell-rendered view) so cursor-skip cells with
                // stale spinner content can't run adjacent words together.
                let new_tts: Vec<String> = self
                    .scanner
                    .scan_for_new_tags(self.screen.raw_view())
                    .into_iter()
                    .collect();
                for content in new_tts {
                    for sentence in segment_sentences(&content) {
                        events.push(ProcessingEvent::TtsSegment(sentence));
                    }
                }
            }
        }

        // Smart flush: emits everything except (a) bytes inside an open tag,
        // or (b) bytes whose rendered tail could still grow into a tag opener
        // (`[`, `[[`, …, `[[TTS]`). The historical "stability timeout" hold
        // is gone — this path is now zero-lag for typing-feedback redraws,
        // which never produce an opener-prefix tail.
        let drained_bytes = self.screen.drain_flushable(&self.scanner, force);

        if !drained_bytes.is_empty() {
            events.push(ProcessingEvent::TerminalBytes(drained_bytes));
        }

        if force {
            // Max-hold also forces unclosed-tag recovery: emit the opener as literal,
            // reset the scanner so subsequent text is treated as outside-tag.
            if self.scanner.has_open_tag() {
                tracing::warn!(
                    target: "tts_stub",
                    "[[TTS]] tag exceeded max-hold without close; treating as literal"
                );
                self.scanner.recover_unclosed(self.screen.raw_view());
            }
        }

        // If everything has flushed, reset the global max-hold anchor.
        if !self.screen.has_pending() && !self.scanner.has_open_tag() {
            self.oldest_pending_at = None;
        }

        // Bound `raw_buffer`: bytes before the lesser of the emit cursor and the
        // scanner's read cursor have been consumed by BOTH and will never be
        // re-read, so drop them and rebase both cursors. While a tag is open the
        // scanner cursor stalls, so the buffer is correctly retained until the
        // close arrives. (Speak-all copies into `all_buf`, so trimming is safe.)
        let watermark = self.screen.emitted_offset().min(self.scanner.scan_offset());
        if watermark >= RAW_COMPACT_THRESHOLD {
            let dropped = self.screen.compact_raw(watermark);
            self.scanner.rebase_offset(dropped);
        }

        events
    }
}

impl Default for ProcessingLayer {
    fn default() -> Self {
        Self::new()
    }
}

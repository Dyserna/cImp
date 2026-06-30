//! Unit tests for the processing layer.
//!
//! V20: the layer no longer extracts TTS from the terminal (AI tabs speak
//! out-of-band via `crate::oob`). It is now a raw-stream forwarder plus the
//! cell model that permission detection reads. These tests cover the segmenter
//! (still used by the out-of-band sources) and the forwarding/cell behaviour.

use crate::processing::segmenter::segment_sentences;
use crate::processing::{ProcessingEvent, ProcessingLayer};

fn collect_terminal_bytes(events: &[ProcessingEvent]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in events {
        let ProcessingEvent::TerminalBytes(b) = e;
        out.extend_from_slice(b);
    }
    out
}

// ---------------- segmenter unit tests ----------------

#[test]
fn segmenter_single_sentence() {
    let segs = segment_sentences("Hello world.");
    assert_eq!(segs, vec!["Hello world.".to_string()]);
}

#[test]
fn segmenter_three_sentences() {
    let segs = segment_sentences("This is the first. This is the second. And here's a third one.");
    assert_eq!(segs.len(), 3);
    assert_eq!(segs[0], "This is the first.");
    assert_eq!(segs[1], "This is the second.");
    assert_eq!(segs[2], "And here's a third one.");
}

#[test]
fn segmenter_abbreviations_dont_split() {
    let segs = segment_sentences("Dr. Smith said hello. e.g. like this.");
    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0], "Dr. Smith said hello.");
    assert_eq!(segs[1], "e.g. like this.");
}

#[test]
fn segmenter_decimals_dont_split() {
    let segs = segment_sentences("The value is 3.14 today.");
    assert_eq!(segs, vec!["The value is 3.14 today.".to_string()]);
}

#[test]
fn segmenter_ellipsis_doesnt_split() {
    let segs = segment_sentences("Just a moment... here it is.");
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0], "Just a moment... here it is.");
}

#[test]
fn segmenter_paragraph_break() {
    let segs = segment_sentences("First para\n\nSecond para");
    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0], "First para");
    assert_eq!(segs[1], "Second para");
}

#[test]
fn segmenter_question_and_exclamation() {
    let segs = segment_sentences("Really? Yes! Indeed.");
    assert_eq!(segs.len(), 3);
    assert_eq!(segs[0], "Really?");
    assert_eq!(segs[1], "Yes!");
    assert_eq!(segs[2], "Indeed.");
}

#[test]
fn segmenter_fragment_no_punctuation() {
    let segs = segment_sentences("Just a moment");
    assert_eq!(segs, vec!["Just a moment".to_string()]);
}

// ---------------- processing layer behaviour (V20 forwarder) ----------------

#[test]
fn empty_input_no_events() {
    let mut layer = ProcessingLayer::new();
    assert!(layer.ingest(b"").is_empty());
}

#[test]
fn plain_text_forwards_immediately() {
    let mut layer = ProcessingLayer::new();
    let events = layer.ingest(b"hello world");
    assert_eq!(collect_terminal_bytes(&events), b"hello world".to_vec());
}

#[test]
fn ansi_styling_forwarded_verbatim() {
    let mut layer = ProcessingLayer::new();
    let bytes = b"\x1b[1mbold\x1b[0m";
    let events = layer.ingest(bytes);
    assert_eq!(collect_terminal_bytes(&events), bytes.to_vec());
}

#[test]
fn tts_markers_are_forwarded_verbatim_not_stripped() {
    // V20: the `[[TTS]]` convention is retired. Any literal markers (there
    // shouldn't be any) pass straight through to the terminal unchanged, and
    // nothing is spoken from the stream — TTS comes from `crate::oob`.
    let mut layer = ProcessingLayer::new();
    let events = layer.ingest(b"[[TTS]]hello[[/TTS]]");
    assert_eq!(
        collect_terminal_bytes(&events),
        b"[[TTS]]hello[[/TTS]]".to_vec()
    );
}

#[test]
fn bytes_forwarded_once_across_chunks() {
    let mut layer = ProcessingLayer::new();
    let mut all = Vec::new();
    all.extend(collect_terminal_bytes(&layer.ingest(b"foo")));
    all.extend(collect_terminal_bytes(&layer.ingest(b"bar")));
    all.extend(collect_terminal_bytes(&layer.ingest(b"baz")));
    assert_eq!(all, b"foobarbaz".to_vec());
    // A flush with nothing pending yields nothing.
    assert!(layer.flush_pending().is_empty());
}

#[test]
fn recent_rendered_reflects_printed_cells() {
    let mut layer = ProcessingLayer::new();
    let _ = layer.ingest(b"approve this action? y/n");
    let tail = layer.recent_rendered(1000);
    assert!(tail.contains("approve this action?"), "got: {tail}");
}

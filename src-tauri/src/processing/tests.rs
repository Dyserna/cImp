//! Unit tests for the processing layer. Use the `_at` variants of ingest/flush
//! so timing is deterministic.

use std::time::{Duration, Instant};

use crate::processing::segmenter::segment_sentences;
use crate::processing::{ProcessingEvent, ProcessingLayer, DEFAULT_MAX_HOLD};

fn t0() -> Instant {
    Instant::now()
}

fn collect_terminal_bytes(events: &[ProcessingEvent]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in events {
        if let ProcessingEvent::TerminalBytes(b) = e {
            out.extend_from_slice(b);
        }
    }
    out
}

fn collect_tts(events: &[ProcessingEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            ProcessingEvent::TtsSegment(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
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

// ---------------- processing layer behaviour ----------------

#[test]
fn empty_input_no_events() {
    let mut layer = ProcessingLayer::new();
    let events = layer.ingest_at(b"", t0());
    assert!(events.is_empty());
}

#[test]
fn plain_text_flushes_immediately() {
    let mut layer = ProcessingLayer::new();
    let now = t0();
    // Smart flush: tail has no opener prefix, no open tag → flush on ingest.
    let events = layer.ingest_at(b"hello world", now);
    assert_eq!(collect_terminal_bytes(&events), b"hello world".to_vec());
    assert!(collect_tts(&events).is_empty());
}

#[test]
fn ansi_styling_flushes_immediately() {
    let mut layer = ProcessingLayer::new();
    let now = t0();
    let bytes = b"\x1b[1mbold\x1b[0m";
    let events = layer.ingest_at(bytes, now);
    assert_eq!(collect_terminal_bytes(&events), bytes.to_vec());
}

#[test]
fn tail_with_partial_opener_is_held() {
    let mut layer = ProcessingLayer::new();
    let now = t0();
    // Tail "[[" looks like the start of a tag opener — hold.
    let mid = layer.ingest_at(b"hello [[", now);
    assert_eq!(collect_terminal_bytes(&mid), Vec::<u8>::new());

    // More bytes arrive that disambiguate (not a tag).
    let post = layer.ingest_at(b" not a tag", now);
    let term = collect_terminal_bytes(&post);
    assert_eq!(term, b"hello [[ not a tag".to_vec());
}

#[test]
fn tail_with_partial_opener_force_flushes_at_max_hold() {
    let mut layer = ProcessingLayer::new();
    let now = t0();
    let _ = layer.ingest_at(b"hello [", now);
    // Within max-hold — held.
    let mid = layer.flush_pending_at(now + Duration::from_millis(100));
    assert_eq!(collect_terminal_bytes(&mid), Vec::<u8>::new());

    // After max-hold — force-flush emits the literal.
    let post = layer.flush_pending_at(now + DEFAULT_MAX_HOLD + Duration::from_millis(10));
    assert_eq!(collect_terminal_bytes(&post), b"hello [".to_vec());
}

#[test]
fn complete_tag_emits_tts_and_strips_markers() {
    let mut layer = ProcessingLayer::new();
    let now = t0();
    let events = layer.ingest_at(b"[[TTS]]hello[[/TTS]]", now);

    let tts: Vec<String> = collect_tts(&events);
    assert_eq!(tts, vec!["hello".to_string()]);

    // Markers replaced with erase-N + cursor-forward-N: keeps cursor in the
    // column Claude expected and wipes stale cells where the marker would
    // otherwise have rendered.
    let term = collect_terminal_bytes(&events);
    assert_eq!(term, b"\x1b[7X\x1b[7Chello\x1b[8X\x1b[8C".to_vec());
}

#[test]
fn cross_chunk_tag_still_detected() {
    let mut layer = ProcessingLayer::new();
    let now = t0();
    let _ = layer.ingest_at(b"[[TT", now);
    let _ = layer.ingest_at(b"S]]hello[[/T", now);
    let events = layer.ingest_at(b"TS]]", now);
    let tts = collect_tts(&events);
    assert_eq!(tts, vec!["hello".to_string()]);
    assert_eq!(
        collect_terminal_bytes(&events),
        b"\x1b[7X\x1b[7Chello\x1b[8X\x1b[8C".to_vec()
    );
}

#[test]
fn two_sentences_in_one_block() {
    let mut layer = ProcessingLayer::new();
    let now = t0();
    let events = layer.ingest_at(
        b"[[TTS]]This is the first. This is the second.[[/TTS]]",
        now,
    );
    let tts = collect_tts(&events);
    assert_eq!(tts.len(), 2);
    assert_eq!(tts[0], "This is the first.");
    assert_eq!(tts[1], "This is the second.");
}

#[test]
fn styled_tag_content_strips_markers_keeps_styling() {
    let mut layer = ProcessingLayer::new();
    let now = t0();
    let bytes = b"[[TTS]]hello \x1b[1mworld\x1b[0m.[[/TTS]]";
    let events = layer.ingest_at(bytes, now);

    let tts = collect_tts(&events);
    assert_eq!(tts.len(), 1);
    // The cell-rendered view sees "hello world." (no ANSI in cells).
    assert_eq!(tts[0], "hello world.");

    let term = collect_terminal_bytes(&events);
    // Markers substituted with erase+advance; inner ANSI styling preserved.
    assert_eq!(
        term,
        b"\x1b[7X\x1b[7Chello \x1b[1mworld\x1b[0m.\x1b[8X\x1b[8C".to_vec()
    );
}

#[test]
fn double_tag_in_one_response() {
    let mut layer = ProcessingLayer::new();
    let now = t0();
    let events = layer.ingest_at(
        b"[[TTS]]first.[[/TTS]] code [[TTS]]second.[[/TTS]]",
        now,
    );
    let tts = collect_tts(&events);
    assert_eq!(tts, vec!["first.".to_string(), "second.".to_string()]);
    assert_eq!(
        collect_terminal_bytes(&events),
        b"\x1b[7X\x1b[7Cfirst.\x1b[8X\x1b[8C code \x1b[7X\x1b[7Csecond.\x1b[8X\x1b[8C".to_vec()
    );
}

#[test]
fn unclosed_tag_recovered_at_max_hold() {
    let mut layer = ProcessingLayer::new();
    let now = t0();
    let _ = layer.ingest_at(b"[[TTS]]some text", now);

    // Within max-hold — held, no TTS, no terminal output.
    let mid = layer.flush_pending_at(now + Duration::from_millis(300));
    assert_eq!(collect_terminal_bytes(&mid), Vec::<u8>::new());
    assert!(collect_tts(&mid).is_empty());

    // After max-hold — emit literal, no TTS.
    let post = layer.flush_pending_at(now + DEFAULT_MAX_HOLD + Duration::from_millis(10));
    let term = collect_terminal_bytes(&post);
    assert_eq!(term, b"[[TTS]]some text".to_vec());
    assert!(collect_tts(&post).is_empty());
}

#[test]
fn rewrite_doesnt_double_emit_tts() {
    let mut layer = ProcessingLayer::new();
    let now = t0();
    // First emit.
    let e1 = layer.ingest_at(b"[[TTS]]hello[[/TTS]]\r\n", now);
    let tts1 = collect_tts(&e1);
    assert_eq!(tts1, vec!["hello".to_string()]);

    // Cursor up to row 0, redraw the same line.
    let e2 = layer.ingest_at(b"\x1b[A[[TTS]]hello[[/TTS]]", now);
    let tts2 = collect_tts(&e2);
    assert!(tts2.is_empty(), "duplicate content should not be re-emitted: got {:?}", tts2);
}

#[test]
fn empty_tag_block_does_nothing() {
    let mut layer = ProcessingLayer::new();
    let now = t0();
    let events = layer.ingest_at(b"[[TTS]][[/TTS]]", now);
    assert!(collect_tts(&events).is_empty());
    // Empty tag block still produces marker substitutions for cursor
    // bookkeeping; nothing visually rendered, but the bytes go out.
    assert_eq!(
        collect_terminal_bytes(&events),
        b"\x1b[7X\x1b[7C\x1b[8X\x1b[8C".to_vec()
    );
}

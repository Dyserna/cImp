# Milestone 2: Processing Layer

## Goal

Insert the processing layer between the PTY and the frontend. Implement vte-based ANSI parsing, the hybrid flush trigger model, and `[[TTS]]...[[/TTS]]` tag detection with stripping. Extracted TTS content is emitted to a stub consumer (logging only — actual synthesis is Milestone 3). The terminal continues to render correctly with tags invisible to the user.

## Why This Milestone Now

This is the most logic-dense component in the application and the seam where Claude Code's output meets our requirements. Building it standalone, with a stub TTS consumer, lets us validate the parsing, stripping, and timing logic before the TTS pipeline adds its own complexity. If tag detection is buggy or rewrite handling is wrong, we want to see those bugs in clear logs, not as garbled audio.

## Scope

### In Scope

- A `processing` module that sits between the PTY reader and the frontend's terminal byte stream
- ANSI parsing via the `vte` crate, maintaining terminal screen state
- Detection of `[[TTS]]...[[/TTS]]` tags across ANSI styling boundaries
- Stripping of detected tags from the byte stream forwarded to the frontend
- Extraction of tag contents into sentence-segmented TTS items
- A stub TTS consumer (logs each segment at INFO level with the text content)
- Hybrid flush trigger model:
  - Stability timeout (default 200ms, hardcoded for this milestone)
  - Maximum hold time (default 500ms, hardcoded for this milestone)
  - Immediate emission for completed TTS tags
- In-place rewrite handling so rewritten regions are not double-emitted

### Out of Scope (Defer)

- Actual TTS synthesis (Milestone 3)
- Audio playback (Milestone 3)
- Configurable timeouts via settings (Milestone 6)
- Avatar pane and visualizer (Milestones 4 and 5)
- Performance optimization beyond reasonable defaults

## Acceptance Criteria

The milestone is complete when all of the following are true:

1. The terminal continues to display Claude Code's output correctly — colors, styling, layout, in-place rewrites all behave as in Milestone 1
2. A user typing in the terminal experiences a perceptible but acceptable delay (200–500ms) between Claude Code emitting output and it appearing on screen
3. When Claude Code emits text wrapped in `[[TTS]]...[[/TTS]]` tags, the tags do not appear in the terminal — only the inner content is visible, with original styling preserved
4. When tags are detected, the contained text is logged via `tracing` at INFO level, broken into individual sentences
5. Tags work correctly when their contents include ANSI styling (e.g., bold, color)
6. Tags work correctly when split across multiple read chunks (i.e., the parser must not assume a single read contains a complete tag)
7. In-place rewrites (such as Claude Code's spinner or input box redraws) do not produce duplicate or corrupted log output
8. If Claude Code emits malformed tags (unclosed `[[TTS]]` with no `[[/TTS]]`), the parser handles it gracefully — eventually emits the buffered content as plain terminal output without crashing or hanging
9. The above works on both Windows and Linux

## Implementation Approach

### Module Structure

Add to the existing project:

```
src-tauri/src/
  processing/
    mod.rs           # public API
    parser.rs        # vte-based byte parser, screen state tracking
    tag_detector.rs  # [[TTS]] tag finding in rendered text
    flush.rs         # hybrid flush trigger logic
    segmenter.rs     # sentence-boundary segmentation
```

### Public API

```
pub struct ProcessingLayer {
    // owns the parser, flush state, output channels
}

pub enum ProcessingEvent {
    TerminalBytes(Vec<u8>),     // bytes for xterm.js
    TtsSegment(String),         // a sentence to be spoken
    Stalled,                    // diagnostic: nothing has flushed in a while
}

impl ProcessingLayer {
    pub fn new(output_tx: mpsc::Sender<ProcessingEvent>) -> Self;
    pub async fn ingest(&mut self, bytes: &[u8]);
    pub async fn flush_pending(&mut self);  // called by timer task
}
```

The PTY reader feeds bytes via `ingest()`. A separate timer task calls `flush_pending()` periodically (e.g., every 50ms) so the stability timeout and max hold can be enforced even when no new bytes are arriving.

### Internal Design

#### Two-View Byte Stream

The core insight from the design doc: maintain both a raw view (with ANSI codes) and a rendered view (text only) of the buffered output.

Approach:

1. Feed bytes through a `vte::Parser`. Implement a custom `vte::Perform` trait implementation that:
   - On `print(c)`: append the character to both the raw view (as UTF-8 bytes) and the rendered view (as a `char` with a position-to-raw-offset mapping)
   - On `execute(byte)`: control characters; usually preserve in raw view, may or may not affect rendered view (e.g., `\n` goes in both)
   - On `csi_dispatch(...)`: ANSI escape sequences; preserve all bytes of the sequence in the raw view, do not append to the rendered view, but track effects on screen state (cursor position, etc.)
   - On `osc_dispatch(...)` and other escape types: preserve in raw view as-is

2. Maintain a position map: each character in the rendered view has a corresponding byte range in the raw view. When we strip a tag detected in the rendered view, we use the map to find the bytes to remove from the raw view.

The raw view is what eventually gets forwarded to xterm.js. The rendered view is what we scan for tags.

#### Tag Detection (`tag_detector.rs`)

Operates on the rendered view (plain text, no ANSI). Finds `[[TTS]]` and `[[/TTS]]` markers. Returns spans that need to be stripped from the raw view and content that needs to be sent to TTS.

Edge cases:

- **Partial tag at end of buffer**: if we see `[[TT` and no more, hold the buffer; do not flush yet (wait for more bytes or for the max hold)
- **Unclosed tag**: if `[[TTS]]` is followed by content but no `[[/TTS]]` arrives within the max hold, treat the opening `[[TTS]]` as literal text and forward everything as plain output (no TTS extraction). This is a malformed-output recovery; log a warning.
- **Nested or malformed sequences**: do not attempt to handle nested tags. The first `[[/TTS]]` after an `[[TTS]]` closes the block. Log warnings for anything weird and recover by treating the malformed region as literal text.

#### Flush Logic (`flush.rs`)

Three triggers, evaluated whenever `flush_pending()` is called or after each `ingest()`:

1. **Completed tag trigger**: if a `[[/TTS]]` has been detected and the corresponding `[[TTS]]` block is complete, emit the TTS content immediately (segmented into sentences). The raw bytes for that span (without the tags) can be flushed to terminal output as well, since the tag is closed and the content is final.

2. **Stability trigger**: if no new bytes have arrived for the configured stability timeout (200ms) AND the buffer has stable content not yet emitted, flush the stable region to terminal output. "Stable" means no in-progress tag and no recent screen rewrites in that region.

3. **Max hold trigger**: if the oldest unflushed byte is older than the max hold time (500ms), force a flush of everything that can be flushed (i.e., everything except in-progress tags, which still wait).

Implementation note: track timestamps for buffered content. The simplest approach is to record `Instant::now()` when each ingest call's bytes arrive and use the oldest pending timestamp as the "buffer age."

#### In-Place Rewrite Handling

The vte parser's screen state tells us when content is being overwritten. When the cursor moves backward and overwrites previously-emitted-but-not-yet-flushed bytes, the older bytes should be discarded — only the latest version reaches the terminal.

Practical approach: maintain logical "rows" in the buffer based on cursor position. When the cursor moves to a row that already has content, mark the existing content for replacement. Only flush rows once they've been stable (no further rewrites within the stability timeout).

This is the trickiest part of the milestone. A simpler initial implementation that may be sufficient: track the cursor position. If a backwards cursor movement occurs, hold the recent buffer until the next stability timeout — letting Claude Code's redraw stabilize before flushing.

If the simpler approach produces visible artifacts (duplicate spinner frames, etc.), upgrade to the row-tracking approach.

#### Sentence Segmentation (`segmenter.rs`)

When a complete `[[TTS]]...[[/TTS]]` block is extracted, segment its content into sentences. Boundaries:

- `.`, `?`, `!` followed by whitespace or end of string
- `\n\n` (paragraph break)

False positive handling:

- Decimal numbers: `3.14` should not split. If `.` is preceded by a digit and followed by a digit, do not split.
- Common abbreviations: `Dr.`, `Mr.`, `Mrs.`, `e.g.`, `i.e.`, `etc.`, `vs.`, `Inc.`, `Ltd.`. If `.` is preceded by one of these patterns, do not split. Hardcoded list is fine.
- Ellipsis: `...` should be treated as a single unit, not three sentence breaks. If `.` is part of `..` or `...`, treat the whole sequence as a non-breaking unit (probably keep as part of the preceding sentence).

If a TTS block contains no sentence-ending punctuation (a fragment like "Just a moment"), emit the whole block as a single segment.

Output: an ordered list of sentence strings. Each is sent as a separate `ProcessingEvent::TtsSegment`.

### Wiring Into the App

Modify the PTY manager from Milestone 1 so that instead of forwarding bytes directly to the frontend, it sends them to the processing layer. The processing layer's output goes to the frontend (terminal bytes) and to a stub TTS consumer (TTS segments).

Pseudocode in `main.rs`:

```
// Milestone 1 had: PTY reader → frontend
// Milestone 2 has: PTY reader → processing layer → {frontend, stub_tts_logger}

let (proc_tx, mut proc_rx) = mpsc::channel(...);
let mut processing = ProcessingLayer::new(proc_tx);

// PTY reader task: loop { read bytes; processing.ingest(bytes).await; }
// Flush timer task: loop { sleep(50ms); processing.flush_pending().await; }
// Output dispatcher task:
//   while let Some(event) = proc_rx.recv().await {
//     match event {
//       ProcessingEvent::TerminalBytes(b) => app.emit("pty-output", b),
//       ProcessingEvent::TtsSegment(s) => tracing::info!(target: "tts_stub", "would speak: {}", s),
//       ProcessingEvent::Stalled => tracing::warn!("processing layer stalled"),
//     }
//   }
```

The flush timer is critical. Without it, the stability timeout never fires when input stops arriving.

## Validation Steps

1. **Basic forwarding**: launch the app, verify Claude Code displays correctly — same visual experience as Milestone 1, with a perceptible 200–500ms lag now
2. **Tag stripping**: ask Claude Code to emit a response wrapped in `[[TTS]]...[[/TTS]]`. Verify the tags don't appear on screen but the content does, and the styling on the content (if any) is preserved
3. **Tag logging**: verify each sentence inside the tags appears in the application logs at INFO level
4. **Sentence splitting**: have Claude emit something like `[[TTS]]This is the first. This is the second. And here's a third one.[[/TTS]]`. Verify three log entries.
5. **Abbreviation handling**: have Claude emit `[[TTS]]Dr. Smith said hello. e.g. like this.[[/TTS]]`. Verify two segments, not four.
6. **Decimal handling**: have Claude emit `[[TTS]]The value is 3.14 today.[[/TTS]]`. Verify one segment.
7. **Cross-chunk tag**: artificially split a tag across PTY reads (you can simulate this in unit tests). Verify the tag is still detected.
8. **Unclosed tag recovery**: have Claude emit `[[TTS]]some text` without closing it. Verify after 500ms the text appears on screen (with `[[TTS]]` literal) and a warning is logged.
9. **Spinner/rewrite handling**: while Claude Code is "thinking" and showing a spinner, verify no duplicate spinner frames appear in the terminal and nothing weird shows up in logs.
10. **Long streaming response**: ask Claude for a long response. Verify text appears in regular bursts (every ~500ms at most) rather than in one big dump at the end.

## Unit Tests

This milestone justifies real unit tests because the parsing logic is intricate and hard to validate by inspection alone.

Suggested test cases for `processing/`:

- Empty input produces no output
- Plain text passes through unchanged (after flush)
- ANSI styling passes through unchanged
- `[[TTS]]hello[[/TTS]]` produces terminal output `hello` (no tags) and one TTS segment "hello"
- `[[TTS]]hello[[/TTS]]` split across multiple ingest calls produces correct output
- `[[TTS]]first sentence. Second sentence.[[/TTS]]` produces two TTS segments
- `[[TTS]]Dr. Smith. e.g. test.[[/TTS]]` produces correct segmentation (two sentences)
- `[[TTS]]hello\x1b[1mworld\x1b[0m[[/TTS]]` produces terminal output with bold "world" (no tags) and TTS segment "hello world"
- Unclosed `[[TTS]]` is eventually flushed as literal text after max hold time
- Double-tag in same response: `[[TTS]]first.[[/TTS]] code [[TTS]]second.[[/TTS]]` produces two TTS segments and correct terminal display

These tests do not need a real PTY — feed byte sequences directly to the processing layer.

## Known Risks and Mitigation

- **vte parser learning curve**: vte's `Perform` trait has many methods. Most are no-ops for our purposes — we mainly care about `print`, `execute`, and `csi_dispatch`. Implement the trait minimally and add handlers as needed.
- **Position mapping complexity**: keeping the rendered-view ↔ raw-view position map correct under all ANSI sequences is fiddly. Start with a simple implementation that handles common cases (basic styling, cursor moves) and add robustness as edge cases surface.
- **Rewrite handling edge cases**: Claude Code's TUI may do things like clearing whole regions, alternate screen buffer entry/exit, etc. The simple "hold buffer on backward cursor move" approach may not cover all cases. If artifacts appear, escalate to the full row-tracking model.
- **Stability timeout tuning**: 200ms is a reasonable default but may feel sluggish or may not be enough during rapid output. Make it easy to adjust by changing a constant during this milestone; expose to settings in Milestone 6.

## What "Done" Looks Like

The terminal experience is preserved (modulo a small input-to-display lag). Tags are invisible. TTS content is being correctly extracted and segmented, evidenced by clear, well-formed log entries. The next milestone can plug a real TTS engine into the same channel that currently feeds the stub logger.

---

## Next Milestone

Milestone 3: TTS Pipeline. Replaces the stub logger with actual Kokoro synthesis and audio playback. Spike on the phonemization question early — pure-Rust path vs. small Python sidecar.

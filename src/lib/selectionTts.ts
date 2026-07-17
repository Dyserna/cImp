// Ctrl+right-click "read-along" for terminal selections.
//
// The gesture speaks the current selection aloud and paints a receding
// highlight that follows the speech sentence-by-sentence:
//   - when a read starts, the whole selection is painted with the "unread"
//     colors (default black-on-red);
//   - the sentence currently being spoken gets the "reading" accent color;
//   - each sentence reverts to its original terminal colors as it finishes;
//   - Esc (handled in the shortcut dispatcher) cancels everything.
//
// Why sentence granularity: the Kokoro TTS engine returns one audio blob per
// synthesis call with no word-level timing, so the finest the highlight can
// follow is one synthesized chunk. We split the selection into sentences on
// the frontend and send the exact chunk strings to the backend — so the text
// that is spoken is exactly the text that is highlighted. The backend's audio
// thread emits a `tts-selection-progress` event (on the `avatar-state`
// channel) as it advances through the chunks; this module advances the
// highlight off those events.
//
// Geometry: we reconstruct the selected text cell-by-cell from the xterm
// buffer (rather than `getSelection()`) so every character maps to an exact
// (row, col). One `IMarker` per selected row is registered up front so the
// highlight tracks the line if the buffer scrolls/trims; decorations are
// cheap and recreated as sentences change state.

import type { Terminal, IDecoration, IMarker } from '@xterm/xterm';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { get, writable } from 'svelte/store';

import { ttsSpeakSelection, ttsStop, ttsSetPaused } from './ipc';
import { splitIntoChunks } from './selectionSplit';
import { settings } from './settings/store';
import { terminalWithSelection } from './terminals';
import { showToast } from './toast';
import type { SelectionHighlightSettings } from './settings/types';

/// One decoration target: a contiguous run of cells on a single buffer row.
interface CellSpan {
  row: number; // absolute buffer line
  x: number; // 0-based start column
  width: number; // cells
}

type ChunkState = 'none' | 'unread' | 'reading' | 'clear';

/// Playback state of the selection read, surfaced to the bottom-bar transport
/// so it can enable/disable buttons and swap play↔pause. `idle` means nothing
/// is being read.
export type SelectionTtsState = 'idle' | 'playing' | 'paused';
export const selectionTtsState = writable<SelectionTtsState>('idle');

let activeSession = 0; // 0 = no active read
let sessionCounter = 0;
let term: Terminal | null = null;
let painting = false;
let colors: SelectionHighlightSettings | null = null;
let chunkCount = 0;
let chunkSpans: CellSpan[][] = [];
let chunkState: ChunkState[] = [];
let chunkDecorations: IDecoration[][] = [];
let rowMarkers: Map<number, IMarker> = new Map();
let unlisten: UnlistenFn | null = null;
/// The last read's spoken chunks + geometry, kept so "restart" can replay the
/// same selection from the beginning even after it finished (the native
/// selection was cleared when the read started, so we can't re-read it).
let lastRead: { term: Terminal; chunks: string[]; spans: CellSpan[][] } | null = null;

/// True while a selection read is in flight (playing OR paused). The Esc
/// shortcut consults this so it only swallows Escape when there is something
/// to cancel.
export function isSelectionTtsActive(): boolean {
  return activeSession !== 0;
}

/// Begin reading the terminal's current selection. `highlight` is read at
/// call time (live settings). Returns immediately after dispatching; progress
/// arrives asynchronously via the event listener. Backs both the
/// Ctrl+right-click gesture and the bottom-bar "play".
export async function beginSelectionTts(
  terminal: Terminal,
  highlight: SelectionHighlightSettings,
): Promise<void> {
  const text = terminal.getSelection();
  const pos = terminal.getSelectionPosition();
  // Diagnostic: the raw selection as xterm sees it, before any of our
  // reconstruction. Compare `getSelection` text against the reconstructed
  // `model.text` below to spot a gap in `buildSelectionModel`.
  console.debug(
    `[selection-tts] getSelection=${JSON.stringify(text)} pos=${JSON.stringify(pos)}`,
  );
  if (!text.trim()) {
    console.debug('[selection-tts] BAIL: getSelection is empty/whitespace');
    return;
  }

  // Tear down any previous read (audio + decorations) before starting.
  await stopAllTts();

  // Build chunk geometry before clearing the native selection.
  const model = buildSelectionModel(terminal);
  if (!model) {
    console.debug('[selection-tts] BAIL: buildSelectionModel returned null');
    return;
  }
  console.debug(`[selection-tts] model.text=${JSON.stringify(model.text)}`);
  const parsed = splitIntoChunks(model.text);
  if (parsed.length === 0) {
    console.debug('[selection-tts] BAIL: splitIntoChunks produced 0 chunks');
    return;
  }

  // Clear xterm's own selection so it doesn't render on top of our
  // decorations (the canvas renderer always draws selection above decorations).
  terminal.clearSelection();

  const chunks = parsed.map((c) => c.text);
  const spans = parsed.map((c) => spansForRange(model, c.start, c.end));
  // Cache for "restart" — the native selection is gone now.
  lastRead = { term: terminal, chunks, spans };

  await launch(terminal, chunks, spans, highlight);
}

/// Start (or replay) a read for already-computed chunks + geometry. Assigns a
/// fresh session, sets up the highlight, and dispatches synthesis. Shared by
/// `beginSelectionTts` (from a live selection) and `restartSelectionTts`
/// (from the cached `lastRead`).
async function launch(
  terminal: Terminal,
  chunks: string[],
  spans: CellSpan[][],
  highlight: SelectionHighlightSettings,
): Promise<void> {
  const session = ++sessionCounter;
  activeSession = session;
  term = terminal;
  colors = highlight;
  painting = highlight.enabled;
  chunkCount = chunks.length;
  chunkSpans = spans;
  chunkState = Array.from({ length: chunkCount }, (): ChunkState => 'none');
  chunkDecorations = Array.from({ length: chunkCount }, () => []);
  rowMarkers = new Map();

  if (painting) {
    // One marker per selected row, created once so decorations can be
    // recreated against a line that tracks scrolling/trimming. If any marker
    // is trimmed out of the buffer the geometry is no longer valid, so we end
    // the highlight (audio keeps playing — only the visual is dropped).
    //
    // Wrapped in try/catch: the terminal may have been disposed between
    // caching the read and this replay (tab closed), in which case
    // `buffer.active` / `registerMarker` throw. Degrade gracefully to
    // audio-only rather than letting the throw escape uncaught.
    try {
      const base = terminal.buffer.active.baseY + terminal.buffer.active.cursorY;
      const rows = new Set<number>();
      for (const sp of spans) for (const s of sp) rows.add(s.row);
      for (const row of rows) {
        const marker = terminal.registerMarker(row - base);
        if (!marker) continue;
        marker.onDispose(() => {
          // A row scrolled out of the buffer: end the highlight entirely.
          // Must be endSession() (not just clearDecorations) so `painting`/
          // `chunkCount` are reset — otherwise a later progress event would
          // call paintState() against the now-empty chunkSpans/chunkState
          // arrays and throw. Audio keeps playing; only the visual is dropped.
          if (activeSession === session) endSession();
        });
        rowMarkers.set(row, marker);
      }
      // Initial state: whole selection painted "unread"; no sentence is
      // "reading" until its playback-start event arrives.
      paintState(-1);
    } catch (err) {
      console.warn('selection highlight setup failed (terminal disposed?):', err);
      painting = false;
    }
  }

  await ensureListener();
  selectionTtsState.set('playing');
  // Diagnostic: the exact chunk strings handed to the backend. Cross-check
  // against the worker's per-chunk synth log to see whether a dropped read is
  // a frontend split gap (missing/short chunk here) or a backend skip.
  console.debug(
    `[selection-tts] session=${session} chunks=${chunks.length}`,
    chunks.map((c, i) => `[${i}] (${c.length}) ${JSON.stringify(c.slice(0, 60))}`),
  );
  try {
    await ttsSpeakSelection(session, chunks);
  } catch (err) {
    console.warn('tts_speak_selection failed:', err);
    if (activeSession === session) endSession();
  }
}

// --- Bottom-bar transport ---------------------------------------------------

/// Guards against a begin already in flight — `beginSelectionTts` is async and
/// only sets state to 'playing' after an await, so a rapid double-click would
/// otherwise launch a second session that tears down the first mid-setup.
let startingSelection = false;

/// Play: resume if paused, otherwise read the current terminal selection.
/// Toasts when nothing is selected (the requested "no text selected" notice).
export function playSelectionTts(): void {
  if (get(selectionTtsState) === 'paused') {
    void resumeSelectionTts();
    return;
  }
  const terminal = terminalWithSelection();
  if (!terminal) {
    showToast('No text selected');
    return;
  }
  if (startingSelection) return; // a begin is already in flight — ignore the repeat
  startingSelection = true;
  void beginSelectionTts(terminal, get(settings).tts.selection_highlight).finally(() => {
    startingSelection = false;
  });
}

/// Pause in-flight playback (no-op unless currently playing).
export async function pauseSelectionTts(): Promise<void> {
  if (get(selectionTtsState) !== 'playing') return;
  try {
    await ttsSetPaused(true);
    selectionTtsState.set('paused');
  } catch (err) {
    console.warn('tts pause failed:', err);
  }
}

/// Resume paused playback (no-op unless currently paused).
export async function resumeSelectionTts(): Promise<void> {
  if (get(selectionTtsState) !== 'paused') return;
  try {
    await ttsSetPaused(false);
    selectionTtsState.set('playing');
  } catch (err) {
    console.warn('tts resume failed:', err);
  }
}

/// xterm throws on most property access after `dispose()`. Probe a benign
/// getter so a replay can detect a cached terminal whose tab was closed.
function isTerminalAlive(t: Terminal): boolean {
  try {
    // Touch internal buffer state; a disposed terminal throws here.
    void t.buffer.active.baseY;
    return true;
  } catch {
    return false;
  }
}

/// Restart the last read from its first sentence. Falls back to reading the
/// current selection (or toasting) when there is no cached read — or when the
/// cached read's terminal has since been disposed (its tab was closed), which
/// would otherwise throw inside `launch` (registerMarker / buffer access) with
/// no catch on this transport path.
export async function restartSelectionTts(): Promise<void> {
  const cached = lastRead;
  if (!cached || !isTerminalAlive(cached.term)) {
    lastRead = null;
    playSelectionTts();
    return;
  }
  await stopAllTts();
  await launch(
    cached.term,
    cached.chunks,
    cached.spans,
    get(settings).tts.selection_highlight,
  );
}

/// Stop playback and clear the highlight (identical to the Esc gesture).
export async function stopSelectionTts(): Promise<void> {
  await stopAllTts();
}

/// Stop ALL TTS playback and clear any read-along highlight. Backs the Esc
/// gesture (the only thing that interrupts speech — typing never does) and is
/// also used to supersede a previous read before starting a new one.
/// Unconditionally calls `tts_stop`, so it also stops ordinary AI-output TTS,
/// not just selection reads.
export async function stopAllTts(): Promise<void> {
  endSession();
  try {
    await ttsStop();
  } catch (err) {
    console.warn('tts_stop failed:', err);
  }
}

/// Subscribe to backend progress once; the handler advances the highlight.
async function ensureListener(): Promise<void> {
  if (unlisten) return;
  unlisten = await listen<{
    type: string;
    session: number;
    index: number;
  }>('avatar-state', (event) => {
    const e = event.payload;
    if (e.type !== 'tts-selection-progress') return;
    if (e.session !== activeSession) return; // stale read
    if (e.index >= chunkCount) {
      // Sentinel: the whole selection finished playing.
      endSession();
      return;
    }
    if (painting) paintState(e.index);
  });
}

/// Apply the three-state model for a given "now reading" chunk index:
///   i <  reading → read (no decoration, original colors)
///   i == reading → the accent "reading" colors
///   i >  reading → the base "unread" colors
function paintState(reading: number): void {
  for (let i = 0; i < chunkCount; i++) {
    const desired: ChunkState =
      i < reading ? 'clear' : i === reading ? 'reading' : 'unread';
    if (chunkState[i] === desired) continue;
    applyChunk(i, desired);
    chunkState[i] = desired;
  }
}

function applyChunk(i: number, kind: ChunkState): void {
  // Defensive: if the session's arrays were cleared out from under us, do
  // nothing rather than iterate an undefined slot.
  if (!chunkDecorations[i] || !chunkSpans[i]) return;
  // Dispose this chunk's current decorations first (decoration colors are
  // immutable, so a recolor is a dispose+recreate).
  for (const d of chunkDecorations[i]) d.dispose();
  chunkDecorations[i] = [];
  if (kind === 'clear' || kind === 'none' || !term || !colors) return;

  // Each channel is overridden only when its `*_custom` flag is set; an
  // un-set channel is left out of the decoration so the cell keeps the
  // terminal's own palette color there. A state with neither channel
  // customized paints nothing (no visible highlight for that state).
  const reading = kind === 'reading';
  const fg = reading
    ? colors.reading_fg_custom
      ? colors.reading_fg
      : undefined
    : colors.unread_fg_custom
      ? colors.unread_fg
      : undefined;
  const bg = reading
    ? colors.reading_bg_custom
      ? colors.reading_bg
      : undefined
    : colors.unread_bg_custom
      ? colors.unread_bg
      : undefined;
  if (fg === undefined && bg === undefined) return;

  for (const span of chunkSpans[i]) {
    const marker = rowMarkers.get(span.row);
    if (!marker || marker.isDisposed) continue;
    const deco = term.registerDecoration({
      marker,
      x: span.x,
      width: span.width,
      ...(fg !== undefined ? { foregroundColor: fg } : {}),
      ...(bg !== undefined ? { backgroundColor: bg } : {}),
      layer: 'top',
    });
    if (deco) chunkDecorations[i].push(deco);
  }
}

function clearDecorations(): void {
  for (const decos of chunkDecorations) for (const d of decos) d.dispose();
  for (const m of rowMarkers.values()) m.dispose();
  chunkDecorations = [];
  rowMarkers = new Map();
  chunkSpans = [];
  chunkState = [];
}

/// Reset all in-memory session state and drop the highlight. Does NOT touch
/// the backend (callers that need to stop audio call `ttsStop` themselves).
function endSession(): void {
  clearDecorations();
  activeSession = 0;
  term = null;
  colors = null;
  painting = false;
  chunkCount = 0;
  selectionTtsState.set('idle');
}

// --- Geometry ---------------------------------------------------------------

interface SelectionModel {
  text: string;
  /// Parallel to `text` by UTF-16 code unit: the cell each unit came from, or
  /// null for an inserted separator (newline between non-wrapped rows).
  cells: ({ row: number; col: number } | null)[];
}

/// Reconstruct the selected text cell-by-cell so every character maps to an
/// exact (row, col). Trailing blank cells per row are trimmed; non-wrapped row
/// boundaries become '\n' (wrapped continuations are joined with no separator
/// so a soft-wrapped word stays intact).
function buildSelectionModel(terminal: Terminal): SelectionModel | null {
  const range = terminal.getSelectionPosition();
  if (!range) return null;
  const buf = terminal.buffer.active;
  const cols = terminal.cols;

  let text = '';
  const cells: ({ row: number; col: number } | null)[] = [];

  for (let row = range.start.y; row <= range.end.y; row++) {
    const line = buf.getLine(row);
    if (!line) continue;
    const colStart = row === range.start.y ? range.start.x : 0;
    const colEndExcl = row === range.end.y ? range.end.x : cols;

    // Collect this row's selected, non-blank-tail cells.
    const rowChars: { ch: string; col: number }[] = [];
    for (let col = colStart; col < colEndExcl; col++) {
      const cell = line.getCell(col);
      if (!cell) continue;
      if (cell.getWidth() === 0) continue; // trailing half of a wide glyph
      let ch = cell.getChars();
      if (ch === '') ch = ' ';
      rowChars.push({ ch, col });
    }
    while (rowChars.length && rowChars[rowChars.length - 1].ch === ' ') {
      rowChars.pop();
    }

    if (row > range.start.y) {
      const sep = line.isWrapped ? '' : '\n';
      for (let k = 0; k < sep.length; k++) {
        text += sep[k];
        cells.push(null);
      }
    }
    for (const { ch, col } of rowChars) {
      text += ch;
      for (let k = 0; k < ch.length; k++) cells.push({ row, col });
    }
  }

  return { text, cells };
}

/// Map a [start, end) code-unit range in the model text to one decoration
/// span per buffer row (min..max column of the cells touched in that row).
function spansForRange(
  model: SelectionModel,
  start: number,
  end: number,
): CellSpan[] {
  const perRow = new Map<number, { min: number; max: number }>();
  for (let i = start; i < end && i < model.cells.length; i++) {
    const c = model.cells[i];
    if (!c) continue;
    const cur = perRow.get(c.row);
    if (!cur) perRow.set(c.row, { min: c.col, max: c.col });
    else {
      if (c.col < cur.min) cur.min = c.col;
      if (c.col > cur.max) cur.max = c.col;
    }
  }
  const spans: CellSpan[] = [];
  for (const [row, { min, max }] of perRow) {
    spans.push({ row, x: min, width: max - min + 1 });
  }
  return spans;
}

// Pure sentence splitter for selection TTS. Kept free of any DOM / xterm /
// Tauri imports so it can be unit-tested in a plain node environment and
// imported by `selectionTts.ts` without pulling those in.
//
// Mirrors the backend `processing::segmenter::segment_sentences` rules so the
// read-along splits the same way the rest of the app speaks: break on
// `.?!` + whitespace/EOS and on blank lines; suppress decimals, ellipses, and
// common abbreviations. Returns trimmed [start, end) ranges into `text`
// (offsets are needed to map back to cells). Sentences longer than
// `MAX_CHUNK_CHARS` are hard-wrapped at whitespace so nothing is truncated.

/// Cap on characters per spoken chunk. Kokoro truncates at 510 phoneme tokens
/// (~roughly one token per character for English); 280 chars keeps every chunk
/// comfortably under that so large selections are never cut off.
export const MAX_CHUNK_CHARS = 280;

const ABBREVS = new Set(
  [
    'Dr', 'Mr', 'Mrs', 'Ms', 'Jr', 'Sr', 'St', 'Inc', 'Ltd', 'Co', 'Corp',
    'vs', 'etc', 'Mt', 'No', 'e.g', 'i.e', 'a.m', 'p.m',
  ].map((s) => s.toLowerCase()),
);

export interface SentenceChunk {
  text: string;
  start: number;
  end: number;
}

export function splitIntoChunks(text: string): SentenceChunk[] {
  const out: SentenceChunk[] = [];
  const isWs = (c: string) =>
    c === ' ' || c === '\t' || c === '\n' || c === '\r' || c === '\f' || c === '\v';
  const isDigit = (c: string) => c >= '0' && c <= '9';

  const emit = (s: number, e: number) => {
    // Trim surrounding whitespace.
    let a = s;
    let b = e;
    while (a < b && isWs(text[a])) a++;
    while (b > a && isWs(text[b - 1])) b--;
    // Hard-wrap over-long runs at whitespace.
    while (b - a > MAX_CHUNK_CHARS) {
      let cut = -1;
      for (let k = Math.min(b - 1, a + MAX_CHUNK_CHARS); k > a; k--) {
        if (isWs(text[k])) {
          cut = k;
          break;
        }
      }
      if (cut < 0) cut = a + MAX_CHUNK_CHARS; // no whitespace: hard cut
      let pieceEnd = cut;
      while (pieceEnd > a && isWs(text[pieceEnd - 1])) pieceEnd--;
      if (pieceEnd > a) out.push({ text: text.slice(a, pieceEnd), start: a, end: pieceEnd });
      a = cut;
      while (a < b && isWs(text[a])) a++;
    }
    if (b > a) out.push({ text: text.slice(a, b), start: a, end: b });
  };

  const n = text.length;
  let segStart = 0;
  let i = 0;
  while (i < n) {
    const c = text[i];
    if (c === '\n' && i + 1 < n && text[i + 1] === '\n') {
      emit(segStart, i);
      i += 2;
      segStart = i;
      continue;
    }
    if (c === '.' || c === '?' || c === '!') {
      if (c === '.') {
        const prevDot = i > 0 && text[i - 1] === '.';
        const nextDot = i + 1 < n && text[i + 1] === '.';
        if (prevDot || nextDot) {
          while (i < n && text[i] === '.') i++;
          continue;
        }
        const prevDigit = i > 0 && isDigit(text[i - 1]);
        const nextDigit = i + 1 < n && isDigit(text[i + 1]);
        if (prevDigit && nextDigit) {
          i++;
          continue;
        }
        if (isAbbreviation(text, i)) {
          i++;
          continue;
        }
      }
      const end = i + 1;
      const nextIsBreak = end >= n || isWs(text[end]);
      if (nextIsBreak) {
        emit(segStart, end);
        i = end;
        segStart = i;
        continue;
      }
    }
    i++;
  }
  if (segStart < n) emit(segStart, n);
  return out;
}

/// The `.` at `dotIdx` is preceded by a word; is that word an abbreviation?
/// Words are alpha runs that may contain internal dots (so "e.g" matches on
/// the second dot of "e.g.").
function isAbbreviation(text: string, dotIdx: number): boolean {
  const isAlpha = (c: string) => (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z');
  let start = dotIdx;
  while (start > 0) {
    const ch = text[start - 1];
    if (isAlpha(ch) || ch === '.') start--;
    else break;
  }
  if (start === dotIdx) return false;
  return ABBREVS.has(text.slice(start, dotIdx).toLowerCase());
}

import { describe, it, expect } from 'vitest';
import { splitIntoChunks } from './selectionSplit';

// `splitIntoChunks` mirrors the backend `processing::segmenter` rules and is
// the foundation of the large-selection fix (each chunk stays under Kokoro's
// token cap). The ranges it returns must also slice back to the chunk text
// exactly, since the frontend maps those offsets to terminal cells.

describe('splitIntoChunks', () => {
  const texts = (s: string) => splitIntoChunks(s).map((c) => c.text);

  it('splits on sentence terminators followed by whitespace', () => {
    expect(texts('Hello world. How are you? Fine!')).toEqual([
      'Hello world.',
      'How are you?',
      'Fine!',
    ]);
  });

  it('returns offsets that slice back to the chunk text', () => {
    const input = 'First sentence. Second one? Third.';
    for (const c of splitIntoChunks(input)) {
      expect(input.slice(c.start, c.end)).toBe(c.text);
    }
  });

  it('does not split decimals or ellipses', () => {
    expect(texts('Pi is 3.14 here.')).toEqual(['Pi is 3.14 here.']);
    expect(texts('Wait... what happened?')).toEqual([
      'Wait... what happened?',
    ]);
  });

  it('does not split common abbreviations', () => {
    expect(texts('Dr. Smith arrived. He left.')).toEqual([
      'Dr. Smith arrived.',
      'He left.',
    ]);
    expect(texts('Use e.g. this one.')).toEqual(['Use e.g. this one.']);
  });

  it('splits on blank lines (paragraph breaks)', () => {
    expect(texts('Para one\n\nPara two')).toEqual(['Para one', 'Para two']);
  });

  it('ignores whitespace-only selections', () => {
    expect(splitIntoChunks('   \n\n  ')).toEqual([]);
  });

  it('hard-wraps an over-long run at whitespace so nothing is truncated', () => {
    const word = 'lorem';
    const long = Array.from({ length: 120 }, () => word).join(' '); // ~720 chars
    const chunks = splitIntoChunks(long);
    expect(chunks.length).toBeGreaterThan(1);
    // Every chunk is within the cap, and offsets still slice correctly.
    for (const c of chunks) {
      expect(c.text.length).toBeLessThanOrEqual(280);
      expect(long.slice(c.start, c.end)).toBe(c.text);
    }
    // No word is broken (every chunk is whole `lorem` tokens).
    for (const c of chunks) {
      for (const tok of c.text.split(/\s+/)) expect(tok).toBe(word);
    }
  });
});

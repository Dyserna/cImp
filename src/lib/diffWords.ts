// V13 Phase B B4 — intra-line word-diff for the Diff pane's changed-line
// pairs. A small LCS (longest common subsequence) over tokens, deliberately
// hand-rolled rather than a dependency (the impl plan explicitly rules one
// out for this: "a small LCS helper in TS — no new dependency").

export interface WordDiffPart {
  text: string;
  kind: 'same' | 'add' | 'del';
}

/// Above this many token pairs the O(n·m) DP table gets expensive for no
/// real payoff (a hunk line that long reads fine as a plain whole-line
/// add/del) — skip straight to the cheap fallback.
const MAX_DP_CELLS = 20_000;

/// Tokenize on runs of "word" characters vs. runs of everything else, so
/// diff boundaries land on identifier/number/whitespace/punctuation runs
/// instead of raw characters — `longVariableName` doesn't explode into one
/// token per character, while a single-character edit inside a token still
/// shows up as a del+add of that whole token (acceptable: word-level, not
/// character-level, diffing is exactly what "intra-line word-diff" asks for).
function tokenize(s: string): string[] {
  return s.match(/\w+|\W+/g) ?? [];
}

/// Word-level diff between one hunk line's old and new text. Returns two
/// parallel part lists: `left` renders the OLD line (`same`/`del` parts
/// only), `right` renders the NEW line (`same`/`add` parts only) — a caller
/// wanting a single interleaved view (unified mode) can concatenate
/// `left`'s `del` parts with `right`'s `add` parts in token order, since
/// both were walked from the same LCS backtrace.
export function wordDiff(oldLine: string, newLine: string): { left: WordDiffPart[]; right: WordDiffPart[] } {
  const a = tokenize(oldLine);
  const b = tokenize(newLine);
  const n = a.length;
  const m = b.length;

  if (n * m > MAX_DP_CELLS) {
    return {
      left: oldLine ? [{ text: oldLine, kind: 'del' }] : [],
      right: newLine ? [{ text: newLine, kind: 'add' }] : [],
    };
  }

  // Standard LCS DP table, built bottom-up from the end so the greedy
  // backtrace below can walk forward (i, j both increasing) while still
  // reading correct "rest of the sequence" lengths at each step.
  const dp: number[][] = Array.from({ length: n + 1 }, () => Array.from({ length: m + 1 }, () => 0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }

  const left: WordDiffPart[] = [];
  const right: WordDiffPart[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      left.push({ text: a[i], kind: 'same' });
      right.push({ text: b[j], kind: 'same' });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      left.push({ text: a[i], kind: 'del' });
      i++;
    } else {
      right.push({ text: b[j], kind: 'add' });
      j++;
    }
  }
  while (i < n) {
    left.push({ text: a[i], kind: 'del' });
    i++;
  }
  while (j < m) {
    right.push({ text: b[j], kind: 'add' });
    j++;
  }
  return { left, right };
}

/// One hunk-line pairing decision, produced by [`pairHunkLines`].
///   - `ctx` — an unchanged (context) line, rendered as-is.
///   - `del` / `add` — a removal/addition with no matching counterpart to
///     word-diff against (rendered as a plain whole-line del/add).
///   - `pair` — a single `-` line immediately followed by a single `+` line
///     (the common "changed one line" shape) — eligible for [`wordDiff`].
export type HunkLineGroup =
  | { type: 'ctx'; text: string }
  | { type: 'del'; text: string }
  | { type: 'add'; text: string }
  | { type: 'pair'; oldText: string; newText: string };

/// Group a hunk's raw `[marker, text][]` lines for rendering: consecutive
/// runs of `-` are paired 1:1 with an immediately-following run of `+` of
/// the SAME length (the unambiguous "these N lines became these N lines"
/// case); any other shape (uneven counts, a `-` run not immediately
/// followed by `+`) renders as plain del/add lines. This is deliberately
/// conservative — pairing a 3-line del run against a 2-line add run by
/// position would produce a misleading word-diff, so it doesn't try.
export function pairHunkLines(lines: [string, string][]): HunkLineGroup[] {
  const out: HunkLineGroup[] = [];
  let i = 0;
  while (i < lines.length) {
    const [marker, text] = lines[i];
    if (marker === ' ') {
      out.push({ type: 'ctx', text });
      i++;
      continue;
    }
    if (marker === '-') {
      let delEnd = i;
      while (delEnd < lines.length && lines[delEnd][0] === '-') delEnd++;
      let addEnd = delEnd;
      while (addEnd < lines.length && lines[addEnd][0] === '+') addEnd++;
      const delCount = delEnd - i;
      const addCount = addEnd - delEnd;
      if (delCount === addCount) {
        for (let k = 0; k < delCount; k++) {
          out.push({ type: 'pair', oldText: lines[i + k][1], newText: lines[delEnd + k][1] });
        }
      } else {
        for (let k = i; k < delEnd; k++) out.push({ type: 'del', text: lines[k][1] });
        for (let k = delEnd; k < addEnd; k++) out.push({ type: 'add', text: lines[k][1] });
      }
      i = addEnd;
      continue;
    }
    // A `+` run with no preceding `-` run (pure addition).
    out.push({ type: 'add', text });
    i++;
  }
  return out;
}

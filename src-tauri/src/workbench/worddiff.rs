//! V42 Phase D — intra-line word diffing and hunk-line grouping, moved here
//! from the frontend's `src/lib/diffWords.ts`.
//!
//! Every diff surface in the app (`DiffView`'s live pane in both unified and
//! side-by-side mode, `CheckpointDiffView`, `WorktreesView`, and the shared
//! `diff/HunkBody.svelte` rows) renders hunks that arrive from
//! [`super::diff`] over IPC. The grouping and the LCS were the last piece of
//! that pipeline still computed in the browser — recomputed by every consumer
//! on every re-render, against data Rust had already produced. They are
//! computed ONCE here now, when the hunk is built, and ride the existing
//! `Hunk` payload.
//!
//! **Payload shape, deliberately.** [`HunkLineGroup`] names lines by their
//! INDEX into [`Hunk::lines`](super::diff::Hunk::lines) rather than carrying
//! their text, so the common case (context lines, and unpaired adds/removes)
//! costs a small integer per line instead of a second copy of the file. Only
//! a `pair` carries text, and only the two lines it word-diffs — which is the
//! content the old TS computed on the client anyway. A full-file diff (the
//! "whole file as one hunk" toggle) is therefore NOT doubled in size.

use serde::Serialize;

/// Above this many token pairs the O(n·m) DP table gets expensive for no real
/// payoff (a hunk line that long reads fine as a plain whole-line add/del) —
/// skip straight to the cheap fallback. Ported verbatim from
/// `diffWords.ts`'s `MAX_DP_CELLS`; the value is part of the behaviour its
/// tests pin.
const MAX_DP_CELLS: usize = 20_000;

/// One span of a word-diffed line. `same` renders plain on both sides; `del`
/// only ever appears in a [`HunkLineGroup::Pair`]'s `left`, `add` only in its
/// `right`.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum WordDiffKind {
    Same,
    Add,
    Del,
}

/// One `{ text, kind }` span of a word-diffed line — the wire mirror of the
/// TS `WordDiffPart` this replaces.
#[derive(Clone, Debug, Serialize, PartialEq, Eq, Hash)]
pub struct WordDiffPart {
    pub text: String,
    pub kind: WordDiffKind,
}

impl WordDiffPart {
    fn new(text: &str, kind: WordDiffKind) -> Self {
        Self {
            text: text.to_string(),
            kind,
        }
    }
}

/// One hunk-line rendering decision, produced by [`pair_hunk_lines`].
///
///   - `Ctx` — an unchanged (context) line, rendered as-is.
///   - `Del` / `Add` — a removal/addition with no matching counterpart to
///     word-diff against (rendered as a plain whole-line del/add).
///   - `Pair` — a `-` line matched 1:1 with a `+` line (the common "changed
///     this line" shape), carrying the word-level spans for both sides.
///
/// `line` / `old_line` / `new_line` are indices into the hunk's own
/// [`lines`](super::diff::Hunk::lines) — see the module docs for why the text
/// is not repeated here.
#[derive(Clone, Debug, Serialize, PartialEq, Eq, Hash)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HunkLineGroup {
    Ctx {
        line: usize,
    },
    Del {
        line: usize,
    },
    Add {
        line: usize,
    },
    Pair {
        old_line: usize,
        new_line: usize,
        left: Vec<WordDiffPart>,
        right: Vec<WordDiffPart>,
    },
}

/// Tokenize on runs of "word" characters vs. runs of everything else, so diff
/// boundaries land on identifier/number/whitespace/punctuation runs instead of
/// raw characters — `longVariableName` doesn't explode into one token per
/// character, while a single-character edit inside a token still shows up as a
/// del+add of that whole token (acceptable: word-level, not character-level,
/// diffing is exactly what "intra-line word-diff" asks for).
///
/// "Word character" is ASCII `[A-Za-z0-9_]`, matching the JavaScript `\w` the
/// ported `/\w+|\W+/g` used (no `u` flag, so no Unicode property escapes):
/// a run of non-ASCII letters groups with the surrounding punctuation exactly
/// as it did in the browser.
fn tokenize(s: &str) -> Vec<&str> {
    fn is_word(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_'
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut cur: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let w = is_word(c);
        match cur {
            Some(prev) if prev == w => {}
            Some(_) => {
                out.push(&s[start..i]);
                start = i;
            }
            None => start = i,
        }
        cur = Some(w);
    }
    if cur.is_some() {
        out.push(&s[start..]);
    }
    out
}

/// Word-level diff between one hunk line's old and new text. Returns two
/// parallel span lists: `left` renders the OLD line (`same`/`del` spans only),
/// `right` renders the NEW line (`same`/`add` spans only) — a caller wanting a
/// single interleaved view can concatenate `left`'s `del` spans with `right`'s
/// `add` spans in token order, since both were walked from the same LCS
/// backtrace.
pub fn word_diff(old_line: &str, new_line: &str) -> (Vec<WordDiffPart>, Vec<WordDiffPart>) {
    let a = tokenize(old_line);
    let b = tokenize(new_line);
    let n = a.len();
    let m = b.len();

    if n * m > MAX_DP_CELLS {
        let left = if old_line.is_empty() {
            Vec::new()
        } else {
            vec![WordDiffPart::new(old_line, WordDiffKind::Del)]
        };
        let right = if new_line.is_empty() {
            Vec::new()
        } else {
            vec![WordDiffPart::new(new_line, WordDiffKind::Add)]
        };
        return (left, right);
    }

    // Standard LCS DP table, built bottom-up from the end so the greedy
    // backtrace below can walk forward (i, j both increasing) while still
    // reading correct "rest of the sequence" lengths at each step.
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < n && j < m {
        if a[i] == b[j] {
            left.push(WordDiffPart::new(a[i], WordDiffKind::Same));
            right.push(WordDiffPart::new(b[j], WordDiffKind::Same));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            left.push(WordDiffPart::new(a[i], WordDiffKind::Del));
            i += 1;
        } else {
            right.push(WordDiffPart::new(b[j], WordDiffKind::Add));
            j += 1;
        }
    }
    while i < n {
        left.push(WordDiffPart::new(a[i], WordDiffKind::Del));
        i += 1;
    }
    while j < m {
        right.push(WordDiffPart::new(b[j], WordDiffKind::Add));
        j += 1;
    }
    (left, right)
}

/// Group a hunk's `(marker, text)` lines for rendering: consecutive runs of
/// `-` are paired 1:1 with an immediately-following run of `+` of the SAME
/// length (the unambiguous "these N lines became these N lines" case); any
/// other shape (uneven counts, a `-` run not immediately followed by `+`)
/// renders as plain del/add lines. This is deliberately conservative —
/// pairing a 3-line del run against a 2-line add run by position would
/// produce a misleading word-diff, so it doesn't try.
///
/// A marker that is neither `' '` nor `'-'` is treated as an addition, which
/// is what the TS this replaces did with its final `else` arm: real hunk
/// bodies only ever carry the three markers, and a fourth would be a parser
/// bug, not a rendering decision to make here.
pub fn pair_hunk_lines(lines: &[(char, String)]) -> Vec<HunkLineGroup> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let marker = lines[i].0;
        if marker == ' ' {
            out.push(HunkLineGroup::Ctx { line: i });
            i += 1;
            continue;
        }
        if marker == '-' {
            let mut del_end = i;
            while del_end < lines.len() && lines[del_end].0 == '-' {
                del_end += 1;
            }
            let mut add_end = del_end;
            while add_end < lines.len() && lines[add_end].0 == '+' {
                add_end += 1;
            }
            let del_count = del_end - i;
            let add_count = add_end - del_end;
            if del_count == add_count {
                for k in 0..del_count {
                    let (left, right) = word_diff(&lines[i + k].1, &lines[del_end + k].1);
                    out.push(HunkLineGroup::Pair {
                        old_line: i + k,
                        new_line: del_end + k,
                        left,
                        right,
                    });
                }
            } else {
                for k in i..del_end {
                    out.push(HunkLineGroup::Del { line: k });
                }
                for k in del_end..add_end {
                    out.push(HunkLineGroup::Add { line: k });
                }
            }
            i = add_end;
            continue;
        }
        // A `+` run with no preceding `-` run (pure addition).
        out.push(HunkLineGroup::Add { line: i });
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── word_diff ────────────────────────────────────────────────────────
    // One test per case in the deleted `src/lib/diffWords.test.ts`, same
    // order, same names, same asserted values.

    fn texts(parts: &[WordDiffPart], kind: WordDiffKind) -> Vec<&str> {
        parts
            .iter()
            .filter(|p| p.kind == kind)
            .map(|p| p.text.as_str())
            .collect()
    }

    #[test]
    fn matches_unchanged_tokens_and_flags_only_the_changed_word() {
        let (left, right) = word_diff("foo bar baz", "foo qux baz");
        // 'foo', ' ', 'baz' are shared; 'bar'/'qux' differ.
        assert_eq!(
            texts(&left, WordDiffKind::Same),
            vec!["foo", " ", " ", "baz"]
        );
        assert!(left
            .iter()
            .any(|p| p.kind == WordDiffKind::Del && p.text == "bar"));
        assert!(right
            .iter()
            .any(|p| p.kind == WordDiffKind::Add && p.text == "qux"));
    }

    #[test]
    fn identical_lines_produce_only_same_parts() {
        let (left, right) = word_diff("unchanged line", "unchanged line");
        assert!(left.iter().all(|p| p.kind == WordDiffKind::Same));
        assert!(right.iter().all(|p| p.kind == WordDiffKind::Same));
    }

    #[test]
    fn wholly_different_lines_produce_del_only_left_and_add_only_right() {
        let (left, right) = word_diff("abc", "xyz");
        assert!(!left.is_empty() && left.iter().all(|p| p.kind == WordDiffKind::Del));
        assert!(!right.is_empty() && right.iter().all(|p| p.kind == WordDiffKind::Add));
    }

    #[test]
    fn a_single_character_edit_inside_an_identifier_diffs_at_word_granularity() {
        let (left, right) = word_diff("longVariableName", "longVariableNamee");
        // Whole-token del/add, not a per-character explosion.
        assert_eq!(
            left,
            vec![WordDiffPart::new("longVariableName", WordDiffKind::Del)]
        );
        assert_eq!(
            right,
            vec![WordDiffPart::new("longVariableNamee", WordDiffKind::Add)]
        );
    }

    #[test]
    fn a_run_of_the_same_character_class_tokenizes_as_one_token_not_one_per_char() {
        // A long run of spaces (common leading indentation) must not explode
        // into one token per space — that would make even a short-looking
        // indentation change look "wholly different" token-for-token. Expect
        // exactly 2 tokens (the whitespace run, the word), not 4 + 8.
        let (left, _) = word_diff("    indented", "    indented");
        assert_eq!(
            left,
            vec![
                WordDiffPart::new("    ", WordDiffKind::Same),
                WordDiffPart::new("indented", WordDiffKind::Same),
            ]
        );
    }

    #[test]
    fn very_long_highly_tokenized_lines_fall_back_to_whole_line_del_add() {
        // Many short distinct "words" (not long runs) so tokenization alone
        // doesn't collapse this back down — this is what actually exercises
        // the MAX_DP_CELLS guard.
        let old_line = (0..150).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let new_line = (0..150).map(|i| format!("x{i}")).collect::<Vec<_>>().join(" ");
        let (left, right) = word_diff(&old_line, &new_line);
        assert_eq!(left, vec![WordDiffPart::new(&old_line, WordDiffKind::Del)]);
        assert_eq!(right, vec![WordDiffPart::new(&new_line, WordDiffKind::Add)]);
    }

    #[test]
    fn empty_lines_produce_no_parts() {
        let (left, right) = word_diff("", "");
        assert!(left.is_empty());
        assert!(right.is_empty());
    }

    /// Not in the TS suite: a one-sided over-long line. The DP guard is
    /// `n * m > MAX_DP_CELLS`, so an EMPTY side (m = 0) never trips it — the
    /// whole-line fallback's `oldLine ? … : []` empty-side arm was
    /// unreachable in the TS too. Asserted so the port's behaviour is on the
    /// record rather than assumed: a 300-word line against an empty one is
    /// spelled out token by token, not collapsed.
    #[test]
    fn an_empty_side_takes_the_dp_path_not_the_whole_line_fallback() {
        let long = (0..300)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let (left, right) = word_diff(&long, "");
        assert!(left.len() > 1 && left.iter().all(|p| p.kind == WordDiffKind::Del));
        assert_eq!(
            left.iter().map(|p| p.text.as_str()).collect::<String>(),
            long
        );
        assert!(right.is_empty());
    }

    /// Not in the TS suite: the tokenizer is byte-index driven now, so a
    /// multi-byte character must not split a token mid-codepoint. `é` and `ö`
    /// are non-ASCII, so JavaScript's `\w` (and this port) treat them as
    /// NON-word characters — the run boundaries fall around them.
    #[test]
    fn non_ascii_text_tokenizes_on_char_boundaries() {
        let (left, right) = word_diff("héllo wörld", "héllo wörld!");
        assert_eq!(
            left.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["h", "é", "llo", " ", "w", "ö", "rld"]
        );
        assert!(left.iter().all(|p| p.kind == WordDiffKind::Same));
        assert!(right
            .iter()
            .any(|p| p.kind == WordDiffKind::Add && p.text == "!"));
    }

    // ── pair_hunk_lines ──────────────────────────────────────────────────

    fn hunk(lines: &[(char, &str)]) -> Vec<(char, String)> {
        lines.iter().map(|(m, t)| (*m, t.to_string())).collect()
    }

    #[test]
    fn context_lines_pass_through_untouched() {
        let lines = hunk(&[(' ', "ctx1"), (' ', "ctx2")]);
        let groups = pair_hunk_lines(&lines);
        assert_eq!(
            groups,
            vec![
                HunkLineGroup::Ctx { line: 0 },
                HunkLineGroup::Ctx { line: 1 },
            ]
        );
        // The TS asserted the TEXT (`{ type: 'ctx', text: 'ctx1' }`); the
        // index resolves to the same string against the hunk it indexes.
        assert_eq!(lines[0].1, "ctx1");
        assert_eq!(lines[1].1, "ctx2");
    }

    #[test]
    fn a_single_del_immediately_followed_by_a_single_add_pairs_for_word_diff() {
        let lines = hunk(&[('-', "old text"), ('+', "new text")]);
        let groups = pair_hunk_lines(&lines);
        let (left, right) = word_diff("old text", "new text");
        assert_eq!(
            groups,
            vec![HunkLineGroup::Pair {
                old_line: 0,
                new_line: 1,
                left,
                right,
            }]
        );
    }

    #[test]
    fn equal_length_multi_line_del_add_runs_pair_index_wise() {
        let lines = hunk(&[('-', "a1"), ('-', "a2"), ('+', "b1"), ('+', "b2")]);
        let groups = pair_hunk_lines(&lines);
        let pairs: Vec<(usize, usize)> = groups
            .iter()
            .map(|g| match g {
                HunkLineGroup::Pair {
                    old_line, new_line, ..
                } => (*old_line, *new_line),
                other => panic!("expected a pair, got {other:?}"),
            })
            .collect();
        // a1↔b1, a2↔b2 — the TS asserted those texts; these are their indices.
        assert_eq!(pairs, vec![(0, 2), (1, 3)]);
    }

    #[test]
    fn uneven_del_add_run_lengths_fall_back_to_plain_del_add() {
        let lines = hunk(&[('-', "a1"), ('-', "a2"), ('+', "b1")]);
        let groups = pair_hunk_lines(&lines);
        assert_eq!(
            groups,
            vec![
                HunkLineGroup::Del { line: 0 },
                HunkLineGroup::Del { line: 1 },
                HunkLineGroup::Add { line: 2 },
            ]
        );
    }

    #[test]
    fn a_pure_addition_with_no_preceding_del_renders_as_plain_add() {
        let lines = hunk(&[('+', "brand new")]);
        assert_eq!(
            pair_hunk_lines(&lines),
            vec![HunkLineGroup::Add { line: 0 }]
        );
    }

    #[test]
    fn a_pure_deletion_with_no_following_add_renders_as_plain_del() {
        let lines = hunk(&[('-', "gone")]);
        assert_eq!(
            pair_hunk_lines(&lines),
            vec![HunkLineGroup::Del { line: 0 }]
        );
    }

    #[test]
    fn mixed_context_pair_del_sequence_preserves_order() {
        let lines = hunk(&[(' ', "ctx"), ('-', "old"), ('+', "new"), (' ', "ctx2")]);
        let groups = pair_hunk_lines(&lines);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0], HunkLineGroup::Ctx { line: 0 });
        assert!(matches!(
            groups[1],
            HunkLineGroup::Pair {
                old_line: 1,
                new_line: 2,
                ..
            }
        ));
        assert_eq!(groups[2], HunkLineGroup::Ctx { line: 3 });
    }

    /// Not in the TS suite: an empty hunk body must not panic or invent a row.
    #[test]
    fn an_empty_hunk_body_produces_no_groups() {
        assert!(pair_hunk_lines(&[]).is_empty());
    }

    /// The wire shape the frontend types mirror — asserted so a rename or a
    /// serde attribute change breaks here rather than silently in the view.
    #[test]
    fn the_serialized_shape_is_what_the_view_reads() {
        let lines = hunk(&[(' ', "ctx"), ('-', "a b"), ('+', "a c")]);
        let json = serde_json::to_string(&pair_hunk_lines(&lines)).unwrap();
        assert_eq!(
            json,
            r#"[{"type":"ctx","line":0},{"type":"pair","old_line":1,"new_line":2,"left":[{"text":"a","kind":"same"},{"text":" ","kind":"same"},{"text":"b","kind":"del"}],"right":[{"text":"a","kind":"same"},{"text":" ","kind":"same"},{"text":"c","kind":"add"}]}]"#
        );
    }
}

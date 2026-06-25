//! Sentence-boundary segmenter for TTS content. Walks the input byte-by-byte
//! and splits on `.`, `?`, `!` followed by whitespace/EOS, plus `\n\n`.
//! Suppresses splits for decimals, common abbreviations, and ellipses.
//!
//! All decision points operate on ASCII bytes; multi-byte UTF-8 is preserved
//! verbatim because we only slice the source string at ASCII boundaries.

const ABBREVS: &[&str] = &[
    "Dr", "Mr", "Mrs", "Ms", "Jr", "Sr", "St", "Inc", "Ltd", "Co", "Corp",
    "vs", "etc", "Mt", "No",
    // Internal-dot abbreviations: when we hit the *second* dot of "e.g."
    // the preceding word (read alpha + dot back) is "e.g" — the list below
    // matches that form.
    "e.g", "i.e", "a.m", "p.m",
];

pub fn segment_sentences(text: &str) -> Vec<String> {
    let cleaned = sanitize_for_tts(text);
    let bytes = cleaned.as_bytes();
    let text = cleaned.as_str();
    let n = bytes.len();
    let mut sentences: Vec<String> = Vec::new();
    let mut start: usize = 0;
    let mut i: usize = 0;

    while i < n {
        let b = bytes[i];

        // Paragraph break: \n\n
        if b == b'\n' && i + 1 < n && bytes[i + 1] == b'\n' {
            push_trim(&mut sentences, &text[start..i]);
            i += 2;
            start = i;
            continue;
        }

        if b == b'.' || b == b'?' || b == b'!' {
            if b == b'.' {
                // Ellipsis: a run of two-or-more dots is treated as a single
                // unit and *not* split on. Skip past the entire run.
                let prev_dot = i > 0 && bytes[i - 1] == b'.';
                let next_dot = i + 1 < n && bytes[i + 1] == b'.';
                if prev_dot || next_dot {
                    while i < n && bytes[i] == b'.' {
                        i += 1;
                    }
                    continue;
                }
                // Decimal: digit.digit
                let prev_digit = i > 0 && bytes[i - 1].is_ascii_digit();
                let next_digit = i + 1 < n && bytes[i + 1].is_ascii_digit();
                if prev_digit && next_digit {
                    i += 1;
                    continue;
                }
                // Abbreviation: preceding word matches the list.
                if is_abbreviation(text, i) {
                    i += 1;
                    continue;
                }
            }
            // Split if followed by whitespace or end-of-string.
            let end = i + 1;
            let next_is_break = end >= n || bytes[end].is_ascii_whitespace();
            if next_is_break {
                push_trim(&mut sentences, &text[start..end]);
                i = end;
                start = i;
                continue;
            }
        }

        i += 1;
    }

    if start < n {
        push_trim(&mut sentences, &text[start..]);
    }

    sentences
}

/// Byte index in `text` just past the end of the last *terminal* sentence
/// boundary — the final `.?!` that is followed by whitespace/EOS and is NOT an
/// ellipsis, decimal point, or abbreviation dot — including any trailing
/// whitespace. Returns 0 when there is no complete sentence.
///
/// Unlike [`segment_sentences`] this does NOT run `sanitize_for_tts`, so the
/// returned index is a valid slice point in the caller's *original* buffer
/// (sanitize can change byte lengths). It exists so the "how much to flush"
/// decision applies the same abbreviation/decimal/ellipsis suppression as the
/// actual segmentation — otherwise a flush could cut "See Dr. Smith now." right
/// after "Dr.", speaking it as two fragments.
pub fn last_boundary(text: &str) -> usize {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut split = 0;
    let mut i = 0;
    while i < n {
        let b = bytes[i];
        if b == b'.' || b == b'?' || b == b'!' {
            if b == b'.' {
                let prev_dot = i > 0 && bytes[i - 1] == b'.';
                let next_dot = i + 1 < n && bytes[i + 1] == b'.';
                if prev_dot || next_dot {
                    while i < n && bytes[i] == b'.' {
                        i += 1;
                    }
                    continue;
                }
                let prev_digit = i > 0 && bytes[i - 1].is_ascii_digit();
                let next_digit = i + 1 < n && bytes[i + 1].is_ascii_digit();
                if prev_digit && next_digit {
                    i += 1;
                    continue;
                }
                if is_abbreviation(text, i) {
                    i += 1;
                    continue;
                }
            }
            let end = i + 1;
            if end >= n || bytes[end].is_ascii_whitespace() {
                let mut j = end;
                while j < n && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                split = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    split
}

fn push_trim(sentences: &mut Vec<String>, s: &str) {
    let trimmed = s.trim();
    if !trimmed.is_empty() {
        sentences.push(trimmed.to_string());
    }
}

/// Strip characters that confuse the phonemizer's word tokenizer. Claude
/// Code's TUI lays out responses with cursor-skip sequences (`\x1b[1C`) and
/// our cell model can carry stale spinner/status chars (box-drawing,
/// braille, arrows) in those skipped positions. Those chars get pulled into
/// the rendered TTS content and run two adjacent words together as one
/// unfamiliar token, which misaki-rs then falls back to spelling letter by
/// letter. We replace anything that's not a letter, digit, ASCII
/// punctuation, or normal whitespace with a single space — leaving real
/// word boundaries intact.
fn sanitize_for_tts(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_ascii_punctuation() || c == ' ' || c == '\n' || c == '\t' {
                c
            } else {
                ' '
            }
        })
        .collect()
}

/// The `.` at byte index `dot_idx` is preceded by a word; check whether that
/// word is in the abbreviation list. Words are alpha runs that may contain
/// internal dots (so "e.g" matches when we're sitting on the second dot of
/// "e.g.").
fn is_abbreviation(text: &str, dot_idx: usize) -> bool {
    let bytes = text.as_bytes();
    let mut start = dot_idx;
    while start > 0 {
        let b = bytes[start - 1];
        if b.is_ascii_alphabetic() || b == b'.' {
            start -= 1;
        } else {
            break;
        }
    }
    if start == dot_idx {
        return false;
    }
    let word = &text[start..dot_idx];
    ABBREVS.iter().any(|a| word.eq_ignore_ascii_case(a))
}

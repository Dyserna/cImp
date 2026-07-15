//! V20: reduce assistant markdown to speakable prose.
//!
//! Out-of-band sources hand us the assistant's raw message text, which is
//! markdown: fenced code blocks, inline code, headings, list markers, links,
//! emphasis. The old `[[TTS]]` convention let the model mark exactly what to
//! speak; without it we speak all prose but must drop the parts that are noise
//! aloud — chiefly fenced code blocks (a screenful of code read character by
//! character is useless) and the markup punctuation.
//!
//! This is intentionally lightweight (line + small inline passes), not a full
//! markdown parser: the downstream [`crate::processing::segment_sentences`]
//! already sanitizes stray symbols, so we only need to remove whole code
//! blocks and the loudest inline markup.

/// Convert assistant markdown to a plain-prose string suitable for TTS.
/// Fenced code blocks are removed entirely; inline markup is unwrapped to its
/// text. Returns possibly-multiline prose; the segmenter splits it.
///
/// Expects one *complete* message text — fence state does not persist across
/// calls, so feeding incremental chunks would mis-handle fences that span
/// chunk boundaries. An unclosed fence swallows the rest of the message
/// (CommonMark: it runs to end of input).
pub fn to_speakable(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    // Open fence: (marker char, opening run length).
    let mut fence: Option<(char, usize)> = None;

    for line in md.lines() {
        let trimmed = line.trim_start();
        if let Some((ch, len)) = fence_marker(trimmed) {
            match fence {
                Some((open_ch, open_len)) => {
                    // A closing fence is the same char, at least as long, and
                    // has nothing else on the line (CommonMark). Anything else
                    // — e.g. a ```python line inside a ````-fenced example —
                    // is code-block content and stays skipped.
                    if ch == open_ch && len >= open_len && trimmed[len..].trim().is_empty() {
                        fence = None;
                    }
                }
                None => fence = Some((ch, len)),
            }
            continue; // never speak the fence line itself
        }
        if fence.is_some() {
            continue; // body of a code block — skip
        }
        if trimmed.starts_with('|') {
            if let Some(row) = table_row_to_prose(trimmed) {
                out.push_str(&strip_inline(&row));
                out.push('\n');
            }
            continue; // separator rows (|---|---|) speak nothing
        }
        out.push_str(&strip_inline(line));
        out.push('\n');
    }

    out
}

/// If `line` (already left-trimmed) starts with a code-fence run (3+ backticks
/// or tildes), return the fence char and run length; else `None`.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let first = line.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let len = line.chars().take_while(|&c| c == first).count();
    (len >= 3).then_some((first, len))
}

/// Render a `| a | b |` table row as prose: cells joined with commas, closed
/// with a period so each row becomes its own sentence. Header-separator rows
/// (only pipes, dashes, colons) return `None`.
fn table_row_to_prose(line: &str) -> Option<String> {
    if line
        .chars()
        .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
    {
        return None;
    }
    let cells: Vec<&str> = line
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    let mut row = cells.join(", ");
    if !row.ends_with(['.', '!', '?']) {
        row.push('.');
    }
    Some(row)
}

/// Strip the loudest inline markdown from one line: drop inline-code backticks
/// (keeping the text), unwrap `[text](url)` to `text`, drop heading/quote/list
/// lead markers and emphasis runs. Leaves words intact for the segmenter.
fn strip_inline(line: &str) -> String {
    // Drop a leading block marker: heading (#), blockquote (>), list bullet
    // (-, *, +) or an ordered-list "N." prefix.
    let mut s = line.trim_end().to_string();
    let lead = s.trim_start();
    let lead_len = s.len() - lead.len();
    let indent = &s[..lead_len];
    let body = strip_lead_marker(lead);
    s = format!("{indent}{body}");

    // Unwrap links: [text](url) -> text. Cheap single pass.
    s = unwrap_links(&s);

    // GFM strikethrough: ~~gone~~ -> gone. A lone ~ ("~5 minutes") survives.
    s = s.replace("~~", "");

    // Remove inline-code backticks and `*` emphasis markers, keeping inner
    // text. `_` is dropped only when it's an emphasis delimiter (adjacent to a
    // non-alphanumeric boundary), so intra-word underscores in identifiers like
    // `do_thing` survive.
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '`' | '*' => continue,
            '_' => {
                let prev_alnum = i
                    .checked_sub(1)
                    .and_then(|j| chars.get(j))
                    .is_some_and(|p| p.is_alphanumeric());
                let next_alnum = chars.get(i + 1).is_some_and(|n| n.is_alphanumeric());
                if prev_alnum && next_alnum {
                    out.push('_'); // intra-word underscore — keep
                }
                // else: emphasis delimiter — drop
            }
            _ => out.push(c),
        }
    }
    out
}

/// Remove a single leading markdown block marker from a left-trimmed line.
fn strip_lead_marker(lead: &str) -> &str {
    // ATX heading: one or more '#', then a space (or nothing). "#hashtag" and
    // "#1 priority" are prose, not headings.
    if lead.starts_with('#') {
        let rest = lead.trim_start_matches('#');
        if rest.is_empty() || rest.starts_with(' ') {
            return rest.trim_start();
        }
        return lead;
    }
    // Blockquote, including nested ("> > deep").
    if lead.starts_with('>') {
        return lead.trim_start_matches(['>', ' ']);
    }
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = lead.strip_prefix(bullet) {
            return rest;
        }
    }
    // Ordered list "12. " -> rest.
    let digits: String = lead.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let after = &lead[digits.len()..];
        if let Some(rest) = after.strip_prefix(". ") {
            return rest;
        }
    }
    lead
}

/// Replace `[text](url)` and `![alt](url)` with the text/alt. Bare `[text]`
/// without a paren group is left as-is (the stray brackets are skipped later
/// at the phonemizer's vocab lookup, so they're inaudible).
fn unwrap_links(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        // `![alt](url)` — parse from the `[`; the `!` is dropped with the url.
        let bracket = if bytes[i] == b'[' {
            Some(i)
        } else if bytes[i] == b'!' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            Some(i + 1)
        } else {
            None
        };
        if let Some(b_idx) = bracket {
            if let Some(close) = s[b_idx + 1..].find(']') {
                let text_start = b_idx + 1;
                let text_end = b_idx + 1 + close;
                let after = text_end + 1;
                if after < bytes.len() && bytes[after] == b'(' {
                    if let Some(paren) = s[after + 1..].find(')') {
                        out.push_str(&s[text_start..text_end]);
                        i = after + 1 + paren + 1;
                        continue;
                    }
                }
            }
        }
        // Push one UTF-8 char starting at i.
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_fenced_code_blocks() {
        let md = "Here is the fix.\n```rust\nfn main() {}\n```\nThat should work.";
        let out = to_speakable(md);
        assert!(out.contains("Here is the fix."));
        assert!(out.contains("That should work."));
        assert!(!out.contains("fn main"));
        assert!(!out.contains("```"));
    }

    #[test]
    fn drops_tilde_fences_and_nested_backticks() {
        let md = "intro\n~~~\ncode ``` still code\n~~~\nouttro";
        let out = to_speakable(md);
        assert!(out.contains("intro"));
        assert!(out.contains("outtro"));
        assert!(!out.contains("still code"));
    }

    #[test]
    fn unwraps_inline_markup() {
        let md = "Call `do_thing()` then see [the docs](http://x) now.";
        let out = to_speakable(md);
        assert!(out.contains("do_thing()"));
        assert!(out.contains("the docs"));
        assert!(!out.contains("http://x"));
        assert!(!out.contains('`'));
    }

    #[test]
    fn strips_lead_markers() {
        let md = "# Heading\n- a bullet\n3. ordered item\n> quoted";
        let out = to_speakable(md);
        assert!(out.contains("Heading"));
        assert!(out.contains("a bullet"));
        assert!(out.contains("ordered item"));
        assert!(out.contains("quoted"));
        assert!(!out.contains('#'));
        assert!(!out.trim_start().starts_with('-'));
    }

    #[test]
    fn strips_emphasis_keeping_words() {
        let out = to_speakable("This is **very** _important_ stuff.");
        assert!(out.contains("very"));
        assert!(out.contains("important"));
        assert!(!out.contains('*'));
        assert!(!out.contains('_'));
    }

    #[test]
    fn code_only_message_speaks_nothing() {
        let md = "```\nall code\nno prose\n```";
        assert!(to_speakable(md).trim().is_empty());
    }

    #[test]
    fn longer_fence_is_not_closed_by_shorter_run() {
        // A ````-fenced example *containing* a ``` block: the inner fences are
        // content, not closers — nothing inside may leak into speech.
        let md = "before\n````markdown\n```python\nprint(1)\n```\n````\nafter";
        let out = to_speakable(md);
        assert!(out.contains("before"));
        assert!(out.contains("after"));
        assert!(!out.contains("print"));
        assert!(!out.contains("python"));
    }

    #[test]
    fn fence_content_line_starting_with_marker_does_not_close() {
        // Closing fence must be the run alone on its line.
        let md = "```\n```echo not a close\nstill code\n```\nprose";
        let out = to_speakable(md);
        assert!(!out.contains("still code"));
        assert!(out.contains("prose"));
    }

    #[test]
    fn table_rows_become_comma_prose() {
        let md = "| Name | Age |\n| --- | --- |\n| Alice | 30 |";
        let out = to_speakable(md);
        assert!(out.contains("Name, Age."));
        assert!(out.contains("Alice, 30."));
        assert!(!out.contains('|'));
        assert!(!out.contains("---"));
    }

    #[test]
    fn strikethrough_is_unwrapped_but_lone_tilde_survives() {
        let out = to_speakable("This is ~~deprecated~~ in ~5 minutes.");
        assert!(out.contains("deprecated"));
        assert!(!out.contains("~~"));
        assert!(out.contains("~5 minutes"));
    }

    #[test]
    fn nested_blockquote_fully_stripped() {
        let out = to_speakable("> > deeply nested");
        assert_eq!(out.trim(), "deeply nested");
    }

    #[test]
    fn image_drops_the_bang() {
        let out = to_speakable("See ![diagram](http://x) below.");
        assert!(out.contains("See diagram below."));
        assert!(!out.contains('!'));
    }

    #[test]
    fn hashtag_word_is_not_a_heading() {
        let out = to_speakable("#hashtag stays\n#1 priority");
        assert!(out.contains("#hashtag stays"));
        assert!(out.contains("#1 priority"));
    }
}

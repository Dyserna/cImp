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
pub fn to_speakable(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut in_fence = false;
    let mut fence_marker: &str = "";

    for line in md.lines() {
        let trimmed = line.trim_start();
        // Fenced code blocks open/close on ``` or ~~~ (3+). Track the marker so
        // a ``` inside a ~~~ block doesn't prematurely close it.
        if let Some(marker) = fence_open_marker(trimmed) {
            if in_fence {
                if trimmed.starts_with(fence_marker) {
                    in_fence = false;
                    fence_marker = "";
                }
            } else {
                in_fence = true;
                fence_marker = marker;
            }
            continue; // never speak the fence line itself
        }
        if in_fence {
            continue; // body of a code block — skip
        }
        out.push_str(&strip_inline(line));
        out.push('\n');
    }

    out
}

/// If `line` (already left-trimmed) opens or closes a code fence, return the
/// fence marker (\"```\" or \"~~~\"); else `None`.
fn fence_open_marker(line: &str) -> Option<&'static str> {
    if line.starts_with("```") {
        Some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
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
    // ATX heading: one or more '#', then a space.
    if let Some(rest) = lead.strip_prefix('#') {
        let rest = rest.trim_start_matches('#');
        return rest.trim_start();
    }
    if let Some(rest) = lead.strip_prefix('>') {
        return rest.trim_start();
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

/// Replace `[text](url)` with `text`. Bare `[text]` without a paren group is
/// left as-is (the brackets are dropped by the segmenter's sanitizer).
fn unwrap_links(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(close) = s[i + 1..].find(']') {
                let text_start = i + 1;
                let text_end = i + 1 + close;
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
}

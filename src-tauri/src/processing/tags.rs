//! V20: dedup-key helper.
//!
//! Before V20 this module held the `[[TTS]]` tag scanner that extracted
//! speakable text from the raw PTY byte stream. With AI tabs running fullscreen
//! and TTS sourced out-of-band (`crate::oob`), that scanner is gone. The only
//! survivor is [`normalize_for_dedup`], still used by the input-echo bookkeeping
//! in `ipc::commands`.

/// Collapse runs of whitespace (spaces, tabs, newlines) in `s` into a single
/// ASCII space and trim. Two strings that differ only in line-wrap whitespace
/// produce the same key.
pub fn normalize_for_dedup(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_whitespace_runs() {
        assert_eq!(normalize_for_dedup("  hello   world \n"), "hello world");
        assert_eq!(
            normalize_for_dedup("a\tb\nc  d"),
            "a b c d"
        );
    }

    #[test]
    fn wrap_whitespace_collapses_to_same_key() {
        assert_eq!(
            normalize_for_dedup("hello world this is"),
            normalize_for_dedup("hello world\nthis is"),
        );
    }
}

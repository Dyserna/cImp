//! Reading this crate's own Rust source **as text**, safely enough to gate a
//! test on.
//!
//! Several tests in this crate are source scanners: [`crate::spawn_ledger`]'s
//! exhaustiveness check (every external-process spawn has a ledger row), the
//! two in [`crate::harness::layering`] (no harness-owned literal outside
//! `harness/`, no harness module importing a capability), and — since the V42
//! review (RV-9) — `offload::loopback`'s `files_containing`. They need the
//! same primitive and the same guarantee, so it lives here once:
//!
//!  1. comments, strings and char literals are blanked before any needle is
//!     looked for, and the blanking is self-checked ([`code_of`] must leave no
//!     `"` behind — a desynced lexer always does). [`uncommented`] is the same
//!     lexer with the literal blanking off, for the scans whose needle IS a
//!     literal;
//!  2. blanking preserves BYTE OFFSETS, so a span found in the blanked text is
//!     a valid span in the original. That is what lets a caller locate
//!     `#[cfg(test)]` items in code and then act on the *unblanked* source, and
//!     it is why [`test_regions`] returns ranges rather than a string.
//!
//! **Why this is a shared module and not a copy per caller.** The bug that
//! created it: `harness::layering` had its own hand-rolled boundary finder
//! (`text.match_indices("\n#[cfg(test)]\n").last()`) which was both
//! line-ending-sensitive and wrong for any file with more than one
//! `#[cfg(test)]` item. It passed on a developer's LF working copy and failed
//! on a CRLF CI checkout of byte-identical content, and it silently truncated
//! `processing/mod.rs` at its `#[cfg(test)] mod tests;` declaration — hiding
//! ~500 lines of production code from a test whose whole job was to read them.
//! A verification test that reads a different slice depending on how Git
//! checked the file out is the vacuous-canary class V35 exists to kill, so the
//! answer is one audited implementation with controls
//! (`spawn_ledger`'s `the_scanner_finds_what_it_claims_to_find`), not three.

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Where a char literal starting at `i` ends, or `None` when the quote is a
/// lifetime/label (`&'a str`, `'outer: loop`) rather than a literal.
fn char_lit_end(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i + 1) == Some(&b'\\') {
        let mut j = i + 3;
        match b.get(i + 2) {
            Some(&b'u') => {
                j = i + 4;
                while j < b.len() && b[j] != b'}' {
                    j += 1;
                }
                j += 1;
            }
            Some(&b'x') => j = i + 5,
            _ => {}
        }
        return if b.get(j) == Some(&b'\'') {
            Some(j)
        } else {
            None
        };
    }
    let start = i + 1;
    let end = start + utf8_len(*b.get(start)?);
    if b.get(end) == Some(&b'\'') {
        Some(end)
    } else {
        None
    }
}

/// If a raw string starts at `i` (`r"`, `r#"`, `br##"` …), the index of its
/// opening quote and the hash count. Requires a token boundary before `i`
/// so the `r` inside an identifier is not mistaken for a prefix.
fn raw_string_start(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
        return None;
    }
    let mut j = i;
    if b.get(j) == Some(&b'b') {
        j += 1;
    }
    if b.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let hash_start = j;
    while b.get(j) == Some(&b'#') {
        j += 1;
    }
    if b.get(j) != Some(&b'"') {
        return None;
    }
    Some((j, j - hash_start))
}

/// What a blanking pass erases.
///
/// Both settings run the SAME lexer over the source; they differ only in which
/// of the spans it identifies get replaced by spaces. That matters: a `//`
/// inside a string literal (`"http://x"`) is not a comment, and only a lexer
/// that tracks strings knows it — so [`Erase::CommentsOnly`] is not "delete
/// everything after `//`", it is the strong pass with the literal blanking
/// switched off.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Erase {
    /// Comments, string literals and char literals. The scanner's default —
    /// what [`code_of`] hands out, and the only mode with a self-check strong
    /// enough to prove the lexer stayed in sync.
    LiteralsAndComments,
    /// Comments only; string and char literals survive verbatim.
    ///
    /// For scans whose needle IS a literal — a `"x-cimp-tab" =>` match arm, a
    /// route path in the dispatch — where blanking strings would make the
    /// needle unfindable and the scan silently vacuous. See [`uncommented`].
    CommentsOnly,
}

/// Replace every byte inside a comment (and, under
/// [`Erase::LiteralsAndComments`], every string or char literal) with a
/// space, keeping newlines and byte offsets intact so line numbers and
/// `#[cfg(test)]` spans still line up.
///
/// Self-checked by the caller: valid Rust code, once its literals are
/// blanked, contains no `"` at all. Any survivor means the lexer lost sync
/// and the scan below cannot be trusted.
fn blank_out(src: &str, erase: Erase) -> String {
    let b = src.as_bytes();
    let n = b.len();
    let mut out = b.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for byte in out.iter_mut().take(to.min(n)).skip(from) {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    // Literal spans are always SKIPPED (that is what keeps a `//` inside a
    // string from being read as a comment); whether they are also erased is
    // the only thing `erase` decides.
    let erase_literals = erase == Erase::LiteralsAndComments;
    let mut i = 0usize;
    while i < n {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            let start = i;
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            blank(&mut out, start, i);
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            let start = i;
            let mut depth = 1usize;
            i += 2;
            while i < n && depth > 0 {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            blank(&mut out, start, i);
        } else if let Some((quote, hashes)) = raw_string_start(b, i) {
            let start = i;
            let mut j = quote + 1;
            loop {
                if j >= n {
                    break;
                }
                if b[j] == b'"' {
                    let mut k = j + 1;
                    let mut seen = 0usize;
                    while seen < hashes && b.get(k) == Some(&b'#') {
                        k += 1;
                        seen += 1;
                    }
                    if seen == hashes {
                        j = k;
                        break;
                    }
                }
                j += 1;
            }
            i = j;
            if erase_literals {
                blank(&mut out, start, i);
            }
        } else if b[i] == b'"' {
            let start = i;
            let mut j = i + 1;
            while j < n {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == b'"' {
                    j += 1;
                    break;
                }
                j += 1;
            }
            i = j;
            if erase_literals {
                blank(&mut out, start, i);
            }
        } else if b[i] == b'\'' {
            match char_lit_end(b, i) {
                Some(end) => {
                    if erase_literals {
                        blank(&mut out, i, end + 1);
                    }
                    i = end + 1;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).expect("blanking only ever writes ASCII spaces")
}

/// Blanking plus its self-check, so every caller gets the guarantee.
pub(crate) fn code_of(rel: &str, src: &str) -> String {
    let code = blank_out(src, Erase::LiteralsAndComments);
    assert_eq!(
        code.len(),
        src.len(),
        "{rel}: blanking changed the byte length — offsets would be wrong"
    );
    assert!(
        !code.contains('"'),
        "{rel}: a double quote survived literal-blanking, so the scanner's lexer lost \
         sync and its needle counts cannot be trusted. Fix `blank_out` before trusting \
         this ledger."
    );
    code
}

/// `src` with its **comments** blanked and its string/char literals intact.
///
/// V42 review, RV-9. `offload::loopback`'s `files_containing` searched raw
/// source, so a doc comment naming a function or a header could satisfy a scan
/// whose whole point was that the CODE does it. [`code_of`] is the obvious
/// answer and the wrong one for those call sites: two of them look for a
/// literal — `"x-cimp-tab" =>`, a match arm on a header name — which the
/// strong pass erases, turning "the header is read" into "the needle is never
/// found" and the assertion into a silent pass.
///
/// So: comments out, literals in. What that buys is that prose can no longer
/// satisfy a scan. What it deliberately does not buy is immunity to a needle
/// planted inside a string; where that matters, use [`code_of`].
///
/// The lexer's self-check still runs — via [`code_of`] on the same input,
/// whose result is discarded. That check ("no `\"` survives the strong pass")
/// is the audited proof that the lexer stayed in sync, and it is the same
/// lexer producing both answers, so it covers this one too. The comments-only
/// output has no comparable self-check of its own: `//` legitimately survives
/// inside a string, which is exactly what makes it unfalsifiable here.
pub(crate) fn uncommented(rel: &str, src: &str) -> String {
    let _ = code_of(rel, src);
    let text = blank_out(src, Erase::CommentsOnly);
    assert_eq!(
        text.len(),
        src.len(),
        "{rel}: comment-blanking changed the byte length — offsets would be wrong"
    );
    text
}

/// Does a `cfg(...)` predicate select a TEST build? Broadened past the bare
/// `cfg(test)` to the `all(...)`/`any(...)` combinator forms, the same
/// broadening `graph::builder` documents for its own test detection —
/// while excluding `not(test)`, which `offload/outbound.rs` really uses and
/// which means the exact opposite.
fn selects_test(pred: &str) -> bool {
    let compact: String = pred.chars().filter(|c| !c.is_whitespace()).collect();
    let compact = compact.replace("not(test)", "");
    compact
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|t| t == "test")
}

/// End of the braced block that starts at `open`, one past its `}`.
fn skip_block(b: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// End of the item that follows an attribute ending at `from`. An item is
/// either brace-bodied (`mod`, `fn`, `impl`, `struct`) or `;`-terminated
/// (`use`, `const`). Parens/brackets are tracked so the `;` in `[u8; 4]`
/// does not end the item early.
fn item_end(b: &[u8], from: usize) -> usize {
    let mut i = from;
    let (mut paren, mut brack) = (0i32, 0i32);
    while i < b.len() {
        match b[i] {
            b'(' => paren += 1,
            b')' => paren -= 1,
            b'[' => brack += 1,
            b']' => brack -= 1,
            b';' if paren == 0 && brack == 0 => return i + 1,
            b'{' if paren == 0 && brack == 0 => return skip_block(b, i),
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// Byte ranges of every `#[cfg(<test-selecting>)]` item in blanked code.
pub(crate) fn test_regions(code: &str) -> Vec<(usize, usize)> {
    let b = code.as_bytes();
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = code[search..].find("#[cfg(") {
        let at = search + rel;
        // Balanced-paren scan over the predicate.
        let mut i = at + "#[cfg".len();
        let mut depth = 0i32;
        let start = i + 1;
        while i < b.len() {
            match b[i] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        let pred = &code[start.min(b.len())..i.min(b.len())];
        // Past the predicate's `)` and the attribute's `]`.
        let mut after = i + 1;
        while after < b.len() && b[after] != b']' {
            after += 1;
        }
        after += 1;
        if selects_test(pred) {
            let end = item_end(b, after);
            out.push((at, end));
        }
        search = after.min(b.len()).max(at + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The controls for [`uncommented`] (V42 review, RV-9). Each input is a
    /// shape a source scan can be fooled by, so a future edit to `blank_out`'s
    /// `erase_literals` branches fails here rather than in a security
    /// assertion that quietly stops matching anything.
    #[test]
    fn uncommented_erases_prose_and_keeps_literals() {
        // 1. A line comment that names a function is not a declaration.
        let src = "// fn hook_exec_roots(app: &AppHandle)
fn other() {}
";
        let text = uncommented("probe.rs", src);
        assert!(!text.contains("hook_exec_roots"), "a line comment survived: {text}");
        assert!(text.contains("fn other() {}"), "code was erased: {text}");

        // 2. Same for a block comment and a doc comment.
        let src = "/* fn planted() */
/// fn documented()
fn real() {}
";
        let text = uncommented("probe.rs", src);
        assert!(!text.contains("planted"), "a block comment survived: {text}");
        assert!(!text.contains("documented"), "a doc comment survived: {text}");
        assert!(text.contains("fn real() {}"));

        // 3. String and char literals are KEPT - the reason this exists rather
        //    than `code_of`: two of the loopback scans look for a match arm on
        //    a header name, which is a literal.
        let src = "match h { \"x-cimp-tab\" => 1, _ => 0 };
let c = 'x';
";
        let text = uncommented("probe.rs", src);
        assert!(text.contains("\"x-cimp-tab\" =>"), "the literal was erased: {text}");
        assert!(text.contains("'x'"));

        // 4. A `//` INSIDE a string is not a comment. This is what makes the
        //    mode a lexer setting rather than a regex over `//`.
        let src = "let u = \"http://example.invalid/keep-me\";
";
        let text = uncommented("probe.rs", src);
        assert!(text.contains("keep-me"), "a URL was read as a comment: {text}");

        // 5. Byte offsets are preserved in both modes, so a span found in one
        //    is a valid span in the original - and `code_of` still blanks the
        //    literals this mode keeps.
        let src = "// gone
fn f() { let s = \"kept\"; }
";
        assert_eq!(uncommented("probe.rs", src).len(), src.len());
        let strong = code_of("probe.rs", src);
        assert_eq!(strong.len(), src.len());
        assert!(!strong.contains("kept"), "code_of must still blank strings: {strong}");
    }
}

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
//! Since R11 the *walk* lives here too — [`src_root`] and [`source_files`], the
//! one audited answer to "every `.rs` file in this crate". Same reason, same
//! kind of incident: five copies of it disagreed about CR-stripping,
//! dot-directory skipping and the vacuity floor, and the details are on
//! [`source_files`].
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

use std::path::{Path, PathBuf};

/// The floor under [`source_files`]'s answer.
///
/// Every scanner built on this walk shares one failure mode: a walk that
/// returns nothing finds no offender and reports `ok` while doing it. Three of
/// the five copies R11 replaced asserted a floor of their own, two did not, and
/// the two that did not are exactly the two whose scans are tree-wide security
/// claims — so the floor lives here, once, where no caller can forget it. It is
/// a vacuity floor, not a census: `src/` holds ~250 `.rs` files, and any answer
/// under half of that is a broken walk rather than a smaller tree.
const MIN_SOURCE_FILES: usize = 100;

/// `<repo>/src-tauri/src`.
///
/// Resolved from `CARGO_MANIFEST_DIR` rather than the process cwd, so every
/// scanner answers the same from any working directory (verified by running the
/// suite from `C:\`), and asserted to exist: a scan whose root is missing has to
/// say so rather than quietly walk nothing.
pub(crate) fn src_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    assert!(
        root.is_dir(),
        "the source tree is missing at {} — a source scan cannot run",
        root.display()
    );
    root
}

/// Every `.rs` file under [`src_root`], as `(slash-relative path, contents)` —
/// sorted, `\r`-normalised, dot-directories skipped, guarded against vacuity.
///
/// Walked ONCE per process and borrowed thereafter — see [`RS_SOURCE_FILES`].
/// [`source_files_ext`] below is deliberately NOT memoized: a bespoke extension
/// set has one caller, and the walk it does is that caller's alone.
///
/// # Why this is one function and not five (R11)
///
/// Five copies of this walk existed — in `harness::layering`, `spawn_gate`,
/// `spawn_ledger`, `graph::index::notes` and `settings` — and they disagreed on
/// exactly the three axes that decide what a *security* scan gets to see. The
/// union is taken in the strict direction on every one, so no scanner reads
/// fewer files, fewer bytes, or a different slice than it did before:
///
///  1. **`\r`.** `spawn_gate` stripped it, `layering` did not. That is the same
///     fault the module docs above record from the other end — identical bytes,
///     green on an LF working copy and red on a CRLF CI checkout — and the only
///     place it can be fixed once is at the READ, before any caller takes an
///     offset into the text. So this hands out LF, and no caller has to
///     remember to.
///  2. **Dot-directories.** `spawn_ledger` and `settings` skipped them; the
///     other three walked in. A `.cimp/` under the crate holds a CozoDB graph
///     database and a shadow git worktree — a whole second checkout of this
///     tree, `.rs` files and all — so descending into it scans a stale copy of
///     the crate and reports hits at paths nobody edits. (It is prophylactic
///     today: the tree has no dot-directory under `src/` right now, and the
///     point is that the day one appears no scan changes meaning.)
///  3. **The vacuity floor.** See [`MIN_SOURCE_FILES`].
///
/// Sorting is the fourth, minor one: four of the five sorted and `spawn_ledger`
/// did not, which made its failure messages depend on directory order.
pub(crate) fn source_files() -> &'static [(String, String)] {
    &RS_SOURCE_FILES
}

/// The memo behind [`source_files`].
///
/// The `.rs` answer is IDENTICAL for every caller — same root, same extension
/// set, same process — and roughly fifteen scanners ask for it in one `cargo
/// test` run, each paying ~250 file reads plus a sort for a tree that cannot
/// have changed since the binary was built. Computed once, borrowed after.
///
/// One consequence worth knowing: [`MIN_SOURCE_FILES`]'s vacuity assert now
/// fires inside this initializer, so a broken walk panics with its own message
/// in whichever test touches it FIRST, and every later test reports a poisoned
/// `LazyLock` instead. The diagnosis is in the first failure; the rest are
/// echoes of it.
static RS_SOURCE_FILES: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| source_files_ext(&["rs"]));

/// The files that exist only in a TEST build, derived from `main.rs`.
///
/// `main.rs` declares two of its modules under `#[cfg(test)]`
/// (`rustsrc` itself, and `testutil`). Their contents are not in the shipped
/// binary at all — but they ARE `.rs` files under `src/` with no inner
/// `#[cfg(test)]` marker, so a scanner that defines "production code" as
/// "everything before the first `#[cfg(test)]` item" reads the whole of each
/// one as production. That is a false positive with teeth: the spawn-gate and
/// spawn-ledger tripwires would demand a security ledger row for a test
/// fixture's `git`, and the pressure that puts on the next author is to weaken
/// a scanner rather than to be honest.
///
/// Parsed from the declaration rather than hand-kept, so a module that stops
/// being test-only stops being exempt on the same commit. A `mod x;` matches
/// `x.rs` and everything under `x/`.
///
/// Vacuity is guarded from both ends: the set must be non-empty (main.rs has
/// always had at least one such module, and a parser that silently found none
/// would exempt nothing while looking like it worked) and every name in it
/// must match at least one walked file.
pub(crate) fn test_only_files() -> std::collections::BTreeSet<String> {
    let main = std::fs::read_to_string(src_root().join("main.rs"))
        .expect("main.rs is readable — the module declarations are the source of this answer");
    let main = main.replace('\r', "");
    let mut mods: Vec<String> = Vec::new();
    let mut armed = false;
    for line in main.lines() {
        let line = line.trim();
        if line == "#[cfg(test)]" {
            armed = true;
            continue;
        }
        if armed {
            if let Some(name) = line.strip_prefix("mod ").and_then(|r| r.strip_suffix(';')) {
                mods.push(name.to_string());
            }
            armed = false;
        }
    }
    assert!(
        !mods.is_empty(),
        "no `#[cfg(test)] mod` found in main.rs — the parser has drifted from the file, and a \
         scanner exemption that silently covers nothing is worse than no exemption"
    );
    let files = source_files();
    let mut out = std::collections::BTreeSet::new();
    for m in mods {
        let own = format!("{m}.rs");
        let dir = format!("{m}/");
        let mut hit = false;
        for (rel, _) in files {
            if rel == &own || rel.starts_with(&dir) {
                out.insert(rel.clone());
                hit = true;
            }
        }
        assert!(hit, "`#[cfg(test)] mod {m};` matches no file under src/");
    }
    out
}

/// [`source_files`] over a caller-chosen extension set.
///
/// `settings`' pointer scan reads `.css` alongside `.rs` — a settings-path
/// pointer is held to the same rule wherever it is written — and that is the
/// only reason this parameter exists.
pub(crate) fn source_files_ext(exts: &[&str]) -> Vec<(String, String)> {
    let root = src_root();
    let out = walk_tree(&root, exts);
    assert!(
        out.len() > MIN_SOURCE_FILES,
        "the source walk found only {} {exts:?} files under {} — a walk that reads nothing finds \
         no offender and passes, which is the one outcome a source scan may never have",
        out.len(),
        root.display()
    );
    out
}

/// The walk itself: an arbitrary root, no vacuity floor.
///
/// Split out so [`the_walk_skips_dot_dirs_normalises_line_endings_and_sorts`]
/// can point it at a synthetic tree whose right answer is written down, rather
/// than inferring the walker's behaviour from the crate it is walking.
fn walk_tree(root: &Path, exts: &[&str]) -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, exts: &[&str], out: &mut Vec<(String, String)>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with('.')
            {
                continue;
            }
            if path.is_dir() {
                walk(&path, root, exts, out);
            } else if path
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| exts.contains(&x))
            {
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("cannot read {} as UTF-8: {e}", path.display()));
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, text.replace('\r', "")));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, exts, &mut out);
    out.sort();
    out
}

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

/// **Production text**: `text` with every `#[cfg(test)]` item and every
/// whole-line comment removed, for the scans whose subject is what the crate
/// DOES rather than where in a file it does it. Used by
/// [`crate::harness::layering`]'s four tree-wide tests and by
/// `delegation`'s reserved-transition scan.
///
/// # Why this is a sibling of [`code_of`] and not a mode of it (R11)
///
/// The three look like they overlap, and merging them would be wrong.
/// [`code_of`] and [`uncommented`] BLANK: they replace spans with spaces, so
/// the result is byte-for-byte the same length as the input and a span found
/// in it is a valid span in the original — which is the entire reason
/// [`test_regions`] can return ranges rather than a string. This one DELETES:
/// it is shorter than its input, and every offset into it is wrong for the
/// original. Folding it into [`Erase`] would hand a caller a string whose
/// offsets mean something different depending on a flag, and the callers of
/// both families are source scanners that locate things by offset. Same
/// module, same lexer underneath, deliberately separate contract.
///
/// It also keeps string and char literals — its callers' needles ARE literals
/// (a harness-owned `"tool_name"`, a `transition::CANCELLED` path) — and drops
/// only whole comment LINES, not trailing ones. Where that distinction matters
/// and offsets must survive, [`uncommented`] is the answer instead.
///
/// Drop every `#[cfg(test)]` item and every comment line.
///
/// **Tests are deliberately out of scope.** A fixture that quotes a harness
/// payload is a *recorded input*, not a dependency on one — the Phase B canary
/// corpus is made of nothing else, and an assertion that Claude's overlay
/// carries a `statusLine` key has to spell `statusLine` to be an assertion at
/// all. What this scan is about is production code that *reads or writes* an
/// upstream name; that is the thing which must sit in `harness/` so a rename
/// upstream is a diff in one directory.
///
/// Comments go for the same reason: prose naming `rate_limits` is
/// documentation, and documentation that explains the seam is wanted
/// everywhere, not confined.
///
/// # Why this delegates instead of finding a boundary itself
///
/// It used to cut at `text.match_indices("\n#[cfg(test)]\n").last()`, and that
/// was wrong in two independent ways — both of which shipped, and one of which
/// only ever fired off this developer's machine:
///
///  1. **It was line-ending-sensitive.** `\r\n#[cfg(test)]\r\n` does not match,
///     so a CRLF checkout found no boundary at all and fell back to scanning the
///     WHOLE file, tests included. Every `.rs` file in this repo is LF *in the
///     index*, but `core.autocrlf` is on by default on Windows, so the CI
///     runner's checkout is CRLF while a working copy whose files were rewritten
///     in place is a mix. The v0.52.0-rc.1 Tests run is the record: byte-identical
///     content, `no_harness_literals_outside_harness` green on the Linux job and
///     red on the Windows job with 26 hits across four files, every one inside a
///     `mod tests`. A verification test whose coverage depends on how Git checked
///     the file out reports on the checkout, not on the code.
///  2. **`.last()` is not "the trailing test module".** `#[cfg(test)]` marks
///     test-only *items*, of which a file may have many: `graph/mcp.rs` has
///     eleven test modules, so the cut landed at the eleventh and left the first
///     ten (~1800 lines) inside the scan. Worse in the other direction, a
///     `#[cfg(test)] mod tests;` **declaration** is the last such item in its
///     file — so `processing/mod.rs` was cut at line 47 of ~500 and
///     `harness/mod.rs` at line 99, hiding the production code both tests exist
///     to read. Silent under-coverage, which is how a canary goes vacuous.
///
/// Neither is fixable by a smarter single cut: `offload/mcp.rs` has production
/// code (`proxy_graph_outcome`) *between* two test modules, so no one boundary
/// separates test from production text. What is needed is every
/// `#[cfg(test)]` item's span, brace-matched, with strings and comments blanked
/// first so a `"#[cfg(test)]"` inside a literal is not mistaken for one — which
/// is exactly what [`code_of`] and [`test_regions`] above already provide,
/// controls and all. So this normalizes line endings, asks for the spans, and removes
/// them.
///
/// What that deliberately still keeps in scope: `#[cfg(test)]`-gated *helpers*
/// are removed along with the modules (they are test-only either way), while a
/// plain `fn` used only by tests but not gated is production text and is
/// scanned. That is the right side to err on — the gate is the declaration.
pub(crate) fn executable_text(rel: &str, text: &str) -> String {
    // FIRST, before any offset is taken: Windows and Linux must scan
    // byte-identical bytes, so the local run is authoritative for CI.
    let norm = text.replace('\r', "");
    let code = code_of(rel, &norm);
    let mut kept = String::with_capacity(norm.len());
    let mut at = 0usize;
    // Sorted by start; a nested `#[cfg(test)]` inside a test module yields a
    // span already covered, hence the `max`.
    for (start, end) in test_regions(&code) {
        let (start, end) = (start.min(norm.len()), end.min(norm.len()));
        if start > at {
            kept.push_str(&norm[at..start]);
        }
        at = at.max(end);
    }
    kept.push_str(&norm[at..]);
    kept.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`executable_text`]'s own unit controls, on input whose answer is written
    /// down rather than inferred from the tree.
    ///
    /// The tree-wide tests in [`crate::harness::layering`] prove the property
    /// end to end; these name the two specific defects, so a future regression
    /// says which one came back. They moved here with the function (R11) — the
    /// primitive and its controls travel together.
    #[test]
    fn executable_text_ignores_line_endings_and_cuts_at_every_test_item() {
        // Defect 1: the same source, two line endings, one answer.
        let src = "fn prod() { let a = \"keep\"; }\n#[cfg(test)]\nmod tests {\n    let b = \"drop\";\n}\n";
        let lf = executable_text("f.rs", src);
        let crlf = executable_text("f.rs", &src.replace('\n', "\r\n"));
        assert_eq!(lf, crlf, "line endings must not change what is scanned");
        assert!(lf.contains("\"keep\""));
        assert!(!lf.contains("\"drop\""), "the test module must be dropped");

        // Defect 2a: a `#[cfg(test)] mod tests;` DECLARATION ends at its semicolon —
        // it must not swallow the production code that follows it, which is how
        // `processing/mod.rs` lost ~500 lines from the scan.
        let decl = "#[cfg(test)]\nmod tests;\n\nfn prod() { let a = \"keep\"; }\n";
        let body = executable_text("f.rs", decl);
        assert!(
            body.contains("\"keep\""),
            "a `#[cfg(test)] mod x;` declaration must not truncate the file: {body:?}"
        );

        // Defect 2b: EVERY test item goes, not just the last one, and production
        // code between two of them survives — `offload/mcp.rs`'s real shape.
        let many = "#[cfg(test)]\nmod a { let x = \"drop_a\"; }\nfn mid() { let m = \"keep_mid\"; }\n\
                    #[cfg(test)]\nmod b { let y = \"drop_b\"; }\n";
        let body = executable_text("f.rs", many);
        assert!(body.contains("\"keep_mid\""), "code between test modules is production");
        assert!(!body.contains("\"drop_a\""), "the FIRST test module must go too");
        assert!(!body.contains("\"drop_b\""));

        // A `#[cfg(test)]` spelt inside a string literal is not a test item.
        let quoted = "fn prod() { let s = \"#[cfg(test)]\\nmod t {\"; let a = \"keep\"; }\n";
        assert!(
            executable_text("f.rs", quoted).contains("\"keep\""),
            "a quoted `#[cfg(test)]` must not start a region"
        );
    }

    /// The walker's controls (R11), on a synthetic tree whose right answer is
    /// written down rather than read off the crate. Permanent, and one case per
    /// divergence that made five copies of this walk disagree — a future edit
    /// that reintroduces one fails here, rather than in a security assertion
    /// that quietly stops matching anything.
    #[test]
    fn the_walk_skips_dot_dirs_normalises_line_endings_and_sorts() {
        let root = std::env::temp_dir().join(format!("rustsrc-walk-{}", uuid::Uuid::new_v4()));
        let write = |rel: &str, body: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().expect("every fixture has a parent"))
                .expect("mkdir under temp");
            std::fs::write(&p, body).expect("write a fixture");
        };
        // A CRLF file and an LF file with byte-identical content modulo the
        // line ending — divergence 1.
        write("z.rs", "fn z() {}\r\nfn zz() {}\r\n");
        write("a/b.rs", "fn b() {}\n");
        write("a/skip.txt", "not source\n");
        write("theme.css", "a{}\n");
        // Divergence 2, in its real shape: `.cimp/` holds a shadow worktree,
        // i.e. a second checkout of the crate, `.rs` files and all.
        write(".cimp/shadow/src/z.rs", "fn stale() {}\n");

        let rs = walk_tree(&root, &["rs"]);
        let paths: Vec<&str> = rs.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            vec!["a/b.rs", "z.rs"],
            "the walk must be sorted, slash-separated, extension-filtered, and must not \
             descend into a dot-directory"
        );
        assert!(
            !rs.iter().any(|(_, t)| t.contains("stale")),
            "a `.cimp/` shadow worktree was scanned as if it were the crate"
        );
        assert_eq!(
            rs[1].1, "fn z() {}\nfn zz() {}\n",
            "`\\r` must be stripped at the read, before any caller takes an offset"
        );

        let both = walk_tree(&root, &["rs", "css"]);
        assert_eq!(
            both.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            vec!["a/b.rs", "theme.css", "z.rs"],
            "widening the extension set must only ever add files"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The test-only exemption is derived, and it is narrow.**
    ///
    /// Two scanners (`spawn_gate`, `spawn_ledger`) skip these files, so the
    /// failure that matters is the set growing: an exemption that covers the
    /// tree silences the tripwire it is attached to. Pinned from both ends —
    /// it names the modules `main.rs` actually declares under `#[cfg(test)]`,
    /// and it stays a rounding error against the walk.
    #[test]
    fn the_test_only_set_is_the_cfg_test_modules_and_nothing_else() {
        let set = test_only_files();
        assert!(
            set.contains("testutil.rs") && set.contains("rustsrc.rs"),
            "the two `#[cfg(test)] mod` declarations in main.rs are missing: {set:?}"
        );
        for shipped in ["main.rs", "spawn_gate.rs", "spawn_ledger.rs", "sandbox/confine.rs"] {
            assert!(
                !set.contains(shipped),
                "{shipped} ships in the binary and must stay policed"
            );
        }
        assert!(
            set.len() * 10 < source_files().len(),
            "the exemption covers {} of {} files — that is not an exemption any more",
            set.len(),
            source_files().len()
        );
    }

    /// …and the same walk over the REAL tree is substantive and normalised.
    ///
    /// The synthetic control above proves the semantics; this one proves they
    /// are the semantics the crate's scanners actually get, which is the half a
    /// fixture can never establish.
    #[test]
    fn the_crate_walk_is_substantive_and_normalised() {
        let files = source_files();
        assert!(
            files.len() > MIN_SOURCE_FILES,
            "only {} files — the floor in `source_files_ext` should have fired first",
            files.len()
        );
        assert!(
            files.iter().any(|(p, _)| p == "rustsrc.rs"),
            "the walk lost its own module, so it is not walking the crate root"
        );
        assert!(
            files.iter().all(|(_, t)| !t.contains('\r')),
            "a `\\r` reached a caller — on a CRLF checkout every offset-taking scan diverges"
        );
        assert!(
            files
                .iter()
                .all(|(p, _)| !p.contains('\\') && !p.starts_with('/')),
            "paths must be `src/`-relative with forward slashes: that is how the allowlists \
             in `harness::layering` and the ledger rows in `spawn_ledger` are written"
        );
        let mut sorted = files.to_vec();
        sorted.sort();
        assert_eq!(files, sorted, "the walk must be sorted");

        // ...and it is walked once. Fifteen scanners ask for this in one
        // `cargo test` run; the answer cannot change inside a process, so
        // they share one. Same backing allocation = the memo is live.
        assert!(
            std::ptr::eq(files.as_ptr(), source_files().as_ptr()),
            "the crate walk is re-reading ~250 files for every caller"
        );

        let widened = source_files_ext(&["rs", "css"]);
        assert!(
            widened.len() > files.len() && widened.iter().any(|(p, _)| p.ends_with(".css")),
            "widening the extension set found no extra file — `settings`' pointer scan reads \
             `.css` too, and would be silently reading less"
        );
    }

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

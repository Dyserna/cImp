//! Extraction: parse a source file with tree-sitter into the
//! language-independent [`FileGraph`] IR. This is the "code → graph" front
//! end; persisting the IR is the store's job (`index`/`schema`).
//!
//! The MVP implements **Rust** via a direct node-kind walk (definitions,
//! containment, call references, imports, doc-comments). Other languages are
//! parsed-but-empty here and gain `tags.scm`-driven extraction in Phase E.
//! The walk is deliberately resilient: a malformed file yields whatever
//! parsed, never a panic.

use tree_sitter::{Node, Parser};

use super::model::{
    doc_chunk_id, fnv1a_hex, symbol_doc_chunk_id, symbol_id, CodeChunk, DocChunk, Edge, EdgeKind,
    FileGraph, Lang, Reference, Symbol, SymbolKind, Visibility,
};

/// Max characters kept for a symbol's one-line signature.
const MAX_SIGNATURE: usize = 200;

/// Minimum span (inclusive line count) for a symbol to earn a semantic code
/// chunk (V11 Phase G) — a one- or two-line definition is already fully
/// captured by its `signature`, so chunking it would just duplicate that row.
const MIN_CODE_CHUNK_LINES: u32 = 3;

/// Max characters kept in a code chunk's embedded text (signature + doc +
/// body), mirroring [`MAX_SIGNATURE`]'s role for the one-line signature.
const MAX_CODE_CHUNK_CHARS: usize = 1024;

/// Parse one file's source into a [`FileGraph`]. `path` is the project-relative
/// path stored on every row; `lang` selects the grammar.
pub fn parse_file(path: &str, src: &str, lang: Lang) -> FileGraph {
    let mut fg = FileGraph {
        path: path.to_string(),
        lang_tag: lang.tag().to_string(),
        hash: content_hash(src),
        ..Default::default()
    };

    match lang {
        Lang::Rust => parse_rust(src, path, &mut fg),
        Lang::Markdown => parse_markdown(src, path, &mut fg),
        Lang::TypeScript | Lang::JavaScript => parse_js_ts(src, path, lang, &mut fg),
        Lang::Python => parse_python(src, path, &mut fg),
        // V9-02: code languages driven by the generic tags engine.
        Lang::Go
        | Lang::Java
        | Lang::C
        | Lang::Cpp
        | Lang::CSharp
        | Lang::Php
        | Lang::Bash
        | Lang::Scala
        | Lang::Ocaml
        | Lang::Ruby
        | Lang::Haskell
        | Lang::Kotlin
        | Lang::Swift
        | Lang::Sql
        | Lang::Erlang
        | Lang::R
        | Lang::Perl
        | Lang::Ada
        | Lang::Asm => {
            if let Some(spec) = crate::graph::tags::tag_spec(lang) {
                crate::graph::tags::parse_with_tags(src, path, &spec, &mut fg);
            }
        }
        // V9-02: markup/data languages are struct-search-only (registered in
        // `language_for`); no symbol/call extraction.
        Lang::Html | Lang::Css | Lang::Json | Lang::Yaml | Lang::Xml => {}
        Lang::Other => {}
    }

    fg
}

/// Chunk a Markdown file into `doc_chunk`s by heading section, so
/// `graph_search_docs` can surface project documentation (READMEs, design
/// docs) alongside code doc-comments. Each ATX heading (`#`..`######`) opens a
/// new section; its text runs until the next heading. Content before the first
/// heading becomes a chunk anchored to the file stem. Anchors are GitHub-style
/// heading slugs, de-duplicated with a numeric suffix.
fn parse_markdown(src: &str, file: &str, fg: &mut FileGraph) {
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    // Current section: (anchor, heading-title, accumulated lines).
    let mut anchor = file_stem(file);
    let mut title = String::new();
    let mut body: Vec<String> = Vec::new();
    // Which fence marker (if any) is currently open. A fence opened by ``` is
    // only closed by ```; a `~~~` line inside it is literal content, and vice
    // versa — tracking just a bool would let the other marker close it early.
    let mut fence: Option<&str> = None;

    let flush = |anchor: &str, title: &str, body: &[String], fg: &mut FileGraph,
                 seen: &mut std::collections::HashMap<String, u32>| {
        let text = compose_chunk(title, body);
        if text.trim().is_empty() {
            return;
        }
        let uniq = dedup_anchor(anchor, seen);
        let id = doc_chunk_id(file, &uniq);
        fg.docs.push(DocChunk {
            id,
            source_path: file.to_string(),
            anchor: uniq,
            text,
        });
    };

    for line in src.lines() {
        let trimmed = line.trim_start();
        // Track fenced code blocks so a `#` inside a fence isn't a heading.
        let marker = if trimmed.starts_with("```") {
            Some("```")
        } else if trimmed.starts_with("~~~") {
            Some("~~~")
        } else {
            None
        };
        if let Some(m) = marker {
            match fence {
                None => fence = Some(m),                 // open a fence
                Some(open) if open == m => fence = None, // matching close
                Some(_) => {}                            // other marker inside fence: literal
            }
            body.push(line.to_string());
            continue;
        }
        if fence.is_none() {
            if let Some(h) = heading_title(trimmed) {
                // Close the previous section, open the new one.
                flush(&anchor, &title, &body, fg, &mut seen);
                anchor = slug(&h);
                title = h;
                body.clear();
                continue;
            }
        }
        body.push(line.to_string());
    }
    flush(&anchor, &title, &body, fg, &mut seen);
}

/// The title of an ATX heading line (`## Foo` → `Foo`), or `None`.
fn heading_title(trimmed: &str) -> Option<String> {
    if !trimmed.starts_with('#') {
        return None;
    }
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    // A real heading needs a space after the hashes (`#foo` is not a heading).
    if !rest.starts_with(' ') && !rest.is_empty() {
        return None;
    }
    Some(rest.trim().trim_end_matches('#').trim().to_string())
}

/// Heading text + body, joined. The title leads so a search for the heading
/// term matches even when the body doesn't repeat it.
fn compose_chunk(title: &str, body: &[String]) -> String {
    let body_text = body.join("\n");
    if title.is_empty() {
        body_text.trim().to_string()
    } else if body_text.trim().is_empty() {
        title.to_string()
    } else {
        format!("{title}\n{}", body_text.trim_end())
    }
}

/// GitHub-style heading slug: lowercase, spaces → hyphens, drop other
/// punctuation. Empty slugs fall back to `section`.
fn slug(title: &str) -> String {
    let mut s = String::new();
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            s.extend(ch.to_lowercase());
        } else if ch == ' ' || ch == '-' || ch == '_' {
            s.push('-');
        }
        // else: dropped
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "section".to_string()
    } else {
        s
    }
}

/// Disambiguate repeated anchors within one file (`foo`, `foo-1`, `foo-2`).
///
/// `seen` doubles as the set of anchors already emitted: a generated suffix
/// (`foo-1`) must not collide with a heading literally named `foo-1`, so we keep
/// bumping the suffix until the candidate is one nobody has used. Presence in
/// `seen` means "don't reuse this exact anchor"; the stored value is just a
/// starting suffix hint for the base slug.
fn dedup_anchor(anchor: &str, seen: &mut std::collections::HashMap<String, u32>) -> String {
    let mut n = *seen.get(anchor).unwrap_or(&0);
    let mut candidate = anchor.to_string();
    while seen.contains_key(&candidate) {
        n += 1;
        candidate = format!("{anchor}-{n}");
    }
    seen.insert(anchor.to_string(), n); // next request for this base starts here
    seen.insert(candidate.clone(), 0); // reserve the exact anchor we're emitting
    candidate
}

/// The file stem (no directory, no extension) for the pre-heading chunk anchor.
fn file_stem(file: &str) -> String {
    let name = file.rsplit('/').next().unwrap_or(file);
    name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name).to_string()
}

/// Deterministic FNV-1a content hash for staleness detection. Delegates to the
/// shared [`fnv1a_hex`] so the builder and the index can never disagree.
fn content_hash(src: &str) -> String {
    fnv1a_hex(src)
}

fn parse_rust(src: &str, file: &str, fg: &mut FileGraph) {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .is_err()
    {
        return;
    }
    let Some(tree) = parser.parse(src, None) else {
        return;
    };
    walk_items(src, file, tree.root_node(), None, false, false, fg);
}

/// Recurse over a node's named children, emitting definitions (with
/// containment + doc edges), import edges, and call references. `parent` is the
/// enclosing symbol id (for `Contains`); `in_impl` makes functions count as
/// methods; `in_test_mod` is true once the walk has descended into a
/// `#[cfg(test)] mod` — every fn/method found from there down is a test (V12
/// Phase C), regardless of its own attributes.
fn walk_items(
    src: &str,
    file: &str,
    node: Node,
    parent: Option<&str>,
    in_impl: bool,
    in_test_mod: bool,
    fg: &mut FileGraph,
) {
    let mut pending_doc: Vec<String> = Vec::new();
    // Attribute text accumulated since the last def/doc reset, so a def can be
    // checked for a preceding `#[test]`/`#[tokio::test]`/`#[rstest]` the same
    // way `pending_doc` accumulates doc-comments.
    let mut pending_attrs: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();

        // Doc-comment accumulation. `///` and `//!` attach to the next def;
        // attributes (`#[...]`) sit between doc and def and don't reset it.
        match kind {
            "line_comment" | "block_comment" => {
                if let Some(d) = doc_comment_text(src, child) {
                    pending_doc.push(d);
                } else {
                    pending_doc.clear();
                }
                continue;
            }
            "attribute_item" | "inner_attribute_item" => {
                pending_attrs.push(node_text(src, child));
                continue;
            }
            _ => {}
        }

        if let Some((name, skind)) = def_name_kind(src, child, in_impl) {
            let doc = take_doc(&mut pending_doc);
            let vis = rust_visibility(child);
            let is_test = in_test_mod
                || (matches!(skind, SymbolKind::Function | SymbolKind::Method)
                    && pending_attrs.iter().any(|a| is_test_attribute(a)));
            let id = emit_symbol(src, file, child, &name, skind, parent, doc, vis, is_test, fg);

            if matches!(skind, SymbolKind::Function | SymbolKind::Method) {
                collect_calls_in(src, file, child, &id, CallSyntax::Rust, fg);
            }
            let child_in_impl = matches!(kind, "impl_item" | "trait_item");
            // A `#[cfg(test)] mod` marks everything nested in it as tests, all
            // the way down (a plain `fn` inside needs no `#[test]` of its own).
            let child_in_test_mod = in_test_mod
                || (kind == "mod_item" && pending_attrs.iter().any(|a| is_cfg_test_attribute(a)));
            walk_items(src, file, child, Some(&id), child_in_impl, child_in_test_mod, fg);
            pending_doc.clear();
            pending_attrs.clear();
        } else if kind == "use_declaration" {
            if let Some(path) = import_path(src, child) {
                fg.edges.push(Edge {
                    kind: EdgeKind::Import,
                    src: file.to_string(),
                    dst: path,
                });
            }
            pending_doc.clear();
            pending_attrs.clear();
        } else {
            // Descend for nested items (module bodies, impl bodies, etc.).
            walk_items(src, file, child, parent, in_impl, in_test_mod, fg);
            pending_doc.clear();
            pending_attrs.clear();
        }
    }
}

/// Whether an accumulated `#[...]`/`#![...]` attribute's text is `#[test]`,
/// `#[tokio::test]`, `#[async_std::test]`, `#[rstest]`, `#[rstest::rstest]`, or
/// similar (`test`/`rstest` as the leading path segment, or the path's last
/// segment). Matches on text rather than a re-parse of the attribute's inner
/// tree — the accepted spellings are simple enough that string matching is
/// both simpler and cheaper than a second grammar walk.
fn is_test_attribute(attr_text: &str) -> bool {
    let inner = attr_text
        .trim()
        .trim_start_matches('#')
        .trim_start_matches('!')
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    // Drop any `(...)`/`= ...` args, keep the leading path (`tokio::test(...)` -> `tokio::test`).
    let path = inner.split(['(', '=']).next().unwrap_or(inner).trim();
    matches!(path, "test" | "rstest") || path.ends_with("::test") || path.ends_with("::rstest")
}

/// Whether an accumulated attribute's text is a `cfg(...)` whose predicate
/// list includes a bare `test` — marks a `mod` block whose entire contents
/// are test-only. Matches the literal `cfg(test)` as well as `test` nested
/// inside `all(...)`/`any(...)` (`cfg(all(test, feature = "x"))`,
/// `cfg(any(test, feature = "x"))`, `cfg(test, feature = "x")`) — broadened
/// (V12 review) past a plain `cfg(test)` substring match, which missed every
/// combinator form. Strips the `#[...]`/`#![...]` wrapper, requires the
/// remainder to start with `cfg(`, then tokenizes on `(`/`)`/`,` and looks
/// for a bare `test` token — so `cfg(test_utils)` (a longer identifier, not
/// the `test` predicate) does NOT match.
fn is_cfg_test_attribute(attr_text: &str) -> bool {
    let inner = attr_text
        .trim()
        .trim_start_matches('#')
        .trim_start_matches('!')
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    let compact: String = inner.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(rest) = compact.strip_prefix("cfg(") else { return false };
    rest.split(['(', ')', ',']).any(|tok| tok == "test")
}

/// If `node` is a Rust definition, return its name + kind.
fn def_name_kind(src: &str, node: Node, in_impl: bool) -> Option<(String, SymbolKind)> {
    let kind = node.kind();
    let named = |field: &str| node.child_by_field_name(field).map(|n| node_text(src, n));
    let (name, sk) = match kind {
        "function_item" | "function_signature_item" => {
            let k = if in_impl {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            (named("name")?, k)
        }
        "struct_item" => (named("name")?, SymbolKind::Struct),
        "union_item" => (named("name")?, SymbolKind::Struct),
        "enum_item" => (named("name")?, SymbolKind::Enum),
        "trait_item" => (named("name")?, SymbolKind::Trait),
        "mod_item" => (named("name")?, SymbolKind::Module),
        "const_item" => (named("name")?, SymbolKind::Const),
        "static_item" => (named("name")?, SymbolKind::Static),
        "type_item" => (named("name")?, SymbolKind::TypeAlias),
        "macro_definition" => (named("name")?, SymbolKind::Macro),
        "impl_item" => (impl_name(src, node)?, SymbolKind::Impl),
        _ => return None,
    };
    Some((name, sk))
}

/// Rust visibility of a definition node from its `visibility_modifier` child:
/// `pub` → Public, `pub(crate)`/`pub(super)`/`pub(in …)` → Crate, absent →
/// Private. The modifier is a direct child of the item node when present.
fn rust_visibility(node: Node) -> Visibility {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            // A bare `pub` has no named children; `pub(crate)`/`pub(super)`/
            // `pub(in path)` add a restriction, which we fold into Crate.
            return if child.named_child_count() > 0 {
                Visibility::Crate
            } else {
                Visibility::Public
            };
        }
    }
    Visibility::Private
}

/// Synthesize a readable name for an `impl` block: `impl Foo` or
/// `impl Trait for Foo`.
fn impl_name(src: &str, node: Node) -> Option<String> {
    let ty = node.child_by_field_name("type").map(|n| node_text(src, n))?;
    if let Some(tr) = node.child_by_field_name("trait").map(|n| node_text(src, n)) {
        Some(format!("impl {tr} for {ty}"))
    } else {
        Some(format!("impl {ty}"))
    }
}

/// How each supported language spells the constructs the call-graph walk cares
/// about. Keeping the per-language variants behind one walk ([`collect_calls_in`])
/// means the recursion + attribution logic lives in a single place and only the
/// node-kind names differ per grammar. (These were previously three
/// near-identical copies whose nested-scope exclusion lists had silently
/// diverged — e.g. the JS copy forgot the anonymous `generator_function`.)
#[derive(Clone, Copy)]
enum CallSyntax {
    Rust,
    Js,
    Python,
}

impl CallSyntax {
    /// The tree-sitter node kind of a call expression in this grammar.
    fn call_kind(self) -> &'static str {
        match self {
            CallSyntax::Python => "call",
            CallSyntax::Rust | CallSyntax::Js => "call_expression",
        }
    }

    /// Best-effort callee name from a call node.
    fn callee_name(self, src: &str, call: Node) -> Option<String> {
        match self {
            CallSyntax::Rust => callee_name(src, call),
            CallSyntax::Js => js_callee_name(src, call),
            CallSyntax::Python => py_callee_name(src, call),
        }
    }

    /// Whether `kind` opens a nested scope whose calls belong to *it*, not the
    /// current symbol — the walk must not descend into these (each gets its own
    /// `collect_calls_in` from the item walk).
    fn is_nested_scope(self, kind: &str) -> bool {
        match self {
            CallSyntax::Rust => matches!(
                kind,
                "function_item" | "closure_expression" | "function_signature_item"
            ),
            CallSyntax::Js => matches!(
                kind,
                "function_declaration"
                    | "generator_function_declaration"
                    | "function"
                    | "generator_function"
                    | "arrow_function"
                    | "method_definition"
            ),
            CallSyntax::Python => {
                matches!(kind, "function_definition" | "class_definition" | "lambda")
            }
        }
    }
}

/// Walk a function body's subtree and emit a `Call` reference + edge for every
/// call expression, attributed to the enclosing symbol `from_id`. Shared across
/// languages via [`CallSyntax`]; does not descend into nested definitions
/// (their calls are attributed to them by their own walk).
fn collect_calls_in(
    src: &str,
    file: &str,
    node: Node,
    from_id: &str,
    syntax: CallSyntax,
    fg: &mut FileGraph,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == syntax.call_kind() {
            if let Some(callee) = syntax.callee_name(src, child) {
                let pos = child.start_position();
                fg.references.push(Reference {
                    name: callee.clone(),
                    file: file.to_string(),
                    line: pos.row as u32 + 1,
                    col: pos.column as u32 + 1,
                    resolved_id: None,
                });
                fg.edges.push(Edge {
                    kind: EdgeKind::Call,
                    src: from_id.to_string(),
                    dst: callee,
                });
            }
        }
        if !syntax.is_nested_scope(child.kind()) {
            collect_calls_in(src, file, child, from_id, syntax, fg);
        }
    }
}

/// Best-effort callee name for a `call_expression`.
fn callee_name(src: &str, call: Node) -> Option<String> {
    let f = call.child_by_field_name("function")?;
    let name = match f.kind() {
        "identifier" => node_text(src, f),
        // `x.method()` → the method name.
        "field_expression" => f
            .child_by_field_name("field")
            .map(|n| node_text(src, n))
            .unwrap_or_else(|| node_text(src, f)),
        // `Type::assoc()` / `path::to::fn()` → the final segment.
        "scoped_identifier" => f
            .child_by_field_name("name")
            .map(|n| node_text(src, n))
            .unwrap_or_else(|| node_text(src, f)),
        // `f::<T>()` → unwrap the generic.
        "generic_function" => f
            .child_by_field_name("function")
            .map(|n| node_text(src, n))
            .unwrap_or_else(|| node_text(src, f)),
        _ => first_line(&node_text(src, f)),
    };
    let name = name.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// The full `use` path text, minus the `use ` prefix and trailing `;`.
fn import_path(src: &str, node: Node) -> Option<String> {
    node.child_by_field_name("argument").map(|n| node_text(src, n))
}

/// A one-line signature: the node's first line, trimmed and length-capped.
/// Capped by **character** count (not bytes) — `String::truncate` panics if a
/// byte offset lands inside a multi-byte UTF-8 char, which a non-ASCII
/// identifier or comment in a long first line can trigger.
fn signature_of(src: &str, node: Node) -> String {
    let line = first_line(&node_text(src, node));
    let capped = match line.char_indices().nth(MAX_SIGNATURE) {
        Some((idx, _)) => &line[..idx],
        None => &line,
    };
    capped.trim_end().to_string()
}

/// Extract `///` / `//!` doc text from a comment node, or `None` if it's a
/// plain `//` comment.
fn doc_comment_text(src: &str, node: Node) -> Option<String> {
    let raw = node_text(src, node);
    let t = raw.trim_start();
    if let Some(rest) = t.strip_prefix("///") {
        Some(rest.trim().to_string())
    } else if let Some(rest) = t.strip_prefix("//!") {
        Some(rest.trim().to_string())
    } else if t.starts_with("/**") && !t.starts_with("/***") {
        // strip_prefix/suffix peel exactly one delimiter; trim_*_matches would
        // greedily eat a repeated `/**` or `*/` that's part of the doc body.
        let inner = t.strip_prefix("/**").unwrap_or(t);
        let inner = inner.strip_suffix("*/").unwrap_or(inner);
        Some(inner.trim().to_string())
    } else {
        None
    }
}

fn node_text(src: &str, node: Node) -> String {
    src.get(node.byte_range()).unwrap_or("").to_string()
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

// ── Structural search (Stage 4): tree-sitter query over source ───────────

/// One structural-search match: where a capture landed.
pub struct StructHit {
    pub file: String,
    pub line: u32,
    pub snippet: String,
}

/// The tree-sitter grammar for `lang`, or `None` for languages without symbol
/// extraction (Markdown/Other). `.tsx` uses the plain TS grammar here — the
/// query engine is error-tolerant enough for declaration-level patterns.
pub fn language_for(lang: Lang) -> Option<tree_sitter::Language> {
    match lang {
        Lang::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Lang::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Lang::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        Lang::Python => Some(tree_sitter_python::LANGUAGE.into()),
        // V9-02 grammars. Registering here unlocks `graph_struct_search` for
        // every language, independent of whether it has a tags query.
        Lang::Go => Some(tree_sitter_go::LANGUAGE.into()),
        Lang::Java => Some(tree_sitter_java::LANGUAGE.into()),
        Lang::C => Some(tree_sitter_c::LANGUAGE.into()),
        Lang::Cpp => Some(tree_sitter_cpp::LANGUAGE.into()),
        Lang::CSharp => Some(tree_sitter_c_sharp::LANGUAGE.into()),
        Lang::Php => Some(tree_sitter_php::LANGUAGE_PHP.into()),
        Lang::Bash => Some(tree_sitter_bash::LANGUAGE.into()),
        Lang::Scala => Some(tree_sitter_scala::LANGUAGE.into()),
        Lang::Ocaml => Some(tree_sitter_ocaml::LANGUAGE_OCAML.into()),
        Lang::Ruby => Some(tree_sitter_ruby::LANGUAGE.into()),
        Lang::Haskell => Some(tree_sitter_haskell::LANGUAGE.into()),
        Lang::Html => Some(tree_sitter_html::LANGUAGE.into()),
        Lang::Css => Some(tree_sitter_css::LANGUAGE.into()),
        Lang::Json => Some(tree_sitter_json::LANGUAGE.into()),
        Lang::Kotlin => Some(tree_sitter_kotlin_ng::LANGUAGE.into()),
        Lang::Swift => Some(tree_sitter_swift::LANGUAGE.into()),
        Lang::Sql => Some(tree_sitter_sequel::LANGUAGE.into()),
        Lang::Yaml => Some(tree_sitter_yaml::LANGUAGE.into()),
        Lang::Xml => Some(tree_sitter_xml::LANGUAGE_XML.into()),
        Lang::Erlang => Some(tree_sitter_erlang::LANGUAGE.into()),
        Lang::R => Some(tree_sitter_r::LANGUAGE.into()),
        Lang::Perl => Some(tree_sitter_perl::LANGUAGE.into()),
        Lang::Ada => Some(tree_sitter_ada::LANGUAGE.into()),
        Lang::Asm => Some(tree_sitter_asm::LANGUAGE.into()),
        Lang::Markdown | Lang::Other => None,
    }
}

/// Run a tree-sitter **query** (an S-expression pattern) over a set of
/// `(path, source)` files of one language, returning each captured node's
/// location + snippet, capped at `max_rows`. The query is compiled once; a
/// malformed query returns an `Err` the model can read and fix.
pub fn struct_search(
    lang: Lang,
    pattern: &str,
    files: &[(String, String)],
    max_rows: usize,
    max_snippet: usize,
) -> Result<Vec<StructHit>, String> {
    use tree_sitter::StreamingIterator;

    let language = language_for(lang).ok_or_else(|| {
        format!("structural search isn't supported for language `{}`", lang.tag())
    })?;
    let query = tree_sitter::Query::new(&language, pattern)
        .map_err(|e| format!("invalid tree-sitter query: {e}"))?;
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|e| format!("grammar load failed: {e}"))?;

    let mut out: Vec<StructHit> = Vec::new();
    for (path, src) in files {
        let Some(tree) = parser.parse(src, None) else {
            continue;
        };
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
        while let Some(m) = it.next() {
            for cap in m.captures {
                let node = cap.node;
                out.push(StructHit {
                    file: path.clone(),
                    line: node.start_position().row as u32 + 1,
                    snippet: snippet_of(src, node, max_snippet),
                });
                if out.len() >= max_rows {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

/// A one-line, length-capped snippet of a matched node.
fn snippet_of(src: &str, node: Node, max: usize) -> String {
    let mut s = first_line(&node_text(src, node)).trim().to_string();
    if s.chars().count() > max {
        s = s.chars().take(max).collect::<String>() + "…";
    }
    s
}

// ── TypeScript / JavaScript ──────────────────────────────────────────────

/// Parse a TS/JS/TSX/JSX file. `.tsx`/`.jsx` get the JSX-aware grammar so JSX
/// markup doesn't desync the parse around the surrounding declarations.
fn parse_js_ts(src: &str, file: &str, lang: Lang, fg: &mut FileGraph) {
    let lower = file.to_ascii_lowercase();
    let language = match lang {
        Lang::TypeScript if lower.ends_with(".tsx") => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        // tree-sitter-javascript's grammar already covers JSX.
        _ => tree_sitter_javascript::LANGUAGE.into(),
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return;
    }
    let Some(tree) = parser.parse(src, None) else {
        return;
    };
    let file_is_test = js_is_test_file(file);
    walk_js(src, file, tree.root_node(), None, false, file_is_test, fg);
}

/// Whether `file`'s path marks every definition in it as a test, by the common
/// JS/TS convention: a `*.test.*`/`*.spec.*` filename, or any `__tests__/`
/// path segment. No per-definition signal (`test()`/`it()`/`describe()`
/// callbacks aren't synthesized as symbols by this walker — see
/// [`js_def_name_kind`]/[`emit_js_var_functions`]), so the file-level rule is
/// the whole story for JS/TS (V12 Phase C).
fn js_is_test_file(file: &str) -> bool {
    let lower = file.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    name.contains(".test.") || name.contains(".spec.") || lower.split('/').any(|seg| seg == "__tests__")
}

/// Recursive JS/TS item walk: declarations (functions, classes, methods,
/// interfaces, enums, type aliases, arrow-fn consts), import edges, call
/// references, JSDoc, and class→method containment. `file_is_test` (V12 Phase
/// C) marks every emitted definition as a test when the file itself is a test
/// file (see [`js_is_test_file`]).
fn walk_js(
    src: &str,
    file: &str,
    node: Node,
    parent: Option<&str>,
    in_class: bool,
    file_is_test: bool,
    fg: &mut FileGraph,
) {
    let mut pending_doc: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();

        if kind == "comment" {
            match js_doc_text(src, child) {
                Some(d) => pending_doc.push(d),
                None => pending_doc.clear(),
            }
            continue;
        }

        // `export function/class/...` and `export default …` wrap the real
        // declaration; unwrap so the JSDoc that precedes the `export` attaches.
        let def_node = if kind == "export_statement" {
            child.child_by_field_name("declaration").unwrap_or(child)
        } else {
            child
        };

        // A declaration wrapped in `export …` (or `export default …`) is public;
        // a bare top-level declaration is module-private. Methods nested in a
        // class stay Private (reachable via their class, never a dead export).
        let exported = kind == "export_statement";
        let vis = if exported { Visibility::Public } else { Visibility::Private };

        if let Some((name, skind)) = js_def_name_kind(src, def_node, in_class) {
            let doc = take_doc(&mut pending_doc);
            let id = emit_symbol(src, file, def_node, &name, skind, parent, doc, vis, file_is_test, fg);
            if matches!(skind, SymbolKind::Function | SymbolKind::Method) {
                collect_calls_in(src, file, def_node, &id, CallSyntax::Js, fg);
            }
            let child_in_class = matches!(def_node.kind(), "class_declaration" | "abstract_class_declaration");
            walk_js(src, file, def_node, Some(&id), child_in_class, file_is_test, fg);
            pending_doc.clear();
        } else if matches!(def_node.kind(), "lexical_declaration" | "variable_declaration") {
            // `const foo = () => {…}` / `const bar = function(){}` → a function.
            let doc = take_doc(&mut pending_doc);
            emit_js_var_functions(src, file, def_node, parent, doc, vis, file_is_test, fg);
            pending_doc.clear();
        } else if kind == "import_statement" {
            if let Some(srcpath) = js_import_source(src, child) {
                fg.edges.push(Edge { kind: EdgeKind::Import, src: file.to_string(), dst: srcpath });
            }
            pending_doc.clear();
        } else {
            // Descend into wrappers (export of a name list, statement blocks,
            // class bodies, namespaces) keeping the enclosing symbol/parent.
            walk_js(src, file, def_node, parent, in_class, file_is_test, fg);
            pending_doc.clear();
        }
    }
}

/// Emit one symbol (+ containment + doc edges) and return its id.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_symbol(
    src: &str,
    file: &str,
    node: Node,
    name: &str,
    skind: SymbolKind,
    parent: Option<&str>,
    doc: Option<String>,
    vis: Visibility,
    is_test: bool,
    fg: &mut FileGraph,
) -> String {
    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    let id = symbol_id(file, name, start);
    fg.symbols.push(Symbol {
        id: id.clone(),
        name: name.to_string(),
        kind: skind,
        file: file.to_string(),
        start_line: start,
        end_line: end,
        signature: signature_of(src, node),
        doc: doc.clone(),
        visibility: vis,
        is_test,
    });
    if let Some(p) = parent {
        fg.edges.push(Edge { kind: EdgeKind::Contains, src: p.to_string(), dst: id.clone() });
    }
    // V11 Phase G: a semantic *code* chunk for this symbol (doc + body), keyed by
    // the symbol's own id. The node text already begins with the signature line,
    // so it is NOT prepended separately (doing so would burn the truncation
    // budget on a duplicate). Uses `doc.as_deref()` rather than consuming `doc` —
    // the doc-chunk block below still needs it. Only "shaped" definitions worth
    // embedding, and only spans long enough that the one-line signature alone
    // wouldn't capture the interesting part.
    if is_code_chunk_kind(skind) && end.saturating_sub(start) + 1 >= MIN_CODE_CHUNK_LINES {
        let mut text = String::new();
        if let Some(d) = doc.as_deref() {
            text.push_str(d);
            text.push('\n');
        }
        text.push_str(&node_text(src, node));
        fg.code_chunks.push(CodeChunk {
            id: id.clone(),
            file: file.to_string(),
            text: truncate_code_chunk(&text, MAX_CODE_CHUNK_CHARS),
        });
    }
    if let Some(text) = doc {
        // Disambiguate the storage key by start line so two same-named defs in
        // one file don't collide and overwrite each other's doc chunk.
        let cid = symbol_doc_chunk_id(file, name, start);
        fg.docs.push(DocChunk {
            id: cid.clone(),
            source_path: file.to_string(),
            anchor: name.to_string(),
            text,
        });
        fg.edges.push(Edge { kind: EdgeKind::Documents, src: cid, dst: id.clone() });
    }
    id
}

/// Symbol kinds "shaped" enough to earn a semantic code chunk — a bare
/// `const`/`static`/`field`/`variant`/`module`/`macro`/`impl` is already fully
/// covered by its one-line signature, so chunking it would just be noise.
fn is_code_chunk_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function
            | SymbolKind::Method
            | SymbolKind::Struct
            | SymbolKind::Class
            | SymbolKind::Enum
            | SymbolKind::Trait
            | SymbolKind::Interface
            | SymbolKind::TypeAlias
    )
}

/// Truncate `s` to at most `n` characters (char-boundary safe — see
/// [`signature_of`]'s note on why a byte-offset truncate can panic here).
fn truncate_code_chunk(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

fn take_doc(pending: &mut Vec<String>) -> Option<String> {
    if pending.is_empty() {
        None
    } else {
        Some(pending.join("\n"))
    }
}

/// If `node` is a JS/TS definition, its name + kind.
fn js_def_name_kind(src: &str, node: Node, in_class: bool) -> Option<(String, SymbolKind)> {
    let named = |field: &str| node.child_by_field_name(field).map(|n| node_text(src, n));
    let pair = match node.kind() {
        "function_declaration" | "generator_function_declaration" => (named("name")?, SymbolKind::Function),
        "method_definition" => (named("name")?, SymbolKind::Method),
        "class_declaration" | "abstract_class_declaration" => (named("name")?, SymbolKind::Class),
        "interface_declaration" => (named("name")?, SymbolKind::Interface),
        "enum_declaration" => (named("name")?, SymbolKind::Enum),
        "type_alias_declaration" => (named("name")?, SymbolKind::TypeAlias),
        // A bare function/method inside a class body without the above kinds.
        "public_field_definition" if in_class => return None,
        _ => return None,
    };
    Some(pair)
}

/// Emit `Function` symbols for `const f = () => …` / `= function(){}` inside a
/// `lexical_declaration`/`variable_declaration`.
#[allow(clippy::too_many_arguments)]
fn emit_js_var_functions(
    src: &str,
    file: &str,
    decl: Node,
    parent: Option<&str>,
    doc: Option<String>,
    vis: Visibility,
    file_is_test: bool,
    fg: &mut FileGraph,
) {
    let mut cursor = decl.walk();
    let mut first = true;
    for d in decl.named_children(&mut cursor) {
        if d.kind() != "variable_declarator" {
            continue;
        }
        let Some(value) = d.child_by_field_name("value") else { continue };
        if !matches!(value.kind(), "arrow_function" | "function" | "function_expression" | "generator_function") {
            continue;
        }
        let Some(name) = d.child_by_field_name("name").map(|n| node_text(src, n)) else { continue };
        // Only the first declarator gets the leading doc-comment.
        let this_doc = if first { doc.clone() } else { None };
        let id = emit_symbol(src, file, d, &name, SymbolKind::Function, parent, this_doc, vis, file_is_test, fg);
        collect_calls_in(src, file, value, &id, CallSyntax::Js, fg);
        first = false;
    }
}

/// Best-effort callee name for a JS/TS `call_expression`.
fn js_callee_name(src: &str, call: Node) -> Option<String> {
    let f = call.child_by_field_name("function")?;
    let name = match f.kind() {
        "identifier" => node_text(src, f),
        // `x.method()` → `method`.
        "member_expression" => f
            .child_by_field_name("property")
            .map(|n| node_text(src, n))
            .unwrap_or_else(|| node_text(src, f)),
        _ => first_line(&node_text(src, f)),
    };
    let name = name.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// The module specifier of an `import_statement` (the quoted source), unquoted.
fn js_import_source(src: &str, node: Node) -> Option<String> {
    let s = node.child_by_field_name("source").map(|n| node_text(src, n))?;
    Some(s.trim().trim_matches(['"', '\'', '`']).to_string())
}

/// JSDoc text from a comment node (a `/** … */` block), or `None` for ordinary
/// `//` or `/* */` comments.
fn js_doc_text(src: &str, node: Node) -> Option<String> {
    let raw = node_text(src, node);
    let t = raw.trim_start();
    if t.starts_with("/**") && !t.starts_with("/***") {
        // Peel exactly one `/**` … `*/` pair; the per-line pass below still
        // strips the leading ` * ` decorations.
        let body = t.strip_prefix("/**").unwrap_or(t);
        let body = body.strip_suffix("*/").unwrap_or(body);
        // Strip leading ` * ` from each JSDoc line.
        let cleaned: Vec<String> = body
            .lines()
            .map(|l| l.trim_start().trim_start_matches('*').trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let text = cleaned.join("\n");
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    } else {
        None
    }
}

// ── Python ───────────────────────────────────────────────────────────────

fn parse_python(src: &str, file: &str, fg: &mut FileGraph) {
    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_python::LANGUAGE.into()).is_err() {
        return;
    }
    let Some(tree) = parser.parse(src, None) else {
        return;
    };
    let file_is_test_path = py_is_test_file(file);
    walk_py(src, file, tree.root_node(), None, false, file_is_test_path, fg);
}

/// Whether `file`'s path matches the pytest test-file convention:
/// `test_*.py` / `*_test.py`, or any `tests/` path segment. Combined with a
/// `test_`-prefixed `def` name at the call site (V12 Phase C) — the file
/// match alone isn't enough (a `tests/conftest.py` helper isn't a test).
fn py_is_test_file(file: &str) -> bool {
    let lower = file.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    (name.starts_with("test_") && name.ends_with(".py"))
        || name.ends_with("_test.py")
        || lower.split('/').any(|seg| seg == "tests")
}

/// Recursive Python walk: `def`/`class` definitions (incl. `@decorated`),
/// docstrings, import edges, call references, class→method containment.
/// `file_is_test_path` (V12 Phase C) is the file-path half of the pytest
/// test-detection rule (see [`py_is_test_file`]); combined with a
/// `test_`-prefixed name at the definition site.
fn walk_py(
    src: &str,
    file: &str,
    node: Node,
    parent: Option<&str>,
    in_class: bool,
    file_is_test_path: bool,
    fg: &mut FileGraph,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        // `@decorator`-wrapped def/class: the real definition is the inner node.
        let def_node = if kind == "decorated_definition" {
            child.child_by_field_name("definition").unwrap_or(child)
        } else {
            child
        };
        match def_node.kind() {
            "function_definition" | "class_definition" => {
                let Some(name) = def_node.child_by_field_name("name").map(|n| node_text(src, n)) else {
                    continue;
                };
                let skind = if def_node.kind() == "class_definition" {
                    SymbolKind::Class
                } else if in_class {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let doc = py_docstring(src, def_node);
                let vis = py_visibility(&name);
                let is_test = file_is_test_path
                    && matches!(skind, SymbolKind::Function | SymbolKind::Method)
                    && name.starts_with("test_");
                let id = emit_symbol(src, file, def_node, &name, skind, parent, doc, vis, is_test, fg);
                if matches!(skind, SymbolKind::Function | SymbolKind::Method) {
                    collect_calls_in(src, file, def_node, &id, CallSyntax::Python, fg);
                }
                let child_in_class = def_node.kind() == "class_definition";
                walk_py(src, file, def_node, Some(&id), child_in_class, file_is_test_path, fg);
            }
            "import_statement" | "import_from_statement" => {
                for m in py_import_modules(src, def_node) {
                    fg.edges.push(Edge { kind: EdgeKind::Import, src: file.to_string(), dst: m });
                }
            }
            _ => walk_py(src, file, def_node, parent, in_class, file_is_test_path, fg),
        }
    }
}

/// Python visibility by name convention: a single leading underscore (but not a
/// dunder like `__init__`) marks a module-private/"internal" name; everything
/// else is treated as public. `__all__` membership is out of scope for the MVP.
fn py_visibility(name: &str) -> Visibility {
    let is_dunder = name.starts_with("__") && name.ends_with("__");
    if name.starts_with('_') && !is_dunder {
        Visibility::Private
    } else {
        Visibility::Public
    }
}

/// The docstring of a `function_definition`/`class_definition` (the first
/// string statement in its block), unquoted and trimmed.
fn py_docstring(src: &str, def: Node) -> Option<String> {
    let body = def.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let first = body.named_children(&mut cursor).next()?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let mut c2 = first.walk();
    let s = first.named_children(&mut c2).next()?;
    if s.kind() != "string" {
        return None;
    }
    let raw = node_text(src, s);
    // Drop an optional string prefix (r/b/u/f) that sits before the opening
    // quote, then peel the quote delimiter itself — triple before single —
    // exactly once at each end. `trim_matches('"')` would greedily eat quote
    // characters that are part of the docstring content (e.g. `"""'x'"""`).
    let t = raw
        .trim()
        .trim_start_matches(['r', 'b', 'u', 'R', 'B', 'U', 'f', 'F']);
    let t = strip_py_quotes(t).trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Strip one matching Python string delimiter (`"""`, `'''`, `"`, or `'`) from
/// each end, preferring the triple forms. Leaves the content untouched if it
/// isn't quote-delimited.
fn strip_py_quotes(s: &str) -> &str {
    for q in ["\"\"\"", "'''", "\"", "'"] {
        if let Some(inner) = s.strip_prefix(q) {
            return inner.strip_suffix(q).unwrap_or(inner);
        }
    }
    s
}

/// Callee name for a Python `call` node (`f()` → `f`, `obj.m()` → `m`).
fn py_callee_name(src: &str, call: Node) -> Option<String> {
    let f = call.child_by_field_name("function")?;
    let name = match f.kind() {
        "identifier" => node_text(src, f),
        "attribute" => f
            .child_by_field_name("attribute")
            .map(|n| node_text(src, n))
            .unwrap_or_else(|| node_text(src, f)),
        _ => first_line(&node_text(src, f)),
    };
    let name = name.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Module names imported by an `import_statement`/`import_from_statement`.
fn py_import_modules(src: &str, node: Node) -> Vec<String> {
    // `import_from_statement` has a `module_name` field; `import_statement`
    // lists dotted names/aliases as its named children.
    if node.kind() == "import_from_statement" {
        if let Some(m) = node.child_by_field_name("module_name") {
            return vec![node_text(src, m)];
        }
    }
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for c in node.named_children(&mut cursor) {
        match c.kind() {
            "dotted_name" | "relative_import" => out.push(node_text(src, c)),
            // `import x as y` → the `name` (dotted_name) field of aliased_import.
            "aliased_import" => {
                if let Some(n) = c.child_by_field_name("name") {
                    out.push(node_text(src, n));
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(fg: &FileGraph, kind: SymbolKind) -> Vec<String> {
        fg.symbols
            .iter()
            .filter(|s| s.kind == kind)
            .map(|s| s.name.clone())
            .collect()
    }

    fn vis_of(fg: &FileGraph, name: &str) -> Visibility {
        fg.symbols.iter().find(|s| s.name == name).unwrap().visibility
    }

    fn is_test_of(fg: &FileGraph, name: &str) -> bool {
        fg.symbols.iter().find(|s| s.name == name).unwrap().is_test
    }

    #[test]
    fn rust_visibility_classified() {
        let src = "pub fn a() {}\nfn b() {}\npub(crate) fn c() {}\npub(super) fn d() {}\n";
        let fg = parse_file("src/v.rs", src, Lang::Rust);
        assert_eq!(vis_of(&fg, "a"), Visibility::Public);
        assert_eq!(vis_of(&fg, "b"), Visibility::Private);
        assert_eq!(vis_of(&fg, "c"), Visibility::Crate);
        assert_eq!(vis_of(&fg, "d"), Visibility::Crate);
    }

    #[test]
    fn js_export_visibility_classified() {
        let src = "export function pub_fn() {}\nfunction priv_fn() {}\nexport const arrow = () => {};\nconst hidden = () => {};\n";
        let fg = parse_file("src/v.ts", src, Lang::TypeScript);
        assert_eq!(vis_of(&fg, "pub_fn"), Visibility::Public);
        assert_eq!(vis_of(&fg, "priv_fn"), Visibility::Private);
        assert_eq!(vis_of(&fg, "arrow"), Visibility::Public);
        assert_eq!(vis_of(&fg, "hidden"), Visibility::Private);
    }

    #[test]
    fn python_underscore_visibility_classified() {
        let src = "def public_fn():\n    pass\ndef _helper():\n    pass\ndef __dunder__():\n    pass\n";
        let fg = parse_file("src/v.py", src, Lang::Python);
        assert_eq!(vis_of(&fg, "public_fn"), Visibility::Public);
        assert_eq!(vis_of(&fg, "_helper"), Visibility::Private);
        assert_eq!(vis_of(&fg, "__dunder__"), Visibility::Public);
    }

    // ── V12 Phase C: is_test detection ──────────────────────────────────

    #[test]
    fn rust_attribute_test_detected() {
        let src = "#[test]\nfn a_test() {}\n#[tokio::test]\nasync fn tokio_test() {}\n#[rstest]\nfn an_rstest() {}\nfn plain() {}\n";
        let fg = parse_file("src/t.rs", src, Lang::Rust);
        assert!(is_test_of(&fg, "a_test"));
        assert!(is_test_of(&fg, "tokio_test"));
        assert!(is_test_of(&fg, "an_rstest"));
        assert!(!is_test_of(&fg, "plain"));
    }

    #[test]
    fn rust_cfg_test_mod_marks_every_fn_inside() {
        // A plain fn (no #[test] of its own) inside a #[cfg(test)] mod is still
        // a test — the mod-level cfg attribute is the signal.
        let src = "fn outer() {}\n#[cfg(test)]\nmod tests {\n    fn helper() {}\n    #[test]\n    fn it_works() {}\n}\n";
        let fg = parse_file("src/t.rs", src, Lang::Rust);
        assert!(!is_test_of(&fg, "outer"));
        assert!(is_test_of(&fg, "helper"), "plain fn inside #[cfg(test)] mod is a test");
        assert!(is_test_of(&fg, "it_works"));
    }

    #[test]
    fn rust_cfg_any_test_mod_marks_every_fn_inside() {
        // `#[cfg(any(test, feature = "x"))]` is a common combinator form —
        // the plain `cfg(test)` substring check used to miss it entirely
        // (V12 review).
        let src = "fn outer() {}\n#[cfg(any(test, feature = \"x\"))]\nmod tests {\n    fn helper() {}\n}\n";
        let fg = parse_file("src/t.rs", src, Lang::Rust);
        assert!(!is_test_of(&fg, "outer"));
        assert!(is_test_of(&fg, "helper"), "plain fn inside #[cfg(any(test, ...))] mod is a test");
    }

    #[test]
    fn rust_cfg_test_utils_mod_is_not_a_cfg_test_mod() {
        // `test_utils` is a longer identifier, not the bare `test` predicate —
        // must NOT be mistaken for a `#[cfg(test)]` mod.
        let src = "#[cfg(test_utils)]\nmod helpers {\n    fn helper() {}\n}\n";
        let fg = parse_file("src/t.rs", src, Lang::Rust);
        assert!(!is_test_of(&fg, "helper"));
    }

    #[test]
    fn js_test_file_marks_every_function_as_test() {
        let src = "export function checkThing() { return 1; }\nfunction helper() { return 2; }\n";
        let fg = parse_file("src/thing.test.ts", src, Lang::TypeScript);
        assert!(is_test_of(&fg, "checkThing"));
        assert!(is_test_of(&fg, "helper"));

        // The same source in a non-test file gets no test bit.
        let fg2 = parse_file("src/thing.ts", src, Lang::TypeScript);
        assert!(!is_test_of(&fg2, "checkThing"));
    }

    #[test]
    fn js_spec_and_dunder_tests_dir_marks_test_file() {
        let src = "function checkThing() { return 1; }\n";
        assert!(is_test_of(&parse_file("src/thing.spec.js", src, Lang::JavaScript), "checkThing"));
        assert!(is_test_of(&parse_file("__tests__/thing.js", src, Lang::JavaScript), "checkThing"));
    }

    #[test]
    fn python_test_prefixed_def_in_test_file_detected() {
        let src = "def test_add():\n    pass\ndef helper():\n    pass\n";
        let fg = parse_file("tests/test_math.py", src, Lang::Python);
        assert!(is_test_of(&fg, "test_add"));
        // Not test_-prefixed, even in a test file: not a test.
        assert!(!is_test_of(&fg, "helper"));
    }

    #[test]
    fn python_test_prefixed_def_outside_test_path_not_detected() {
        // `test_`-prefixed name alone isn't enough — the file must also match
        // the pytest convention (name or `tests/` path).
        let src = "def test_add():\n    pass\n";
        let fg = parse_file("src/math.py", src, Lang::Python);
        assert!(!is_test_of(&fg, "test_add"));
    }

    #[test]
    fn python_underscore_suffix_test_file_detected() {
        let src = "def test_sub():\n    pass\n";
        let fg = parse_file("math_test.py", src, Lang::Python);
        assert!(is_test_of(&fg, "test_sub"));
    }

    #[test]
    fn generic_tags_engine_path_fallback_detects_go_test_file() {
        let src = "package main\n\nfunc TestAdd(t *T) {\n    helper()\n}\n\nfunc helper() {}\n";
        let fg = parse_file("pkg/math_test.go", src, Lang::Go);
        assert!(is_test_of(&fg, "TestAdd"));
        assert!(is_test_of(&fg, "helper"), "every fn in a _test.go file is a candidate test");

        // The same source in a non-test-named Go file gets no test bit.
        let fg2 = parse_file("pkg/math.go", src, Lang::Go);
        assert!(!is_test_of(&fg2, "TestAdd"));
    }

    #[test]
    fn generic_tags_engine_tests_dir_fallback() {
        let src = "class Greeter\n  def greet(name)\n    name\n  end\nend\n";
        let fg = parse_file("spec/greeter_spec.rb", src, Lang::Ruby);
        assert!(is_test_of(&fg, "greet"));
    }

    const SRC: &str = r#"
use std::collections::HashMap;

/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    helper(a) + b
}

fn helper(x: i32) -> i32 { x * 2 }

pub struct Point { x: i32, y: i32 }

impl Point {
    /// Construct the origin.
    pub fn origin() -> Self {
        Point { x: 0, y: 0 }
    }
}

pub trait Shape {
    fn area(&self) -> f64;
}
"#;

    #[test]
    fn extracts_rust_definitions() {
        let fg = parse_file("src/geo.rs", SRC, Lang::Rust);

        let fns = names(&fg, SymbolKind::Function);
        assert!(fns.contains(&"add".to_string()));
        assert!(fns.contains(&"helper".to_string()));

        let methods = names(&fg, SymbolKind::Method);
        assert!(methods.contains(&"origin".to_string()));

        assert!(names(&fg, SymbolKind::Struct).contains(&"Point".to_string()));
        assert!(names(&fg, SymbolKind::Trait).contains(&"Shape".to_string()));
        assert!(names(&fg, SymbolKind::Impl).contains(&"impl Point".to_string()));
    }

    #[test]
    fn captures_doc_signature_call_and_import() {
        let fg = parse_file("src/geo.rs", SRC, Lang::Rust);

        let add = fg.symbols.iter().find(|s| s.name == "add").unwrap();
        assert_eq!(add.doc.as_deref(), Some("Adds two numbers."));
        assert!(add.signature.contains("fn add"));

        // `add` calls `helper`.
        assert!(fg.edges.iter().any(|e| e.kind == EdgeKind::Call
            && e.src == add.id
            && e.dst == "helper"));

        // The file imports the HashMap path.
        assert!(fg
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Import && e.dst.contains("HashMap")));

        // Containment: the impl contains `origin`.
        let impl_sym = fg.symbols.iter().find(|s| s.name == "impl Point").unwrap();
        let origin = fg.symbols.iter().find(|s| s.name == "origin").unwrap();
        assert!(fg.edges.iter().any(|e| e.kind == EdgeKind::Contains
            && e.src == impl_sym.id
            && e.dst == origin.id));
    }

    #[test]
    fn hash_is_stable_and_content_sensitive() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
    }

    #[test]
    fn signature_with_multibyte_utf8_past_cap_does_not_panic() {
        // A first line longer than MAX_SIGNATURE chars whose boundary lands on
        // a multi-byte char. Byte-offset truncation would panic here.
        let pad = "α".repeat(MAX_SIGNATURE); // each 'α' is 2 bytes
        let src = format!("/// doc\npub fn ɸunc_{pad}() {{}}\n");
        let fg = parse_file("src/u.rs", &src, Lang::Rust);
        let f = fg.symbols.iter().find(|s| s.name.starts_with("ɸunc_")).unwrap();
        // Capped to MAX_SIGNATURE chars (not bytes), and valid UTF-8.
        assert!(f.signature.chars().count() <= MAX_SIGNATURE);
    }

    #[test]
    fn same_named_symbols_in_one_file_get_distinct_doc_chunks() {
        // Two `fn new()` in two impl blocks: distinct doc chunks, no overwrite.
        let src = r#"
pub struct A;
pub struct B;
impl A {
    /// Make an A.
    pub fn new() -> A { A }
}
impl B {
    /// Make a B.
    pub fn new() -> B { B }
}
"#;
        let fg = parse_file("src/dup.rs", src, Lang::Rust);
        let new_docs: Vec<&DocChunk> = fg.docs.iter().filter(|d| d.anchor == "new").collect();
        assert_eq!(new_docs.len(), 2, "both new() docs survive");
        // Distinct storage ids (so neither upsert clobbers the other).
        assert_ne!(new_docs[0].id, new_docs[1].id);
        let texts: Vec<&str> = new_docs.iter().map(|d| d.text.as_str()).collect();
        assert!(texts.contains(&"Make an A."));
        assert!(texts.contains(&"Make a B."));
    }

    const MD: &str = r#"# Overview

Intro paragraph about the project.

## Setup

Run `cargo build`. Note the `# not a heading` inside this code:

```
# this hash is inside a fence, not a heading
```

## Setup

Duplicate heading to exercise de-dup.
"#;

    #[test]
    fn markdown_chunks_by_heading() {
        let fg = parse_file("docs/README.md", MD, Lang::Markdown);
        assert_eq!(fg.lang_tag, "markdown");
        // No code symbols/edges for markdown.
        assert!(fg.symbols.is_empty());

        let anchors: Vec<&str> = fg.docs.iter().map(|d| d.anchor.as_str()).collect();
        assert!(anchors.contains(&"overview"));
        assert!(anchors.contains(&"setup"));
        // The duplicate "## Setup" is disambiguated.
        assert!(anchors.contains(&"setup-1"));

        // The fenced `# this hash…` line did NOT open a new section.
        assert!(!anchors.iter().any(|a| a.contains("this-hash")));

        // The Overview chunk carries its body text (searchable).
        let overview = fg.docs.iter().find(|d| d.anchor == "overview").unwrap();
        assert!(overview.text.contains("Intro paragraph"));
    }

    const TS: &str = r#"
import { mount } from './host';

/** Adds two numbers. */
export function add(a: number, b: number): number {
  return helper(a) + b;
}

function helper(x: number): number { return x * 2; }

export const build = (n: number) => helper(n);

export interface Shape { area(): number; }

export type Id = string;

export class Point {
  origin(): Point { return mount(this); }
}
"#;

    #[test]
    fn extracts_typescript() {
        let fg = parse_file("src/geo.ts", TS, Lang::TypeScript);
        assert_eq!(fg.lang_tag, "typescript");

        assert!(names(&fg, SymbolKind::Function).contains(&"add".to_string()));
        assert!(names(&fg, SymbolKind::Function).contains(&"helper".to_string()));
        // `const build = (n) => …` is captured as a function.
        assert!(names(&fg, SymbolKind::Function).contains(&"build".to_string()));
        assert!(names(&fg, SymbolKind::Class).contains(&"Point".to_string()));
        assert!(names(&fg, SymbolKind::Interface).contains(&"Shape".to_string()));
        assert!(names(&fg, SymbolKind::TypeAlias).contains(&"Id".to_string()));
        assert!(names(&fg, SymbolKind::Method).contains(&"origin".to_string()));

        // JSDoc attaches across the `export`.
        let add = fg.symbols.iter().find(|s| s.name == "add").unwrap();
        assert_eq!(add.doc.as_deref(), Some("Adds two numbers."));

        // `add` calls `helper`; the file imports './host'.
        assert!(fg.edges.iter().any(|e| e.kind == EdgeKind::Call && e.src == add.id && e.dst == "helper"));
        assert!(fg.edges.iter().any(|e| e.kind == EdgeKind::Import && e.dst == "./host"));
        // class Point contains method origin.
        let point = fg.symbols.iter().find(|s| s.name == "Point").unwrap();
        let origin = fg.symbols.iter().find(|s| s.name == "origin").unwrap();
        assert!(fg.edges.iter().any(|e| e.kind == EdgeKind::Contains && e.src == point.id && e.dst == origin.id));
    }

    #[test]
    fn extracts_javascript() {
        let js = "export function go() { return run(); }\nconst f = () => go();\n";
        let fg = parse_file("src/app.js", js, Lang::JavaScript);
        assert_eq!(fg.lang_tag, "javascript");
        assert!(names(&fg, SymbolKind::Function).contains(&"go".to_string()));
        assert!(names(&fg, SymbolKind::Function).contains(&"f".to_string()));
        let go = fg.symbols.iter().find(|s| s.name == "go").unwrap();
        assert!(fg.edges.iter().any(|e| e.kind == EdgeKind::Call && e.src == go.id && e.dst == "run"));
    }

    const PY: &str = r#"
import os
from pkg.sub import thing

def add(a, b):
    """Adds two numbers."""
    return helper(a) + b

def helper(x):
    return x * 2

class Point:
    def origin(self):
        return os.getcwd()
"#;

    #[test]
    fn struct_search_matches_by_ast_shape() {
        let files = vec![(
            "src/a.rs".to_string(),
            "fn add() { x.unwrap() }\nfn helper() { y.unwrap() }\nfn no_unwrap() {}\n".to_string(),
        )];
        // Every `.unwrap()` call (a field_expression method call named unwrap).
        let q = r#"(call_expression function: (field_expression field: (field_identifier) @m) (#eq? @m "unwrap"))"#;
        let hits = struct_search(Lang::Rust, q, &files, 100, 80).expect("query ok");
        assert_eq!(hits.len(), 2, "two unwrap() calls");
        assert!(hits.iter().all(|h| h.file == "src/a.rs"));
        assert!(hits.iter().any(|h| h.line == 1));
        assert!(hits.iter().any(|h| h.line == 2));

        // A malformed query is a readable error, not a panic.
        let err = struct_search(Lang::Rust, "(not_a_node", &files, 10, 80);
        assert!(err.is_err());
    }

    #[test]
    fn extracts_python() {
        let fg = parse_file("src/geo.py", PY, Lang::Python);
        assert_eq!(fg.lang_tag, "python");

        assert!(names(&fg, SymbolKind::Function).contains(&"add".to_string()));
        assert!(names(&fg, SymbolKind::Function).contains(&"helper".to_string()));
        assert!(names(&fg, SymbolKind::Class).contains(&"Point".to_string()));
        assert!(names(&fg, SymbolKind::Method).contains(&"origin".to_string()));

        // Docstring → doc.
        let add = fg.symbols.iter().find(|s| s.name == "add").unwrap();
        assert_eq!(add.doc.as_deref(), Some("Adds two numbers."));
        // add calls helper; imports os + pkg.sub; origin calls getcwd (attr).
        assert!(fg.edges.iter().any(|e| e.kind == EdgeKind::Call && e.src == add.id && e.dst == "helper"));
        assert!(fg.edges.iter().any(|e| e.kind == EdgeKind::Import && e.dst == "os"));
        assert!(fg.edges.iter().any(|e| e.kind == EdgeKind::Import && e.dst == "pkg.sub"));
        let origin = fg.symbols.iter().find(|s| s.name == "origin").unwrap();
        assert!(fg.edges.iter().any(|e| e.kind == EdgeKind::Call && e.src == origin.id && e.dst == "getcwd"));
    }

    #[test]
    fn dedup_anchor_avoids_collision_with_named_heading() {
        let mut seen = std::collections::HashMap::new();
        // A literal "foo-1" heading precedes a repeated "foo": the dedup of the
        // second "foo" must not land on the already-used "foo-1".
        let a = dedup_anchor("foo-1", &mut seen);
        let b = dedup_anchor("foo", &mut seen);
        let c = dedup_anchor("foo", &mut seen);
        assert_eq!(a, "foo-1");
        assert_eq!(b, "foo");
        assert_ne!(c, a, "second 'foo' must not reuse the 'foo-1' anchor");
        assert_eq!(c, "foo-2");
    }

    #[test]
    fn markdown_other_fence_marker_inside_is_literal() {
        // A `~~~` line inside a ``` fence must NOT close it, so the `#` line that
        // follows stays fenced content and never becomes its own section.
        let md = "# Top\n\n```\n~~~\n# not a heading\n```\n\n## Real\n\nbody\n";
        let fg = parse_file("docs/x.md", md, Lang::Markdown);
        let anchors: Vec<&str> = fg.docs.iter().map(|d| d.anchor.as_str()).collect();
        assert!(!anchors.iter().any(|a| a.contains("not-a-heading")));
        assert!(anchors.contains(&"top"));
        assert!(anchors.contains(&"real"));
    }

    #[test]
    fn py_docstring_preserves_trailing_inner_quote() {
        // Content ending in a quote: greedy `trim_matches('\'')` would drop the
        // closing quote; peeling the delimiter exactly once keeps it.
        let py = "def f():\n    \"\"\"ends with 'q'\"\"\"\n    return 1\n";
        let fg = parse_file("src/q.py", py, Lang::Python);
        let f = fg.symbols.iter().find(|s| s.name == "f").unwrap();
        assert_eq!(f.doc.as_deref(), Some("ends with 'q'"));
    }

    #[test]
    fn js_calls_in_anonymous_generator_not_attributed_to_enclosing_fn() {
        // The call inside an anonymous `function*(){}` belongs to the generator,
        // not to `outer` — the JS exclusion list must skip `generator_function`.
        let js = "function outer() { const g = function*() { inner(); }; }\n";
        let fg = parse_file("src/g.js", js, Lang::JavaScript);
        let outer = fg.symbols.iter().find(|s| s.name == "outer").unwrap();
        assert!(!fg
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Call && e.src == outer.id && e.dst == "inner"));
    }
}

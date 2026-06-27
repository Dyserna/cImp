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
    doc_chunk_id, symbol_id, DocChunk, Edge, EdgeKind, FileGraph, Lang, Reference, Symbol,
    SymbolKind,
};

/// Max characters kept for a symbol's one-line signature.
const MAX_SIGNATURE: usize = 200;

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
    let mut in_fence = false;

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
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            body.push(line.to_string());
            continue;
        }
        if !in_fence {
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
fn dedup_anchor(anchor: &str, seen: &mut std::collections::HashMap<String, u32>) -> String {
    let n = seen.entry(anchor.to_string()).or_insert(0);
    let out = if *n == 0 {
        anchor.to_string()
    } else {
        format!("{anchor}-{n}")
    };
    *n += 1;
    out
}

/// The file stem (no directory, no extension) for the pre-heading chunk anchor.
fn file_stem(file: &str) -> String {
    let name = file.rsplit('/').next().unwrap_or(file);
    name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name).to_string()
}

/// Deterministic FNV-1a 64-bit content hash. Stable across runs (unlike
/// `DefaultHasher`), so the watcher's staleness check survives a restart.
fn content_hash(src: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in src.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
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
    walk_items(src, file, tree.root_node(), None, false, fg);
}

/// Recurse over a node's named children, emitting definitions (with
/// containment + doc edges), import edges, and call references. `parent` is the
/// enclosing symbol id (for `Contains`); `in_impl` makes functions count as
/// methods.
fn walk_items(
    src: &str,
    file: &str,
    node: Node,
    parent: Option<&str>,
    in_impl: bool,
    fg: &mut FileGraph,
) {
    let mut pending_doc: Vec<String> = Vec::new();
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
            "attribute_item" | "inner_attribute_item" => continue,
            _ => {}
        }

        if let Some((name, skind)) = def_name_kind(src, child, in_impl) {
            let start = child.start_position().row as u32 + 1;
            let end = child.end_position().row as u32 + 1;
            let id = symbol_id(file, &name, start);
            let doc = if pending_doc.is_empty() {
                None
            } else {
                Some(pending_doc.join("\n"))
            };

            fg.symbols.push(Symbol {
                id: id.clone(),
                name: name.clone(),
                kind: skind,
                file: file.to_string(),
                start_line: start,
                end_line: end,
                signature: signature_of(src, child),
                doc: doc.clone(),
            });
            if let Some(p) = parent {
                fg.edges.push(Edge {
                    kind: EdgeKind::Contains,
                    src: p.to_string(),
                    dst: id.clone(),
                });
            }
            if let Some(text) = doc {
                let cid = doc_chunk_id(file, &name);
                fg.docs.push(DocChunk {
                    id: cid.clone(),
                    source_path: file.to_string(),
                    anchor: name.clone(),
                    text,
                });
                fg.edges.push(Edge {
                    kind: EdgeKind::Documents,
                    src: cid,
                    dst: id.clone(),
                });
            }

            if matches!(skind, SymbolKind::Function | SymbolKind::Method) {
                collect_calls(src, file, child, &id, fg);
            }
            let child_in_impl = matches!(kind, "impl_item" | "trait_item");
            walk_items(src, file, child, Some(&id), child_in_impl, fg);
            pending_doc.clear();
        } else if kind == "use_declaration" {
            if let Some(path) = import_path(src, child) {
                fg.edges.push(Edge {
                    kind: EdgeKind::Import,
                    src: file.to_string(),
                    dst: path,
                });
            }
            pending_doc.clear();
        } else {
            // Descend for nested items (module bodies, impl bodies, etc.).
            walk_items(src, file, child, parent, in_impl, fg);
            pending_doc.clear();
        }
    }
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

/// Walk a function body's subtree and emit a `Call` reference + edge for every
/// call expression, attributed to the enclosing symbol `from_id`.
fn collect_calls(src: &str, file: &str, node: Node, from_id: &str, fg: &mut FileGraph) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "call_expression" {
            if let Some(callee) = callee_name(src, child) {
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
        // Recurse — but not into nested function definitions, whose calls
        // belong to them (they get their own `collect_calls` from the walk).
        if !matches!(
            child.kind(),
            "function_item" | "closure_expression" | "function_signature_item"
        ) {
            collect_calls(src, file, child, from_id, fg);
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
fn signature_of(src: &str, node: Node) -> String {
    let mut s = first_line(&node_text(src, node));
    s.truncate(MAX_SIGNATURE);
    s.trim_end().to_string()
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
        Some(
            t.trim_start_matches("/**")
                .trim_end_matches("*/")
                .trim()
                .to_string(),
        )
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
    walk_js(src, file, tree.root_node(), None, false, fg);
}

/// Recursive JS/TS item walk: declarations (functions, classes, methods,
/// interfaces, enums, type aliases, arrow-fn consts), import edges, call
/// references, JSDoc, and class→method containment.
fn walk_js(src: &str, file: &str, node: Node, parent: Option<&str>, in_class: bool, fg: &mut FileGraph) {
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

        if let Some((name, skind)) = js_def_name_kind(src, def_node, in_class) {
            let doc = take_doc(&mut pending_doc);
            let id = emit_symbol(src, file, def_node, &name, skind, parent, doc, fg);
            if matches!(skind, SymbolKind::Function | SymbolKind::Method) {
                collect_calls_js(src, file, def_node, &id, fg);
            }
            let child_in_class = matches!(def_node.kind(), "class_declaration" | "abstract_class_declaration");
            walk_js(src, file, def_node, Some(&id), child_in_class, fg);
            pending_doc.clear();
        } else if matches!(def_node.kind(), "lexical_declaration" | "variable_declaration") {
            // `const foo = () => {…}` / `const bar = function(){}` → a function.
            let doc = take_doc(&mut pending_doc);
            emit_js_var_functions(src, file, def_node, parent, doc, fg);
            pending_doc.clear();
        } else if kind == "import_statement" {
            if let Some(srcpath) = js_import_source(src, child) {
                fg.edges.push(Edge { kind: EdgeKind::Import, src: file.to_string(), dst: srcpath });
            }
            pending_doc.clear();
        } else {
            // Descend into wrappers (export of a name list, statement blocks,
            // class bodies, namespaces) keeping the enclosing symbol/parent.
            walk_js(src, file, def_node, parent, in_class, fg);
            pending_doc.clear();
        }
    }
}

/// Emit one symbol (+ containment + doc edges) and return its id.
fn emit_symbol(
    src: &str,
    file: &str,
    node: Node,
    name: &str,
    skind: SymbolKind,
    parent: Option<&str>,
    doc: Option<String>,
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
    });
    if let Some(p) = parent {
        fg.edges.push(Edge { kind: EdgeKind::Contains, src: p.to_string(), dst: id.clone() });
    }
    if let Some(text) = doc {
        let cid = doc_chunk_id(file, name);
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
fn emit_js_var_functions(
    src: &str,
    file: &str,
    decl: Node,
    parent: Option<&str>,
    doc: Option<String>,
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
        let id = emit_symbol(src, file, d, &name, SymbolKind::Function, parent, this_doc, fg);
        collect_calls_js(src, file, value, &id, fg);
        first = false;
    }
}

/// Emit a `Call` reference/edge for every call expression under `node`,
/// attributed to `from_id`. Doesn't descend into nested function definitions
/// (their calls are attributed to them).
fn collect_calls_js(src: &str, file: &str, node: Node, from_id: &str, fg: &mut FileGraph) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "call_expression" {
            if let Some(callee) = js_callee_name(src, child) {
                let pos = child.start_position();
                fg.references.push(Reference {
                    name: callee.clone(),
                    file: file.to_string(),
                    line: pos.row as u32 + 1,
                    col: pos.column as u32 + 1,
                    resolved_id: None,
                });
                fg.edges.push(Edge { kind: EdgeKind::Call, src: from_id.to_string(), dst: callee });
            }
        }
        if !matches!(
            child.kind(),
            "function_declaration" | "generator_function_declaration" | "function" | "arrow_function" | "method_definition"
        ) {
            collect_calls_js(src, file, child, from_id, fg);
        }
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
        let body = t.trim_start_matches("/**").trim_end_matches("*/");
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
    walk_py(src, file, tree.root_node(), None, false, fg);
}

/// Recursive Python walk: `def`/`class` definitions (incl. `@decorated`),
/// docstrings, import edges, call references, class→method containment.
fn walk_py(src: &str, file: &str, node: Node, parent: Option<&str>, in_class: bool, fg: &mut FileGraph) {
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
                let id = emit_symbol(src, file, def_node, &name, skind, parent, doc, fg);
                if matches!(skind, SymbolKind::Function | SymbolKind::Method) {
                    collect_calls_py(src, file, def_node, &id, fg);
                }
                let child_in_class = def_node.kind() == "class_definition";
                walk_py(src, file, def_node, Some(&id), child_in_class, fg);
            }
            "import_statement" | "import_from_statement" => {
                for m in py_import_modules(src, def_node) {
                    fg.edges.push(Edge { kind: EdgeKind::Import, src: file.to_string(), dst: m });
                }
            }
            _ => walk_py(src, file, def_node, parent, in_class, fg),
        }
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
    let t = raw
        .trim()
        .trim_start_matches(['r', 'b', 'u', 'R', 'B', 'U', 'f', 'F'])
        .trim_matches('"')
        .trim_matches('\'')
        .trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn collect_calls_py(src: &str, file: &str, node: Node, from_id: &str, fg: &mut FileGraph) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "call" {
            if let Some(callee) = py_callee_name(src, child) {
                let pos = child.start_position();
                fg.references.push(Reference {
                    name: callee.clone(),
                    file: file.to_string(),
                    line: pos.row as u32 + 1,
                    col: pos.column as u32 + 1,
                    resolved_id: None,
                });
                fg.edges.push(Edge { kind: EdgeKind::Call, src: from_id.to_string(), dst: callee });
            }
        }
        if !matches!(child.kind(), "function_definition" | "class_definition" | "lambda") {
            collect_calls_py(src, file, child, from_id, fg);
        }
    }
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
}

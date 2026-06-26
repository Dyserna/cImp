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

    if lang == Lang::Rust {
        parse_rust(src, path, &mut fg);
    }
    // Other languages: file row only for now (symbols land in Phase E).

    fg
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
}

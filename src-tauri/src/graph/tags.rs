//! V9-02: generic, query-driven symbol/call extraction.
//!
//! Where V9-01 hand-writes a bespoke walker per language (`parse_rust`,
//! `parse_js_ts`, `parse_python`), this module extracts symbols + calls from
//! *any* grammar that ships a tree-sitter `tags.scm`, with no per-language Rust.
//! A vendored query (under `src-tauri/queries/<lang>/tags.scm`) names the
//! definition/name/call captures; the engine maps them onto the shared
//! [`FileGraph`] schema (`Symbol`, `Contains`/`Call` `Edge`, `Reference`) and
//! reuses the existing name-based reference resolver downstream.
//!
//! Containment and call attribution are computed purely from **byte spans** —
//! the smallest enclosing definition is the parent / caller — so the logic is
//! entirely grammar-agnostic. Captures the engine doesn't understand
//! (`@reference.type`, `@doc`, …) are ignored, so vendoring an upstream tags
//! file verbatim is safe.

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

use crate::graph::builder::{char_col, emit_symbol, language_for};
use crate::graph::model::{symbol_id, Edge, EdgeKind, FileGraph, Lang, Reference, SymbolKind, Visibility};

/// A grammar + its vendored tags query.
pub(crate) struct TagSpec {
    pub lang: Lang,
    pub language: tree_sitter::Language,
    pub tags_query: &'static str,
}

/// The tags spec for a language driven by the generic engine, or `None` for
/// languages handled by a bespoke walker (Rust/TS/JS/Python), markup/data
/// languages (no symbol extraction), or `Other`.
pub(crate) fn tag_spec(lang: Lang) -> Option<TagSpec> {
    let tags_query = match lang {
        Lang::Go => include_str!("../../queries/go/tags.scm"),
        Lang::Java => include_str!("../../queries/java/tags.scm"),
        Lang::C => include_str!("../../queries/c/tags.scm"),
        Lang::Cpp => include_str!("../../queries/cpp/tags.scm"),
        Lang::CSharp => include_str!("../../queries/csharp/tags.scm"),
        Lang::Php => include_str!("../../queries/php/tags.scm"),
        Lang::Bash => include_str!("../../queries/bash/tags.scm"),
        Lang::Scala => include_str!("../../queries/scala/tags.scm"),
        Lang::Ocaml => include_str!("../../queries/ocaml/tags.scm"),
        Lang::Ruby => include_str!("../../queries/ruby/tags.scm"),
        Lang::Haskell => include_str!("../../queries/haskell/tags.scm"),
        Lang::Kotlin => include_str!("../../queries/kotlin/tags.scm"),
        Lang::Swift => include_str!("../../queries/swift/tags.scm"),
        Lang::Sql => include_str!("../../queries/sql/tags.scm"),
        Lang::R => include_str!("../../queries/r/tags.scm"),
        Lang::Perl => include_str!("../../queries/perl/tags.scm"),
        Lang::Ada => include_str!("../../queries/ada/tags.scm"),
        Lang::Erlang => include_str!("../../queries/erlang/tags.scm"),
        // Assembly: labels only — registered for struct-search, no tags query.
        _ => return None,
    };
    let language = language_for(lang)?;
    Some(TagSpec { lang, language, tags_query })
}

/// V12 Phase C **fallback** test detection for the generic tags engine: no
/// vendored `tags.scm` currently ships a `@definition.test` capture, so every
/// generic-tags language falls back to a path heuristic. The conventions are
/// scoped per language rather than applied globally — the `spec/` segment is
/// **Ruby-specific** (RSpec), and applying it language-agnostically wrongly
/// tagged every symbol in an unrelated `spec/` directory (OpenAPI / protocol
/// specs are common in Go/Kotlin/Swift/… projects). Kept broad: Go's `_test.go`
/// filename suffix, and the widely shared `tests/` directory layout. Applied to
/// Function/Method definitions only. A project that follows no convention simply
/// never sets the bit — accurate, not wrong, the same posture as
/// [`tags_visibility`]'s `Unknown`.
fn tags_is_test_path(lang: Lang, file: &str) -> bool {
    let lower = file.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if name.ends_with("_test.go") {
        return true;
    }
    let has_seg = |seg: &str| lower.split('/').any(|s| s == seg);
    match lang {
        // RSpec's `spec/` and minitest's `test/`, both Ruby conventions.
        Lang::Ruby => has_seg("tests") || has_seg("spec") || has_seg("test"),
        // `tests/` is the cross-language test-dir convention; `spec/` is NOT
        // applied here (it is not a test directory outside Ruby).
        _ => has_seg("tests"),
    }
}

/// Best-effort visibility for a generic-tags language. Only Go is decidable
/// cheaply from the name (exported iff the identifier starts uppercase); the
/// other grammars need modifier inspection we don't do yet, so they stay
/// `Unknown` — honest, and keeps them out of `dead_exports` rather than guessing.
fn tags_visibility(lang: Lang, name: &str) -> Visibility {
    match lang {
        Lang::Go => match name.chars().next() {
            Some(c) if c.is_uppercase() => Visibility::Public,
            Some(_) => Visibility::Private,
            None => Visibility::Unknown,
        },
        _ => Visibility::Unknown,
    }
}

/// Map a `@definition.<suffix>` capture suffix to a [`SymbolKind`].
fn kind_from_suffix(suffix: &str) -> SymbolKind {
    match suffix {
        "function" => SymbolKind::Function,
        "method" | "constructor" => SymbolKind::Method,
        "class" => SymbolKind::Class,
        "interface" | "protocol" => SymbolKind::Interface,
        "trait" => SymbolKind::Trait,
        "struct" | "object" | "union" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        "variant" | "enumerator" => SymbolKind::Variant,
        "module" | "namespace" | "package" => SymbolKind::Module,
        "type" => SymbolKind::TypeAlias,
        "field" | "property" | "member" | "variable" => SymbolKind::Field,
        "constant" => SymbolKind::Const,
        "macro" => SymbolKind::Macro,
        _ => SymbolKind::Other,
    }
}

/// One captured definition, before id/parent resolution.
struct Def<'a> {
    node: Node<'a>,
    name: String,
    kind: SymbolKind,
}

/// One captured call reference, attributed to a caller by byte position.
struct CallRef {
    name: String,
    byte: usize,
    line: u32,
    col: u32,
}

fn node_text(src: &str, node: Node) -> String {
    src.get(node.byte_range()).unwrap_or("").to_string()
}

/// Parse `src` and populate `fg` with symbols, containment, and call edges via
/// the language's tags query. A parse failure or an invalid query disables the
/// file gracefully (no symbols), never panics.
pub(crate) fn parse_with_tags(src: &str, file: &str, spec: &TagSpec, fg: &mut FileGraph) {
    let mut parser = Parser::new();
    if parser.set_language(&spec.language).is_err() {
        return;
    }
    let Some(tree) = parser.parse(src, None) else {
        return;
    };
    let query = match Query::new(&spec.language, spec.tags_query) {
        Ok(q) => q,
        Err(e) => {
            // A vendored query that doesn't compile against this grammar version
            // disables symbol extraction for the language but never crashes the
            // indexer; struct-search still works.
            tracing::warn!("graph: tags query failed to compile, symbols disabled: {e}");
            return;
        }
    };
    let names = query.capture_names();

    let mut defs: Vec<Def> = Vec::new();
    let mut calls: Vec<CallRef> = Vec::new();

    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), src.as_bytes());
    while let Some(m) = it.next() {
        let mut def: Option<(Node, SymbolKind)> = None;
        let mut name_node: Option<Node> = None;
        let mut is_call = false;
        for cap in m.captures {
            let cname = names[cap.index as usize];
            if let Some(suffix) = cname.strip_prefix("definition.") {
                def = Some((cap.node, kind_from_suffix(suffix)));
            } else if cname == "name" {
                name_node = Some(cap.node);
            } else if cname == "reference.call" {
                is_call = true;
            }
        }
        match (def, name_node) {
            (Some((dn, kind)), Some(nn)) => {
                let name = node_text(src, nn);
                let name = name.trim();
                if !name.is_empty() {
                    defs.push(Def { node: dn, name: name.to_string(), kind });
                }
            }
            (None, Some(nn)) if is_call => {
                let name = node_text(src, nn);
                let name = name.trim();
                if !name.is_empty() {
                    calls.push(CallRef {
                        name: name.to_string(),
                        byte: nn.start_byte(),
                        line: nn.start_position().row as u32 + 1,
                        // F26: character column, not tree-sitter's byte offset.
                        col: char_col(src, nn),
                    });
                }
            }
            _ => {}
        }
    }

    // Stable ids (deterministic from file + name + start line) and byte spans.
    let ids: Vec<String> = defs
        .iter()
        .map(|d| symbol_id(file, &d.name, d.node.start_position().row as u32 + 1))
        .collect();
    let spans: Vec<(usize, usize)> = defs.iter().map(|d| (d.node.start_byte(), d.node.end_byte())).collect();

    // Emit each symbol with its parent = the smallest *other* definition whose
    // span strictly contains it. Dedup ids so a def captured by two patterns
    // (or two same-named defs on one line) emits once.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (i, d) in defs.iter().enumerate() {
        let parent = smallest_container(&spans, spans[i].0, spans[i].1, Some(i), |_| true);
        if !seen.insert(ids[i].as_str()) {
            continue;
        }
        let parent_id = parent.map(|p| ids[p].as_str());
        let vis = tags_visibility(spec.lang, &d.name);
        let is_test =
            matches!(d.kind, SymbolKind::Function | SymbolKind::Method) && tags_is_test_path(spec.lang, file);
        emit_symbol(src, file, d.node, &d.name, d.kind, parent_id, None, vis, is_test, fg);
    }

    // Record every call as a reference (so `find references` sees it), then add
    // a Call edge attributed to the smallest enclosing function/method. A call
    // with no such enclosing definition (a top-level statement in a script) is
    // still a valid reference — it just has no caller edge.
    for c in &calls {
        fg.references.push(Reference {
            name: c.name.clone(),
            file: file.to_string(),
            line: c.line,
            col: c.col,
            resolved_id: None,
        });
        let caller = smallest_container(&spans, c.byte, c.byte + 1, None, |j| {
            matches!(defs[j].kind, SymbolKind::Function | SymbolKind::Method)
        });
        if let Some(j) = caller {
            fg.edges.push(Edge {
                kind: EdgeKind::Call,
                src: ids[j].clone(),
                dst: c.name.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::builder::parse_file;

    /// Every vendored tags query must compile against its grammar — this is the
    /// guard that catches a node/field name that drifted in a grammar update
    /// (the Haskell risk called out in the spec).
    #[test]
    fn every_vendored_query_compiles() {
        let langs = [
            Lang::Go, Lang::Java, Lang::C, Lang::Cpp, Lang::CSharp, Lang::Php,
            Lang::Bash, Lang::Scala, Lang::Ocaml, Lang::Ruby, Lang::Haskell,
            Lang::Kotlin, Lang::Swift, Lang::Sql, Lang::R, Lang::Perl,
            Lang::Ada, Lang::Erlang,
        ];
        for lang in langs {
            let spec = tag_spec(lang).unwrap_or_else(|| panic!("no tag_spec for {lang:?}"));
            Query::new(&spec.language, spec.tags_query)
                .unwrap_or_else(|e| panic!("{lang:?} tags.scm failed to compile: {e}"));
        }
    }

    fn names_of(fg: &FileGraph, kind: SymbolKind) -> Vec<String> {
        fg.symbols.iter().filter(|s| s.kind == kind).map(|s| s.name.clone()).collect()
    }
    fn call_targets(fg: &FileGraph) -> Vec<String> {
        fg.edges.iter().filter(|e| e.kind == EdgeKind::Call).map(|e| e.dst.clone()).collect()
    }

    #[test]
    fn go_fixture_symbols_and_calls() {
        const SRC: &str = r#"
package main

func Add(a int, b int) int {
    return helper(a) + b
}

func helper(x int) int {
    return x * 2
}

type Shape struct {
    sides int
}

func (s Shape) Area() int {
    return compute(s.sides)
}
"#;
        let fg = parse_file("x.go", SRC, Lang::Go);
        let funcs = names_of(&fg, SymbolKind::Function);
        assert!(funcs.contains(&"Add".to_string()), "funcs: {funcs:?}");
        assert!(funcs.contains(&"helper".to_string()), "funcs: {funcs:?}");
        assert!(names_of(&fg, SymbolKind::Method).contains(&"Area".to_string()));
        assert!(names_of(&fg, SymbolKind::TypeAlias).contains(&"Shape".to_string()));
        let calls = call_targets(&fg);
        assert!(calls.contains(&"helper".to_string()), "calls: {calls:?}");
        assert!(calls.contains(&"compute".to_string()), "calls: {calls:?}");
    }

    #[test]
    fn ruby_fixture_symbols_and_calls() {
        const SRC: &str = r#"
class Greeter
  def greet(name)
    format(name)
  end
end
"#;
        let fg = parse_file("x.rb", SRC, Lang::Ruby);
        assert!(names_of(&fg, SymbolKind::Class).contains(&"Greeter".to_string()));
        assert!(names_of(&fg, SymbolKind::Method).contains(&"greet".to_string()));
        // `greet` should be Contained by `Greeter`.
        let class_id = symbol_id("x.rb", "Greeter", fg.symbols.iter()
            .find(|s| s.name == "Greeter").unwrap().start_line);
        assert!(fg.edges.iter().any(|e| e.kind == EdgeKind::Contains && e.src == class_id),
            "expected Greeter to contain a method");
        assert!(call_targets(&fg).contains(&"format".to_string()));
    }

    #[test]
    fn kotlin_fixture_symbols() {
        const SRC: &str = r#"
class Greeter {
    fun greet(name: String): String {
        return name
    }
}

fun main() {
    println("hi")
}
"#;
        let fg = parse_file("X.kt", SRC, Lang::Kotlin);
        assert!(names_of(&fg, SymbolKind::Class).contains(&"Greeter".to_string()));
        let funcs = names_of(&fg, SymbolKind::Function);
        assert!(funcs.contains(&"greet".to_string()), "funcs: {funcs:?}");
        assert!(funcs.contains(&"main".to_string()), "funcs: {funcs:?}");
    }

    #[test]
    fn swift_fixture_symbols() {
        const SRC: &str = r#"
class Calculator {
    func add(a: Int, b: Int) -> Int {
        return a + b
    }
}

func run() {
    print("hi")
}
"#;
        let fg = parse_file("X.swift", SRC, Lang::Swift);
        assert!(names_of(&fg, SymbolKind::Class).contains(&"Calculator".to_string()));
        // Methods are classified as functions in the trimmed Swift query.
        let funcs = names_of(&fg, SymbolKind::Function);
        assert!(funcs.contains(&"add".to_string()), "funcs: {funcs:?}");
        assert!(funcs.contains(&"run".to_string()), "funcs: {funcs:?}");
    }

    #[test]
    fn c_fixture_body_span_and_calls() {
        // Regression: @definition.function must span the body so a call inside
        // it attributes to the caller (the upstream declarator-only capture
        // excluded the body and dropped all C/C++ call edges).
        const SRC: &str = "int add(int a, int b) {\n    return helper(a) + b;\n}\n";
        let fg = parse_file("x.c", SRC, Lang::C);
        let add = fg.symbols.iter().find(|s| s.name == "add").expect("add symbol");
        assert!(add.end_line >= 3, "end_line should cover the body, got {}", add.end_line);
        assert!(call_targets(&fg).contains(&"helper".to_string()), "calls: {:?}", call_targets(&fg));
    }

    #[test]
    fn cpp_fixture_symbols_and_calls() {
        const SRC: &str = "struct Shape { int n; };\nint area(Shape s) {\n    return compute(s.n);\n}\n";
        let fg = parse_file("x.cpp", SRC, Lang::Cpp);
        assert!(names_of(&fg, SymbolKind::Function).contains(&"area".to_string()));
        assert!(call_targets(&fg).contains(&"compute".to_string()), "calls: {:?}", call_targets(&fg));
    }

    #[test]
    fn top_level_call_recorded_as_reference() {
        // A call with no enclosing function is still a reference (find-references),
        // just without a caller edge.
        const SRC: &str = "helper()\n";
        let fg = parse_file("x.rb", SRC, Lang::Ruby);
        assert!(fg.references.iter().any(|r| r.name == "helper"),
            "top-level call should be a reference");
    }

    #[test]
    fn java_fixture_symbols() {
        const SRC: &str = r#"
class Calculator {
    int add(int a, int b) {
        return compute(a, b);
    }
}
"#;
        let fg = parse_file("X.java", SRC, Lang::Java);
        assert!(names_of(&fg, SymbolKind::Class).contains(&"Calculator".to_string()));
        assert!(names_of(&fg, SymbolKind::Method).contains(&"add".to_string()));
        assert!(call_targets(&fg).contains(&"compute".to_string()));
    }

    #[test]
    fn test_path_heuristic_scopes_spec_to_ruby() {
        // F3: `spec/` is RSpec-only — it must not tag non-Ruby files (OpenAPI /
        // protocol `spec/` dirs are common in Go/Swift/Kotlin projects).
        assert!(!tags_is_test_path(Lang::Go, "api/spec/openapi.go"));
        assert!(!tags_is_test_path(Lang::Swift, "sources/spec/model.swift"));
        assert!(tags_is_test_path(Lang::Ruby, "spec/models/user_spec.rb"));
        // `tests/` is the shared cross-language convention.
        assert!(tags_is_test_path(Lang::Java, "tests/footest.java"));
        assert!(tags_is_test_path(Lang::Go, "tests/integration.go"));
        // Go's own `_test.go` suffix works anywhere.
        assert!(tags_is_test_path(Lang::Go, "pkg/foo_test.go"));
        // A plain source file is not a test.
        assert!(!tags_is_test_path(Lang::Go, "pkg/foo.go"));
    }
}

/// Index of the smallest span in `spans` that contains `[s, e)`, excluding
/// `exclude` and any span equal to `[s, e)`, and satisfying `accept`.
fn smallest_container(
    spans: &[(usize, usize)],
    s: usize,
    e: usize,
    exclude: Option<usize>,
    accept: impl Fn(usize) -> bool,
) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (j, &(ps, pe)) in spans.iter().enumerate() {
        if Some(j) == exclude || !(ps <= s && e <= pe) || (ps, pe) == (s, e) || !accept(j) {
            continue;
        }
        match best {
            Some(b) => {
                let (bs, be) = spans[b];
                if pe - ps < be - bs {
                    best = Some(j);
                }
            }
            None => best = Some(j),
        }
    }
    best
}

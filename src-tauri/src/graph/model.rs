//! The language-independent intermediate representation produced by the
//! parser (`builder`) and consumed by the store (`index`/`schema`). Keeping
//! extraction decoupled from storage means the tree-sitter front end and the
//! CozoDB back end can be built and tested independently.
//!
//! These types map 1:1 onto the CozoDB relations described in the milestone
//! doc (`file` / `symbol` / `ref` / `edge_*` / `doc_chunk`).

// Most of these are consumed by later stages (builder, index, query). Allow
// dead code while the module is being built out; the `pub use` re-exports in
// `mod.rs` will light them up as each stage lands.
#![allow(dead_code)]

use std::path::Path;

/// A source language ccImp knows how to index. `Other` is parsed-but-skipped
/// (logged, never fatal) so an unknown extension can't break a build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Markdown,
    Other,
}

impl Lang {
    /// Best-effort language detection from a path's extension.
    pub fn from_path(path: &Path) -> Lang {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("rs") => Lang::Rust,
            Some("ts") | Some("tsx") | Some("mts") | Some("cts") => Lang::TypeScript,
            Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => Lang::JavaScript,
            Some("py") | Some("pyi") => Lang::Python,
            Some("md") | Some("markdown") => Lang::Markdown,
            _ => Lang::Other,
        }
    }

    /// Stable lowercase tag stored in the `file.lang` column and matched
    /// against `GraphSettings::languages`.
    pub fn tag(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::TypeScript => "typescript",
            Lang::JavaScript => "javascript",
            Lang::Python => "python",
            Lang::Markdown => "markdown",
            Lang::Other => "other",
        }
    }
}

/// The kind of a definition. Broad enough to cover the MVP languages; the
/// parser maps each grammar's node kinds onto these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Impl,
    Class,
    Interface,
    Const,
    Static,
    Module,
    TypeAlias,
    Macro,
    Field,
    Variant,
    Other,
}

impl SymbolKind {
    pub fn tag(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Impl => "impl",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "interface",
            SymbolKind::Const => "const",
            SymbolKind::Static => "static",
            SymbolKind::Module => "module",
            SymbolKind::TypeAlias => "type_alias",
            SymbolKind::Macro => "macro",
            SymbolKind::Field => "field",
            SymbolKind::Variant => "variant",
            SymbolKind::Other => "other",
        }
    }
}

/// The kind of an edge between graph nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    /// `caller` invokes `callee` (callee may be an unresolved name).
    Call,
    /// A file imports a module / symbol.
    Import,
    /// Structural containment (module → fn, impl → method, struct → field).
    Contains,
    /// A doc chunk documents a symbol or file.
    Documents,
}

impl EdgeKind {
    pub fn tag(self) -> &'static str {
        match self {
            EdgeKind::Call => "call",
            EdgeKind::Import => "import",
            EdgeKind::Contains => "contains",
            EdgeKind::Documents => "documents",
        }
    }
}

/// A definition site. `id` is stable across re-indexes of the same file as
/// long as the definition keeps its name and start line — see [`symbol_id`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub id: String,
    pub name: String,
    pub kind: SymbolKind,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    /// A one-line signature/header for display (e.g. the `fn` line).
    pub signature: String,
    /// Associated doc-comment text, if any.
    pub doc: Option<String>,
}

/// A reference (use site) of a name. `resolved_id` is `Some` when the parser
/// or stack-graphs bound it to a concrete [`Symbol::id`]; otherwise the edge
/// is name-only and the consumer is told the confidence is approximate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub resolved_id: Option<String>,
}

/// A directed edge. `src`/`dst` are symbol ids except where the milestone
/// schema stores a name (an unresolved call target, or an import module path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub kind: EdgeKind,
    pub src: String,
    pub dst: String,
}

/// A chunk of documentation (a markdown section or a doc-comment) linked to
/// the code it describes via a `Documents` edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocChunk {
    pub id: String,
    pub source_path: String,
    /// A within-file anchor (heading slug, or the documented symbol's name).
    pub anchor: String,
    pub text: String,
}

/// Everything extracted from a single file in one parse. This is the unit the
/// builder produces and the index writes transactionally (delete-then-insert
/// by `path`), so a re-index of one file never touches another's rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileGraph {
    pub path: String,
    pub lang_tag: String,
    /// Content hash, used for staleness detection by the watcher.
    pub hash: String,
    pub symbols: Vec<Symbol>,
    pub references: Vec<Reference>,
    pub edges: Vec<Edge>,
    pub docs: Vec<DocChunk>,
}

/// Build a stable symbol id from its file, name, and start line. Stable across
/// edits elsewhere in the file (so unrelated re-indexes don't churn ids) but
/// distinct per definition. Two same-named definitions can't start on the same
/// line of the same file, so this is collision-free in practice.
pub fn symbol_id(file: &str, name: &str, start_line: u32) -> String {
    format!("{file}#{name}@{start_line}")
}

/// A stable doc-chunk id from its source path and anchor.
pub fn doc_chunk_id(source_path: &str, anchor: &str) -> String {
    format!("{source_path}#doc:{anchor}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn lang_from_path_covers_mvp_languages() {
        assert_eq!(Lang::from_path(&PathBuf::from("a/b.rs")), Lang::Rust);
        assert_eq!(Lang::from_path(&PathBuf::from("a/b.TS")), Lang::TypeScript);
        assert_eq!(Lang::from_path(&PathBuf::from("a/b.tsx")), Lang::TypeScript);
        assert_eq!(Lang::from_path(&PathBuf::from("a/b.mjs")), Lang::JavaScript);
        assert_eq!(Lang::from_path(&PathBuf::from("a/b.py")), Lang::Python);
        assert_eq!(Lang::from_path(&PathBuf::from("README.md")), Lang::Markdown);
        assert_eq!(Lang::from_path(&PathBuf::from("a/b.bin")), Lang::Other);
        assert_eq!(Lang::from_path(&PathBuf::from("noext")), Lang::Other);
    }

    #[test]
    fn symbol_id_is_stable_and_distinct() {
        let a = symbol_id("src/main.rs", "build_pre_args", 120);
        assert_eq!(a, symbol_id("src/main.rs", "build_pre_args", 120));
        assert_ne!(a, symbol_id("src/main.rs", "build_pre_args", 121));
        assert_ne!(a, symbol_id("src/lib.rs", "build_pre_args", 120));
    }

    #[test]
    fn tags_are_lowercase_and_nonempty() {
        for k in [SymbolKind::Function, SymbolKind::Struct, SymbolKind::Impl] {
            assert!(!k.tag().is_empty());
            assert_eq!(k.tag(), k.tag().to_ascii_lowercase());
        }
        assert_eq!(Lang::Rust.tag(), "rust");
        assert_eq!(EdgeKind::Call.tag(), "call");
    }
}

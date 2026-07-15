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

/// A source language cImp knows how to index. `Other` is parsed-but-skipped
/// (logged, never fatal) so an unknown extension can't break a build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Markdown,
    // V9-02 multi-language fan-out. Tier 1 (code): full symbol/call graph via
    // the generic tags engine. Tier 2/3 (markup/data): struct-search + anchors.
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Php,
    Bash,
    Scala,
    Ocaml,
    Ruby,
    Haskell,
    Html,
    Css,
    Json,
    Kotlin,
    Swift,
    Sql,
    Yaml,
    Xml,
    Erlang,
    R,
    Perl,
    Ada,
    Asm,
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
            Some("go") => Lang::Go,
            Some("java") => Lang::Java,
            Some("c") => Lang::C,
            // `.h` is ambiguous C/C++; the C++ grammar is a near-superset that
            // parses C headers fine while also handling class/template/namespace
            // in C++ headers, so it's the safer default for mixed projects.
            Some("cc") | Some("cpp") | Some("cxx") | Some("hpp") | Some("hh") | Some("hxx")
            | Some("h") => Lang::Cpp,
            Some("cs") => Lang::CSharp,
            Some("php") | Some("phtml") => Lang::Php,
            Some("sh") | Some("bash") | Some("zsh") => Lang::Bash,
            Some("scala") | Some("sc") => Lang::Scala,
            // Only `.ml` (implementation). `.mli` interface files need the
            // distinct OCaml *interface* grammar; parsing them with the impl
            // grammar yields ERROR nodes, so they're left unindexed until a
            // dedicated interface variant is added.
            Some("ml") => Lang::Ocaml,
            Some("rb") => Lang::Ruby,
            Some("hs") => Lang::Haskell,
            Some("html") | Some("htm") => Lang::Html,
            Some("css") => Lang::Css,
            Some("json") | Some("jsonc") => Lang::Json,
            Some("kt") | Some("kts") => Lang::Kotlin,
            Some("swift") => Lang::Swift,
            Some("sql") => Lang::Sql,
            Some("yaml") | Some("yml") => Lang::Yaml,
            Some("xml") | Some("xsd") | Some("xsl") | Some("xslt") | Some("svg") => Lang::Xml,
            Some("erl") | Some("hrl") => Lang::Erlang,
            Some("r") => Lang::R,
            Some("pl") | Some("pm") => Lang::Perl,
            Some("adb") | Some("ads") => Lang::Ada,
            Some("asm") | Some("s") => Lang::Asm,
            _ => Lang::Other,
        }
    }

    /// Inverse of [`Self::tag`] — resolve a stored/CLI lang tag back to a
    /// `Lang` (`Other` for anything unrecognized).
    pub fn from_tag(tag: &str) -> Lang {
        match tag.trim().to_ascii_lowercase().as_str() {
            "rust" => Lang::Rust,
            "typescript" => Lang::TypeScript,
            "javascript" => Lang::JavaScript,
            "python" => Lang::Python,
            "markdown" => Lang::Markdown,
            "go" => Lang::Go,
            "java" => Lang::Java,
            "c" => Lang::C,
            "cpp" => Lang::Cpp,
            "csharp" => Lang::CSharp,
            "php" => Lang::Php,
            "bash" => Lang::Bash,
            "scala" => Lang::Scala,
            "ocaml" => Lang::Ocaml,
            "ruby" => Lang::Ruby,
            "haskell" => Lang::Haskell,
            "html" => Lang::Html,
            "css" => Lang::Css,
            "json" => Lang::Json,
            "kotlin" => Lang::Kotlin,
            "swift" => Lang::Swift,
            "sql" => Lang::Sql,
            "yaml" => Lang::Yaml,
            "xml" => Lang::Xml,
            "erlang" => Lang::Erlang,
            "r" => Lang::R,
            "perl" => Lang::Perl,
            "ada" => Lang::Ada,
            "asm" => Lang::Asm,
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
            Lang::Go => "go",
            Lang::Java => "java",
            Lang::C => "c",
            Lang::Cpp => "cpp",
            Lang::CSharp => "csharp",
            Lang::Php => "php",
            Lang::Bash => "bash",
            Lang::Scala => "scala",
            Lang::Ocaml => "ocaml",
            Lang::Ruby => "ruby",
            Lang::Haskell => "haskell",
            Lang::Html => "html",
            Lang::Css => "css",
            Lang::Json => "json",
            Lang::Kotlin => "kotlin",
            Lang::Swift => "swift",
            Lang::Sql => "sql",
            Lang::Yaml => "yaml",
            Lang::Xml => "xml",
            Lang::Erlang => "erlang",
            Lang::R => "r",
            Lang::Perl => "perl",
            Lang::Ada => "ada",
            Lang::Asm => "asm",
            Lang::Other => "other",
        }
    }

    /// Human-facing display name (proper casing/punctuation) for the language
    /// buttons in the Code Graph tab. Distinct from [`Self::tag`], which is the
    /// stable lowercase storage/config key.
    pub fn label(self) -> &'static str {
        match self {
            Lang::Rust => "Rust",
            Lang::TypeScript => "TypeScript",
            Lang::JavaScript => "JavaScript",
            Lang::Python => "Python",
            Lang::Markdown => "Markdown",
            Lang::Go => "Go",
            Lang::Java => "Java",
            Lang::C => "C",
            Lang::Cpp => "C++",
            Lang::CSharp => "C#",
            Lang::Php => "PHP",
            Lang::Bash => "Bash",
            Lang::Scala => "Scala",
            Lang::Ocaml => "OCaml",
            Lang::Ruby => "Ruby",
            Lang::Haskell => "Haskell",
            Lang::Html => "HTML",
            Lang::Css => "CSS",
            Lang::Json => "JSON",
            Lang::Kotlin => "Kotlin",
            Lang::Swift => "Swift",
            Lang::Sql => "SQL",
            Lang::Yaml => "YAML",
            Lang::Xml => "XML",
            Lang::Erlang => "Erlang",
            Lang::R => "R",
            Lang::Perl => "Perl",
            Lang::Ada => "Ada",
            Lang::Asm => "Assembly",
            Lang::Other => "Other",
        }
    }
}

/// Display name for a well-known **programming language** that cImp's graph
/// engine does *not* support, keyed by file extension. Returns `(slug, label)`
/// for the Code Graph tab's red buttons; `None` for data/config/markup/unknown
/// files, which fold into the tab's single "Other" bucket. Only genuine
/// programming languages are named here on purpose — everything else stays
/// "Other" rather than sprouting a chip per stray extension.
pub fn unsupported_lang_name(path: &Path) -> Option<(&'static str, &'static str)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    let named = match ext.as_str() {
        "lua" => ("lua", "Lua"),
        "zig" => ("zig", "Zig"),
        "ex" | "exs" => ("elixir", "Elixir"),
        "dart" => ("dart", "Dart"),
        "jl" => ("julia", "Julia"),
        "clj" | "cljs" | "cljc" => ("clojure", "Clojure"),
        "nim" | "nims" => ("nim", "Nim"),
        "cr" => ("crystal", "Crystal"),
        "groovy" | "gradle" => ("groovy", "Groovy"),
        "vala" => ("vala", "Vala"),
        "fs" | "fsx" | "fsi" => ("fsharp", "F#"),
        "vb" => ("vbnet", "Visual Basic"),
        "mm" => ("objcpp", "Objective-C++"),
        "f" | "for" | "f90" | "f95" | "f03" | "f08" => ("fortran", "Fortran"),
        "cob" | "cbl" => ("cobol", "COBOL"),
        "pas" => ("pascal", "Pascal"),
        "rkt" => ("racket", "Racket"),
        "lisp" | "lsp" => ("lisp", "Lisp"),
        "el" => ("elisp", "Emacs Lisp"),
        "elm" => ("elm", "Elm"),
        "hx" => ("haxe", "Haxe"),
        "d" => ("d", "D"),
        "vhd" | "vhdl" => ("vhdl", "VHDL"),
        "tcl" => ("tcl", "Tcl"),
        "sol" => ("solidity", "Solidity"),
        "ps1" | "psm1" | "psd1" => ("powershell", "PowerShell"),
        "bat" | "cmd" => ("batch", "Batch"),
        "coffee" => ("coffeescript", "CoffeeScript"),
        "re" => ("reason", "Reason"),
        "res" => ("rescript", "ReScript"),
        "purs" => ("purescript", "PureScript"),
        _ => return None,
    };
    Some(named)
}

/// Whether a definition is visible outside its own module/file. Feeds
/// `dead_exports` (only genuinely public, unused symbols are candidates) and the
/// context ranker. `Unknown` is the honest default for languages whose walker
/// can't yet tell — it's treated as "don't claim it's dead", never as public.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
    /// Rust `pub(crate)`/`pub(super)`/`pub(in …)` — visible within the crate but
    /// not the public API.
    Crate,
    Unknown,
}

impl Visibility {
    /// Stable lowercase tag stored in the `symbol.visibility` column.
    pub fn tag(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
            Visibility::Crate => "crate",
            Visibility::Unknown => "unknown",
        }
    }

    /// Inverse of [`Self::tag`] — resolve a stored tag back to a `Visibility`
    /// (`Unknown` for anything unrecognized).
    pub fn from_tag(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "public" => Visibility::Public,
            "private" => Visibility::Private,
            "crate" => Visibility::Crate,
            _ => Visibility::Unknown,
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

/// How certain the graph is that an edge/reference points where it claims —
/// V15 Feature 3, the "honest about what it knows" layer. A same-file call the
/// parser resolved directly is not the same fact as a cross-file name-collision
/// guess, and consumers (impact, callers, paths) must be able to tell them
/// apart. Populated at parse time (`Extracted` vs `Inferred`, from same-file
/// evidence); `Ambiguous` is applied at query time, the only place a name's
/// global candidate count is visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Confidence {
    /// The parser is certain: a same-file definition (or a structural
    /// containment/import the grammar produced directly). One unambiguous target.
    Extracted,
    /// A cross-file, name-keyed resolution with a single candidate but no local
    /// proof. The honest default whenever certainty can't be established —
    /// never silently upgraded to `Extracted`.
    #[default]
    Inferred,
    /// The name resolves to more than one candidate symbol; we picked/reported
    /// one, but callers/callees/paths here are a superset. Assigned at query time.
    Ambiguous,
}

impl Confidence {
    /// Stable lowercase tag stored in the `edge.confidence` / `ref.confidence`
    /// columns and surfaced in tool output badges.
    pub fn tag(self) -> &'static str {
        match self {
            Confidence::Extracted => "extracted",
            Confidence::Inferred => "inferred",
            Confidence::Ambiguous => "ambiguous",
        }
    }

    /// Inverse of [`Self::tag`] — resolve a stored tag back to a `Confidence`
    /// (`Inferred`, the honest default, for anything unrecognized).
    pub fn from_tag(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "extracted" => Confidence::Extracted,
            "ambiguous" => Confidence::Ambiguous,
            _ => Confidence::Inferred,
        }
    }

    /// Strict parse for user/LLM-supplied tags: `None` on anything
    /// unrecognized so the caller can reject it with a clear error, rather
    /// than silently coercing (as [`Self::from_tag`] does for stored tags).
    pub fn parse_tag(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "extracted" => Some(Confidence::Extracted),
            "inferred" => Some(Confidence::Inferred),
            "ambiguous" => Some(Confidence::Ambiguous),
            _ => None,
        }
    }

    /// Certainty rank, higher = more certain. `Ambiguous` (0) is the weakest,
    /// `Extracted` (2) the strongest. Used to combine confidences along a chain.
    pub fn rank(self) -> u8 {
        match self {
            Confidence::Ambiguous => 0,
            Confidence::Inferred => 1,
            Confidence::Extracted => 2,
        }
    }

    /// The weaker (less certain) of two confidences — a chain of edges is only
    /// as trustworthy as its least-certain link.
    pub fn weaker(self, other: Confidence) -> Confidence {
        if self.rank() <= other.rank() {
            self
        } else {
            other
        }
    }

    /// The stronger (more certain) of two confidences.
    pub fn stronger(self, other: Confidence) -> Confidence {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }

    /// The natural default confidence for an edge of `kind` at parse time.
    /// Structural edges the grammar emits directly (`Contains`), explicit
    /// import statements, and doc-linkage are `Extracted`; name-keyed `Call`
    /// edges start `Inferred` and are upgraded to `Extracted` by
    /// [`FileGraph::classify_confidence`] only with same-file proof.
    pub fn default_for(kind: EdgeKind) -> Self {
        match kind {
            EdgeKind::Call => Confidence::Inferred,
            EdgeKind::Import | EdgeKind::Contains | EdgeKind::Documents => Confidence::Extracted,
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
    /// Whether the definition is externally visible (drives dead-export
    /// detection). `Unknown` for languages whose walker can't tell yet.
    pub visibility: Visibility,
    /// Whether this definition IS a test (V12 Phase C) — feeds
    /// `GraphIndex::tests_for`/`graph_tests_for`. `false` is the honest default
    /// for languages/constructs whose walker has no test signal, same posture
    /// as `Visibility::Unknown`: never claim a test that isn't one.
    pub is_test: bool,
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
    /// How certain this reference is (V15 Feature 3). `Inferred` at
    /// construction; upgraded to `Extracted` by [`FileGraph::classify_confidence`]
    /// when `name` is defined in the same file.
    pub confidence: Confidence,
}

/// A directed edge. `src`/`dst` are symbol ids except where the milestone
/// schema stores a name (an unresolved call target, or an import module path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub kind: EdgeKind,
    pub src: String,
    pub dst: String,
    /// How certain this edge is (V15 Feature 3). Set from
    /// [`Confidence::default_for`] at construction; same-file `Call` edges are
    /// upgraded to `Extracted` by [`FileGraph::classify_confidence`].
    pub confidence: Confidence,
}

impl Edge {
    /// An edge with the honest default confidence for its kind
    /// ([`Confidence::default_for`]). Structural/import/doc edges are
    /// `Extracted`; `Call` edges start `Inferred`.
    pub fn new(kind: EdgeKind, src: impl Into<String>, dst: impl Into<String>) -> Self {
        Edge {
            kind,
            src: src.into(),
            dst: dst.into(),
            confidence: Confidence::default_for(kind),
        }
    }
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

/// A chunk of a symbol's own source (signature + doc + body), embedded for
/// semantic **code** search (V11 Phase G) — the `DocChunk` twin, but over a
/// definition's implementation rather than prose. `id` is the owning symbol's
/// id, so `code_vec.chunk_id` joins straight back to `symbol` with no extra
/// join table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeChunk {
    pub id: String,
    pub file: String,
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
    pub code_chunks: Vec<CodeChunk>,
}

impl FileGraph {
    /// V15 Feature 3: upgrade name-keyed `Call` edges and `Reference`s to
    /// `Extracted` when their target name is **defined in this same file** —
    /// the one cross-reference the parser can prove locally. Everything else
    /// stays at its constructed confidence (`Inferred` for calls/refs, already
    /// `Extracted` for structural/import edges). `Ambiguous` is deliberately
    /// *not* decided here: a name's global candidate count isn't visible from a
    /// single file, so it's applied at query time instead.
    ///
    /// Called once by the builder after a file is fully parsed, so it sees the
    /// complete same-file symbol set regardless of walk order. Idempotent.
    pub fn classify_confidence(&mut self) {
        let local: std::collections::HashSet<&str> =
            self.symbols.iter().map(|s| s.name.as_str()).collect();
        let local_ids: std::collections::HashSet<&str> =
            self.symbols.iter().map(|s| s.id.as_str()).collect();
        for e in &mut self.edges {
            if e.kind == EdgeKind::Call && local.contains(e.dst.as_str()) {
                e.confidence = Confidence::Extracted;
            }
        }
        for r in &mut self.references {
            match &r.resolved_id {
                // Already bound to a concrete symbol: it's only a certain
                // same-file target if that symbol lives in THIS file. A
                // reference stack-graphs resolved to another file must not be
                // promoted just because a local symbol happens to share its
                // name — that would defeat the honesty the confidence layer
                // exists to provide.
                Some(id) => {
                    if local_ids.contains(id.as_str()) {
                        r.confidence = Confidence::Extracted;
                    }
                }
                // Name-only reference: a same-file definition is the one target
                // the parser can prove locally.
                None => {
                    if local.contains(r.name.as_str()) {
                        r.confidence = Confidence::Extracted;
                    }
                }
            }
        }
    }
}

/// Build a stable symbol id from its file, name, and start line. Stable across
/// edits elsewhere in the file (so unrelated re-indexes don't churn ids) but
/// distinct per definition. Two same-named definitions can't start on the same
/// line of the same file, so this is collision-free in practice.
pub fn symbol_id(file: &str, name: &str, start_line: u32) -> String {
    format!("{file}#{name}@{start_line}")
}

/// A stable doc-chunk id from its source path and anchor. The anchor must be
/// unique within the file (markdown slugs are de-duplicated by the builder);
/// for a symbol's doc-comment use [`symbol_doc_chunk_id`], which folds in the
/// start line so two same-named definitions in one file don't collide.
pub fn doc_chunk_id(source_path: &str, anchor: &str) -> String {
    format!("{source_path}#doc:{anchor}")
}

/// A doc-chunk id for a symbol's doc-comment, disambiguated by start line.
/// Without the line, two same-named definitions in one file (e.g. `fn new()`
/// in two `impl` blocks) would map to the same id and the second's
/// `:put doc_chunk` would silently overwrite the first — losing the first's
/// doc and mis-pointing its `Documents` edge. The display anchor stays the
/// bare name; only the storage key is disambiguated.
pub fn symbol_doc_chunk_id(source_path: &str, name: &str, start_line: u32) -> String {
    format!("{source_path}#doc:{name}@{start_line}")
}

/// Deterministic FNV-1a 64-bit hash of `s`, lowercase hex. Stable across runs
/// (unlike `DefaultHasher`), so a content hash survives a restart. Shared by
/// the builder's per-file content hash and the index's per-chunk text hash so
/// the two can never disagree on what "changed".
pub fn fnv1a_hex(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
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

    #[test]
    fn labels_are_human_readable_and_tags_round_trip() {
        assert_eq!(Lang::Cpp.label(), "C++");
        assert_eq!(Lang::CSharp.label(), "C#");
        assert_eq!(Lang::Asm.label(), "Assembly");
        // Every supported tag must round-trip tag → from_tag → same variant, so
        // the census can rebuild a Lang from a stored tag to get its label.
        for l in [
            Lang::Rust,
            Lang::TypeScript,
            Lang::Cpp,
            Lang::CSharp,
            Lang::Ada,
        ] {
            assert_eq!(Lang::from_tag(l.tag()).label(), l.label());
        }
    }

    #[test]
    fn classify_confidence_upgrades_only_same_file_targets() {
        let mut fg = FileGraph::default();
        fg.symbols.push(Symbol {
            id: symbol_id("m.rs", "foo", 1),
            name: "foo".into(),
            kind: SymbolKind::Function,
            file: "m.rs".into(),
            start_line: 1,
            end_line: 2,
            signature: "fn foo()".into(),
            doc: None,
            visibility: Visibility::Private,
            is_test: false,
        });
        // A call to a same-file def, a call to a cross-file name, and a
        // structural containment edge.
        fg.edges.push(Edge::new(EdgeKind::Call, "caller", "foo")); // same-file → Extracted
        fg.edges.push(Edge::new(EdgeKind::Call, "caller", "bar")); // cross-file → stays Inferred
        fg.edges
            .push(Edge::new(EdgeKind::Contains, "caller", "foo")); // structural → Extracted
        fg.references.push(Reference {
            name: "foo".into(),
            file: "m.rs".into(),
            line: 3,
            col: 1,
            resolved_id: None,
            confidence: Confidence::Inferred,
        });
        fg.references.push(Reference {
            name: "bar".into(),
            file: "m.rs".into(),
            line: 4,
            col: 1,
            resolved_id: None,
            confidence: Confidence::Inferred,
        });

        fg.classify_confidence();

        let call_foo = fg
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Call && e.dst == "foo")
            .unwrap();
        let call_bar = fg
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Call && e.dst == "bar")
            .unwrap();
        assert_eq!(
            call_foo.confidence,
            Confidence::Extracted,
            "same-file call is Extracted"
        );
        assert_eq!(
            call_bar.confidence,
            Confidence::Inferred,
            "cross-file call stays Inferred"
        );
        // Structural edges are Extracted from construction and untouched.
        let contains = fg
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Contains)
            .unwrap();
        assert_eq!(contains.confidence, Confidence::Extracted);
        // References mirror the same rule.
        assert_eq!(fg.references[0].confidence, Confidence::Extracted);
        assert_eq!(fg.references[1].confidence, Confidence::Inferred);
    }

    #[test]
    fn confidence_rank_orders_and_combines() {
        assert!(Confidence::Extracted.rank() > Confidence::Inferred.rank());
        assert!(Confidence::Inferred.rank() > Confidence::Ambiguous.rank());
        assert_eq!(
            Confidence::Extracted.weaker(Confidence::Inferred),
            Confidence::Inferred
        );
        assert_eq!(
            Confidence::Inferred.weaker(Confidence::Ambiguous),
            Confidence::Ambiguous
        );
        assert_eq!(
            Confidence::Inferred.stronger(Confidence::Ambiguous),
            Confidence::Inferred
        );
        assert_eq!(Confidence::from_tag("extracted"), Confidence::Extracted);
        assert_eq!(Confidence::from_tag("nonsense"), Confidence::Inferred);
    }

    #[test]
    fn unsupported_lang_name_maps_known_and_ignores_the_rest() {
        assert_eq!(
            unsupported_lang_name(&PathBuf::from("a/b.zig")),
            Some(("zig", "Zig"))
        );
        assert_eq!(
            unsupported_lang_name(&PathBuf::from("m.exs")),
            Some(("elixir", "Elixir"))
        );
        assert_eq!(
            unsupported_lang_name(&PathBuf::from("a.fsx")),
            Some(("fsharp", "F#"))
        );
        // Data/config/unknown extensions are NOT named — they fold into "other".
        assert_eq!(unsupported_lang_name(&PathBuf::from("data.bin")), None);
        assert_eq!(unsupported_lang_name(&PathBuf::from("noext")), None);
        // A supported language's extension is never a "red" name (it's a Lang,
        // handled by the census before the unsupported map is consulted).
        assert_eq!(unsupported_lang_name(&PathBuf::from("main.rs")), None);
    }
}

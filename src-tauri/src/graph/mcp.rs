//! Graph tool surface shared by both consumers:
//! - the **cloud Opus session**, via the `cimp --offload-mcp` server (this
//!   module's [`tools`] descriptors + [`handle_call`]); and
//! - the **local offload worker**, via [`offload_query`] (wired into the
//!   offload native-tool router).
//!
//! Both go through the **self-contained** path: resolve the project root, open
//! the on-disk `graph.db` read-only, and run the query. The single source of
//! truth for the tool set is [`tool_specs`] (name + description + JSON schema),
//! so the MCP descriptors and the offload `ToolDef`s can't drift; the single
//! dispatch+format core is [`run_tool`]. The warm app-side service + loopback
//! route is the Phase-C upgrade; this adapter is the fallback that also works
//! before the app owns a warm index.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::index::{DocHit, GraphIndex, RefHit, SymbolHit};

/// One graph tool's identity, description, and JSON-Schema parameters — the
/// shared definition both surfaces render into their own shape.
pub struct GraphToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

/// The canonical graph tool set. Adding a tool here surfaces it to BOTH the
/// MCP descriptors and the offload worker's `ToolDef`s.
pub fn tool_specs() -> Vec<GraphToolSpec> {
    let one = |name: &'static str, description: &'static str, params: &[(&str, &str)]| {
        let mut props = serde_json::Map::new();
        let mut required = Vec::new();
        for (key, desc) in params {
            props.insert((*key).to_string(), json!({ "type": "string", "description": desc }));
            required.push(Value::String((*key).to_string()));
        }
        GraphToolSpec {
            name,
            description,
            parameters: json!({
                "type": "object",
                "properties": Value::Object(props),
                "required": required,
            }),
        }
    };

    vec![
        one(
            "graph_find_symbol",
            "Find where a symbol (function, struct, trait, etc.) is DEFINED in this project. \
             Returns each definition's file, line, kind, and signature. Prefer this over grep \
             for 'where is X defined'.",
            &[("name", "The exact symbol name to look up.")],
        ),
        one(
            "graph_callers",
            "List the functions/methods that CALL the given symbol (its call sites, resolved to \
             the calling definition). Use for 'who calls X' / impact analysis.",
            &[("name", "The called symbol's name.")],
        ),
        one(
            "graph_callees",
            "List the symbols CALLED BY the given symbol. Use for 'what does X call'.",
            &[("name", "The calling symbol's name.")],
        ),
        one(
            "graph_references",
            "List every reference (use site) of a name — file, line, column.",
            &[("name", "The name to find references of.")],
        ),
        one(
            "graph_imports",
            "List the modules/paths imported by a file.",
            &[("file", "Project-relative file path (as indexed).")],
        ),
        one(
            "graph_outline",
            "List every definition in a file, in source order (a structural outline).",
            &[("file", "Project-relative file path (as indexed).")],
        ),
        GraphToolSpec {
            name: "graph_transitive",
            description: "Transitive call chain for a symbol. direction 'callees' (default) returns \
                everything it transitively calls; 'callers' returns everything that transitively calls it.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The symbol name." },
                    "direction": {
                        "type": "string",
                        "enum": ["callees", "callers"],
                        "description": "'callees' (what it reaches) or 'callers' (what reaches it). Default 'callees'."
                    }
                },
                "required": ["name"]
            }),
        },
        one(
            "graph_search_docs",
            "Search documentation and doc-comments for a keyword. Returns matching doc snippets \
             with their source.",
            &[("query", "Keyword or phrase to search for.")],
        ),
        GraphToolSpec {
            name: "graph_struct_search",
            description: "Find code by AST shape using a tree-sitter QUERY (an S-expression \
                pattern), not text. Example (Rust, find every `.unwrap()`): \
                `(call_expression function: (field_expression field: (field_identifier) @m) (#eq? @m \"unwrap\"))`. \
                Returns the file, line, and snippet of each captured node. Use for structural \
                questions text search can't express.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "A tree-sitter query S-expression with at least one @capture." },
                    "lang": { "type": "string", "enum": ["rust", "typescript", "javascript", "python"], "description": "Which language's files to search." }
                },
                "required": ["query", "lang"]
            }),
        },
        GraphToolSpec {
            name: "graph_dead_exports",
            description: "List CANDIDATE unused public symbols — public/exported definitions with \
                no reference and no inbound call edge anywhere in the project. Candidates only: a \
                symbol reached via dynamic dispatch, an external consumer, a macro, or reflection \
                has no static edge and may appear here as a false positive. Takes no arguments.",
            parameters: json!({ "type": "object", "properties": {}, "required": [] }),
        },
        GraphToolSpec {
            name: "graph_cycles",
            description: "List import cycles between files (each a loop of two or more files that \
                transitively import one another). Uses best-effort per-language import resolution; \
                modules that don't resolve to an indexed file are ignored. Takes no arguments.",
            parameters: json!({ "type": "object", "properties": {}, "required": [] }),
        },
    ]
}

/// The `graph_*` MCP tool descriptors, or an empty list when the graph feature
/// is disabled (so they're not advertised to Opus).
pub fn tools() -> Vec<Value> {
    let settings = current_settings();
    if !settings.graph.enabled {
        return Vec::new();
    }
    let mut specs = tool_specs();
    if settings.graph.semantic_search {
        specs.push(semantic_spec());
    }
    specs
        .into_iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "inputSchema": s.parameters,
            })
        })
        .collect()
}

/// Dispatch a `graph_*` MCP tool call. Returns a JSON-RPC `tools/call` result;
/// a missing index or bad args come back as a (non-protocol) tool error so Opus
/// can read and adapt. Unknown tool names are a protocol error.
pub async fn handle_call(params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    let settings = current_settings();
    let sub = db_subdir(&settings);

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (root, idx) = match open_project_index(&cwd, &sub) {
        Ok(pair) => pair,
        Err(msg) => return Ok(tool_error(&msg)),
    };

    let result = dispatch_recorded(&root, &idx, &settings, "claude", name, &args).await;

    match result {
        Ok(text) => Ok(json!({ "content": [{ "type": "text", "text": text }] })),
        Err(msg) if msg.starts_with("unknown graph tool") => Err((-32602, msg)),
        Err(msg) => Ok(tool_error(&msg)),
    }
}

/// Execute one resolved `graph_*` tool against an open index — dispatching to
/// the semantic / structural / plain path — and record it in the activity ring
/// for the monitor tab. `source` is `"claude"` (cloud) or `"offload"` (local
/// worker). Shared by the cloud (warm + fallback) and worker paths so each call
/// is captured exactly once.
pub(crate) async fn dispatch_recorded(
    root: &Path,
    idx: &GraphIndex,
    settings: &crate::settings::Settings,
    source: &str,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    let (max_rows, max_snippet) = limits(settings);
    let started = super::activity::now_ms();
    let result = if name == "graph_semantic_docs" {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        semantic_query(idx, settings, query, max_rows, max_snippet).await
    } else if name == "graph_struct_search" {
        run_struct_search(root, idx, args, max_rows, max_snippet)
    } else {
        run_tool(idx, name, args, max_rows, max_snippet)
    };
    super::activity::record(super::activity::GraphCall {
        ts_ms: started,
        source: source.to_string(),
        tool: name.to_string(),
        target: arg_summary(name, args),
        chars: result.as_ref().map(|t| t.chars().count()).unwrap_or(0),
        ms: super::activity::now_ms().saturating_sub(started),
        ok: result.is_ok(),
    });
    result
}

/// The primary argument of a graph tool (symbol / file / query) for the
/// history's at-a-glance column.
fn arg_summary(name: &str, args: &Value) -> String {
    let key = match name {
        "graph_imports" | "graph_outline" => "file",
        "graph_search_docs" | "graph_semantic_docs" | "graph_struct_search" => "query",
        _ => "name",
    };
    args.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// Run one graph tool against an open index and format its result as compact,
/// token-bounded text. Shared by the MCP adapter and the offload worker. `Err`
/// is a human-readable message the caller surfaces to its model.
pub fn run_tool(
    idx: &GraphIndex,
    name: &str,
    args: &Value,
    max_rows: usize,
    max_snippet: usize,
) -> Result<String, String> {
    let arg = |key: &str| -> String {
        args.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    // A required string arg. Rejects missing / blank / wrong-typed values rather
    // than silently coercing them to "" — a `find_symbol("")` (from a `null` or
    // numeric arg the LLM sent) would otherwise match everything or nothing and
    // mislead the model instead of surfacing a clear error.
    let req = |key: &str| -> Result<String, String> {
        match args.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
            Some(Value::String(_)) | None => {
                Err(format!("{name} requires a non-empty string `{key}`"))
            }
            Some(_) => Err(format!("{name} argument `{key}` must be a string")),
        }
    };
    match name {
        "graph_find_symbol" => idx
            .find_symbol(&req("name")?)
            .map(|v| fmt_symbols(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_callers" => idx
            .callers(&req("name")?)
            .map(|v| fmt_symbols(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_callees" => idx
            .callees(&req("name")?)
            .map(|v| fmt_symbols(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_references" => idx
            .references(&req("name")?)
            .map(|v| fmt_refs(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_imports" => idx
            .imports(&req("file")?)
            .map(|v| fmt_list(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_outline" => idx
            .outline(&req("file")?)
            .map(|v| fmt_symbols(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_transitive" => {
            let target = req("name")?;
            let dir = arg("direction");
            // Omitted defaults to callees/forward; reject any other value so a
            // typo (e.g. "downstream") can't silently run the opposite traversal
            // from what the caller asked for.
            let forward = if dir.is_empty() || dir.eq_ignore_ascii_case("callees") {
                true
            } else if dir.eq_ignore_ascii_case("callers") {
                false
            } else {
                return Err(format!(
                    "graph_transitive `direction` must be \"callers\" or \"callees\" (got `{dir}`)"
                ));
            };
            idx.transitive(&target, forward)
                .map(|v| fmt_list(&v, max_rows))
                .map_err(|e| e.to_string())
        }
        "graph_search_docs" => idx
            .search_docs(&req("query")?, max_rows, max_snippet)
            .map(|v| fmt_docs(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_dead_exports" => idx
            .dead_exports(max_rows)
            .map(|v| fmt_dead_exports(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_cycles" => idx
            .import_cycles(max_rows)
            .map(|v| fmt_cycles(&v, max_rows))
            .map_err(|e| e.to_string()),
        // Sync path / no-embedder fallback for semantic search: degrade to
        // labelled full-text. The embedder-backed ranking is applied in the
        // async wrappers ([`handle_call`] / [`offload_query`]).
        "graph_semantic_docs" => idx
            .search_docs(&req("query")?, max_rows, max_snippet)
            .map(|v| {
                let body = fmt_docs(&v, max_rows);
                format!("(full-text fallback — semantic embedder unavailable)\n{body}")
            })
            .map_err(|e| e.to_string()),
        other => Err(format!("unknown graph tool: {other}")),
    }
}

/// The semantic-doc-search tool spec, advertised only when `semantic_search`
/// is enabled (it degrades to full-text at runtime when the embedder is down).
pub fn semantic_spec() -> GraphToolSpec {
    GraphToolSpec {
        name: "graph_semantic_docs",
        description: "Semantic (meaning-based) search over the project's documentation and \
            doc-comments — finds relevant chunks even when they don't share keywords with the \
            query. Falls back to full-text search when the embedding backend is unavailable.",
        parameters: json!({
            "type": "object",
            "properties": { "query": { "type": "string", "description": "A natural-language description of what you're looking for." } },
            "required": ["query"]
        }),
    }
}

/// Embedder-backed semantic doc search, with a full-text fallback on any
/// miss (no embedder configured/reachable, no vectors yet, or a dim mismatch
/// after a model change). Used by the async tool wrappers.
async fn semantic_query(
    idx: &GraphIndex,
    settings: &crate::settings::Settings,
    query: &str,
    max_rows: usize,
    max_snippet: usize,
) -> Result<String, String> {
    let g = &settings.graph;
    let fallback = |idx: &GraphIndex| {
        idx.search_docs(query, max_rows, max_snippet)
            .map(|v| format!("(full-text fallback)\n{}", fmt_docs(&v, max_rows)))
            .map_err(|e| e.to_string())
    };

    if !g.semantic_search {
        return fallback(idx);
    }
    let epoch = match idx.current_epoch() {
        Ok(Some(e)) => e,
        _ => return fallback(idx),
    };
    let Some(embedder) = super::embed::Embedder::new(&g.embedding_endpoint, &g.embedding_model)
    else {
        return fallback(idx);
    };
    let qv = match embedder.embed_one(query).await {
        Ok(v) => v,
        Err(_) => return fallback(idx),
    };
    match idx.semantic_doc_search(&qv, &epoch, max_rows, max_snippet) {
        Ok(hits) if !hits.is_empty() => {
            // `dist` is a cosine DISTANCE (lower = more similar); label it as
            // such so the model doesn't read it as a higher-is-better score.
            let mut lines: Vec<String> = hits
                .iter()
                .take(max_rows)
                .map(|(d, dist)| format!("{} [{}] (distance {:.3}): {}", d.source_path, d.anchor, dist, d.snippet))
                .collect();
            if hits.len() > max_rows {
                lines.push(format!("… (+{} more)", hits.len() - max_rows));
            }
            let body = lines.join("\n");
            Ok(format!("(semantic — nearest first; lower distance = more similar)\n{body}"))
        }
        // No vectors matched yet (mid-backfill) or a query error → full-text.
        _ => fallback(idx),
    }
}

/// The offload worker's entry point: resolve the graph store from `roots`
/// (the worker's confinement roots), open it read-only, and run `name`. `Err`
/// is fed back to the worker's model as a tool result. The caller is
/// responsible for the local/remote opt-in gate — this just executes.
pub async fn offload_query(
    roots: &[PathBuf],
    name: &str,
    args: &Value,
) -> Result<String, String> {
    let settings = current_settings();
    let sub = db_subdir(&settings);

    // First configured root that already has a built graph wins. (Most setups
    // have a single root; multiple roots fall back to the first that's indexed.)
    let mut last_err =
        "no code graph found under the offload roots — enable + index the project in cImp"
            .to_string();
    for root in roots {
        match open_project_index_confined(root, &sub) {
            Ok((resolved, idx)) => {
                return dispatch_recorded(&resolved, &idx, &settings, "offload", name, args).await;
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

// ── result formatting (compact, token-bounded text for the model) ────────

fn fmt_symbols(syms: &[SymbolHit], max_rows: usize) -> String {
    if syms.is_empty() {
        return "No matching symbols.".to_string();
    }
    let mut lines: Vec<String> = syms
        .iter()
        .take(max_rows)
        .map(|s| format!("{} ({}) — {}:{}  {}", s.name, s.kind, s.file, s.start_line, s.signature))
        .collect();
    if syms.len() > max_rows {
        lines.push(format!("… (+{} more)", syms.len() - max_rows));
    }
    lines.join("\n")
}

fn fmt_dead_exports(syms: &[SymbolHit], max_rows: usize) -> String {
    if syms.is_empty() {
        return "No candidate dead exports found.".to_string();
    }
    let mut lines: Vec<String> = syms
        .iter()
        .take(max_rows)
        .map(|s| format!("{} ({}) — {}:{}", s.name, s.kind, s.file, s.start_line))
        .collect();
    if syms.len() > max_rows {
        lines.push(format!("… (+{} more)", syms.len() - max_rows));
    }
    format!(
        "Candidate unused public symbols (may include false positives — dynamic dispatch, external API, macros/reflection):\n{}",
        lines.join("\n")
    )
}

fn fmt_cycles(cycles: &[Vec<String>], max_rows: usize) -> String {
    if cycles.is_empty() {
        return "No import cycles found.".to_string();
    }
    let mut lines: Vec<String> = cycles
        .iter()
        .take(max_rows)
        .map(|c| {
            let mut s = c.join(" → ");
            if let Some(first) = c.first() {
                s.push_str(&format!(" → {first}"));
            }
            s
        })
        .collect();
    if cycles.len() > max_rows {
        lines.push(format!("… (+{} more)", cycles.len() - max_rows));
    }
    lines.join("\n")
}

fn fmt_refs(refs: &[RefHit], max_rows: usize) -> String {
    if refs.is_empty() {
        return "No references.".to_string();
    }
    let mut lines: Vec<String> = refs
        .iter()
        .take(max_rows)
        .map(|r| format!("{}:{}:{}", r.file, r.line, r.col))
        .collect();
    if refs.len() > max_rows {
        lines.push(format!("… (+{} more)", refs.len() - max_rows));
    }
    lines.join("\n")
}

fn fmt_docs(docs: &[DocHit], max_rows: usize) -> String {
    if docs.is_empty() {
        return "No documentation matches.".to_string();
    }
    let mut lines: Vec<String> = docs
        .iter()
        .take(max_rows)
        .map(|d| format!("{} [{}]: {}", d.source_path, d.anchor, d.snippet))
        .collect();
    if docs.len() > max_rows {
        lines.push(format!("… (+{} more)", docs.len() - max_rows));
    }
    lines.join("\n")
}

fn fmt_list(items: &[String], max_rows: usize) -> String {
    if items.is_empty() {
        return "No results.".to_string();
    }
    let mut lines: Vec<String> = items.iter().take(max_rows).cloned().collect();
    if items.len() > max_rows {
        lines.push(format!("… (+{} more)", items.len() - max_rows));
    }
    lines.join("\n")
}

fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

// ── project resolution + settings helpers ────────────────────────────────

fn current_settings() -> crate::settings::Settings {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::settings::load_readonly(&cwd)
}

pub(crate) fn limits(settings: &crate::settings::Settings) -> (usize, usize) {
    (
        settings.graph.max_rows_per_query.max(1) as usize,
        settings.graph.max_snippet_bytes.max(40) as usize,
    )
}

fn db_subdir(settings: &crate::settings::Settings) -> String {
    settings.graph.effective_db_subdir()
}

/// Open the existing graph store for the project containing `start`, resolving
/// the root by walking up for a `<dir>/<sub>/graph.db`. Returns the resolved
/// project root alongside the index (structural search reads files from it).
fn open_project_index(start: &Path, sub: &str) -> Result<(PathBuf, GraphIndex), String> {
    let root = find_graph_root(start, sub).ok_or_else(|| {
        format!(
            "no code graph found from {} — enable the graph and index this project in cImp",
            start.display()
        )
    })?;
    let idx = GraphIndex::open_existing(&root, sub).map_err(|e| e.to_string())?;
    Ok((root, idx))
}

/// Execute `graph_struct_search`: re-parse the indexed files of `lang` under
/// `root` and run the tree-sitter `query` over them. Bounded by `max_rows` and
/// a file cap so a huge tree can't run away.
fn run_struct_search(
    root: &Path,
    idx: &GraphIndex,
    args: &Value,
    max_rows: usize,
    max_snippet: usize,
) -> Result<String, String> {
    const MAX_FILES: usize = 4000;
    let pattern = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
    let lang_tag = args.get("lang").and_then(|v| v.as_str()).unwrap_or("");
    if pattern.trim().is_empty() {
        return Err("graph_struct_search needs a `query` (a tree-sitter S-expression)".into());
    }
    let lang = super::model::Lang::from_tag(lang_tag);
    if super::builder::language_for(lang).is_none() {
        return Err(format!(
            "graph_struct_search `lang` must be one of rust/typescript/javascript/python (got `{lang_tag}`)"
        ));
    }

    let paths = idx.files_for_lang(lang.tag()).map_err(|e| e.to_string())?;
    let mut files: Vec<(String, String)> = Vec::new();
    for rel in paths.into_iter().take(MAX_FILES) {
        if let Ok(src) = std::fs::read_to_string(root.join(&rel)) {
            files.push((rel, src));
        }
    }
    let hits = super::builder::struct_search(lang, pattern, &files, max_rows, max_snippet)?;
    if hits.is_empty() {
        return Ok("No structural matches.".to_string());
    }
    let mut lines: Vec<String> = hits
        .iter()
        .take(max_rows)
        .map(|h| format!("{}:{}  {}", h.file, h.line, h.snippet))
        .collect();
    if hits.len() > max_rows {
        lines.push(format!("… (+{} more)", hits.len() - max_rows));
    }
    Ok(lines.join("\n"))
}

/// Like [`open_project_index`] but **confined**: the resolved project root must
/// be at or below `allowed_root`. The offload worker is sandboxed to its
/// confinement roots; [`find_graph_root`] walks ancestors, so a `graph.db` in a
/// *parent* directory (a project nested inside a larger indexed repo, or a
/// stray `~/.cimp/graph.db`) would otherwise let `graph_struct_search` read
/// source files outside the sandbox. Since the search starts at `allowed_root`,
/// "at or below" means the resolved root equals `allowed_root`.
fn open_project_index_confined(
    allowed_root: &Path,
    sub: &str,
) -> Result<(PathBuf, GraphIndex), String> {
    let resolved = find_graph_root(allowed_root, sub).ok_or_else(|| {
        format!(
            "no code graph found from {} — enable the graph and index this project in cImp",
            allowed_root.display()
        )
    })?;
    if !resolved.starts_with(allowed_root) {
        return Err(format!(
            "the code graph for {} lives above the offload worker's allowed root — refusing to read outside the sandbox",
            allowed_root.display()
        ));
    }
    let idx = GraphIndex::open_existing(&resolved, sub).map_err(|e| e.to_string())?;
    Ok((resolved, idx))
}

/// Walk up from `start` looking for an ancestor containing `<sub>/graph.db`.
pub(crate) fn find_graph_root(start: &Path, sub: &str) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if dir.join(sub).join("graph.db").is_file() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

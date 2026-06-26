//! Graph tool surface shared by both consumers:
//! - the **cloud Opus session**, via the `ccimp --offload-mcp` server (this
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
    ]
}

/// The `graph_*` MCP tool descriptors, or an empty list when the graph feature
/// is disabled (so they're not advertised to Opus).
pub fn tools() -> Vec<Value> {
    if !current_settings().graph.enabled {
        return Vec::new();
    }
    tool_specs()
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
pub fn handle_call(params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    let settings = current_settings();
    let (max_rows, max_snippet) = limits(&settings);
    let sub = db_subdir(&settings);

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let idx = match open_project_index(&cwd, &sub) {
        Ok(i) => i,
        Err(msg) => return Ok(tool_error(&msg)),
    };

    match run_tool(&idx, name, &args, max_rows, max_snippet) {
        Ok(text) => Ok(json!({ "content": [{ "type": "text", "text": text }] })),
        Err(msg) if msg.starts_with("unknown graph tool") => Err((-32602, msg)),
        Err(msg) => Ok(tool_error(&msg)),
    }
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
    match name {
        "graph_find_symbol" => idx
            .find_symbol(&arg("name"))
            .map(|v| fmt_symbols(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_callers" => idx
            .callers(&arg("name"))
            .map(|v| fmt_symbols(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_callees" => idx
            .callees(&arg("name"))
            .map(|v| fmt_symbols(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_references" => idx
            .references(&arg("name"))
            .map(|v| fmt_refs(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_imports" => idx
            .imports(&arg("file"))
            .map(|v| fmt_list(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_outline" => idx
            .outline(&arg("file"))
            .map(|v| fmt_symbols(&v, max_rows))
            .map_err(|e| e.to_string()),
        "graph_transitive" => {
            let forward = !arg("direction").eq_ignore_ascii_case("callers");
            idx.transitive(&arg("name"), forward)
                .map(|v| fmt_list(&v, max_rows))
                .map_err(|e| e.to_string())
        }
        "graph_search_docs" => idx
            .search_docs(&arg("query"), max_rows, max_snippet)
            .map(|v| fmt_docs(&v))
            .map_err(|e| e.to_string()),
        other => Err(format!("unknown graph tool: {other}")),
    }
}

/// The offload worker's entry point: resolve the graph store from `roots`
/// (the worker's confinement roots), open it read-only, and run `name`. `Err`
/// is fed back to the worker's model as a tool result. The caller is
/// responsible for the local/remote opt-in gate — this just executes.
pub fn offload_query(
    roots: &[PathBuf],
    name: &str,
    args: &Value,
) -> Result<String, String> {
    let settings = current_settings();
    let (max_rows, max_snippet) = limits(&settings);
    let sub = db_subdir(&settings);

    // First configured root that already has a built graph wins. (Most setups
    // have a single root; multiple roots fall back to the first that's indexed.)
    let mut last_err =
        "no code graph found under the offload roots — enable + index the project in ccImp"
            .to_string();
    for root in roots {
        match open_project_index(root, &sub) {
            Ok(idx) => return run_tool(&idx, name, args, max_rows, max_snippet),
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

fn fmt_docs(docs: &[DocHit]) -> String {
    if docs.is_empty() {
        return "No documentation matches.".to_string();
    }
    docs.iter()
        .map(|d| format!("{} [{}]: {}", d.source_path, d.anchor, d.snippet))
        .collect::<Vec<_>>()
        .join("\n")
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

fn limits(settings: &crate::settings::Settings) -> (usize, usize) {
    (
        settings.graph.max_rows_per_query.max(1) as usize,
        settings.graph.max_snippet_bytes.max(40) as usize,
    )
}

fn db_subdir(settings: &crate::settings::Settings) -> String {
    let s = &settings.graph.db_subdir;
    if s.trim().is_empty() {
        ".ccimp".to_string()
    } else {
        s.clone()
    }
}

/// Open the existing graph store for the project containing `start`, resolving
/// the root by walking up for a `<dir>/<sub>/graph.db`.
fn open_project_index(start: &Path, sub: &str) -> Result<GraphIndex, String> {
    let root = find_graph_root(start, sub).ok_or_else(|| {
        format!(
            "no code graph found from {} — enable the graph and index this project in ccImp",
            start.display()
        )
    })?;
    GraphIndex::open_existing(&root, sub).map_err(|e| e.to_string())
}

/// Walk up from `start` looking for an ancestor containing `<sub>/graph.db`.
fn find_graph_root(start: &Path, sub: &str) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if dir.join(sub).join("graph.db").is_file() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

//! MCP adapter for the code knowledge graph — the `graph_*` tools exposed to
//! Claude through the `ccimp --offload-mcp` server (V8's bridge, reused).
//!
//! This stage uses the **self-contained** path: the MCP child resolves the
//! project root from its cwd and opens the on-disk `graph.db` read-only. The
//! warm app-side service + loopback route (`POST /graph/query`) is the Phase-C
//! upgrade; this adapter is the fallback that also makes the tools work before
//! the app owns a warm index.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::index::{DocHit, GraphIndex, RefHit, SymbolHit};

/// The `graph_*` tool descriptors, or an empty list when the graph feature is
/// disabled (so they're not advertised to Opus).
pub fn tools() -> Vec<Value> {
    let settings = current_settings();
    if !settings.graph.enabled {
        return Vec::new();
    }
    vec![
        tool(
            "graph_find_symbol",
            "Find where a symbol (function, struct, trait, etc.) is DEFINED in this project. \
             Returns each definition's file, line, kind, and signature. Prefer this over grep \
             for 'where is X defined'.",
            &[("name", "The exact symbol name to look up.")],
        ),
        tool(
            "graph_callers",
            "List the functions/methods that CALL the given symbol (its call sites, resolved to \
             the calling definition). Use for 'who calls X' / impact analysis.",
            &[("name", "The called symbol's name.")],
        ),
        tool(
            "graph_callees",
            "List the symbols CALLED BY the given symbol. Use for 'what does X call'.",
            &[("name", "The calling symbol's name.")],
        ),
        tool(
            "graph_references",
            "List every reference (use site) of a name — file, line, column.",
            &[("name", "The name to find references of.")],
        ),
        tool(
            "graph_imports",
            "List the modules/paths imported by a file.",
            &[("file", "Project-relative file path (as indexed).")],
        ),
        tool(
            "graph_outline",
            "List every definition in a file, in source order (a structural outline).",
            &[("file", "Project-relative file path (as indexed).")],
        ),
        transitive_tool(),
        tool(
            "graph_search_docs",
            "Search documentation and doc-comments for a keyword. Returns matching doc snippets \
             with their source.",
            &[("query", "Keyword or phrase to search for.")],
        ),
    ]
}

/// Dispatch a `graph_*` tool call. Returns a JSON-RPC `tools/call` result; a
/// missing index or bad args come back as a (non-protocol) tool error so Opus
/// can read and adapt. Unknown tool names are a protocol error.
pub fn handle_call(params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    let idx = match open_project_index() {
        Ok(i) => i,
        Err(msg) => return Ok(tool_error(&msg)),
    };
    let settings = current_settings();
    let max_rows = settings.graph.max_rows_per_query.max(1) as usize;
    let max_snippet = settings.graph.max_snippet_bytes.max(40) as usize;

    let arg = |key: &str| -> String {
        args.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    let text: Result<String, String> = match name {
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
        other => return Err((-32602, format!("unknown graph tool: {other}"))),
    };

    match text {
        Ok(t) => Ok(json!({ "content": [{ "type": "text", "text": t }] })),
        Err(msg) => Ok(tool_error(&msg)),
    }
}

// ── tool descriptors ────────────────────────────────────────────────────

fn tool(name: &str, description: &str, params: &[(&str, &str)]) -> Value {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for (key, desc) in params {
        props.insert(
            (*key).to_string(),
            json!({ "type": "string", "description": desc }),
        );
        required.push(Value::String((*key).to_string()));
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": Value::Object(props),
            "required": required,
        }
    })
}

fn transitive_tool() -> Value {
    json!({
        "name": "graph_transitive",
        "description": "Transitive call chain for a symbol. direction 'callees' (default) returns \
            everything it transitively calls; 'callers' returns everything that transitively calls it.",
        "inputSchema": {
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
        }
    })
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

// ── project resolution ──────────────────────────────────────────────────

fn current_settings() -> crate::settings::Settings {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::settings::load_readonly(&cwd)
}

/// Open the existing graph store for the current project, resolving the root by
/// walking up from the cwd for a `<dir>/<db_subdir>/graph.db`.
fn open_project_index() -> Result<GraphIndex, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let sub = {
        let s = current_settings();
        if s.graph.db_subdir.trim().is_empty() {
            ".ccimp".to_string()
        } else {
            s.graph.db_subdir.clone()
        }
    };
    let root = find_graph_root(&cwd, &sub).ok_or_else(|| {
        format!(
            "no code graph found from {} — enable the graph and index this project in ccImp",
            cwd.display()
        )
    })?;
    GraphIndex::open_existing(&root, &sub).map_err(|e| e.to_string())
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

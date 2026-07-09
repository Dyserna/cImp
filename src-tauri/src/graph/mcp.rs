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
use super::memory::{MemNote, ProjectFact, WorkingSetEntry};

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
            name: "graph_repo_map",
            description: "A budget-bounded map of the project's most call-central files with their \
                top exported signatures — a fast way to orient at the start of a task without \
                exploring. Session-hot files are lifted up the ranking. Optional `budget_chars` \
                overrides the configured size.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "budget_chars": { "type": "integer", "description": "Character budget for the map (default from settings)." }
                },
                "required": []
            }),
        },
        GraphToolSpec {
            name: "graph_snippet",
            description: "Fetch just a DEFINITION'S BODY instead of reading the whole file — the \
                token-cheap way to see one function/type in a large file. Give `symbol` (a name; \
                an ambiguous name returns a disambiguation list, not a body) OR `file`+`line` \
                (returns the smallest definition whose span encloses that line). Optional \
                `context_lines` adds N lines above and below. Prefer this (often after \
                `graph_outline`) over Read for a single definition.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Symbol name to fetch the body of." },
                    "file": { "type": "string", "description": "Project-relative file path (use with `line`)." },
                    "line": { "type": "integer", "description": "1-based line inside the wanted definition (use with `file`)." },
                    "context_lines": { "type": "integer", "description": "Extra source lines above and below the span (default 0)." }
                },
                "required": []
            }),
        },
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
            name: "graph_impact",
            description: "Blast-radius / impact analysis: what could this change break? With no \
                `symbols`, analyzes the CURRENT WORKING-TREE DIFF vs HEAD — maps changed line \
                ranges to indexed symbols, then finds everything that transitively calls them (their \
                dependents), up to `depth` hops. Pass `symbols` (comma/space-separated names) to \
                analyze specific symbols instead of the diff. Results are name-keyed and therefore \
                APPROXIMATE, same honesty convention as `graph_references`. The default diff mode \
                requires a git repository. `include_tests: true` appends an affected-tests block \
                (candidate tests reaching the changed symbols) — chain into a filtered test run.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "symbols": { "type": "string", "description": "Comma/space-separated symbol names to use as roots instead of the working-tree diff." },
                    "depth": { "type": "integer", "description": "Max hops to traverse from a changed symbol (default 3, clamped 1-6)." },
                    "include_tests": { "type": "boolean", "description": "Append a candidate affected-tests block (file:line · name) below the dependent report. Default false." }
                },
                "required": []
            }),
        },
        GraphToolSpec {
            name: "graph_tests_for",
            description: "Which tests (candidates) would exercise a symbol or file if it changed — \
                the transitive dependents of the given root(s), filtered to definitions a walker \
                tagged as tests (`#[test]`/pytest `test_*`/`*.test.ts`/etc., language-dependent — \
                see each language's detection convention). Give `symbol` (one name) OR `file` \
                (unions every definition in that file as roots). CANDIDATES ONLY: dynamic dispatch, \
                fixtures, and parametrized runners have no static call edge and won't appear here; \
                a symbol with no detected test coverage may still be well-tested indirectly.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "A symbol name to find tests for." },
                    "file": { "type": "string", "description": "A project-relative file path — finds tests for every definition in it." },
                    "depth": { "type": "integer", "description": "Max hops to traverse (default 3, clamped 1-6)." }
                },
                "required": []
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
        GraphToolSpec {
            name: "graph_recent_changes",
            description: "What's been happening in this project lately — files ranked by git churn \
                (most-touched, then most-recent first), each with its touch count and last commit \
                subject. Good for orienting at the start of a fresh session. File-level only (no \
                per-line blame), bounded to a 90-day history window. Reports unavailable when the \
                project isn't a git repository.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "days": { "type": "integer", "description": "Only files touched within the last N days (default 14, clamped 1-90)." },
                    "path_prefix": { "type": "string", "description": "Only files under this project-relative path prefix." }
                },
                "required": []
            }),
        },
        GraphToolSpec {
            name: "context_recall",
            description: "Recall what THIS session has been working on — the ranked working set of \
                files it read/edited/queried, with the symbols touched. Use at the start of a \
                follow-up task to reload your working context. Takes no arguments.",
            parameters: json!({ "type": "object", "properties": {}, "required": [] }),
        },
        GraphToolSpec {
            name: "context_note",
            description: "Remember a non-obvious decision or fact for this project's session memory \
                (e.g. 'we chose FNV hashing because …'). Set pin=true to keep it project-wide across \
                sessions.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The decision/fact to remember." },
                    "pin": { "type": "boolean", "description": "Keep across sessions (project-wide). Default false." }
                },
                "required": ["text"]
            }),
        },
        GraphToolSpec {
            name: "context_notes",
            description: "List this session's remembered notes plus every pinned note for the \
                project (pinned first, newest first). Takes no arguments.",
            parameters: json!({ "type": "object", "properties": {}, "required": [] }),
        },
    ]
}

/// The `graph_*` / `run_check` MCP tool descriptors. The `graph_*` set is
/// empty when the graph feature is disabled (so it's not advertised to Opus);
/// `run_check` (V12 Phase A) is independent of the graph — it's gated only on
/// `checks` being non-empty, so a project with checks configured but the
/// graph off still gets it.
pub fn tools() -> Vec<Value> {
    let settings = current_settings();
    let mut specs: Vec<GraphToolSpec> = Vec::new();
    if settings.graph.enabled {
        specs.extend(tool_specs());
        if settings.graph.semantic_search {
            specs.push(semantic_spec());
        }
        // Code semantic search needs the embedder too: the code-embedding
        // backfill only runs when `semantic_search` is on, so advertising the
        // tool on `embed_code_bodies` alone would offer a tool that can never
        // return results.
        if settings.graph.semantic_search && settings.graph.embed_code_bodies {
            specs.push(semantic_code_spec());
        }
    }
    if !settings.checks.is_empty() {
        specs.push(run_check_spec());
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

/// The activity/memory **source** string for a consumer name — the value
/// carried through the activity ring and used to scope the `context_*` memory
/// tools to the calling agent. `"opencode"` stays itself; anything else
/// (Claude, or unset) is `"claude"`.
pub fn source_for_consumer(consumer: &str) -> &'static str {
    if consumer.eq_ignore_ascii_case("opencode") {
        "opencode"
    } else {
        "claude"
    }
}

/// Map an activity source to the memory agent the `context_*` tools scope to:
/// a tab agent (`claude`/`opencode`) filters to its own sessions; the offload
/// worker (`offload`) has no tab session, so it reads the project-wide latest.
fn mem_agent(source: &str) -> Option<&str> {
    match source {
        "offload" => None,
        other => Some(other),
    }
}

/// Dispatch a `graph_*` / `context_*` MCP tool call for the given `consumer`
/// (`"claude"` / `"opencode"`). Returns a JSON-RPC `tools/call` result; a
/// missing index or bad args come back as a (non-protocol) tool error so the
/// agent can read and adapt. Unknown tool names are a protocol error.
pub async fn handle_call(params: &Value, consumer: &str) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    let settings = current_settings();
    let sub = db_subdir(&settings);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let source = source_for_consumer(consumer);

    // `run_check` needs a project root but NOT a built code graph (V12 Phase
    // A: checks are independent of the graph feature). Resolve root the same
    // way graph tools do when a graph.db already exists (so the two features
    // agree on "the project root" in a mixed setup), else fall back to the
    // working directory itself — never require opening an index for this tool.
    if name == "run_check" {
        let root = find_graph_root(&cwd, &sub).unwrap_or(cwd);
        return match run_check_tool(&root, &settings, source, &args).await {
            Ok(text) => Ok(json!({ "content": [{ "type": "text", "text": text }] })),
            Err(msg) => Ok(tool_error(&msg)),
        };
    }

    let (root, idx) = match open_project_index(&cwd, &sub) {
        Ok(pair) => pair,
        Err(msg) => return Ok(tool_error(&msg)),
    };

    let result = dispatch_recorded(&root, &idx, &settings, source, name, &args).await;

    match result {
        Ok(text) => Ok(json!({ "content": [{ "type": "text", "text": text }] })),
        Err(msg) if msg.starts_with("unknown graph tool") => Err((-32602, msg)),
        Err(msg) => Ok(tool_error(&msg)),
    }
}

/// Execute one resolved `graph_*` / `context_*` tool against an open index —
/// dispatching to the semantic / structural / plain path — and record it in the
/// activity ring for the monitor tab. `source` is `"claude"` / `"opencode"`
/// (a tab agent) or `"offload"` (the local worker); it drives both the ring's
/// source badge and the `context_*` tools' per-agent session scoping. Shared by
/// the cloud (warm + fallback) and worker paths so each call is captured once.
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
    } else if name == "graph_semantic_code" {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let k = args
            .get("k")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(max_rows)
            .clamp(1, max_rows.max(1));
        semantic_code_query(idx, settings, query, k).await
    } else if name == "graph_struct_search" {
        run_struct_search(root, idx, args, max_rows, max_snippet)
    } else if name == "graph_snippet" {
        run_snippet(root, idx, args, max_rows, settings.graph.max_body_bytes as usize)
    } else if name == "graph_repo_map" {
        run_repo_map(idx, settings, args, mem_agent(source))
    } else if name == "graph_impact" {
        run_impact(root, idx, args, max_rows)
    } else if name == "graph_tests_for" {
        run_tests_for(idx, args, max_rows)
    } else {
        run_tool(idx, name, args, max_rows, max_snippet, mem_agent(source))
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
    // graph_snippet/graph_tests_for have no single primary key — prefer the
    // symbol, else file.
    if name == "graph_snippet" || name == "graph_tests_for" {
        let sym = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
        if !sym.is_empty() {
            return sym.to_string();
        }
        return args.get("file").and_then(|v| v.as_str()).unwrap_or("").to_string();
    }
    let key = match name {
        "graph_imports" | "graph_outline" => "file",
        "graph_search_docs" | "graph_semantic_docs" | "graph_semantic_code" | "graph_struct_search" => "query",
        "context_note" => "text",
        "graph_impact" => "symbols",
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
    // The calling agent for the `context_*` memory tools, so they scope to the
    // caller's own session (`Some("claude")`/`Some("opencode")`), or `None` for
    // the project-wide most-recent session (the offload worker's sub-tasks).
    agent: Option<&str>,
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
        "graph_recent_changes" => {
            let days = args
                .get("days")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32)
                .unwrap_or(14)
                .clamp(1, 90);
            let prefix_owned = args.get("path_prefix").and_then(|v| v.as_str()).map(|s| s.trim().to_string());
            let prefix = prefix_owned.as_deref().filter(|s| !s.is_empty());
            idx.recent_changes(days, prefix, max_rows)
                .map(|v| fmt_recent_changes(&v, max_rows))
                .map_err(|e| e.to_string())
        }
        "context_recall" => {
            let Some(sid) = idx.mem_current_session_for(agent).map_err(|e| e.to_string())? else {
                return Ok("No session activity recorded yet.".to_string());
            };
            let ws = idx.mem_working_set(&sid, max_rows).map_err(|e| e.to_string())?;
            let mut out = fmt_working_set(&ws, max_rows);
            // V12 Phase E: a trailing project-facts section (pinned first,
            // capped separately from the working set) — durable knowledge
            // that outlived the sessions it came from.
            let facts = idx.list_project_facts(false, 15).map_err(|e| e.to_string())?;
            if !facts.is_empty() {
                out.push_str("\n\n## Project facts\n");
                out.push_str(&fmt_facts(&facts, 15));
            }
            Ok(out)
        }
        "context_notes" => {
            let sid = idx.mem_current_session_for(agent).map_err(|e| e.to_string())?.unwrap_or_default();
            idx.mem_notes(&sid)
                .map(|v| fmt_notes(&v, max_rows))
                .map_err(|e| e.to_string())
        }
        "context_note" => {
            let text = req("text")?;
            let pin = args.get("pin").and_then(|v| v.as_bool()).unwrap_or(false);
            let sid = idx.mem_current_session_for(agent).map_err(|e| e.to_string())?.unwrap_or_default();
            let note_id = uuid::Uuid::new_v4().to_string();
            let ts = super::activity::now_ms() as i64;
            idx.mem_add_note(&note_id, &sid, &text, ts, pin)
                .map(|_| format!("Noted{}.", if pin { " (pinned, kept across sessions)" } else { "" }))
                .map_err(|e| e.to_string())
        }
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
        // No text-search relation backs `code_chunk` the way `doc_chunk` backs
        // `graph_semantic_docs`, so there's no full-text fallback here — just a
        // clear degrade pointing at the structural alternatives. (Unreachable
        // through `dispatch_recorded`, which special-cases this name for the
        // embedder-backed async path; kept for direct `run_tool` callers.)
        "graph_semantic_code" => Ok(
            "(semantic code search unavailable — no embedder configured/reachable; \
             try `graph_find_symbol` or `graph_struct_search` instead)"
                .to_string(),
        ),
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

/// The semantic-code-search tool spec, advertised only when `embed_code_bodies`
/// is enabled. Unlike [`semantic_spec`], there's no full-text fallback at
/// runtime — `code_chunk` isn't a keyword-searchable relation the way
/// `doc_chunk` is — so a miss degrades to a clear "unavailable" message
/// pointing at `graph_find_symbol`/`graph_struct_search`.
pub fn semantic_code_spec() -> GraphToolSpec {
    GraphToolSpec {
        name: "graph_semantic_code",
        description: "Semantic (meaning-based) search over indexed symbol BODIES (functions, \
            methods, structs, classes, ...) — finds relevant code even when it doesn't share \
            keywords with the query. Returns file:line, kind, signature, and a distance score for \
            each hit — never the body text. Chain into `graph_snippet` to fetch the actual code. \
            Optional `k` caps the result count.",
        parameters: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "A natural-language description of the code you're looking for." },
                "k": { "type": "integer", "description": "Max results (default from settings)." }
            },
            "required": ["query"]
        }),
    }
}

/// The `run_check` tool spec (V12 Phase A), advertised only when `checks` is
/// non-empty (see [`tools`]) — independent of the graph tool set.
pub fn run_check_spec() -> GraphToolSpec {
    GraphToolSpec {
        name: "run_check",
        description: "Run one of this project's configured checker commands (build / typecheck / \
            lint / test) and get back DEDUPLICATED, STRUCTURED diagnostics instead of a raw dump — \
            the cheap way to see what broke after an edit. `name` selects among the project's \
            configured checks (omit it when only one is configured; an unknown or omitted-with- \
            multiple name returns the list of configured names). The command itself is fixed by the \
            user's project config — never model-supplied. `changed_only: true` filters diagnostics \
            to files touched since HEAD (pairs well with editing loops).",
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Which configured check to run. Omit if only one is configured." },
                "changed_only": { "type": "boolean", "description": "Filter diagnostics to files changed since HEAD. Default false." }
            },
            "required": []
        }),
    }
}

/// Dispatch `run_check`: look up the named (or sole) configured [`CheckDef`],
/// run it, and format the result. Deliberately bypasses [`dispatch_recorded`]
/// (which requires an already-open [`GraphIndex`]) — `run_check` touches
/// neither the graph nor an index, so it can't be gated behind opening one
/// (V12 Phase A: the checks feature must not require the graph). Records the
/// call in the activity ring itself, in the same shape, so it still shows up
/// in the monitor tab.
pub(crate) async fn run_check_tool(
    root: &Path,
    settings: &crate::settings::Settings,
    source: &str,
    args: &Value,
) -> Result<String, String> {
    let started = super::activity::now_ms();
    let result = run_check_inner(root, settings, args).await;
    super::activity::record(super::activity::GraphCall {
        ts_ms: started,
        source: source.to_string(),
        tool: "run_check".to_string(),
        target: args.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        chars: result.as_ref().map(|t| t.chars().count()).unwrap_or(0),
        ms: super::activity::now_ms().saturating_sub(started),
        ok: result.is_ok(),
    });
    result
}

async fn run_check_inner(root: &Path, settings: &crate::settings::Settings, args: &Value) -> Result<String, String> {
    if settings.checks.is_empty() {
        return Ok(
            "run_check is not configured for this project — add entries to the top-level `checks` \
             array in .cimp/config.json (each a { name, cmd, parser, timeout_secs })."
                .to_string(),
        );
    }
    let requested = args.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
    let names = || settings.checks.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ");
    let def = if requested.is_empty() {
        match settings.checks.as_slice() {
            [only] => only,
            _ => {
                return Err(format!(
                    "run_check needs a `name` — this project has {} configured checks: {}",
                    settings.checks.len(),
                    names()
                ))
            }
        }
    } else {
        match settings.checks.iter().find(|c| c.name == requested) {
            Some(c) => c,
            None => return Err(format!("run_check: no configured check named `{requested}` — configured: {}", names())),
        }
    };
    let changed_only = args.get("changed_only").and_then(|v| v.as_bool()).unwrap_or(false);
    let max_rows = limits(settings).0;
    crate::checks::run(root, def, changed_only)
        .await
        .map(|report| fmt_check_report(&report, max_rows))
        .map_err(|e| format!("run_check `{}` failed: {e}", def.name))
}

/// Render a [`crate::checks::CheckReport`] compactly: a header line (exit
/// code, duration, timeout flag) then one line per diagnostic group
/// (`severity · message (code folded in) · ×count · sample sites`), bounded
/// by `max_rows` like every other graph tool's result.
fn fmt_check_report(report: &crate::checks::CheckReport, max_rows: usize) -> String {
    let mut out = format!(
        "{} — exit {} · {} ms{}\n",
        report.name,
        report.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".to_string()),
        report.duration_ms,
        if report.timed_out { " · TIMED OUT (partial output parsed)" } else { "" },
    );
    if report.groups.is_empty() {
        out.push_str("No diagnostics.");
        return out;
    }
    let mut lines: Vec<String> = report
        .groups
        .iter()
        .take(max_rows)
        .map(|g| {
            let sites: Vec<String> = g.sites.iter().map(|(f, l)| format!("{f}:{l}")).collect();
            format!("{} · {} · ×{} · {}", g.severity.as_str(), g.message, g.count, sites.join(", "))
        })
        .collect();
    if report.groups.len() > max_rows {
        lines.push(format!("… (+{} more groups)", report.groups.len() - max_rows));
    }
    out.push_str(&lines.join("\n"));
    out
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

/// Embedder-backed semantic **code** search, degrading to a clear
/// "unavailable" message on any miss (feature off, no embedder configured/
/// reachable, no code vectors yet, or a dim mismatch after a model change) —
/// there's no full-text fallback for code chunks, so this can't silently
/// re-run as a keyword search the way [`semantic_query`] does.
async fn semantic_code_query(
    idx: &GraphIndex,
    settings: &crate::settings::Settings,
    query: &str,
    k: usize,
) -> Result<String, String> {
    let unavailable = || {
        Ok("(semantic code search unavailable — try `graph_find_symbol` or `graph_struct_search` instead)"
            .to_string())
    };
    let g = &settings.graph;
    if !g.embed_code_bodies {
        return unavailable();
    }
    let epoch = match idx.current_code_epoch() {
        Ok(Some(e)) => e,
        _ => return unavailable(),
    };
    let Some(embedder) = super::embed::Embedder::new(&g.embedding_endpoint, &g.embedding_model)
    else {
        return unavailable();
    };
    let qv = match embedder.embed_one(query).await {
        Ok(v) => v,
        Err(_) => return unavailable(),
    };
    match idx.semantic_code_search(&qv, &epoch, k) {
        Ok(hits) if !hits.is_empty() => {
            // `dist` is a cosine DISTANCE (lower = more similar), matching the
            // doc-search convention. No body text — chain into `graph_snippet`.
            let mut lines: Vec<String> = hits
                .iter()
                .take(k)
                .map(|(s, dist)| {
                    format!("{}:{} · {} · {} · distance {:.3}", s.file, s.start_line, s.kind, s.signature, dist)
                })
                .collect();
            if hits.len() > k {
                lines.push(format!("… (+{} more)", hits.len() - k));
            }
            let body = lines.join("\n");
            Ok(format!(
                "(semantic code — nearest first; lower distance = more similar; use `graph_snippet` for the body)\n{body}"
            ))
        }
        _ => unavailable(),
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
        .map(|s| {
            let tag = if s.is_test { " [test]" } else { "" };
            format!("{} ({}) — {}:{}  {}{}", s.name, s.kind, s.file, s.start_line, s.signature, tag)
        })
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

/// Render `graph_recent_changes` rows (churn-ranked, most-touched first) as
/// `file · N× · "subject" (age)` lines.
fn fmt_recent_changes(rows: &[super::gitmeta::FileChurn], max_rows: usize) -> String {
    if rows.is_empty() {
        return "No git churn recorded in this window — not a git repository, or nothing touched \
                 in range."
            .to_string();
    }
    let now = super::gitmeta::now_ts();
    let mut lines: Vec<String> = rows
        .iter()
        .take(max_rows)
        .map(|c| {
            format!(
                "{} · {}× · \"{}\" ({})",
                c.file,
                c.touches_90d,
                super::gitmeta::truncate_subject(&c.last_subject, 60),
                super::gitmeta::relative_age(now, c.last_ts)
            )
        })
        .collect();
    if rows.len() > max_rows {
        lines.push(format!("… (+{} more)", rows.len() - max_rows));
    }
    lines.join("\n")
}

fn fmt_working_set(ws: &[WorkingSetEntry], max_rows: usize) -> String {
    if ws.is_empty() {
        return "No files touched in this session yet.".to_string();
    }
    let mut lines: Vec<String> = ws
        .iter()
        .take(max_rows)
        .map(|e| {
            let syms = if e.top_symbols.is_empty() {
                String::new()
            } else {
                format!("  [{}]", e.top_symbols.join(", "))
            };
            format!("{} — {}× (last {}){}", e.path, e.touches, e.last_kind, syms)
        })
        .collect();
    if ws.len() > max_rows {
        lines.push(format!("… (+{} more)", ws.len() - max_rows));
    }
    format!("Current session working set (most active first):\n{}", lines.join("\n"))
}

fn fmt_notes(notes: &[MemNote], max_rows: usize) -> String {
    if notes.is_empty() {
        return "No notes recorded for this session.".to_string();
    }
    let mut lines: Vec<String> = notes
        .iter()
        .take(max_rows)
        .map(|n| format!("{}{}", if n.pinned { "📌 " } else { "• " }, n.text))
        .collect();
    if notes.len() > max_rows {
        lines.push(format!("… (+{} more)", notes.len() - max_rows));
    }
    lines.join("\n")
}

/// V12 Phase E: render project facts (pinned first, then newest — the order
/// [`super::index::GraphIndex::list_project_facts`] already returns them in).
fn fmt_facts(facts: &[ProjectFact], max_rows: usize) -> String {
    if facts.is_empty() {
        return "No project facts recorded yet.".to_string();
    }
    let mut lines: Vec<String> = facts
        .iter()
        .take(max_rows)
        .map(|f| format!("{}{}", if f.pinned { "📌 " } else { "• " }, f.text))
        .collect();
    if facts.len() > max_rows {
        lines.push(format!("… (+{} more)", facts.len() - max_rows));
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

/// Execute `graph_snippet`: resolve a definition (by `symbol`, or by
/// `file`+`line`) and return its source body sliced from disk with a compact
/// header. Reads the file at call time — spans can drift a few lines between
/// watcher debounces, so a possibly-stale span is flagged by comparing the
/// stored content hash against the current one. Bounded by `max_body_bytes`.
fn run_snippet(
    root: &Path,
    idx: &GraphIndex,
    args: &Value,
    max_rows: usize,
    max_body_bytes: usize,
) -> Result<String, String> {
    let symbol = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("").trim();
    let file_arg = args.get("file").and_then(|v| v.as_str()).unwrap_or("").trim();
    let context_lines = args
        .get("context_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(200) as usize;

    // Resolve the target definition.
    let hit = if !symbol.is_empty() {
        let hits = idx.find_symbol(symbol).map_err(|e| e.to_string())?;
        match hits.len() {
            0 => return Ok(format!("No symbol named `{symbol}` is defined in this project.")),
            1 => hits.into_iter().next().unwrap(),
            // Ambiguous — return the disambiguation list, never a body.
            _ => {
                return Ok(format!(
                    "`{symbol}` is defined in {} places — pass `file`+`line` (or a more specific name) to pick one:\n{}",
                    hits.len(),
                    fmt_symbols(&hits, max_rows)
                ))
            }
        }
    } else if !file_arg.is_empty() {
        let line = match args.get("line").and_then(|v| v.as_u64()) {
            Some(l) if l >= 1 => l as u32,
            _ => return Err("graph_snippet with `file` also needs a 1-based `line`".into()),
        };
        match idx.symbol_at(file_arg, line).map_err(|e| e.to_string())? {
            Some(h) => h,
            None => {
                return Ok(format!(
                    "No indexed definition encloses {file_arg}:{line} (it may be an import/blank region). \
                     Use Read with offset/limit for exact text."
                ))
            }
        }
    } else {
        return Err("graph_snippet needs either `symbol`, or `file` + `line`".into());
    };

    // Read the file from disk, confined to the project root.
    let abs = confine_to_root(root, &hit.file)?;
    let content = std::fs::read_to_string(&abs).map_err(|e| format!("cannot read {}: {e}", hit.file))?;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    // Whole-file span (top-level scripts): don't dump the file — outline + hint.
    // Require the span to reach the last line (`>= total`), not merely near it,
    // so an ordinary function that happens to end a line before EOF isn't
    // mistaken for a whole-file symbol.
    let whole_file = hit.start_line <= 1 && (hit.end_line as usize) >= total && total > 3;
    if whole_file {
        let outline = idx.outline(&hit.file).map_err(|e| e.to_string())?;
        return Ok(format!(
            "{} — {} spans the whole file ({} lines); here's its outline. Use Read with offset/limit for the body:\n{}",
            hit.file,
            hit.name,
            total,
            fmt_symbols(&outline, max_rows)
        ));
    }

    // Slice [start_line, end_line] ± context_lines (1-based → 0-based indices).
    let first = (hit.start_line as usize).saturating_sub(1).saturating_sub(context_lines);
    let last = ((hit.end_line as usize) + context_lines).min(total); // exclusive
    let slice = if first < last { lines[first..last].join("\n") } else { String::new() };
    let (body, truncated) = cap_bytes(&slice, max_body_bytes);

    // Staleness: on-disk content hash vs the stored one.
    let stale = matches!(
        idx.stored_file_hash(&hit.file).map_err(|e| e.to_string())?,
        Some(stored) if stored != super::model::fnv1a_hex(&content)
    );

    let callers = idx.callers_count(&hit.name).unwrap_or(0);
    let header = format!(
        "{}:{}-{} · {} · {} · {} callers",
        hit.file, hit.start_line, hit.end_line, hit.kind, hit.visibility, callers
    );
    let mut out = String::new();
    if stale {
        out.push_str("note: file changed after the last index pass; the span may have drifted\n");
    }
    out.push_str(&header);
    out.push('\n');
    out.push_str(&body);
    if truncated {
        out.push_str(&format!("\n… [truncated at {max_body_bytes} bytes]"));
    }
    Ok(out)
}

/// Execute `graph_repo_map`: render the project orientation map, folding in the
/// caller's session working set when the call can be scoped to a session.
fn run_repo_map(
    idx: &GraphIndex,
    settings: &crate::settings::Settings,
    args: &Value,
    agent: Option<&str>,
) -> Result<String, String> {
    let budget = args
        .get("budget_chars")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(settings.graph.repo_map_budget_chars as usize);
    let boost = repo_map_session_boost(idx, agent);
    let map = super::context::repo_map(idx, budget, &boost);
    if map.is_empty() {
        Ok("No project map yet — the graph has no call edges to rank files by.".to_string())
    } else {
        Ok(map)
    }
}

/// Execute `graph_impact` (V12 Phase B): resolve the roots — either the
/// `symbols` argument (comma/space-separated names) or, by default, the
/// working-tree diff vs `HEAD` mapped through symbol spans
/// ([`super::impact::changed_symbols`]) — then find their transitive
/// dependents and render a compact report: the changed/root symbols, the
/// flattened dependent list (`~` marks every hit as approximate — the call
/// graph is name-keyed, never id-resolved), a file-level rollup, and any
/// changed-but-unindexed files. `include_tests: true` (V12 Phase C) appends a
/// candidate affected-tests block below, computed with the same roots/depth
/// via [`GraphIndex::tests_for`].
fn run_impact(root: &Path, idx: &GraphIndex, args: &Value, max_rows: usize) -> Result<String, String> {
    let depth = args
        .get("depth")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(3)
        .clamp(1, 6);
    let symbols_arg = args.get("symbols").and_then(|v| v.as_str()).unwrap_or("").trim();

    let (root_names, changed_syms, unindexed): (Vec<String>, Vec<SymbolHit>, Vec<String>) =
        if !symbols_arg.is_empty() {
            let names: Vec<String> = symbols_arg
                .split(|c: char| c == ',' || c.is_whitespace())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            (names, Vec::new(), Vec::new())
        } else {
            let set = super::impact::changed_symbols(root, idx).map_err(|e| e.to_string())?;
            let mut names: Vec<String> = set.changed.iter().map(|s| s.name.clone()).collect();
            names.sort();
            names.dedup();
            (names, set.changed, set.unindexed)
        };

    if root_names.is_empty() {
        return Ok(if unindexed.is_empty() {
            "No changes detected (working tree matches HEAD).".to_string()
        } else {
            format!(
                "No indexed symbols changed. Changed but not indexed ({}): {}",
                unindexed.len(),
                unindexed.join(", ")
            )
        });
    }

    let dependents = idx.dependents_transitive(&root_names, depth, max_rows).map_err(|e| e.to_string())?;

    let mut out = String::new();
    if !changed_syms.is_empty() {
        let list: Vec<String> = changed_syms
            .iter()
            .map(|s| format!("{} ({}:{})", s.name, s.file, s.start_line))
            .collect();
        out.push_str(&format!("Changed symbols ({}): {}\n\n", changed_syms.len(), list.join(", ")));
    } else {
        out.push_str(&format!("Roots: {}\n\n", root_names.join(", ")));
    }

    if dependents.is_empty() {
        out.push_str("No dependents found (nothing in the index transitively calls the changed symbol(s)).");
    } else {
        let mut lines: Vec<String> = dependents
            .iter()
            .take(max_rows)
            .map(|d| {
                format!(
                    "{}{}:{} · {} · depth {}",
                    if d.approx { "~" } else { "" },
                    d.symbol.file,
                    d.symbol.start_line,
                    d.symbol.name,
                    d.depth
                )
            })
            .collect();
        if dependents.len() > max_rows {
            lines.push(format!("… (+{} more)", dependents.len() - max_rows));
        }
        out.push_str(&lines.join("\n"));
        let files: std::collections::BTreeSet<&str> =
            dependents.iter().map(|d| d.symbol.file.as_str()).collect();
        out.push_str(&format!(
            "\n\n{} file{} depend on your change (approximate — call edges are name-keyed, not id-resolved).",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        ));
    }

    if !unindexed.is_empty() {
        out.push_str(&format!(
            "\n\nChanged but not indexed ({}): {}",
            unindexed.len(),
            unindexed.join(", ")
        ));
    }

    if args.get("include_tests").and_then(|v| v.as_bool()).unwrap_or(false) {
        let tests = idx.tests_for(&root_names, depth, max_rows).map_err(|e| e.to_string())?;
        out.push_str("\n\n");
        out.push_str(&fmt_affected_tests(&tests, max_rows));
    }

    Ok(out)
}

/// Execute `graph_tests_for` (V12 Phase C): resolve the root symbol name(s) —
/// either the `symbol` argument directly, or every NON-TEST definition name in
/// `file` (via `outline`) — then render the candidate tests reaching them
/// ([`GraphIndex::tests_for`]). File mode drops the file's own test
/// definitions from the root set: `dependents_transitive` never reports a
/// root as its own dependent, so a test living in the same file as the code
/// it exercises (a `#[cfg(test)] mod`, very common) would otherwise
/// self-exclude from its own result.
fn run_tests_for(idx: &GraphIndex, args: &Value, max_rows: usize) -> Result<String, String> {
    let symbol = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("").trim();
    let file = args.get("file").and_then(|v| v.as_str()).unwrap_or("").trim();
    let depth = args
        .get("depth")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(3)
        .clamp(1, 6);

    let roots: Vec<String> = if !symbol.is_empty() {
        vec![symbol.to_string()]
    } else if !file.is_empty() {
        let syms = idx.outline(file).map_err(|e| e.to_string())?;
        if syms.is_empty() {
            return Ok(format!("No indexed definitions in {file}."));
        }
        let mut names: Vec<String> = syms.iter().filter(|s| !s.is_test).map(|s| s.name.clone()).collect();
        names.sort();
        names.dedup();
        if names.is_empty() {
            return Ok(format!("{file} has no non-test definitions to find tests for."));
        }
        names
    } else {
        return Err("graph_tests_for needs either `symbol` or `file`".into());
    };

    let tests = idx.tests_for(&roots, depth, max_rows).map_err(|e| e.to_string())?;
    Ok(fmt_affected_tests(&tests, max_rows))
}

/// Render a candidate test list (from [`GraphIndex::tests_for`]) as
/// `file:line · name` rows under a labeled, caveated header — shared by
/// `graph_tests_for` and `graph_impact`'s `include_tests` block.
fn fmt_affected_tests(tests: &[SymbolHit], max_rows: usize) -> String {
    if tests.is_empty() {
        return "No candidate tests found reaching this change (dynamic dispatch/fixtures aren't \
                 captured — absence here isn't proof of no coverage)."
            .to_string();
    }
    let mut lines: Vec<String> = tests
        .iter()
        .take(max_rows)
        .map(|s| format!("{}:{} · {}", s.file, s.start_line, s.name))
        .collect();
    if tests.len() > max_rows {
        lines.push(format!("… (+{} more)", tests.len() - max_rows));
    }
    format!(
        "Candidate affected tests ({}) — dynamic dispatch/fixtures aren't captured, so this may \
         under-report:\n{}",
        tests.len(),
        lines.join("\n")
    )
}

/// The caller's session working set as `(path, weight)` for repo-map ranking,
/// or empty when there's no scoped session (the offload worker) or no activity.
fn repo_map_session_boost(idx: &GraphIndex, agent: Option<&str>) -> Vec<(String, f64)> {
    let Ok(Some(sid)) = idx.mem_current_session_for(agent) else {
        return Vec::new();
    };
    let Ok(ws) = idx.mem_working_set(&sid, 20) else {
        return Vec::new();
    };
    ws.iter()
        .map(|e| (e.path.clone(), super::context::session_weight(&e.last_kind)))
        .collect()
}

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary.
/// Returns `(text, truncated)`.
fn cap_bytes(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// Resolve a project-relative indexed path to an absolute file inside `root`,
/// refusing anything that escapes it. Indexed paths are already project-relative
/// and trusted, but `graph_snippet` reads arbitrary disk, so this is defense in
/// depth (same posture as the `read_file` native tool's confinement).
fn confine_to_root(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let canon_root = root.canonicalize().map_err(|e| format!("cannot resolve project root: {e}"))?;
    let canon = canon_root
        .join(rel)
        .canonicalize()
        .map_err(|_| format!("{rel} not found on disk"))?;
    if !canon.starts_with(&canon_root) {
        return Err(format!("refusing to read {rel} — outside the project root"));
    }
    Ok(canon)
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

#[cfg(test)]
mod run_check_tests {
    use super::{fmt_check_report, run_check_inner, run_check_spec};
    use crate::checks::{CheckDef, CheckReport, DiagGroup, ParserKind, Severity};
    use crate::settings::Settings;
    use serde_json::json;

    fn def(name: &str, cmd: &str) -> CheckDef {
        CheckDef { name: name.to_string(), cmd: cmd.to_string(), parser: ParserKind::GenericGcc, timeout_secs: 30 }
    }

    #[test]
    fn spec_has_no_required_args() {
        let spec = run_check_spec();
        assert_eq!(spec.name, "run_check");
        assert_eq!(spec.parameters["required"], json!([]));
    }

    #[tokio::test]
    async fn empty_config_reports_not_configured() {
        let settings = Settings::default();
        assert!(settings.checks.is_empty());
        let out = run_check_inner(&std::env::temp_dir(), &settings, &json!({})).await.expect("ok result");
        assert!(out.contains("not configured"), "{out}");
        assert!(out.contains("checks"), "{out}");
    }

    #[tokio::test]
    async fn unknown_name_lists_configured_checks() {
        let mut settings = Settings::default();
        settings.checks = vec![def("cargo", "cargo check")];
        let err = run_check_inner(&std::env::temp_dir(), &settings, &json!({ "name": "nope" }))
            .await
            .expect_err("unknown name should error");
        assert!(err.contains("no configured check named `nope`"), "{err}");
        assert!(err.contains("cargo"), "{err}");
    }

    #[tokio::test]
    async fn ambiguous_without_name_lists_configured_checks() {
        let mut settings = Settings::default();
        settings.checks = vec![def("cargo", "cargo check"), def("tsc", "tsc --noEmit")];
        let err = run_check_inner(&std::env::temp_dir(), &settings, &json!({}))
            .await
            .expect_err("multiple configured checks with no name should error");
        assert!(err.contains("needs a `name`"), "{err}");
        assert!(err.contains("cargo") && err.contains("tsc"), "{err}");
    }

    #[tokio::test]
    async fn sole_configured_check_runs_without_a_name() {
        let cargo = which::which("cargo").expect("cargo on PATH");
        let mut settings = Settings::default();
        settings.checks = vec![def("only", &format!("\"{}\" --version", cargo.display()))];
        let out = run_check_inner(&std::env::temp_dir(), &settings, &json!({})).await.expect("ok result");
        assert!(out.contains("only"), "{out}");
        assert!(out.contains("exit 0"), "{out}");
    }

    #[test]
    fn fmt_check_report_renders_header_groups_and_overflow() {
        let report = CheckReport {
            name: "cargo".to_string(),
            exit_code: Some(1),
            duration_ms: 42,
            timed_out: false,
            groups: vec![
                DiagGroup {
                    key: "k1".into(),
                    severity: Severity::Error,
                    message: "E0425: cannot find value ‹…› in this scope".into(),
                    count: 3,
                    sites: vec![("src/a.rs".into(), 10), ("src/b.rs".into(), 20)],
                },
                DiagGroup { key: "k2".into(), severity: Severity::Warning, message: "unused import".into(), count: 1, sites: vec![("src/c.rs".into(), 1)] },
            ],
        };
        let out = fmt_check_report(&report, 1);
        assert!(out.starts_with("cargo — exit 1 · 42 ms"), "{out}");
        assert!(out.contains("error · E0425"), "{out}");
        assert!(out.contains("src/a.rs:10, src/b.rs:20"), "{out}");
        // Capped at max_rows=1: only the first group's line, plus an overflow note.
        assert!(!out.contains("unused import"), "{out}");
        assert!(out.contains("+1 more group"), "{out}");
    }

    #[test]
    fn fmt_check_report_no_diagnostics() {
        let report = CheckReport { name: "cargo".into(), exit_code: Some(0), duration_ms: 5, timed_out: false, groups: vec![] };
        let out = fmt_check_report(&report, 50);
        assert!(out.contains("No diagnostics."), "{out}");
    }

    #[test]
    fn fmt_check_report_flags_timeout() {
        let report = CheckReport { name: "slow".into(), exit_code: None, duration_ms: 10_000, timed_out: true, groups: vec![] };
        let out = fmt_check_report(&report, 50);
        assert!(out.contains("TIMED OUT"), "{out}");
    }
}

#[cfg(test)]
mod snippet_tests {
    use super::{cap_bytes, run_snippet, GraphIndex};
    use crate::graph::{parse_file, Lang};
    use serde_json::json;
    use std::path::PathBuf;

    const SRC: &str = "pub fn alpha() -> i32 {\n    let x = 1;\n    x + 1\n}\npub fn beta() -> i32 { alpha() }\n";

    /// Build a temp project on disk (real source files) + its graph index.
    fn setup(tag: &str, files: &[(&str, &str)]) -> (PathBuf, GraphIndex) {
        let dir = std::env::temp_dir().join(format!("snip-{tag}-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        for (rel, src) in files {
            let abs = dir.join(rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(&abs, src).unwrap();
            idx.index_file_graph(&parse_file(rel, src, Lang::Rust)).expect("index");
        }
        (dir, idx)
    }

    #[test]
    fn by_symbol_returns_body_not_whole_file() {
        let (dir, idx) = setup("body", &[("src/geo.rs", SRC)]);
        let out = run_snippet(&dir, &idx, &json!({ "symbol": "alpha" }), 50, 16_384).unwrap();
        assert!(out.contains("src/geo.rs:"), "header present: {out}");
        assert!(out.contains("let x = 1;"), "body present: {out}");
        assert!(!out.contains("fn beta"), "did not dump the rest of the file: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ambiguous_name_lists_without_a_body() {
        let (dir, idx) = setup(
            "amb",
            &[("src/a.rs", "pub fn dup() -> i32 { 1 }\n"), ("src/b.rs", "pub fn dup() -> i32 { 2 }\n")],
        );
        let out = run_snippet(&dir, &idx, &json!({ "symbol": "dup" }), 50, 16_384).unwrap();
        assert!(out.contains("defined in 2 places"), "disambiguation: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_line_resolves_enclosing_symbol() {
        let (dir, idx) = setup("fl", &[("src/geo.rs", SRC)]);
        // Line 2 sits inside alpha's body.
        let out = run_snippet(&dir, &idx, &json!({ "file": "src/geo.rs", "line": 2 }), 50, 16_384).unwrap();
        assert!(out.contains("let x = 1;"), "resolved alpha's body: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn byte_cap_truncates() {
        let (dir, idx) = setup("cap", &[("src/geo.rs", SRC)]);
        let out = run_snippet(&dir, &idx, &json!({ "symbol": "alpha" }), 50, 8).unwrap();
        assert!(out.contains("[truncated at 8 bytes]"), "truncated: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_symbol_reports_clearly() {
        let (dir, idx) = setup("miss", &[("src/geo.rs", SRC)]);
        let out = run_snippet(&dir, &idx, &json!({ "symbol": "nope" }), 50, 16_384).unwrap();
        assert!(out.contains("No symbol named"), "{out}");
        // No args at all is an error, not a body.
        assert!(run_snippet(&dir, &idx, &json!({}), 50, 16_384).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cap_bytes_respects_char_boundary() {
        // 'h' = 1 byte, 'é' = 2 bytes; a 2-byte cap must not split 'é'.
        let (s, truncated) = cap_bytes("héllo", 2);
        assert!(truncated);
        assert_eq!(s, "h");
        let (s2, t2) = cap_bytes("abc", 10);
        assert!(!t2);
        assert_eq!(s2, "abc");
    }
}

#[cfg(test)]
mod impact_tool_tests {
    use super::{run_impact, GraphIndex};
    use crate::graph::{parse_file, Lang};
    use serde_json::json;
    use std::path::PathBuf;
    use std::process::Command as StdCommand;

    /// A throwaway git repo with `src/chain.rs` committed (`c` calls `b` calls
    /// `a`) plus its graph index — the fixture both the symbols-arg and
    /// diff-mode tests build on.
    fn setup(tag: &str) -> (PathBuf, GraphIndex) {
        let dir = std::env::temp_dir().join(format!("impact-tool-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let git = |args: &[&str]| {
            let out = StdCommand::new("git").args(args).current_dir(&dir).output().expect("git");
            assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        // Ignore the graph store's own db dir (mirrors the real `.cimp/`
        // gitignore rule) so it doesn't show up as an untracked "change".
        std::fs::write(dir.join(".gitignore"), ".ckg/\n").unwrap();
        let src = "pub fn a() {}\npub fn b() { a() }\npub fn c() { b() }\n";
        std::fs::write(dir.join("src/chain.rs"), src).unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);

        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("src/chain.rs", src, Lang::Rust)).expect("index");
        (dir, idx)
    }

    #[test]
    fn symbols_arg_reports_dependents_with_tilde() {
        let (dir, idx) = setup("symbols");
        let out = run_impact(&dir, &idx, &json!({ "symbols": "a" }), 50).expect("run_impact");
        assert!(out.contains("Roots: a"), "{out}");
        assert!(out.contains("~src/chain.rs:2 · b · depth 1"), "{out}");
        assert!(out.contains("~src/chain.rs:3 · c · depth 2"), "{out}");
        assert!(out.contains("2 files depend on your change") || out.contains("1 file depend"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn symbols_arg_respects_depth() {
        let (dir, idx) = setup("depth");
        let out = run_impact(&dir, &idx, &json!({ "symbols": "a", "depth": 1 }), 50).expect("run_impact");
        assert!(out.contains("· b · depth 1"), "{out}");
        assert!(!out.contains("· c ·"), "depth=1 must not reach c: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diff_mode_maps_the_edit_to_its_symbol() {
        let (dir, idx) = setup("diff");
        // Edit a()'s body — still empty braces content-wise but touches its line.
        std::fs::write(dir.join("src/chain.rs"), "pub fn a() { /* changed */ }\npub fn b() { a() }\npub fn c() { b() }\n").unwrap();

        let out = run_impact(&dir, &idx, &json!({}), 50).expect("run_impact");
        assert!(out.contains("Changed symbols"), "{out}");
        assert!(out.contains("a ("), "{out}");
        assert!(out.contains("· b · depth 1"), "{out}");
        assert!(out.contains("· c · depth 2"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diff_mode_no_git_repo_is_an_error() {
        let dir = std::env::temp_dir().join(format!("impact-tool-norepo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        let err = run_impact(&dir, &idx, &json!({}), 50).expect_err("not a git repo");
        assert!(err.contains("not a git repository") || err.to_lowercase().contains("git"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_changes_no_symbols_reports_clean_tree() {
        let (dir, idx) = setup("clean");
        let out = run_impact(&dir, &idx, &json!({}), 50).expect("run_impact");
        assert!(out.contains("No changes detected"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn include_tests_appends_affected_tests_block() {
        // Same a<-b<-c chain, plus a #[test] fn reaching c() transitively —
        // include_tests should surface it; omitting the flag should not.
        let dir = std::env::temp_dir().join(format!("impact-tool-tests-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        let src = "pub fn a() {}\npub fn b() { a() }\npub fn c() { b() }\n#[test]\nfn test_c() { c() }\n";
        idx.index_file_graph(&parse_file("src/chain.rs", src, Lang::Rust)).expect("index");

        let out = run_impact(&dir, &idx, &json!({ "symbols": "a", "include_tests": true }), 50).expect("run_impact");
        assert!(out.contains("Candidate affected tests"), "{out}");
        assert!(out.contains("test_c"), "{out}");

        // Without the flag, test_c still shows up as an ordinary dependent
        // (it IS a real caller) but the labeled affected-tests block is absent.
        let out_off = run_impact(&dir, &idx, &json!({ "symbols": "a" }), 50).expect("run_impact");
        assert!(!out_off.contains("Candidate affected tests"), "{out_off}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests_for_tool_tests {
    use super::{run_tests_for, GraphIndex};
    use crate::graph::{parse_file, Lang};
    use serde_json::json;
    use std::path::PathBuf;

    /// one() <- two() <- test_it() (a #[test] fn), all in one file — the
    /// fixture that exercises the file-mode self-exclusion fix.
    fn setup(tag: &str) -> (PathBuf, GraphIndex) {
        let dir = std::env::temp_dir().join(format!("tests-for-tool-{tag}-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        let src = "pub fn one() {}\npub fn two() { one() }\n#[test]\nfn test_it() { two() }\n";
        idx.index_file_graph(&parse_file("src/chain.rs", src, Lang::Rust)).expect("index");
        (dir, idx)
    }

    #[test]
    fn symbol_mode_finds_transitive_test() {
        let (dir, idx) = setup("symbol");
        let out = run_tests_for(&idx, &json!({ "symbol": "one" }), 50).expect("run_tests_for");
        assert!(out.contains("test_it"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_mode_unions_non_test_file_symbols_as_roots() {
        // The file's own test (test_it) must NOT self-exclude — file mode
        // roots on {one, two} only, so test_it still surfaces as depth-1.
        let (dir, idx) = setup("file");
        let out = run_tests_for(&idx, &json!({ "file": "src/chain.rs" }), 50).expect("run_tests_for");
        assert!(out.contains("test_it"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_args_is_an_error() {
        let (dir, idx) = setup("missing");
        assert!(run_tests_for(&idx, &json!({}), 50).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_tests_found_is_a_clear_message() {
        let dir = std::env::temp_dir().join(format!("tests-for-tool-none-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("src/x.rs", "pub fn lonely() {}\n", Lang::Rust)).expect("index");
        let out = run_tests_for(&idx, &json!({ "symbol": "lonely" }), 50).expect("run_tests_for");
        assert!(out.contains("No candidate tests"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod recall_facts_tests {
    use super::{run_tool, GraphIndex};
    use serde_json::json;

    #[test]
    fn context_recall_appends_a_project_facts_section() {
        let dir = std::env::temp_dir().join(format!("recall-facts-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.record_mem_event("s1", "claude", "read", "a.rs", None, None, 100, None).unwrap();
        idx.add_project_fact("f1", "we chose FNV hashing for stability", "s1", 50, true).unwrap();
        idx.add_project_fact("f2", "the retry cap is 30s by design", "s1", 60, false).unwrap();

        let out = run_tool(&idx, "context_recall", &json!({}), 50, 200, Some("claude")).expect("run_tool");
        assert!(out.contains("## Project facts"), "{out}");
        assert!(out.contains("we chose FNV hashing for stability"), "{out}");
        assert!(out.contains("the retry cap is 30s by design"), "{out}");
        // Pinned fact renders first within the facts section.
        let facts_idx = out.find("## Project facts").unwrap();
        let pinned_pos = out[facts_idx..].find("FNV hashing").unwrap();
        let unpinned_pos = out[facts_idx..].find("retry cap").unwrap();
        assert!(pinned_pos < unpinned_pos, "{out}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn context_recall_omits_the_facts_section_when_there_are_none() {
        let dir = std::env::temp_dir().join(format!("recall-nofacts-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.record_mem_event("s1", "claude", "read", "a.rs", None, None, 100, None).unwrap();

        let out = run_tool(&idx, "context_recall", &json!({}), 50, 200, Some("claude")).expect("run_tool");
        assert!(!out.contains("## Project facts"), "{out}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

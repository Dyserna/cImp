//! Native `code_search` tool — case-insensitive literal/substring search
//! across an `allowed_root`, returning `path:line: snippet` hits. This is
//! the deep-search case that motivated the milestone: the local model
//! greps the tree and ccImp returns only the hits, so Opus never sees the
//! raw bytes.
//!
//! Dependency-free (no `regex`/`ripgrep`): a bounded manual walk with
//! substring matching. Skips VCS/build/dependency dirs and binary/large
//! files. Caps results and bytes so one call can't blow the budget.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use crate::offload::openai::ToolDef;

use super::ToolCtx;

const DEFAULT_MAX_RESULTS: usize = 100;
const MAX_RESULTS_CAP: usize = 500;
/// Skip files larger than this (likely binary/generated).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Bound the walk so a huge tree can't hang the tool.
const MAX_FILES_SCANNED: usize = 50_000;
const SNIPPET_MAX: usize = 240;

const IGNORE_DIRS: &[&str] = &[
    ".git", "node_modules", "target", "dist", "build", ".venv", "venv",
    "__pycache__", ".next", ".cache", "vendor", ".svelte-kit",
];

#[derive(Deserialize)]
struct Args {
    /// Literal substring to find (case-insensitive).
    query: String,
    /// Optional subdirectory (relative to a root, or absolute) to scope.
    #[serde(default)]
    path: Option<String>,
    /// Optional filename-suffix filter, e.g. ".rs" or ".ts".
    #[serde(default)]
    suffix: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

pub fn def() -> ToolDef {
    ToolDef::function(
        "code_search",
        "Search files under the allowed roots for a case-insensitive literal \
         substring. Returns up to max_results `path:line: snippet` hits. \
         Optionally scope with `path` (a subdirectory) and `suffix` (e.g. \
         \".rs\"). This is a literal search, not a regex.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Literal substring to find (case-insensitive)." },
                "path": { "type": "string", "description": "Optional subdirectory to scope the search." },
                "suffix": { "type": "string", "description": "Optional filename suffix filter, e.g. \".rs\"." },
                "max_results": { "type": "integer", "description": "Max hits (default 100, max 500)." }
            },
            "required": ["query"]
        }),
    )
}

pub async fn execute(args: serde_json::Value, ctx: &ToolCtx) -> Result<String, String> {
    let args: Args = serde_json::from_value(args).map_err(|e| format!("invalid code_search args: {e}"))?;
    if args.query.is_empty() {
        return Err("query must not be empty".into());
    }
    // Determine the search root: a confined subpath, else all roots.
    let roots: Vec<PathBuf> = match &args.path {
        Some(p) => vec![ctx.confine(p)?],
        None => ctx.allowed_roots.clone(),
    };
    let max_results = args.max_results.unwrap_or(DEFAULT_MAX_RESULTS).min(MAX_RESULTS_CAP);
    let needle = args.query.to_lowercase();
    let suffix = args.suffix.clone();

    // Filesystem walk is blocking — run it off the async runtime.
    let result = tokio::task::spawn_blocking(move || {
        let mut hits: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        let mut truncated = false;
        'outer: for root in &roots {
            let mut stack = vec![root.clone()];
            while let Some(dir) = stack.pop() {
                let entries = match std::fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let file_type = match entry.file_type() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    // Never traverse symlinks: a link under an allowed root
                    // could point outside it and leak confined content to the
                    // model. `file_type()` does not follow links (so this is
                    // already true today), but assert it explicitly so a
                    // future switch to `metadata()` can't silently open the
                    // escape.
                    if file_type.is_symlink() {
                        continue;
                    }
                    if file_type.is_dir() {
                        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        if IGNORE_DIRS.contains(&name) {
                            continue;
                        }
                        stack.push(path);
                        continue;
                    }
                    if !file_type.is_file() {
                        continue;
                    }
                    if let Some(suf) = &suffix {
                        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        if !name.ends_with(suf.as_str()) {
                            continue;
                        }
                    }
                    scanned += 1;
                    if scanned > MAX_FILES_SCANNED {
                        truncated = true;
                        break 'outer;
                    }
                    match std::fs::metadata(&path) {
                        Ok(m) if m.len() > MAX_FILE_BYTES => continue,
                        Ok(_) => {}
                        Err(_) => continue,
                    }
                    let content = match std::fs::read(&path) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    // Skip likely-binary files (NUL in the first chunk).
                    if content.iter().take(8192).any(|&b| b == 0) {
                        continue;
                    }
                    let text = String::from_utf8_lossy(&content);
                    for (lineno, line) in text.lines().enumerate() {
                        if line.to_lowercase().contains(&needle) {
                            let snippet: String = line.trim().chars().take(SNIPPET_MAX).collect();
                            hits.push(format!("{}:{}: {}", display_path(&path, root), lineno + 1, snippet));
                            if hits.len() >= max_results {
                                truncated = true;
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
        (hits, truncated, scanned)
    })
    .await
    .map_err(|e| format!("search task failed: {e}"))?;

    let (hits, truncated, scanned) = result;
    if hits.is_empty() {
        return Ok(format!("(no matches for `{}` in {} file(s) scanned)", args.query, scanned));
    }
    let mut out = hits.join("\n");
    if truncated {
        out.push_str(&format!(
            "\n[truncated at {} hit(s) — narrow with `path`/`suffix` or a more specific query]",
            hits.len()
        ));
    }
    Ok(out)
}

/// Render a hit path relative to its search root when possible, else
/// the full path.
fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

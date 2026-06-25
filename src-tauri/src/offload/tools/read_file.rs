//! Native `read_file` tool — bounded line/byte reads confined to an
//! `allowed_root`. `offset`/`limit` give the agent windowed reads so it
//! can map-reduce inputs larger than its working budget.

use serde::Deserialize;
use serde_json::json;

use crate::offload::openai::ToolDef;

use super::ToolCtx;

/// Default and max lines returned in one call (the loop also token-caps
/// the result downstream).
const DEFAULT_LIMIT: u64 = 400;
const MAX_LIMIT: u64 = 4000;
/// Hard byte ceiling on the rendered output regardless of line count.
const MAX_BYTES: usize = 256 * 1024;
/// Largest file (on disk) this tool will load into memory. Generous enough to
/// page through ordinary source files; refuses multi-MB blobs before reading.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    limit: Option<u64>,
}

pub fn def() -> ToolDef {
    ToolDef::function(
        "read_file",
        "Read a UTF-8 text file confined to the allowed roots. Returns lines \
         [offset, offset+limit) (1-based offset). Use offset/limit to page \
         through large files instead of reading them whole.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute, or relative to the project root)." },
                "offset": { "type": "integer", "description": "1-based line to start at (default 1)." },
                "limit": { "type": "integer", "description": "Max lines to return (default 400, max 4000)." }
            },
            "required": ["path"]
        }),
    )
}

pub async fn execute(args: serde_json::Value, ctx: &ToolCtx) -> Result<String, String> {
    let args: Args = serde_json::from_value(args).map_err(|e| format!("invalid read_file args: {e}"))?;
    let path = ctx.confine(&args.path)?;
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let start = args.offset.saturating_sub(1); // 1-based → 0-based; offset 0 or 1 → start 0

    // Refuse oversized files BEFORE reading them into memory. The previous
    // guard checked `limit >= MAX_LIMIT`, so a default-limit call on a huge
    // file slipped through and allocated the whole thing. Size is the right
    // criterion, and stat-then-refuse caps the allocation. The generous
    // ceiling still lets offset/limit page through ordinary large source files.
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "file is {} bytes — too large to read (limit {} bytes for this tool)",
            meta.len(),
            MAX_FILE_BYTES
        ));
    }
    let content = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    let text = String::from_utf8_lossy(&content);

    let mut out = String::new();
    let mut emitted = 0u64;
    let mut byte_budget = MAX_BYTES;
    let mut truncated = false;
    // Set when the very first line we tried to emit was itself larger than the
    // whole byte budget — distinct from "offset past EOF", which produces the
    // same empty `out`. Without this the model is told "no lines at offset N"
    // for a non-empty file and loops adjusting the offset that never helps.
    let mut oversized_line = false;
    for (idx, line) in text.lines().enumerate() {
        let lineno = idx as u64;
        if lineno < start {
            continue;
        }
        if emitted >= limit {
            truncated = true;
            break;
        }
        let rendered = format!("{}\t{}\n", lineno + 1, line);
        if rendered.len() > byte_budget {
            if out.is_empty() {
                oversized_line = true;
            }
            truncated = true;
            break;
        }
        byte_budget -= rendered.len();
        out.push_str(&rendered);
        emitted += 1;
    }

    if out.is_empty() {
        let total_lines = text.lines().count();
        if oversized_line {
            return Ok(format!(
                "(line {} is larger than the {}-byte single-call render budget, so no \
                 lines could be returned — the file has {} line(s). This is an oversized \
                 line, not a bad offset; the line is too large to display here.)",
                start + 1,
                MAX_BYTES,
                total_lines
            ));
        }
        return Ok(format!(
            "(no lines at offset {} — file has {} line(s))",
            args.offset.max(1),
            total_lines
        ));
    }
    if truncated {
        out.push_str(&format!(
            "\n[truncated — returned {} line(s) from offset {}; call again with a higher offset to continue]",
            emitted,
            args.offset.max(1)
        ));
    }
    Ok(out)
}

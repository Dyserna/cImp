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
/// Hard byte ceiling regardless of line count.
const MAX_BYTES: usize = 256 * 1024;

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

    let content = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    if content.len() > MAX_BYTES && limit >= MAX_LIMIT {
        // Whole-file path with a huge file: refuse and steer to paging.
        return Err(format!(
            "file is {} bytes — too large to read whole; pass offset/limit to page through it",
            content.len()
        ));
    }
    let text = String::from_utf8_lossy(&content);

    let mut out = String::new();
    let mut emitted = 0u64;
    let mut byte_budget = MAX_BYTES;
    let mut truncated = false;
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
            truncated = true;
            break;
        }
        byte_budget -= rendered.len();
        out.push_str(&rendered);
        emitted += 1;
    }

    if out.is_empty() {
        return Ok(format!(
            "(no lines at offset {} — file has {} line(s))",
            args.offset.max(1),
            text.lines().count()
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

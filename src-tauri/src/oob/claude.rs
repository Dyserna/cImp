//! V20: Claude Code out-of-band TTS via transcript tail.
//!
//! Claude Code appends a JSONL transcript per session at
//! `~/.claude/projects/<slug>/<id>.jsonl`, where `<slug>` is the project cwd
//! with every `\`, `/`, and `:` replaced by `-`. Assistant lines look like
//! `{"type":"assistant","message":{"id":..,"content":[{"type":"text",..},
//! {"type":"thinking",..}]}}` and the `text` block is written **complete at
//! message finish** (block-level). We tail the newest `*.jsonl` in the project
//! dir, emit each new assistant `text` block to TTS, and skip `thinking` and
//! tool blocks.
//!
//! Latency is sub-second in practice (spike 0b), well within TTS comfort.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::time::sleep;
use tracing::{debug, trace};

use super::OobContext;

const POLL: Duration = Duration::from_millis(200);

/// Tail the active transcript for `project_dir`, speaking new assistant text
/// until the tab's cancel token fires. Resilient: if the project dir or any
/// file is missing it simply waits; transient read/parse errors are skipped.
pub async fn run(project_dir: PathBuf, ctx: OobContext) {
    let root = match project_root(&project_dir) {
        Some(r) => r,
        None => {
            debug!(tab = ?ctx.tab, "Claude OOB: no home dir; transcript tail disabled");
            return;
        }
    };
    debug!(tab = ?ctx.tab, root = %root.display(), "Claude OOB: watching transcripts");

    let mut seen: HashSet<String> = HashSet::new();
    let mut cur: Option<PathBuf> = None;
    let mut offset: u64 = 0;
    // The first file we attach to may already hold a long backlog from before
    // launch; skip it by seeking to EOF. Files that appear *later* (a new
    // session) are read from the start.
    let mut first_attach = true;

    loop {
        if ctx.cancel.is_cancelled() {
            return;
        }

        match newest_jsonl(&root) {
            Some(path) if Some(&path) != cur.as_ref() => {
                // Rotated to a new (or first) transcript file.
                offset = if first_attach {
                    std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
                } else {
                    0
                };
                first_attach = false;
                cur = Some(path);
            }
            Some(_) => {}
            None => {
                // No transcript yet; wait for one to appear.
                tokio::select! {
                    _ = ctx.cancel.cancelled() => return,
                    _ = sleep(POLL) => continue,
                }
            }
        }

        if let Some(path) = cur.clone() {
            offset = drain_new_lines(&path, offset, &mut seen, &ctx).await;
        }

        tokio::select! {
            _ = ctx.cancel.cancelled() => return,
            _ = sleep(POLL) => {}
        }
    }
}

/// Read complete new lines from `path` starting at `offset`, speaking assistant
/// text, and return the new offset (advanced only past whole lines).
async fn drain_new_lines(
    path: &Path,
    mut offset: u64,
    seen: &mut HashSet<String>,
    ctx: &OobContext,
) -> u64 {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return offset, // rotated away mid-loop; retry next tick.
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(offset);
    if len <= offset {
        return offset; // nothing new (or truncated/rotated).
    }
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return offset;
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return offset;
    }

    // Only consume up to the last newline; a trailing partial line is left for
    // the next tick (offset not advanced past it).
    let last_nl = match buf.rfind('\n') {
        Some(i) => i,
        None => return offset, // no complete line yet.
    };
    let complete = &buf[..=last_nl];
    offset += complete.len() as u64;

    for line in complete.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(obj) = serde_json::from_str::<Value>(line) {
            for (key, text) in assistant_texts(&obj) {
                if seen.insert(key) {
                    trace!(tab = ?ctx.tab, "Claude OOB: speaking assistant block");
                    ctx.speak(&text).await;
                }
            }
        }
    }
    offset
}

/// Extract `(dedup_key, text)` for each assistant `text` block in a transcript
/// line. `thinking` and tool blocks are skipped. The key is `messageID` +
/// content prefix so a re-read (rotation/compaction) doesn't re-speak.
fn assistant_texts(obj: &Value) -> Vec<(String, String)> {
    if obj.get("type").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    let msg = match obj.get("message") {
        Some(m) => m,
        None => return Vec::new(),
    };
    let mid = msg.get("id").and_then(Value::as_str).unwrap_or("");
    let mut out = Vec::new();
    if let Some(parts) = msg.get("content").and_then(Value::as_array) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("text") {
                let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let prefix: String = text.chars().take(40).collect();
                out.push((format!("{mid}:{prefix}"), text.to_string()));
            }
        }
    }
    out
}

/// `~/.claude/projects/<slug>/` for `project_dir`. `None` if no home dir.
fn project_root(project_dir: &Path) -> Option<PathBuf> {
    let home = home_dir()?;
    Some(home.join(".claude").join("projects").join(slug_for(project_dir)))
}

/// Claude Code's project-dir slug: every path separator and `:` becomes `-`.
/// e.g. `P:\Documents\foo` -> `P--Documents-foo`.
fn slug_for(dir: &Path) -> String {
    dir.to_string_lossy()
        .chars()
        .map(|c| if c == '\\' || c == '/' || c == ':' { '-' } else { c })
        .collect()
}

/// Newest `*.jsonl` (by mtime) under `root`, or `None` if the dir is missing
/// or empty.
fn newest_jsonl(root: &Path) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if newest.as_ref().map_or(true, |(t, _)| mtime > *t) {
            newest = Some((mtime, path));
        }
    }
    newest.map(|(_, p)| p)
}

/// Resolve the user's home directory without pulling in a new dependency.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_replaces_separators_and_colon() {
        let s = slug_for(Path::new(r"P:\Documents\AI-private\cc-avatar\cctts"));
        assert_eq!(s, "P--Documents-AI-private-cc-avatar-cctts");
    }

    #[test]
    fn assistant_texts_skips_thinking_and_tools() {
        let obj: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"id":"m1","content":[
                {"type":"thinking","thinking":"hmm"},
                {"type":"text","text":"Hello there."},
                {"type":"tool_use","name":"Bash"}
            ]}}"#,
        )
        .unwrap();
        let got = assistant_texts(&obj);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "Hello there.");
        assert!(got[0].0.starts_with("m1:"));
    }

    #[test]
    fn non_assistant_lines_yield_nothing() {
        let obj: Value =
            serde_json::from_str(r#"{"type":"user","message":{"content":"hi"}}"#).unwrap();
        assert!(assistant_texts(&obj).is_empty());
    }

    #[test]
    fn empty_text_blocks_are_ignored() {
        let obj: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"   "}]}}"#,
        )
        .unwrap();
        assert!(assistant_texts(&obj).is_empty());
    }
}

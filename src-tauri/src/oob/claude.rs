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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::time::sleep;
use tracing::{debug, trace};

use super::OobContext;
use crate::state::StateSignal;

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
    // Tool-use IDs of `Task` sub-agents launched but not yet resolved. Non-
    // empty ⇒ at least one agent is running, which holds the avatar in Thinking
    // (see `update_agents`). Keyed by the `toolu_…` id so out-of-order results
    // and parallel launches are matched exactly.
    let mut agents: HashSet<String> = HashSet::new();
    // V14 Phase C: tool_use_id -> name, so a later tool_result can be
    // attributed to the tool that produced it for usage accounting. Same
    // per-session lifetime as `agents` — cleared on session rotation below.
    let mut tool_names = ToolNameRing::default();
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
                // Any agents we were tracking belonged to the previous session
                // file; a new file is a new session. Clear and, if we had
                // announced "agents active", release the avatar so it can't
                // wedge in Thinking across the rotation.
                if !agents.is_empty() {
                    agents.clear();
                    ctx.signal(StateSignal::AgentsActiveChanged {
                        tab: ctx.tab.clone(),
                        active: false,
                    });
                }
                // The tool-name ring is per-session too: a new file means old
                // tool_use ids can never see a matching tool_result.
                tool_names.clear();
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
            // The transcript filename stem is the Claude session id — the memory
            // scope key. `<id>.jsonl` → `<id>`.
            let session_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            offset = drain_new_lines(
                &path,
                offset,
                &mut seen,
                &mut agents,
                &mut tool_names,
                &project_dir,
                &session_id,
                &ctx,
            )
            .await;
        }

        tokio::select! {
            _ = ctx.cancel.cancelled() => return,
            _ = sleep(POLL) => {}
        }
    }
}

/// Read complete new lines from `path` starting at `offset`, speaking assistant
/// text, and return the new offset (advanced only past whole lines).
#[allow(clippy::too_many_arguments)]
async fn drain_new_lines(
    path: &Path,
    mut offset: u64,
    seen: &mut HashSet<String>,
    agents: &mut HashSet<String>,
    tool_names: &mut ToolNameRing,
    project_dir: &Path,
    session_id: &str,
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
            update_agents(&obj, agents, ctx);
            record_tool_events(&obj, project_dir, session_id, ctx);
            record_usage(&obj, tool_names, project_dir, session_id, ctx);
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

/// The `tool_use` name Claude Code emits when it launches a sub-agent. Keyed
/// as a named constant so the one dependency on this string is greppable if a
/// future release renames it (see also the `esc to interrupt` marker in
/// `processing::permission`).
const TASK_TOOL_NAME: &str = "Task";

/// The content-block array of a transcript line's `message`, or `None` when the
/// line has no array content (a plain-string user prompt, or a non-message
/// line). Shared by `assistant_texts` and `update_agents` so the
/// `message.content[]` shape is unwrapped in exactly one place.
fn message_parts(obj: &Value) -> Option<&Vec<Value>> {
    obj.get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
}

/// Extract `(dedup_key, text)` for each assistant `text` block in a transcript
/// line. `thinking` and tool blocks are skipped. The key is `messageID` +
/// content prefix so a re-read (rotation/compaction) doesn't re-speak.
fn assistant_texts(obj: &Value) -> Vec<(String, String)> {
    if obj.get("type").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    let mid = obj
        .get("message")
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut out = Vec::new();
    if let Some(parts) = message_parts(obj) {
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

/// True when `obj` is a genuine user prompt — a `type:"user"` line whose content
/// is plain text (a string, or an array carrying a non-`tool_result` block)
/// rather than a tool-result carrier. Such a line is a turn boundary: the prior
/// turn is over, so any still-tracked `Task` ids (e.g. orphaned by an
/// Esc-interrupt that never wrote their `tool_result`) can be reclaimed.
fn is_user_prompt(obj: &Value) -> bool {
    if obj.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match obj.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(_)) => true,
        Some(Value::Array(parts)) => parts
            .iter()
            .any(|p| p.get("type").and_then(Value::as_str) != Some("tool_result")),
        _ => false,
    }
}

/// Update the in-flight `Task` sub-agent set from one transcript line and emit
/// `AgentsActiveChanged` when the running count crosses the zero boundary.
///
/// A `Task` launch is a `tool_use` block with `"name":"Task"` (in an assistant
/// message); its completion is a `tool_result` block whose `tool_use_id` matches
/// (in the following user message). `agents` holds only Task ids, so removing a
/// non-Task `tool_use_id` is a harmless no-op — we don't need to know which tool
/// a result belongs to, only whether it clears a tracked Task.
///
/// Sidechain lines (a sub-agent's own internal messages, `isSidechain:true`) are
/// skipped so a nested tool_use/result inside an agent can't perturb the parent
/// count. The empty↔non-empty edge is all the state machine needs: parallel
/// launches in one message flip active once, and only the last result flips it
/// back.
///
/// A genuine new user prompt ([`is_user_prompt`]) is treated as a turn boundary
/// that clears the whole set: an Esc-interrupt can abort an agent without ever
/// writing its `tool_result`, so without this a stale id would keep the avatar
/// wedged in Thinking until the process exits. (The state manager also has a
/// time-based backstop for the walk-away case.)
fn update_agents(obj: &Value, agents: &mut HashSet<String>, ctx: &OobContext) {
    if obj.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return;
    }

    let was_active = !agents.is_empty();
    if is_user_prompt(obj) {
        agents.clear();
    } else if let Some(parts) = message_parts(obj) {
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("tool_use")
                    if part.get("name").and_then(Value::as_str) == Some(TASK_TOOL_NAME) =>
                {
                    if let Some(id) = part.get("id").and_then(Value::as_str) {
                        agents.insert(id.to_string());
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = part.get("tool_use_id").and_then(Value::as_str) {
                        agents.remove(id);
                    }
                }
                _ => {}
            }
        }
    } else {
        return;
    }

    let now_active = !agents.is_empty();
    if now_active != was_active {
        debug!(tab = ?ctx.tab, count = agents.len(), active = now_active, "Claude OOB: agents active edge");
        ctx.signal(StateSignal::AgentsActiveChanged {
            tab: ctx.tab.clone(),
            active: now_active,
        });
    }
}

/// V10: record file/query memory events from an assistant line's `tool_use`
/// blocks. Maps Claude's tool names → a memory `kind` + target
/// ([`crate::graph::classify_tool`]); tools not in that map (Task, TodoWrite,
/// our own `mcp__cimp-offload__*`) are ignored. Sidechain (sub-agent) lines are
/// skipped so an agent's internal reads don't pollute the parent session. A
/// no-op when memory isn't wired.
fn record_tool_events(obj: &Value, project_dir: &Path, session_id: &str, ctx: &OobContext) {
    if ctx.mem.is_none() {
        return;
    }
    if obj.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return;
    }
    if obj.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(parts) = message_parts(obj) else { return };
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(name) = part.get("name").and_then(Value::as_str) else { continue };
        let Some((kind, arg)) = crate::graph::classify_tool(name) else { continue };
        let input = part.get("input");
        let get = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
        let (path, detail) = match arg {
            // Read/Edit key the target as `file_path`; NotebookRead/NotebookEdit
            // key it as `notebook_path`.
            crate::graph::MemArg::Path => (
                get("file_path").or_else(|| get("notebook_path")).unwrap_or("").to_string(),
                None,
            ),
            crate::graph::MemArg::Pattern => (
                get("pattern").or_else(|| get("path")).unwrap_or("").to_string(),
                None,
            ),
            crate::graph::MemArg::Command => (
                String::new(),
                get("command").map(|c| c.chars().take(200).collect::<String>()),
            ),
        };
        // A read/edit/grep with no target carries nothing worth recording.
        if matches!(arg, crate::graph::MemArg::Path | crate::graph::MemArg::Pattern)
            && path.is_empty()
        {
            continue;
        }
        ctx.record_mem(project_dir, session_id, "claude", kind, &path, None, None, detail.as_deref());
    }
}

// ── V14 Phase C: token/cost usage tap ─────────────────────────────────────

/// Small ring of `tool_use_id -> tool name`, populated from every `tool_use`
/// block (ALL tools, unlike [`record_tool_events`]'s `classify_tool` filter —
/// usage accounting wants every tool named, not just the memory-worthy ones)
/// and consulted when the matching `tool_result` arrives so its estimated
/// chars can be attributed to a tool ("Read of `foo.rs` cost 18k twice" needs
/// the name). Bounded so a very long session can't grow it unboundedly —
/// oldest entries are evicted first, same ring posture as `mem_event`'s cap.
#[derive(Default)]
struct ToolNameRing {
    names: HashMap<String, String>,
    order: VecDeque<String>,
}

/// Ring cap — generous relative to how many tool calls a single session
/// realistically has outstanding at once (this only needs to bridge a
/// `tool_use` to its own `tool_result`, which normally arrives within the
/// same or next line).
const TOOL_NAME_RING_CAP: usize = 512;

impl ToolNameRing {
    fn insert(&mut self, id: String, name: String) {
        if !self.names.contains_key(&id) {
            self.order.push_back(id.clone());
            while self.order.len() > TOOL_NAME_RING_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.names.remove(&old);
                }
            }
        }
        self.names.insert(id, name);
    }

    fn get(&self, id: &str) -> Option<&str> {
        self.names.get(id).map(String::as_str)
    }

    /// Drop everything — called on session rotation (a new transcript file
    /// means old `tool_use` ids can never see a matching `tool_result`).
    fn clear(&mut self) {
        self.names.clear();
        self.order.clear();
    }
}

/// Pure: extract a [`crate::graph::UsageEvent::Turn`] from an assistant
/// transcript line's `message.usage` block. Tolerant of absent fields
/// (older transcript lines, or a partial line mid-stream before the block
/// firms up) — missing token counts default to 0, which is exactly right for
/// the UPSERT-by-`msg_id` semantics in `record_usage_event`: a later line
/// carrying the SAME `msg_id` with the real numbers overwrites this one in
/// place rather than leaving a duplicate zero row. `None` for any
/// non-assistant line or an assistant line with no `message.id`.
fn parse_usage_line(obj: &Value) -> Option<crate::graph::UsageEvent> {
    if obj.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = obj.get("message")?;
    let msg_id = message.get("id").and_then(Value::as_str)?.to_string();
    let model = message.get("model").and_then(Value::as_str).map(str::to_string);
    let usage = message.get("usage");
    let tok = |k: &str| -> u32 { usage.and_then(|u| u.get(k)).and_then(Value::as_u64).unwrap_or(0) as u32 };
    Some(crate::graph::UsageEvent::Turn {
        msg_id,
        model,
        in_tok: tok("input_tokens"),
        out_tok: tok("output_tokens"),
        cache_read: tok("cache_read_input_tokens"),
        cache_make: tok("cache_creation_input_tokens"),
    })
}

/// Pure: `(tool_use_id, chars)` for every `tool_result` content block in a
/// user-role transcript line (the carrier for one or more parallel tool
/// results). `chars` is an estimated-token proxy for the result's size — no
/// exact token count exists for tool output, only for assistant messages.
fn extract_tool_results(obj: &Value) -> Vec<(String, usize)> {
    if obj.get("type").and_then(Value::as_str) != Some("user") {
        return Vec::new();
    }
    let Some(parts) = message_parts(obj) else { return Vec::new() };
    let mut out = Vec::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some(id) = part.get("tool_use_id").and_then(Value::as_str) else { continue };
        let chars = tool_result_chars(part.get("content").unwrap_or(&Value::Null));
        out.push((id.to_string(), chars));
    }
    out
}

/// Character length of a `tool_result` block's `content`, which is either a
/// plain string or an array of blocks (`{"type":"text","text":...}` plus
/// possibly non-text blocks, e.g. images — only text blocks count towards
/// the char estimate).
fn tool_result_chars(content: &Value) -> usize {
    match content {
        Value::String(s) => s.chars().count(),
        Value::Array(items) => items
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .map(|t| t.chars().count())
            .sum(),
        _ => 0,
    }
}

/// V14 Phase C: feed the token/cost X-ray from one transcript line — a `Turn`
/// event from an assistant message's `usage` block, and a `ToolResult` event
/// per `tool_result` block in a tool-carrier user line (joined to its tool
/// name via `tool_names`). A no-op when memory isn't wired (mirrors
/// `record_tool_events`). Unlike `record_tool_events`, sidechain (sub-agent)
/// lines are NOT skipped: a sub-agent's tokens are real spend against the
/// same session and must be counted, even though its file touches aren't
/// tracked as the parent's working set.
fn record_usage(obj: &Value, tool_names: &mut ToolNameRing, project_dir: &Path, session_id: &str, ctx: &OobContext) {
    if ctx.mem.is_none() {
        return;
    }

    // Learn tool_use_id -> name for every tool_use block, regardless of
    // whether `classify_tool` recognizes it.
    if let Some(parts) = message_parts(obj) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let (Some(id), Some(name)) =
                    (part.get("id").and_then(Value::as_str), part.get("name").and_then(Value::as_str))
                {
                    tool_names.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    if let Some(event) = parse_usage_line(obj) {
        ctx.record_usage(project_dir, session_id, "claude", event);
    }

    for (tool_use_id, chars) in extract_tool_results(obj) {
        let tool = tool_names.get(&tool_use_id).map(str::to_string);
        ctx.record_usage(
            project_dir,
            session_id,
            "claude",
            crate::graph::UsageEvent::ToolResult { tool, chars: chars as u32 },
        );
    }
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

    // --- Task sub-agent tracking (update_agents) ---

    use crate::settings::{Settings, SettingsHandle};
    use crate::state::TabId;
    use crate::tts::TtsRequest;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn agent_ctx() -> (OobContext, mpsc::Receiver<StateSignal>) {
        let (tts_tx, _tts_rx) = mpsc::channel::<TtsRequest>(64);
        let (sig_tx, sig_rx) = mpsc::channel::<StateSignal>(64);
        let defaults = Settings::default();
        let settings = SettingsHandle::new(defaults.clone(), defaults, std::env::temp_dir());
        let ctx = OobContext {
            tab: TabId::Claude,
            tts: tts_tx,
            state_signals: sig_tx,
            settings,
            cancel: CancellationToken::new(),
            mem: None,
        };
        (ctx, sig_rx)
    }

    fn obj(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    /// Assistant message launching one Task agent.
    fn launch(id: &str) -> Value {
        obj(&format!(
            r#"{{"type":"assistant","message":{{"id":"a1","content":[
                {{"type":"tool_use","id":"{id}","name":"Task","input":{{}}}}
            ]}}}}"#
        ))
    }

    /// User message carrying the tool_result for `id`.
    fn result(id: &str) -> Value {
        obj(&format!(
            r#"{{"type":"user","message":{{"content":[
                {{"type":"tool_result","tool_use_id":"{id}","content":"done"}}
            ]}}}}"#
        ))
    }

    #[test]
    fn launch_then_result_emits_active_then_inactive() {
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();

        update_agents(&launch("toolu_a"), &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::AgentsActiveChanged { active: true, .. })
        ));

        update_agents(&result("toolu_a"), &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::AgentsActiveChanged { active: false, .. })
        ));
        assert!(sig.try_recv().is_err(), "no further edges");
        assert!(agents.is_empty());
    }

    #[test]
    fn parallel_launch_flips_active_once_and_inactive_on_last() {
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();

        // Two agents launched in one assistant message → single active edge.
        let both = obj(
            r#"{"type":"assistant","message":{"id":"a1","content":[
                {"type":"tool_use","id":"toolu_1","name":"Task","input":{}},
                {"type":"tool_use","id":"toolu_2","name":"Task","input":{}}
            ]}}"#,
        );
        update_agents(&both, &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::AgentsActiveChanged { active: true, .. })
        ));
        assert!(sig.try_recv().is_err(), "only one active edge for a batch");

        // First result: still one outstanding → no edge.
        update_agents(&result("toolu_1"), &mut agents, &ctx);
        assert!(sig.try_recv().is_err(), "still one agent running");

        // Last result: crosses to zero → inactive edge.
        update_agents(&result("toolu_2"), &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::AgentsActiveChanged { active: false, .. })
        ));
    }

    #[test]
    fn non_task_tool_use_is_ignored() {
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        let bash = obj(
            r#"{"type":"assistant","message":{"id":"a1","content":[
                {"type":"tool_use","id":"toolu_b","name":"Bash","input":{}}
            ]}}"#,
        );
        update_agents(&bash, &mut agents, &ctx);
        assert!(sig.try_recv().is_err(), "non-Task tool must not mark agents active");
        assert!(agents.is_empty());
    }

    #[test]
    fn sidechain_lines_do_not_perturb_the_count() {
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        // A sub-agent's own internal Task-shaped line, marked isSidechain.
        let side = obj(
            r#"{"type":"assistant","isSidechain":true,"message":{"id":"s1","content":[
                {"type":"tool_use","id":"toolu_nested","name":"Task","input":{}}
            ]}}"#,
        );
        update_agents(&side, &mut agents, &ctx);
        assert!(sig.try_recv().is_err(), "sidechain must be ignored");
        assert!(agents.is_empty());
    }

    #[test]
    fn stray_result_for_untracked_id_is_noop() {
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        // Result for a tool we never tracked (e.g. a Read) must not emit.
        update_agents(&result("toolu_never_seen"), &mut agents, &ctx);
        assert!(sig.try_recv().is_err());
    }

    #[test]
    fn user_prompt_clears_orphaned_agents() {
        // Esc-interrupt: a Task launched but its tool_result never arrives.
        // The next genuine user prompt is a turn boundary that reclaims it,
        // emitting the inactive edge so the avatar can settle.
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        update_agents(&launch("toolu_orphan"), &mut agents, &ctx);
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::AgentsActiveChanged { active: true, .. })
        ));

        // Plain-string user prompt.
        let prompt = obj(r#"{"type":"user","message":{"role":"user","content":"try again please"}}"#);
        update_agents(&prompt, &mut agents, &ctx);
        assert!(agents.is_empty(), "turn boundary must clear orphaned agents");
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::AgentsActiveChanged { active: false, .. })
        ));
    }

    #[test]
    fn user_prompt_with_text_block_is_a_boundary() {
        // Some prompts arrive as a content array with a text block rather than
        // a bare string — still a turn boundary.
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        update_agents(&launch("toolu_x"), &mut agents, &ctx);
        let _ = sig.try_recv();
        let prompt = obj(
            r#"{"type":"user","message":{"content":[{"type":"text","text":"next"}]}}"#,
        );
        update_agents(&prompt, &mut agents, &ctx);
        assert!(agents.is_empty());
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::AgentsActiveChanged { active: false, .. })
        ));
    }

    #[test]
    fn tool_result_carrier_is_not_a_boundary() {
        // A user message that carries only tool_results (the normal agent-
        // result path) must NOT be treated as a turn boundary — it should
        // remove just its own id, leaving other agents outstanding.
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        let both = obj(
            r#"{"type":"assistant","message":{"id":"a1","content":[
                {"type":"tool_use","id":"toolu_1","name":"Task","input":{}},
                {"type":"tool_use","id":"toolu_2","name":"Task","input":{}}
            ]}}"#,
        );
        update_agents(&both, &mut agents, &ctx);
        assert!(matches!(sig.try_recv(), Ok(StateSignal::AgentsActiveChanged { active: true, .. })));

        // tool_result for one — is_user_prompt is false (only tool_result
        // parts), so it removes toolu_1 and leaves toolu_2 running: no edge.
        update_agents(&result("toolu_1"), &mut agents, &ctx);
        assert!(sig.try_recv().is_err(), "one agent still outstanding");
        assert_eq!(agents.len(), 1);
        assert!(agents.contains("toolu_2"));
    }

    #[test]
    fn user_prompt_with_no_agents_is_silent() {
        // A turn boundary with nothing outstanding must not emit a phantom edge.
        let (ctx, mut sig) = agent_ctx();
        let mut agents = HashSet::new();
        let prompt = obj(r#"{"type":"user","message":{"content":"hello"}}"#);
        update_agents(&prompt, &mut agents, &ctx);
        assert!(sig.try_recv().is_err());
    }

    // ── V14 Phase C: usage tap (parse_usage_line / extract_tool_results) ──

    #[test]
    fn parse_usage_line_extracts_full_usage_block() {
        let line = obj(
            r#"{"type":"assistant","message":{"id":"m1","model":"claude-x","usage":{
                "input_tokens":100,"output_tokens":20,
                "cache_read_input_tokens":50,"cache_creation_input_tokens":5}}}"#,
        );
        let ev = parse_usage_line(&line).expect("assistant line with usage yields an event");
        match ev {
            crate::graph::UsageEvent::Turn { msg_id, model, in_tok, out_tok, cache_read, cache_make } => {
                assert_eq!(msg_id, "m1");
                assert_eq!(model.as_deref(), Some("claude-x"));
                assert_eq!(in_tok, 100);
                assert_eq!(out_tok, 20);
                assert_eq!(cache_read, 50);
                assert_eq!(cache_make, 5);
            }
            other => panic!("expected Turn, got {other:?}"),
        }
    }

    #[test]
    fn parse_usage_line_tolerates_absent_usage() {
        // Older transcript lines (or a partial line mid-stream) may carry no
        // `usage` block at all: still a Turn event (so the msg_id UPSERT can
        // later overwrite it with real numbers), just with zeroed tokens.
        let line = obj(r#"{"type":"assistant","message":{"id":"m2"}}"#);
        let ev = parse_usage_line(&line).expect("absent usage still yields an event");
        match ev {
            crate::graph::UsageEvent::Turn { msg_id, model, in_tok, out_tok, cache_read, cache_make } => {
                assert_eq!(msg_id, "m2");
                assert_eq!(model, None);
                assert_eq!((in_tok, out_tok, cache_read, cache_make), (0, 0, 0, 0));
            }
            other => panic!("expected Turn, got {other:?}"),
        }
    }

    #[test]
    fn parse_usage_line_partial_usage_defaults_missing_fields() {
        // A usage block with only some fields present (a plausible partial
        // stream update) — present fields are read, absent ones default to 0.
        let line = obj(
            r#"{"type":"assistant","message":{"id":"m3","usage":{"input_tokens":7}}}"#,
        );
        let ev = parse_usage_line(&line).unwrap();
        match ev {
            crate::graph::UsageEvent::Turn { in_tok, out_tok, cache_read, cache_make, .. } => {
                assert_eq!(in_tok, 7);
                assert_eq!((out_tok, cache_read, cache_make), (0, 0, 0));
            }
            other => panic!("expected Turn, got {other:?}"),
        }
    }

    #[test]
    fn parse_usage_line_none_for_non_assistant() {
        let line = obj(r#"{"type":"user","message":{"content":"hi"}}"#);
        assert!(parse_usage_line(&line).is_none());
    }

    #[test]
    fn parse_usage_line_none_without_message_id() {
        let line = obj(r#"{"type":"assistant","message":{"usage":{"input_tokens":1}}}"#);
        assert!(parse_usage_line(&line).is_none());
    }

    #[test]
    fn extract_tool_results_reads_string_content() {
        let line = obj(
            r#"{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_1","content":"hello world"}
            ]}}"#,
        );
        let got = extract_tool_results(&line);
        assert_eq!(got, vec![("toolu_1".to_string(), "hello world".chars().count())]);
    }

    #[test]
    fn extract_tool_results_sums_text_blocks_and_skips_non_text() {
        let line = obj(
            r#"{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_2","content":[
                    {"type":"text","text":"abc"},
                    {"type":"image","source":{}},
                    {"type":"text","text":"de"}
                ]}
            ]}}"#,
        );
        let got = extract_tool_results(&line);
        assert_eq!(got, vec![("toolu_2".to_string(), 5)], "only the two text blocks (3+2 chars) count");
    }

    #[test]
    fn extract_tool_results_handles_multiple_parallel_results() {
        let line = obj(
            r#"{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_a","content":"aa"},
                {"type":"tool_result","tool_use_id":"toolu_b","content":"bbbb"}
            ]}}"#,
        );
        let got = extract_tool_results(&line);
        assert_eq!(got, vec![("toolu_a".to_string(), 2), ("toolu_b".to_string(), 4)]);
    }

    #[test]
    fn extract_tool_results_ignores_non_tool_result_and_non_user_lines() {
        // A real user prompt (text block, not a tool_result carrier).
        let prompt = obj(r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#);
        assert!(extract_tool_results(&prompt).is_empty());
        // An assistant line is never a tool_result carrier.
        let assistant = obj(r#"{"type":"assistant","message":{"id":"m1","content":[]}}"#);
        assert!(extract_tool_results(&assistant).is_empty());
    }

    #[test]
    fn tool_name_ring_joins_and_evicts_beyond_cap() {
        let mut ring = ToolNameRing::default();
        ring.insert("toolu_1".to_string(), "Read".to_string());
        assert_eq!(ring.get("toolu_1"), Some("Read"));
        assert_eq!(ring.get("toolu_missing"), None);

        // Insert one more than the cap; the oldest (`toolu_1`) is evicted,
        // the newest survives.
        for i in 0..TOOL_NAME_RING_CAP {
            ring.insert(format!("toolu_gen_{i}"), "Bash".to_string());
        }
        assert_eq!(ring.get("toolu_1"), None, "oldest entry evicted beyond the cap");
        assert_eq!(ring.get(&format!("toolu_gen_{}", TOOL_NAME_RING_CAP - 1)), Some("Bash"));
    }

    #[test]
    fn tool_name_ring_clear_drops_everything() {
        let mut ring = ToolNameRing::default();
        ring.insert("toolu_1".to_string(), "Read".to_string());
        ring.clear();
        assert_eq!(ring.get("toolu_1"), None);
    }

    #[test]
    fn record_usage_is_a_noop_without_graph_memory() {
        // agent_ctx()'s ctx.mem is None; record_usage must not panic, and —
        // mirroring record_tool_events's early return — must not even touch
        // the ring, since without memory there's nothing to join it into.
        let (ctx, _sig) = agent_ctx();
        let mut ring = ToolNameRing::default();
        let dir = std::env::temp_dir();
        let line = obj(
            r#"{"type":"assistant","message":{"id":"m1","content":[
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{}}
            ],"usage":{"input_tokens":10,"output_tokens":2}}}"#,
        );
        record_usage(&line, &mut ring, &dir, "s1", &ctx);
        assert_eq!(ring.get("toolu_1"), None, "mem is None, so the tap is a full no-op");
    }
}

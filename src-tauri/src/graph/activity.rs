//! V9-01 — a small process-wide ring of recent graph tool calls, for the Code
//! Graph monitor tab's "Recent calls" list.
//!
//! Both consumer paths funnel through [`super::mcp::dispatch_recorded`], which
//! appends here. When the app is running, the cloud-Claude path executes
//! in-process (via the loopback warm path) and the local offload worker also
//! runs in-process, so both land in this one ring — a unified history. (In the
//! app-not-running fallback the same code runs in the `--offload-mcp` child and
//! records into *that* process's ring, which is simply never read — there's no
//! monitor tab without the app.)

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many recent calls to retain. Oldest are dropped past this.
const CAP: usize = 200;

/// One recorded graph tool call, serialized to the monitor tab.
#[derive(Clone, Debug, serde::Serialize)]
pub struct GraphCall {
    /// Unix epoch millis when the call started.
    pub ts_ms: u64,
    /// The project root the call ran against, in [`root_key`] form. The ring
    /// is process-wide across every indexed root; this is what lets a
    /// per-project consumer (the Graph View pulse feed) keep another
    /// project's activity from lighting up same-named nodes in its graph.
    pub root: String,
    /// Who issued it: `"claude"` (cloud session) or `"offload"` (local worker).
    pub source: String,
    /// The tool name, e.g. `graph_find_symbol`.
    pub tool: String,
    /// The primary argument (symbol / file / query) for at-a-glance context.
    pub target: String,
    /// Response size in characters (the graph tools return text, not tokens).
    pub chars: usize,
    /// Wall-clock duration in milliseconds.
    pub ms: u64,
    /// Whether the call succeeded.
    pub ok: bool,
}

static RING: Mutex<VecDeque<GraphCall>> = Mutex::new(VecDeque::new());

/// Append a call to the ring, dropping the oldest once `CAP` is reached.
pub fn record(call: GraphCall) {
    if let Ok(mut ring) = RING.lock() {
        if ring.len() >= CAP {
            ring.pop_front();
        }
        ring.push_back(call);
    }
}

/// A newest-first snapshot of the recent calls (for the `graph_history` IPC).
pub fn snapshot() -> Vec<GraphCall> {
    RING.lock()
        .map(|ring| ring.iter().rev().cloned().collect())
        .unwrap_or_default()
}

/// Memo for [`root_key`]: `canonicalize` is a real filesystem syscall and the
/// key is requested on every recorded tool call and advisor/auto-check event,
/// always for the same small set of roots (plus a few per-tab cwds) — cache
/// the successful answers. Failures (e.g. a vanished directory) are NOT
/// cached, so a transient error can't stick.
static ROOT_KEYS: LazyLock<Mutex<HashMap<PathBuf, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The canonical string form a root is recorded (and filtered) under.
/// Recorders and the scoped `graph_history` filter must both go through this:
/// the same directory reaches them in different spellings (the launch cwd vs.
/// a `find_graph_root` ancestor walk, drive-letter vs. verbatim `\\?\` form on
/// Windows), and canonicalizing both sides makes those compare equal. Falls
/// back to the path as given (e.g. the directory vanished mid-call).
pub fn root_key(root: &Path) -> String {
    if let Ok(cache) = ROOT_KEYS.lock() {
        if let Some(key) = cache.get(root) {
            return key.clone();
        }
    }
    match std::fs::canonicalize(root) {
        Ok(canon) => {
            let key = canon.to_string_lossy().to_string();
            if let Ok(mut cache) = ROOT_KEYS.lock() {
                // Bounded memo: the key set is tiny in practice; a wholesale
                // clear on the (unexpected) way past the cap keeps it O(1)
                // without needing an eviction policy.
                if cache.len() >= 256 {
                    cache.clear();
                }
                cache.insert(root.to_path_buf(), key.clone());
            }
            key
        }
        Err(_) => root.to_string_lossy().to_string(),
    }
}

/// Current Unix epoch in milliseconds (0 if the clock is before the epoch).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(target: &str) -> GraphCall {
        GraphCall {
            ts_ms: now_ms(),
            root: root_key(Path::new(".")),
            source: "offload".into(),
            tool: "graph_outline".into(),
            target: target.into(),
            chars: 0,
            ms: 0,
            ok: true,
        }
    }

    #[test]
    fn root_key_is_stable_across_spellings() {
        // The same directory reached via different relative spellings must
        // produce the same key — that equality is what the scoped
        // `graph_history` filter relies on.
        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(root_key(&cwd), root_key(Path::new(".")));
        // A path that doesn't exist falls back to its literal form.
        assert_eq!(
            root_key(Path::new("definitely/not/a/real/dir")),
            Path::new("definitely/not/a/real/dir").to_string_lossy()
        );
    }

    #[test]
    fn records_newest_first_within_cap() {
        // The ring is process-global; assert only on the entries we just pushed,
        // which must be the most recent, newest-first, with the ring bounded.
        for t in ["a0", "a1", "a2"] {
            record(call(t));
        }
        let snap = snapshot();
        assert!(snap.len() <= CAP);
        assert_eq!(snap[0].target, "a2");
        assert_eq!(snap[1].target, "a1");
        assert_eq!(snap[2].target, "a0");
    }
}

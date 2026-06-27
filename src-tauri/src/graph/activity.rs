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

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// How many recent calls to retain. Oldest are dropped past this.
const CAP: usize = 200;

/// One recorded graph tool call, serialized to the monitor tab.
#[derive(Clone, Debug, serde::Serialize)]
pub struct GraphCall {
    /// Unix epoch millis when the call started.
    pub ts_ms: u64,
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
            source: "offload".into(),
            tool: "graph_outline".into(),
            target: target.into(),
            chars: 0,
            ms: 0,
            ok: true,
        }
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

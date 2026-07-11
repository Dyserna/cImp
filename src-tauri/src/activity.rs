//! Unified, **persistent** tool-activity store — the backing feed of the Tool
//! Activity tab.
//!
//! Grew out of the V9-01 in-memory graph-call ring (`graph/activity.rs`): one
//! process-wide, newest-first history of tool calls, now covering *both*
//! consumers — `graph_*`/`context_*` calls (recorded by
//! `graph::mcp::dispatch_recorded` and friends) and completed `offload_task`
//! runs (recorded by `offload::service`) — with the actual request/response
//! payloads captured (truncated) so the UI can show them in a detail popup.
//!
//! Entries survive an app restart: the ring is mirrored to a JSONL file next
//! to the executable (`<exe-dir>/tool-activity.jsonl`, the same portable
//! location as `settings.json`). Each `record` appends one line; `delete`/
//! `clear` rewrite the file; the load path compacts an over-long file back to
//! the retention caps. Payloads are size-capped at record time so a single
//! line stays modest and the file stays bounded by the caps.
//!
//! Retention is **per kind**, not global: graph/context calls are chatty
//! (every `graph_*` call, plus read-advisor reminders and auto-check
//! injections), while `offload_task` runs are rare and comparatively
//! valuable. A single shared cap would let a graph-heavy session silently
//! evict every offload row — the exact crowd-out the pre-V0.40.1 split
//! rings (graph 200 / offload 50 per backend) existed to prevent — so each
//! kind keeps its own newest-N window instead.
//!
//! Process notes: when the app is running, the cloud-Claude path executes
//! in-process (loopback warm path) and the local offload worker also runs
//! in-process, so both land in this one store. In the app-not-running
//! fallback (and on a transient loopback failure while the app IS up) the
//! same code runs in the `cimp --offload-mcp` child, which appends to the
//! same file — those entries become visible in the app on the next launch.
//! Because another process can append lines this process's ring has never
//! seen, every rewrite that is *not* an explicit wipe (delete-one,
//! compaction, load-repair) first re-merges unknown on-disk entries into the
//! ring so it can't clobber them; `clear` intentionally skips the merge —
//! wiping the history is the point. A child appending in the instant between
//! that merge-read and the atomic rename can still lose that one entry;
//! that residual race is accepted for a best-effort activity log.
//!
//! Blocking note: `record`/`delete`/`clear` perform synchronous file I/O
//! under the store lock. Callers on async runtime threads must not call them
//! directly — use [`record_bg`] (recorders) or wrap in `spawn_blocking`
//! (IPC command handlers) so a slow disk / AV scan never stalls a tokio
//! worker.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-kind retention: how many entries of each kind to keep (in memory and
/// after compaction). `"offload"` gets its own window; every other kind
/// (currently just `"graph"`) uses the graph cap.
const GRAPH_CAP: usize = 400;
const OFFLOAD_CAP: usize = 100;
/// Payload caps (chars) applied at record time — a request is typically a
/// small JSON args object or an offload instruction; responses can be large
/// tool output. Anything past the cap is cut with an explicit marker.
const REQUEST_CAP_CHARS: usize = 16_000;
const RESPONSE_CAP_CHARS: usize = 24_000;
/// On-disk mirror, next to the executable (same location as `settings.json`).
const FILE_NAME: &str = "tool-activity.jsonl";
/// Compact (rewrite) once the appended file holds this many lines — bounds
/// file growth between loads without rewriting on every record.
const FILE_COMPACT_LINES: usize = 1000;

/// The feed kind an activity belongs to. Kept as a closed enum at every
/// recording site (see [`ActivityEntry::new`]) so a new recorder can't typo a
/// kind string that would compile fine and silently vanish from the
/// kind-filtered feeds; the serialized form stays a plain string for
/// JSONL/IPC compatibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    /// A `graph_*` / `context_*` / `run_check` tool call (including the
    /// backend-internal read-advisor and auto-check recorders).
    Graph,
    /// One completed `offload_task` run.
    Offload,
}

impl ActivityKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ActivityKind::Graph => "graph",
            ActivityKind::Offload => "offload",
        }
    }
}

/// The retention cap for a (serialized) kind. Unknown strings (a future kind
/// loaded from a newer file) share the graph window rather than erroring.
fn kind_cap(kind: &str) -> usize {
    if kind == ActivityKind::Offload.as_str() {
        OFFLOAD_CAP
    } else {
        GRAPH_CAP
    }
}

/// One recorded tool activity, WITHOUT payloads — the shape list consumers
/// (the Tool Activity feed poll, the Graph View pulse feed) receive every
/// couple of seconds, so it must stay light.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ActivityEntry {
    /// Stable id (unique across restarts) — what delete-by-id keys on.
    /// Derived from the record timestamp plus a randomly-seeded per-process
    /// sequence (see [`next_id`]); the load path additionally repairs any
    /// duplicate that slips through from another process.
    #[serde(default)]
    pub id: u64,
    /// Unix epoch millis when the call started.
    pub ts_ms: u64,
    /// Feed kind, the serialized form of [`ActivityKind`].
    #[serde(default = "default_kind")]
    pub kind: String,
    /// The project root the call ran against, in [`root_key`] form (empty for
    /// offload runs with no session cwd). Lets a per-project consumer (the
    /// Graph View pulse feed) filter out other projects' activity.
    pub root: String,
    /// Who issued it: `"claude"` / `"opencode"` / `"offload"` /
    /// `"read_advisor"` / `"auto_check"` for graph entries; the backend name
    /// for offload entries.
    pub source: String,
    /// The tool name, e.g. `graph_find_symbol` or `offload_task`.
    pub tool: String,
    /// The primary argument (symbol / file / query / instruction headline).
    pub target: String,
    /// Response size in characters.
    pub chars: usize,
    /// Wall-clock duration in milliseconds.
    pub ms: u64,
    /// Whether the call succeeded.
    pub ok: bool,
}

impl ActivityEntry {
    /// The one way recorders build an entry: `kind` is the closed enum (no
    /// free-form strings at call sites) and `id` is always store-assigned.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: ActivityKind,
        ts_ms: u64,
        root: String,
        source: String,
        tool: String,
        target: String,
        chars: usize,
        ms: u64,
        ok: bool,
    ) -> Self {
        Self {
            id: 0,
            ts_ms,
            kind: kind.as_str().to_string(),
            root,
            source,
            tool,
            target,
            chars,
            ms,
            ok,
        }
    }
}

fn default_kind() -> String {
    ActivityKind::Graph.as_str().to_string()
}

/// The full stored record: the light entry plus the captured (truncated)
/// request/response payloads. This is what's persisted per JSONL line and
/// what the on-demand `activity_detail` IPC returns.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ActivityRecord {
    #[serde(flatten)]
    pub entry: ActivityEntry,
    #[serde(default)]
    pub request: String,
    #[serde(default)]
    pub response: String,
}

struct StoreInner {
    loaded: bool,
    /// Kept sorted by `ts_ms` ascending (ties keep insertion order), so
    /// "oldest of a kind" is simply the front-most match.
    ring: VecDeque<ActivityRecord>,
    /// Per-process sequence feeding id generation. Seeded randomly so two
    /// processes appending to the same file (app + `--offload-mcp` child)
    /// don't both start their id residues at 0.
    seq: u64,
    /// Lines currently in the on-disk file (kept + appended); drives the
    /// periodic compaction.
    file_lines: usize,
}

/// The store: an in-memory ring mirrored to a JSONL file. Constructed with an
/// explicit path so tests get their own isolated file; production goes
/// through the [`store`] global.
pub struct ActivityStore {
    path: PathBuf,
    inner: Mutex<StoreInner>,
}

impl ActivityStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            inner: Mutex::new(StoreInner {
                loaded: false,
                ring: VecDeque::new(),
                seq: uuid::Uuid::new_v4().as_u128() as u64 % 1000,
                file_lines: 0,
            }),
        }
    }

    /// Append `rec` (payloads truncated, id assigned when 0) and mirror it to
    /// disk. The oldest entries OF ITS KIND are dropped past that kind's cap.
    ///
    /// Does synchronous file I/O — see the module's blocking note.
    pub fn record(&self, mut rec: ActivityRecord) {
        rec.request = truncate_chars(&rec.request, REQUEST_CAP_CHARS);
        rec.response = truncate_chars(&rec.response, RESPONSE_CAP_CHARS);
        let Ok(mut inner) = self.inner.lock() else { return };
        self.load_locked(&mut inner);
        if rec.entry.id == 0 {
            rec.entry.id = next_id(&mut inner);
        }
        // Serialize before the move into the ring — the append path then
        // needs no clone of the (payload-heavy) record.
        let line = match serde_json::to_string(&rec) {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::warn!(error = %e, "activity: serialize record failed");
                None
            }
        };
        insert_sorted(&mut inner.ring, rec);
        enforce_kind_caps(&mut inner.ring);
        if inner.file_lines >= FILE_COMPACT_LINES {
            // Compaction rewrites the whole file; fold in any lines another
            // process appended first so the rewrite can't clobber them.
            self.merge_from_disk_locked(&mut inner);
            self.rewrite_locked(&mut inner);
        } else if let Some(line) = line {
            if self.append_line(&line) {
                inner.file_lines += 1;
            }
        }
    }

    /// A newest-first, payload-free snapshot of the entries with
    /// `ts_ms > since` (pass 0 for everything).
    pub fn snapshot_since(&self, since: u64) -> Vec<ActivityEntry> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        self.load_locked(&mut inner);
        inner
            .ring
            .iter()
            .rev()
            .filter(|r| r.entry.ts_ms > since)
            .map(|r| r.entry.clone())
            .collect()
    }

    /// The full record (with payloads) for one entry, if it still exists.
    pub fn detail(&self, id: u64) -> Option<ActivityRecord> {
        let mut inner = self.inner.lock().ok()?;
        self.load_locked(&mut inner);
        inner.ring.iter().find(|r| r.entry.id == id).cloned()
    }

    /// Remove one entry by id (rewrites the file). Returns whether it existed.
    /// Re-merges foreign on-disk lines first so a concurrent child's appends
    /// survive the rewrite.
    pub fn delete(&self, id: u64) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        self.load_locked(&mut inner);
        self.merge_from_disk_locked(&mut inner);
        let before = inner.ring.len();
        inner.ring.retain(|r| r.entry.id != id);
        let removed = inner.ring.len() != before;
        if removed {
            self.rewrite_locked(&mut inner);
        }
        removed
    }

    /// Drop every entry and truncate the file. Deliberately does NOT merge
    /// from disk first: the user asked for the history to be wiped, and that
    /// includes lines appended by another process.
    pub fn clear(&self) {
        let Ok(mut inner) = self.inner.lock() else { return };
        self.load_locked(&mut inner);
        inner.ring.clear();
        self.rewrite_locked(&mut inner);
    }

    /// One-time lazy load of the on-disk mirror. Unparseable lines are
    /// skipped; pre-id (legacy in-memory era) lines get ids assigned;
    /// duplicate ids from a cross-process collision are repaired; an
    /// over-cap or repaired file is compacted/rewritten.
    fn load_locked(&self, inner: &mut StoreInner) {
        if inner.loaded {
            return;
        }
        inner.loaded = true;
        let Ok(text) = fs::read_to_string(&self.path) else {
            // Absent file: fresh install or first run after the upgrade.
            return;
        };
        let mut total_lines = 0usize;
        let mut repaired = false;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            total_lines += 1;
            match serde_json::from_str::<ActivityRecord>(line) {
                Ok(rec) => inner.ring.push_back(rec),
                Err(e) => {
                    repaired = true;
                    tracing::warn!(error = %e, "activity: skipping unparseable history line");
                }
            }
        }
        // File append order is usually time order, but two writers make no
        // guarantee — restore the sorted invariant explicitly.
        inner
            .ring
            .make_contiguous()
            .sort_by_key(|r| r.entry.ts_ms);
        // Assign missing ids and repair duplicates (two processes can — very
        // rarely — mint the same id; `delete`'s retain would then drop both).
        let mut seen = HashSet::new();
        for i in 0..inner.ring.len() {
            let id = inner.ring[i].entry.id;
            if id == 0 || !seen.insert(id) {
                let fresh = next_id(inner);
                inner.ring[i].entry.id = fresh;
                seen.insert(fresh);
                repaired = true;
            }
        }
        if enforce_kind_caps(&mut inner.ring) {
            repaired = true;
        }
        inner.file_lines = total_lines;
        if repaired {
            self.rewrite_locked(inner);
        }
    }

    /// Fold on-disk lines this process has never seen (appended by an
    /// `--offload-mcp` child) into the ring, so an upcoming rewrite doesn't
    /// discard them. No-op when the file matches the ring.
    fn merge_from_disk_locked(&self, inner: &mut StoreInner) {
        let Ok(text) = fs::read_to_string(&self.path) else {
            return;
        };
        let known: HashSet<u64> = inner.ring.iter().map(|r| r.entry.id).collect();
        let mut merged = false;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<ActivityRecord>(line) else {
                continue;
            };
            // id 0 would collide as a set key; such lines only exist in
            // legacy files, which load_locked already repaired in the
            // process that owns them — skip rather than re-import.
            if rec.entry.id != 0 && !known.contains(&rec.entry.id) {
                insert_sorted(&mut inner.ring, rec);
                merged = true;
            }
        }
        if merged {
            enforce_kind_caps(&mut inner.ring);
        }
    }

    /// Best-effort single-line append of a pre-serialized record. Returns
    /// whether the write succeeded.
    fn append_line(&self, line: &str) -> bool {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let res = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = res {
            tracing::warn!(error = %e, path = %self.path.display(), "activity: append failed");
            return false;
        }
        true
    }

    /// Rewrite the whole file from the ring (atomic replace).
    fn rewrite_locked(&self, inner: &mut StoreInner) {
        let mut out = String::new();
        for rec in inner.ring.iter() {
            match serde_json::to_string(rec) {
                Ok(line) => {
                    out.push_str(&line);
                    out.push('\n');
                }
                Err(e) => tracing::warn!(error = %e, "activity: serialize record failed"),
            }
        }
        if let Err(e) = crate::settings::write_atomic(&self.path, out.as_bytes()) {
            tracing::warn!(error = %e, path = %self.path.display(), "activity: rewrite failed");
        }
        inner.file_lines = inner.ring.len();
    }
}

/// Insert keeping the ring sorted by `ts_ms` ascending (ties append after —
/// stable with respect to arrival order).
fn insert_sorted(ring: &mut VecDeque<ActivityRecord>, rec: ActivityRecord) {
    let mut idx = ring.len();
    while idx > 0 && ring[idx - 1].entry.ts_ms > rec.entry.ts_ms {
        idx -= 1;
    }
    ring.insert(idx, rec);
}

/// Drop the oldest entries of any kind that exceeds its cap (the ring is
/// ts-sorted, so front-most = oldest). Returns whether anything was removed.
fn enforce_kind_caps(ring: &mut VecDeque<ActivityRecord>) -> bool {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for r in ring.iter() {
        *counts.entry(r.entry.kind.clone()).or_default() += 1;
    }
    let mut removed = false;
    let mut i = 0;
    while i < ring.len() {
        let kind = ring[i].entry.kind.clone();
        let count = counts.get_mut(&kind).expect("counted above");
        if *count > kind_cap(&kind) {
            *count -= 1;
            ring.remove(i);
            removed = true;
        } else {
            i += 1;
        }
    }
    removed
}

/// Timestamp-based id: unique within the process via `seq` (randomly seeded
/// per process, so two writers to the same file don't share residues), and
/// double-checked against the local ring. A cross-process same-millisecond
/// collision is still theoretically possible; the load path repairs any that
/// make it into one file. Stays below 2^53 so it round-trips through the JS
/// frontend as a number.
fn next_id(inner: &mut StoreInner) -> u64 {
    loop {
        let id = now_ms() * 1000 + inner.seq % 1000;
        inner.seq += 1;
        if id != 0 && !inner.ring.iter().any(|r| r.entry.id == id) {
            return id;
        }
    }
}

/// Cut `s` to at most `cap` chars, with an explicit truncation marker.
fn truncate_chars(s: &str, cap: usize) -> String {
    // Fast path: byte length bounds char count from above, so a short-by-bytes
    // string can never exceed the char cap — skip the full decode.
    if s.len() <= cap {
        return s.to_string();
    }
    let total = s.chars().count();
    if total <= cap {
        return s.to_string();
    }
    let kept: String = s.chars().take(cap).collect();
    format!("{kept}\n… [truncated {} chars]", total - cap)
}

/// `<exe-dir>/tool-activity.jsonl` — the portable location, same rationale as
/// `settings::global_path`. Falls back to the working directory when
/// `current_exe` is unavailable.
fn default_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(FILE_NAME)
}

static STORE: LazyLock<ActivityStore> = LazyLock::new(|| {
    // Pin the process-start timestamp no later than the first store access:
    // "since this run" consumers (effectiveness_totals) compare entry
    // timestamps against it to exclude restored pre-restart entries.
    let _ = process_start_ms();
    ActivityStore::new(default_path())
});

fn store() -> &'static ActivityStore {
    &STORE
}

/// Append an activity record to the process-wide store (synchronous file
/// I/O — do not call directly from async contexts; see [`record_bg`]).
pub fn record(rec: ActivityRecord) {
    store().record(rec);
}

/// [`record`] moved off the async runtime: on a tokio thread the write runs
/// via `spawn_blocking` (fire-and-forget — an entry lost to a process exit
/// mid-flight is acceptable for an activity log); outside a runtime it
/// records inline.
pub fn record_bg(rec: ActivityRecord) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(move || record(rec));
        }
        Err(_) => record(rec),
    }
}

/// A newest-first, payload-free snapshot (all kinds).
pub fn snapshot() -> Vec<ActivityEntry> {
    store().snapshot_since(0)
}

/// A newest-first, payload-free snapshot of entries with `ts_ms > since` —
/// lets the 1.5–2s pollers skip re-serializing hundreds of rows they already
/// have.
pub fn snapshot_since(since: u64) -> Vec<ActivityEntry> {
    store().snapshot_since(since)
}

/// The full record (with request/response payloads) for one entry.
pub fn detail(id: u64) -> Option<ActivityRecord> {
    store().detail(id)
}

/// Remove one entry by id.
pub fn delete(id: u64) -> bool {
    store().delete(id)
}

/// Drop the whole history.
pub fn clear() {
    store().clear()
}

static PROCESS_START_MS: LazyLock<u64> = LazyLock::new(now_ms);

/// Epoch millis captured at (or before) the first store access this process —
/// the "since restart" cutoff for consumers that must not count entries
/// restored from disk.
pub fn process_start_ms() -> u64 {
    *PROCESS_START_MS
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

    fn rec(target: &str) -> ActivityRecord {
        rec_kind(ActivityKind::Graph, target)
    }

    fn rec_kind(kind: ActivityKind, target: &str) -> ActivityRecord {
        ActivityRecord {
            entry: ActivityEntry::new(
                kind,
                now_ms(),
                root_key(Path::new(".")),
                "offload".into(),
                "graph_outline".into(),
                target.into(),
                0,
                0,
                true,
            ),
            request: format!("{{\"file\": \"{target}\"}}"),
            response: format!("outline of {target}"),
        }
    }

    fn temp_store(name: &str) -> ActivityStore {
        let path = std::env::temp_dir()
            .join("cimp-activity-tests")
            .join(format!("{name}-{}.jsonl", uuid::Uuid::new_v4()));
        ActivityStore::new(path)
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
        let store = temp_store("order");
        for t in ["a0", "a1", "a2"] {
            store.record(rec(t));
        }
        let snap = store.snapshot_since(0);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].target, "a2");
        assert_eq!(snap[1].target, "a1");
        assert_eq!(snap[2].target, "a0");
        // Every entry got a distinct non-zero id.
        assert!(snap.iter().all(|e| e.id != 0));
        assert_ne!(snap[0].id, snap[1].id);
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn persists_across_reload() {
        let store = temp_store("persist");
        store.record(rec("kept"));
        let id = store.snapshot_since(0)[0].id;

        // A fresh store on the same path (a "restart") sees the entry, with
        // payloads intact.
        let reloaded = ActivityStore::new(store.path.clone());
        let snap = reloaded.snapshot_since(0);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].target, "kept");
        assert_eq!(snap[0].id, id);
        let full = reloaded.detail(id).expect("detail");
        assert!(full.request.contains("kept"));
        assert!(full.response.contains("outline of kept"));
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn delete_and_clear_persist() {
        let store = temp_store("delete");
        store.record(rec("a"));
        store.record(rec("b"));
        let snap = store.snapshot_since(0);
        let (b_id, a_id) = (snap[0].id, snap[1].id);

        assert!(store.delete(a_id));
        assert!(!store.delete(a_id), "second delete is a no-op");
        let after = ActivityStore::new(store.path.clone()).snapshot_since(0);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, b_id);

        store.clear();
        assert!(store.snapshot_since(0).is_empty());
        assert!(ActivityStore::new(store.path.clone()).snapshot_since(0).is_empty());
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn load_compacts_past_kind_cap() {
        let store = temp_store("cap");
        for i in 0..(GRAPH_CAP + 25) {
            store.record(rec(&format!("t{i}")));
        }
        assert_eq!(store.snapshot_since(0).len(), GRAPH_CAP);
        // Reload keeps only the newest GRAPH_CAP, newest first.
        let reloaded = ActivityStore::new(store.path.clone());
        let snap = reloaded.snapshot_since(0);
        assert_eq!(snap.len(), GRAPH_CAP);
        assert_eq!(snap[0].target, format!("t{}", GRAPH_CAP + 24));
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn offload_entries_survive_a_graph_flood() {
        // The caps are per kind: a graph-heavy session must never evict the
        // (rare, valuable) offload rows — the crowd-out the old split rings
        // existed to prevent.
        let store = temp_store("flood");
        store.record(rec_kind(ActivityKind::Offload, "the offload run"));
        for i in 0..(GRAPH_CAP + 50) {
            store.record(rec(&format!("g{i}")));
        }
        let snap = store.snapshot_since(0);
        assert_eq!(
            snap.iter().filter(|e| e.kind == "offload").count(),
            1,
            "offload entry was evicted by graph traffic"
        );
        assert_eq!(snap.iter().filter(|e| e.kind == "graph").count(), GRAPH_CAP);
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn delete_preserves_foreign_appends() {
        // Simulate an `--offload-mcp` child appending a line this process's
        // ring has never seen: a delete-rewrite must fold it in, not clobber
        // it.
        let store = temp_store("foreign");
        store.record(rec("mine"));
        let mine = store.snapshot_since(0)[0].id;
        let mut foreign = rec("from-the-child");
        foreign.entry.id = mine + 7; // a distinct, non-zero foreign id
        let line = serde_json::to_string(&foreign).unwrap();
        {
            let mut f = fs::OpenOptions::new().append(true).open(&store.path).unwrap();
            writeln!(f, "{line}").unwrap();
        }

        assert!(store.delete(mine));
        let after = ActivityStore::new(store.path.clone()).snapshot_since(0);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].target, "from-the-child");
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn load_repairs_duplicate_ids() {
        // Two processes can (rarely) mint the same id; on load the duplicate
        // must be re-keyed so delete-by-id can't remove both.
        let store = temp_store("dupe");
        let mut a = rec("first");
        let mut b = rec("second");
        a.entry.id = 42_000_000_000_000_000;
        b.entry.id = a.entry.id;
        let path = store.path.clone();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = serde_json::to_string(&a).unwrap();
        text.push('\n');
        text.push_str(&serde_json::to_string(&b).unwrap());
        text.push('\n');
        fs::write(&path, text).unwrap();

        let snap = store.snapshot_since(0);
        assert_eq!(snap.len(), 2);
        assert_ne!(snap[0].id, snap[1].id);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn snapshot_since_filters_older_entries() {
        let store = temp_store("since");
        let mut old = rec("old");
        old.entry.ts_ms = 1000;
        let mut new = rec("new");
        new.entry.ts_ms = 2000;
        store.record(old);
        store.record(new);
        let fresh = store.snapshot_since(1000);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].target, "new");
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn payloads_are_truncated_with_marker() {
        let store = temp_store("truncate");
        let mut r = rec("big");
        r.response = "x".repeat(RESPONSE_CAP_CHARS + 500);
        store.record(r);
        let id = store.snapshot_since(0)[0].id;
        let full = store.detail(id).expect("detail");
        assert!(full.response.contains("… [truncated 500 chars]"));
        assert!(full.response.chars().count() < RESPONSE_CAP_CHARS + 100);
        let _ = fs::remove_file(&store.path);
    }
}

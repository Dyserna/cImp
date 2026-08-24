//! The **read advisor** — V11 Phase E, V16 Feature 4, V17 Phases A–C.
//!
//! Split out of [`super`] by V42 R6 (#117) as pure code motion. The advisor was
//! already a free-function island with value-type seams: the verdict is a pure
//! function of a [`VerdictIn`], the first-read tier a pure function of a
//! [`FirstReadIn`], and the snapshot LRU a self-contained [`ReadSeenStore`].
//! What sits here is that island plus the three [`GraphService`] methods that
//! drive it — [`should_read`](GraphService::should_read), `record_remind` and
//! [`check_bypass`](GraphService::check_bypass) — and the bypass tap's
//! command-matching helpers.
//!
//! The advisor's STATE stays on `GraphService` (`reminded`, `read_seen`,
//! `read_seen_touch`, `bypassed_advice_chars`): a child module already sees its
//! parent's private fields, so moving the code needed no new visibility into
//! the service's state. What DID widen is six items the parent still names,
//! all to `pub(super)`: [`RemindMark`] and [`ReadSeenStore`] (the types of two
//! `GraphService` fields), [`ReadSeenStore::clear_session`] (called by
//! `mem_clear`), and [`ReadSeen`] / [`SNAP_TOTAL_MAX`] / [`READ_SEEN_MAX_ENTRIES`]
//! (named by those fields' doc comments).

use super::*;

/// V16 Feature 4: when (and how big) a read-advisor reminder was, so the
/// transcript tap's bypass matcher can test "shell read within the window"
/// and un-count the displaced chars. One mark per `(session, file)` — the
/// remind-once semantics are unchanged.
pub(super) struct RemindMark {
    /// The session's retrieve-turn counter when the reminder fired (0 when
    /// context injection is off and the clock never ticks).
    turn: u32,
    /// Wall clock of the reminder — the bypass window's fallback when the
    /// turn clock isn't ticking.
    ts_ms: u64,
    /// Chars of the file content the reminder displaced (the file size, not
    /// the reminder text — what a bypass re-spends).
    chars: u64,
    /// Chars of the reminder TEXT that was returned (the Activity `remind`
    /// event's own size). Kept alongside `chars` because the two are
    /// different units: the panel's displaced figure sums reminder text, so
    /// netting a bypass out of it must subtract reminder text too — not the
    /// whole-file `chars` (one big-file bypass would wipe out the entire
    /// metric).
    advice_chars: u64,
    /// Set once a bypass was recorded against this reminder, so repeated
    /// `cat`s of the same file count one bypass, not N.
    bypassed: bool,
    /// V17 Phase A: how many reminders have fired for this `(session, file)`.
    /// A CHANGED re-read re-arms an already-reminded file (the old remind
    /// promised "unchanged"; the change makes that stale), but only while
    /// `count < READ_REMIND_CAP` — so the advisor can never fight an insistent
    /// agent in a loop. Bumped on every diff remind; an unchanged reminded file
    /// never re-reminds regardless of count. First remind sets it to 1.
    count: u32,
}

/// V17 Phase A — the read advisor's per-`(session, file)` observation: the
/// content hash + turn it was last seen at (the staleness/TTL comparison keys),
/// plus an optional in-memory SNAPSHOT of that content so a later changed
/// re-read can be answered with a diff against exactly what the agent read.
/// The snapshot is dropped (set to `None`) on LRU eviction — but the
/// `(hash, turn)` observation is NEVER forgotten by eviction (only the content
/// is), so the advisor's staleness logic is unaffected by memory pressure.
pub(super) struct ReadSeen {
    /// Content hash the advisor last observed the agent read.
    hash: String,
    /// The session's retrieve-turn when that read was observed (TTL clock).
    turn: u32,
    /// The content itself, kept only for files ≥ `read_advisor_min_lines` and
    /// ≤ [`SNAP_ENTRY_MAX`] bytes, and only until evicted by the [`SNAP_TOTAL_MAX`]
    /// byte-budget LRU. `None` = small file, over-cap, evicted, or a branch that
    /// deliberately keeps no snapshot (Phase C's first-read tier).
    snapshot: Option<Arc<str>>,
    /// Monotonic touch order for the LRU: both snapshot eviction and the
    /// entry backstop drop the smallest-`touch` first.
    touch: u64,
}

/// V17 Phase A: per-entry snapshot cap — content larger than this is observed
/// (hash/turn recorded) but never snapshotted, so one huge file can't dominate
/// the diff budget.
const SNAP_ENTRY_MAX: usize = 512 * 1024;
/// V17 Phase A: whole-store snapshot byte budget. On overflow the oldest-touched
/// snapshots are dropped (content only — the observation survives).
pub(super) const SNAP_TOTAL_MAX: usize = 16 * 1024 * 1024;
/// V17 Phase A: backstop bound on the number of `read_seen` OBSERVATIONS (rows,
/// snapshot or not) — the byte-budget LRU alone bounds only snapshotted content,
/// so a long session that touches thousands of small files still needs an entry
/// cap. Subsumes the old 1024-entry blanket clear; evicts oldest-touched whole
/// rows instead of wiping the map (clearing is safe — a dropped row just allows
/// one fresh read).
pub(super) const READ_SEEN_MAX_ENTRIES: usize = 4096;
/// V17 Phase A: max reminders per `(session, file)` before a changed re-read
/// just passes (see [`RemindMark::count`]). A const, not a setting — promote if
/// field data demands.
const READ_REMIND_CAP: u32 = 3;

/// V17 Phase A: capture the read-advisor snapshot for `content`, or `None` when
/// it isn't worth keeping (fewer than `min_lines` lines, or over
/// [`SNAP_ENTRY_MAX`] bytes). Pure.
fn capture_snapshot(content: &str, min_lines: u32) -> Option<Arc<str>> {
    if (content.lines().count() as u32) >= min_lines && content.len() <= SNAP_ENTRY_MAX {
        Some(Arc::from(content))
    } else {
        None
    }
}

/// V17 Phase A: total bytes of all live snapshots in the store. O(n) over the
/// map — the GROUND TRUTH the incremental [`ReadSeenStore::snap_bytes`] running
/// total must always equal (asserted in tests). Test-only since V22 — the hot
/// path now trusts the incremental running total instead of re-summing.
#[cfg(test)]
fn snapshot_bytes(seen: &HashMap<(String, String), ReadSeen>) -> usize {
    seen.values()
        .filter_map(|v| v.snapshot.as_ref().map(|s| s.len()))
        .sum()
}

/// V22 efficiency: the read-advisor's snapshot store — the
/// `(session, file) → ReadSeen` map plus a running sum of live snapshot bytes.
///
/// `snap_bytes` is maintained INCREMENTALLY at every mutation (insert, replace,
/// eviction, session/whole clear), so the per-Read [`Self::insert`] path enforces
/// the [`SNAP_TOTAL_MAX`] byte budget without the old unconditional O(n)
/// `snapshot_bytes` re-sum. Its value always equals `snapshot_bytes(&self.map)`.
///
/// The O(n) `min_by_key` victim scans remain — but only inside the eviction
/// loops, which run solely when a cap is actually exceeded (rare, and each drop
/// is bounded by one over-budget insert's contribution), so they stay off the
/// common path. Left as-is: a heap/priority index over `touch` would be a
/// disproportionate rebuild for a scan that no longer runs per insert.
#[derive(Default)]
pub(super) struct ReadSeenStore {
    map: HashMap<(String, String), ReadSeen>,
    /// Running sum of `snapshot.len()` across all live rows. Invariant:
    /// `snap_bytes == snapshot_bytes(&map)` after every method returns.
    snap_bytes: usize,
}

/// Bytes a `ReadSeen`'s snapshot contributes to the running total (0 if none).
fn snap_len(v: &ReadSeen) -> usize {
    v.snapshot.as_ref().map(|s| s.len()).unwrap_or(0)
}

impl ReadSeenStore {
    /// V17 Phase A / V22: insert/replace `key`'s observation and enforce both
    /// bounds — the [`READ_SEEN_MAX_ENTRIES`] entry backstop (evicts whole oldest
    /// rows) and the [`SNAP_TOTAL_MAX`] snapshot byte budget (drops oldest-touched
    /// snapshots but keeps their `hash`/`turn`). Keeps `snap_bytes` consistent at
    /// each step. `touch` is a fresh monotonic value from the service's counter.
    fn insert(
        &mut self,
        key: (String, String),
        hash: String,
        turn: u32,
        snapshot: Option<Arc<str>>,
        touch: u64,
    ) {
        let added = snapshot.as_ref().map(|s| s.len()).unwrap_or(0);
        // A replace drops the old row's snapshot bytes before adding the new.
        if let Some(old) = self.map.insert(
            key,
            ReadSeen {
                hash,
                turn,
                snapshot,
                touch,
            },
        ) {
            self.snap_bytes -= snap_len(&old);
        }
        self.snap_bytes += added;
        // Entry backstop: drop whole oldest-touched rows past the cap.
        while self.map.len() > READ_SEEN_MAX_ENTRIES {
            let victim = self
                .map
                .iter()
                .min_by_key(|(_, v)| v.touch)
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    if let Some(v) = self.map.remove(&k) {
                        self.snap_bytes -= snap_len(&v);
                    }
                }
                None => break,
            }
        }
        // Snapshot byte budget: drop oldest-touched SNAPSHOTS (keep hash/turn).
        while self.snap_bytes > SNAP_TOTAL_MAX {
            let victim = self
                .map
                .iter()
                .filter(|(_, v)| v.snapshot.is_some())
                .min_by_key(|(_, v)| v.touch)
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    if let Some(v) = self.map.get_mut(&k) {
                        self.snap_bytes -= snap_len(v);
                        v.snapshot = None;
                    }
                }
                None => break,
            }
        }
    }

    /// V22: drop rows for one session (`Some`) or all (`None`), keeping
    /// `snap_bytes` consistent — the read-advisor half of [`GraphService::mem_clear`].
    pub(super) fn clear_session(&mut self, session_id: Option<&str>) {
        match session_id {
            Some(s) => {
                let dropped = &mut self.snap_bytes;
                self.map.retain(|(sid, _), v| {
                    let keep = sid != s;
                    if !keep {
                        *dropped -= snap_len(v);
                    }
                    keep
                });
            }
            None => {
                self.map.clear();
                self.snap_bytes = 0;
            }
        }
    }
}

/// V17 Phase A — the read advisor's verdict, as a pure decision over the facts
/// [`GraphService::should_read`] has already gathered (no locks, no I/O), so the
/// TTL / re-arm-cap / diff-threshold rules are unit-testable without a live
/// service (which needs an `AppHandle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadAdvice {
    /// Let the read through. `restamp` = whether `read_seen` should be
    /// (re)written to the new observation: true on never-seen, on a changed
    /// pass, and on a TTL re-stamp; false on the unchanged pass-throughs
    /// (already-reminded / small-file) that leave the prior observation intact.
    Pass { restamp: bool },
    /// Emit the outline reminder (unchanged, not yet reminded, ≥ min_lines).
    Outline,
    /// Emit the diff reminder (changed, snapshot present, diff worth it, under cap).
    Diff,
}

/// The facts a read verdict depends on. All cheap to compute in `should_read`.
struct VerdictIn {
    /// `read_seen` had an entry for this `(session, file)`.
    seen: bool,
    /// `seen` and the stored hash equals the current hash.
    unchanged: bool,
    /// `unchanged`, the TTL is enabled, and it has expired.
    ttl_expired: bool,
    /// The file was already reminded this session.
    reminded: bool,
    /// The reminder count so far (0 when not reminded).
    remind_count: u32,
    /// Current content is ≥ `read_advisor_min_lines` lines.
    big_enough: bool,
    /// `read_advisor_diffs` is on.
    diffs_on: bool,
    /// A snapshot of the prior content survives (not evicted / not too small).
    have_snapshot: bool,
    /// The rendered diff is ≤ 50% of the new content's length.
    diff_worth_it: bool,
}

/// V17 Phase A — pure verdict. See [`ReadAdvice`]. The never-seen arm returns
/// `Pass { restamp: true }`; Phase C's first-read branch slots in *before* this
/// is consulted (it evaluates the never-seen case itself).
fn read_verdict(i: &VerdictIn) -> ReadAdvice {
    if !i.seen {
        // Never seen this session ⇒ record-and-pass.
        return ReadAdvice::Pass { restamp: true };
    }
    if i.unchanged {
        // Trust TTL expired ⇒ pass and re-stamp the observation's turn.
        if i.ttl_expired {
            return ReadAdvice::Pass { restamp: true };
        }
        // Immediate-second-ask hatch: same file, same content, already reminded
        // (or too small to bother) ⇒ pass, leaving the prior observation.
        if i.reminded || !i.big_enough {
            return ReadAdvice::Pass { restamp: false };
        }
        return ReadAdvice::Outline;
    }
    // Changed. A reminded file re-arms only while under the cap.
    if i.reminded && i.remind_count >= READ_REMIND_CAP {
        return ReadAdvice::Pass { restamp: true };
    }
    if i.diffs_on && i.have_snapshot && i.diff_worth_it {
        return ReadAdvice::Diff;
    }
    // Changed but no diff to offer (feature off / snapshot gone / near-rewrite)
    // ⇒ record-and-pass with the new observation, exactly as pre-V17.
    ReadAdvice::Pass { restamp: true }
}

/// V17 Phase C — the facts the first-read tier gates on, all cheap to compute in
/// `should_read`. Split out (like [`VerdictIn`]) so the eligibility decision is
/// unit-testable without an `AppHandle`.
struct FirstReadIn {
    /// `read_advisor_first_read_kb` (0 = tier off).
    first_read_kb: u32,
    /// Bytes of the already-read content.
    content_len: usize,
    /// A deliberate slice (`Read({offset})` / `{limit}`) is in play.
    slice: bool,
    /// The file parses to code (its outline is non-empty).
    is_code: bool,
}

/// V17 Phase C — does the first-read substitution tier APPLY to this never-seen
/// read? True only when the tier is enabled, the whole-file content is at or over
/// the KiB threshold, the read isn't a deliberate slice, and the file isn't code
/// (data/logs/lockfiles qualify, source never does). A `true` result still needs
/// a cached digest to actually remind — the caller does that impure lookup and
/// enqueues on a miss. Pure.
fn first_read_eligible(i: &FirstReadIn) -> bool {
    i.first_read_kb > 0
        && i.content_len >= (i.first_read_kb as usize).saturating_mul(1024)
        && !i.slice
        && !i.is_code
}

/// V17 Phase B5: whether a Bash command is a provable whole-file read the read
/// advisor already accounts for, so the bypass tap must skip it. Only meaningful
/// when the shell sub-toggle is on (otherwise the Bash hook isn't installed and
/// such a command really is an un-intercepted read the canary should score).
fn intercepted_whole_file_read(shell_on: bool, command: &str) -> bool {
    shell_on && crate::graph::shellread::whole_file_read(command).is_some()
}

/// V16 Feature 4: extract path-like candidate tokens from a shell command —
/// quoted segments (single or double) plus whitespace-split tokens that
/// contain a path separator. Deliberately NOT a shell parser (the milestone
/// spec's "simple heuristic"): the consumer only ever compares candidates
/// against a small set of just-reminded files, so false candidates cost
/// nothing and false negatives only under-count (events are labeled est.).
fn path_like_tokens(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |t: &str| {
        let t = t
            .trim()
            .trim_matches(|c| c == ',' || c == ';' || c == ')' || c == '(');
        if t.len() > 1 && out.iter().all(|p| p != t) {
            out.push(t.to_string());
        }
    };
    // Quoted segments, both kinds — a path with spaces only survives here.
    for quote in ['"', '\''] {
        let mut parts = command.split(quote);
        parts.next(); // before the first quote
        while let (Some(inside), rest) = (parts.next(), parts.next()) {
            push(inside);
            if rest.is_none() {
                break;
            }
        }
    }
    for tok in command.split_whitespace() {
        if tok.contains('/') || tok.contains('\\') {
            push(tok.trim_matches(|c| c == '"' || c == '\''));
        }
    }
    out
}

/// V16 Feature 4: whether a command token plausibly refers to the reminded
/// file at (project-relative, `/`-separated) `rel`. Full-path match, a
/// longer path ENDING in the relative path (an absolute spelling of it), or
/// a bare basename match — normalized to `/` so `src\a.rs` and `src/a.rs`
/// compare equal.
fn token_matches_path(token: &str, rel: &str) -> bool {
    let norm = token.replace('\\', "/");
    let norm = norm.trim_end_matches('/');
    if norm.is_empty() || rel.is_empty() {
        return false;
    }
    if norm == rel {
        return true;
    }
    if norm.len() > rel.len() && norm.ends_with(rel) {
        // Require a boundary before the suffix so `notsrc/a.rs` doesn't
        // match `src/a.rs`.
        let boundary = norm.as_bytes()[norm.len() - rel.len() - 1];
        if boundary == b'/' {
            return true;
        }
    }
    let base = rel.rsplit('/').next().unwrap_or(rel);
    let tok_base = norm.rsplit('/').next().unwrap_or(norm);
    !base.is_empty() && tok_base == base
}

impl GraphService {
    /// V11 Phase E / V17 Phase A — the read advisor's verdict for a `Read` of
    /// `file_path`: `Some(reminder_text)` to deny-with-content, or `None` to let
    /// the read proceed. Passes when: the advisor is off; there's no session; the
    /// session is recovering from a compaction; the file was never seen this
    /// session (record-and-pass); it was seen UNCHANGED and either already
    /// reminded, under the min-lines floor, or past its trust TTL; or it CHANGED
    /// but no diff is available (feature off / snapshot evicted / near-rewrite /
    /// re-arm cap reached). It REMINDS when: seen unchanged, not yet reminded,
    /// ≥ min-lines (outline, plus the body in substitute mode); or seen CHANGED
    /// with a surviving snapshot and a small-enough diff (a unified diff against
    /// exactly what the agent last read). A changed file re-arms an already-fired
    /// reminder up to [`READ_REMIND_CAP`] times.
    ///
    /// V17 Phase C adds the first-read tier: when `read_advisor_first_read_kb > 0`
    /// a NEVER-seen whole-file read of a large **non-code** file (empty outline)
    /// with a cached digest is answered with a digest + head/tail sample instead
    /// of the full content (a digest miss enqueues one and passes).
    pub fn should_read(
        &self,
        root: &Path,
        session_id: Option<&str>,
        file_path: &str,
        offset: Option<u32>,
        limit: Option<u32>,
    ) -> Option<String> {
        let g = self.settings.current().graph;
        if !g.enabled || !g.read_advisor {
            return None;
        }
        // V17 Phase B threads `limit` through; V17 Phase C's first-read branch
        // (in the never-seen Pass arm below) consumes it — a deliberate slice
        // (`offset` OR `limit` present) always passes. Existing offset-only
        // behavior is unchanged.
        let sid = session_id.filter(|s| !s.is_empty())?;
        // Recovering from a compaction ⇒ pass everything (content was lost).
        if self.is_post_compaction(sid) {
            return None;
        }
        let rel = relativize_path(root, file_path);
        if rel.is_empty() {
            return None;
        }
        let key = (sid.to_string(), rel.clone());
        let idx = self.index_for(root).ok()?;

        // The session's turn counter — the V16 trust-TTL clock (and the
        // bypass window's turn stamp). Ticked by `retrieve_context` when
        // injection is on, by the transcript tap's [`Self::note_user_turn`]
        // otherwise.
        let cur_turn = self.session_turn(sid);

        // V17 Phase A: the `reminded` short-circuit moved BELOW the fs read +
        // hash compare — the re-arm rule needs the current hash to tell an
        // unchanged re-ask (always passes) from a CHANGED re-read of an
        // already-reminded file (may re-arm, capped). The fs read was already
        // unconditional on the remind path, so this only adds a read to the
        // already-reminded case.
        let abs = root.join(&rel);
        let content = std::fs::read_to_string(&abs).ok()?;
        let cur_hash = crate::graph::model::fnv1a_hex(&content);

        // What (if anything) we last observed the agent read for this file, plus
        // the snapshot of that content if one survived the LRU. Compare against
        // THIS, not the index hash: once the watcher re-indexes an edited file
        // the index hash equals the NEW content, yet the agent's context still
        // holds the pre-edit version — an index-hash match would wrongly suppress
        // the re-read it genuinely needs. Content hash rather than mtime, so a
        // filesystem clock skew (network shares, WSL2 bind-mounts) can't mislead.
        let prev = {
            let seen = self.read_seen.lock().ok()?;
            seen.map
                .get(&key)
                .map(|v| (v.hash.clone(), v.turn, v.snapshot.clone()))
        };
        let (seen, unchanged, prev_turn, prev_snapshot) = match &prev {
            Some((h, t, snap)) => (true, *h == cur_hash, *t, snap.clone()),
            None => (false, false, 0, None),
        };
        let ttl = g.read_advisor_ttl_turns;
        let ttl_expired = unchanged && ttl > 0 && cur_turn.saturating_sub(prev_turn) > ttl;

        // Already reminded this session? (Count drives the re-arm cap.)
        let (reminded_before, prev_count) = {
            let set = self.reminded.lock().ok()?;
            match set.get(sid).and_then(|m| m.get(&rel)) {
                Some(mark) => (true, mark.count),
                None => (false, 0),
            }
        };
        let big_enough = (content.lines().count() as u32) >= g.read_advisor_min_lines;
        let diffs_on = g.read_advisor_diffs;

        // Render the diff only when a changed re-read could actually use it
        // (feature on, snapshot survived, not already at the re-arm cap). The
        // "worth it" gate: a rendered diff over half the new content is a
        // near-rewrite — not worth a denial. `read_to_string` above already
        // guarantees UTF-8, so binary files never reach here (they fail the read).
        let diff_eligible = !unchanged
            && diffs_on
            && prev_snapshot.is_some()
            && !(reminded_before && prev_count >= READ_REMIND_CAP);
        let mut rendered_diff: Option<String> = None;
        let mut diff_worth_it = false;
        if diff_eligible {
            if let Some(old) = prev_snapshot.as_deref() {
                let d = crate::graph::context::unified_diff(old, &content, &rel);
                // ≤ 50% of the new content's length (chars).
                if d.chars().count().saturating_mul(2) <= content.chars().count() {
                    diff_worth_it = true;
                    rendered_diff = Some(d);
                }
            }
        }

        let vin = VerdictIn {
            seen,
            unchanged,
            ttl_expired,
            reminded: reminded_before,
            remind_count: prev_count,
            big_enough,
            diffs_on,
            have_snapshot: prev_snapshot.is_some(),
            diff_worth_it,
        };

        match read_verdict(&vin) {
            ReadAdvice::Pass { restamp } => {
                // ─── Phase C (C1): first-read tier for huge non-code files ──
                // The NEVER-SEEN case lands here (`seen == false`), evaluated
                // BEFORE the record-and-pass below. When the tier is on and the
                // file is large + non-code + a deliberate slice isn't in play,
                // and a digest is cached for the current hash, substitute a
                // `first_read_advice` reminder; on a digest MISS, enqueue one and
                // fall through to a plain pass (never-block — protection begins on
                // the next, cross-session encounter, since digests are
                // content-hash keyed and survive sessions).
                // Cheap gates first (setting, size, slice) so the `outline` DB
                // query only runs when the tier is on AND the file qualifies —
                // never on the common tiny-first-read path or when the tier is off.
                let slice = offset.is_some() || limit.is_some();
                let big = g.read_advisor_first_read_kb > 0
                    && content.len()
                        >= (g.read_advisor_first_read_kb as usize).saturating_mul(1024);
                if !seen && big && !slice {
                    let outline_empty = idx.outline(&rel).map(|o| o.is_empty()).unwrap_or(false);
                    let fin = FirstReadIn {
                        first_read_kb: g.read_advisor_first_read_kb,
                        content_len: content.len(),
                        slice,
                        is_code: !outline_empty,
                    };
                    if first_read_eligible(&fin) {
                        match idx.get_digest(&rel, &cur_hash) {
                            Ok(Some(digest)) => {
                                let text =
                                    crate::graph::context::first_read_advice(&rel, &content, &digest);
                                let displaced = content.chars().count() as u64;
                                let request = format!(
                                    "agent read of `{rel}` (huge non-code — digest substituted, first-read)"
                                );
                                let out = self.record_remind(
                                    root, &idx, sid, &rel, text, request, displaced, 1, cur_turn,
                                );
                                // C3: the file enters `reminded` (via record_remind)
                                // but read_seen keeps NO snapshot — generated-file
                                // diffs are useless and would blow the LRU. A later
                                // CHANGED re-read has no snapshot, so it just passes.
                                let touch = self.read_seen_touch.fetch_add(1, Ordering::Relaxed);
                                if let Ok(mut seen_map) = self.read_seen.lock() {
                                    seen_map.insert(key, cur_hash, cur_turn, None, touch);
                                }
                                return out;
                            }
                            _ => {
                                // Miss (or read error) on an otherwise-qualifying
                                // file ⇒ enqueue a digest and fall through to pass.
                                self.enqueue_digest(root, &rel, &cur_hash);
                            }
                        }
                    }
                }
                if restamp {
                    // Capture a fresh snapshot of the current content (never-seen,
                    // changed-pass, and TTL re-stamp all record the new observation).
                    let snap = capture_snapshot(&content, g.read_advisor_min_lines);
                    let touch = self.read_seen_touch.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut seen) = self.read_seen.lock() {
                        seen.insert(key, cur_hash, cur_turn, snap, touch);
                    }
                }
                None
            }
            ReadAdvice::Outline => {
                // Unchanged, first remind: outline (+ body in substitute mode).
                let substitute = g.read_advisor_mode.eq_ignore_ascii_case("substitute");
                let text = crate::graph::context::read_advice(
                    &idx,
                    root,
                    &rel,
                    offset,
                    substitute,
                    g.max_body_bytes as usize,
                );
                let displaced = content.chars().count() as u64;
                let request =
                    format!("agent re-read of `{rel}` (the trigger — no explicit request)");
                // read_seen stays as-is (content is unchanged).
                self.record_remind(
                    root,
                    &idx,
                    sid,
                    &rel,
                    text,
                    request,
                    displaced,
                    prev_count.saturating_add(1),
                    cur_turn,
                )
            }
            ReadAdvice::Diff => {
                // Changed: answer with a diff against the last-read snapshot.
                let diff_body = rendered_diff.unwrap_or_default();
                let text = crate::graph::context::diff_advice(&rel, prev_turn, &diff_body);
                let displaced = content.chars().count() as u64;
                let request = format!("agent re-read of `{rel}` (changed — diff substituted)");
                let out = self.record_remind(
                    root,
                    &idx,
                    sid,
                    &rel,
                    text,
                    request,
                    displaced,
                    prev_count.saturating_add(1),
                    cur_turn,
                );
                // After a diff remind the agent holds the CURRENT content: update
                // read_seen to (new hash, cur turn, new snapshot) so a further
                // change diffs against what it now knows. The bypass window keys
                // off RemindMark.turn/ts_ms, which `record_remind` just re-stamped.
                let snap = capture_snapshot(&content, g.read_advisor_min_lines);
                let touch = self.read_seen_touch.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut seen) = self.read_seen.lock() {
                    seen.insert(key, cur_hash, cur_turn, snap, touch);
                }
                out
            }
        }
    }

    /// V17 Phase A: shared read-advisor remind bookkeeping for the outline and
    /// diff branches — inserts the `RemindMark` (re-arm `count` included), adds
    /// the displaced content to the session's compounding base, persists the
    /// `mem_event{kind:"remind"}` row, and records the Activity event. Returns
    /// `Some(text)` (the reminder) so the caller can hand it straight back.
    #[allow(clippy::too_many_arguments)]
    fn record_remind(
        &self,
        root: &Path,
        idx: &GraphIndex,
        sid: &str,
        rel: &str,
        text: String,
        request: String,
        displaced: u64,
        new_count: u32,
        cur_turn: u32,
    ) -> Option<String> {
        let advice_chars = text.chars().count() as u64;
        // F13: cap the map — one entry per (session, file), never pruned on
        // session end (clearing is safe: a dropped key just allows one re-remind).
        {
            let mut set = self.reminded.lock().ok()?;
            let total: usize = set.values().map(HashMap::len).sum();
            if total > 4096 && !set.get(sid).is_some_and(|m| m.contains_key(rel)) {
                set.clear();
            }
            set.entry(sid.to_string()).or_default().insert(
                rel.to_string(),
                RemindMark {
                    turn: cur_turn,
                    ts_ms: crate::activity::now_ms(),
                    chars: displaced,
                    advice_chars,
                    bypassed: false,
                    count: new_count,
                },
            );
        }
        // V16 Feature 9: the displaced file content joins this session's
        // compounding base — every later retrieve turn re-counts it as a cache
        // read avoided. (Session-scoped, unlike the process-wide Activity sum the
        // panel also shows; the two coexist — Activity stays the audit trail.)
        if let Ok(mut map) = self.injected.lock() {
            let st = map.entry(sid.to_string()).or_default();
            st.displaced_chars_total = st.displaced_chars_total.saturating_add(displaced);
        }
        let ts = crate::activity::now_ms() as i64;
        // V14 Phase D2: also persist a root+session-scoped `mem_event{kind:
        // "remind"}` row — distinct from the process-wide Activity event below —
        // so `GraphIndex::advisor_reread_rate` can precisely check whether the
        // agent re-read this exact file afterward. Reaching a remind means the
        // agent has already read this file at least once this session
        // (`read_seen` held its prior hash), so the session row normally exists.
        // V40 Phase A (locked decisions 2/20): a MISSING agent is no longer
        // recorded as Claude's. A `mem_event` stamped with the wrong agent is
        // worse than one that is absent - `advisor_reread_rate` reads these rows
        // per agent, so a mis-stamped row moves a statistic about a harness that
        // did not produce it. The remind itself is unaffected; only the
        // provenance row is skipped, and the log line says so.
        match idx.session_agent(sid).ok().flatten() {
            Some(agent) => {
                let _ = idx.record_mem_event(sid, &agent, "remind", rel, None, None, ts, None);
            }
            None => tracing::debug!(
                session = %sid,
                "no agent recorded for this session; skipping the remind provenance row rather                  than attributing it to a harness that may not have produced it"
            ),
        }
        // Activity: `chars` is the reminder's actual size (what we returned),
        // consistent with every other graph tool's honest response-size figure —
        // not a fabricated token estimate.
        crate::activity::record_bg(crate::activity::ActivityRecord {
            request,
            response: text.clone(),
            entry: crate::activity::ActivityEntry::new(
                crate::activity::ActivityKind::Graph,
                ts as u64,
                crate::activity::root_key(root),
                "read_advisor".to_string(),
                "remind".to_string(),
                rel.to_string(),
                text.chars().count(),
                0,
                true,
                // cImp's own read advisor — positively no calling tab.
                crate::activity::Attribution::Headless,
                None,
                None,
                None,
            ),
        });
        Some(text)
    }

    /// V16 Feature 4: test a Bash command's path-like tokens against this
    /// session's recent read-advisor reminders; record a `bypass` Activity
    /// event (and un-count the displaced chars) for each hit. Called from
    /// the OOB transcript tap on every Claude Bash `tool_use` — detection is
    /// free there; no new hook (a `PostToolUse` shim spawn per shell command
    /// was considered and rejected, see the milestone doc).
    ///
    /// Matching is deliberately heuristic (labeled `est.` everywhere it's
    /// counted): a token matches a reminded file when it equals the file's
    /// relative path, is a path ending in it, or shares its basename. The
    /// window is ≤3 retrieve turns after the remind, with a 5-minute
    /// wall-clock fallback for sessions where injection is off and the turn
    /// clock never ticks. One bypass per reminder (`RemindMark::bypassed`).
    pub fn check_bypass(&self, root: &Path, session_id: &str, command: &str) {
        const BYPASS_TURNS: u32 = 3;
        const BYPASS_MS: u64 = 5 * 60 * 1000;
        if session_id.is_empty() {
            return;
        }
        let g = self.settings.current().graph;
        if !g.enabled || !g.read_advisor {
            return;
        }
        // V17 Phase B5: a provable whole-file shell read (`cat foo`) is handled
        // by the read advisor's Bash hook — it was either intercepted-and-denied
        // (the remind is already recorded by `should_read`) or verdict-passed
        // (not a bypass). Skip it BEFORE scoring: otherwise the denied `cat`
        // still shows up as a `tool_use` in the transcript and this tap would
        // double-count it as a bypass, poisoning `drift.read_bypass.v1`. With
        // the guard, the canary measures only RESIDUAL escape routes (`sed -n`,
        // `head`, redirections — the strict parser rejects those).
        if intercepted_whole_file_read(g.read_advisor_shell, command) {
            return;
        }
        let tokens = path_like_tokens(command);
        if tokens.is_empty() {
            return;
        }
        let cur_turn = self.session_turn(session_id);
        let now = crate::activity::now_ms();

        // Collect hits under the lock, record outside it. Session-keyed map:
        // only this session's own reminders are scanned.
        let mut hits: Vec<(String, u64, u64)> = Vec::new();
        if let Ok(mut set) = self.reminded.lock() {
            if let Some(marks) = set.get_mut(session_id) {
                for (rel, mark) in marks.iter_mut() {
                    if mark.bypassed {
                        continue;
                    }
                    // "Within 3 retrieve turns of the remind" when the turn
                    // clock is ticking; the 5-minute wall-clock window when it
                    // isn't (injection off ⇒ the counter never advances, and a
                    // 0-0 turn delta would otherwise match forever).
                    let in_window = if cur_turn > mark.turn {
                        cur_turn - mark.turn <= BYPASS_TURNS
                    } else {
                        now.saturating_sub(mark.ts_ms) <= BYPASS_MS
                    };
                    if !in_window {
                        continue;
                    }
                    if tokens.iter().any(|t| token_matches_path(t, rel)) {
                        mark.bypassed = true;
                        hits.push((rel.clone(), mark.chars, mark.advice_chars));
                    }
                }
            }
        }
        for (rel, chars, advice_chars) in hits {
            // Un-count from the session's compounding base — a bypassed
            // remind displaced nothing, so it stops compounding from this
            // turn forward (already-compounded turns stay counted; the
            // readout is measured, not retroactive).
            if let Ok(mut map) = self.injected.lock() {
                if let Some(st) = map.get_mut(session_id) {
                    st.displaced_chars_total = st.displaced_chars_total.saturating_sub(chars);
                }
            }
            // The panel's displaced figure sums reminder TEXT — net this
            // bypass out of it in the same unit (`effectiveness_totals`),
            // not in whole-file chars (which would let one big-file bypass
            // zero the entire metric).
            self.bypassed_advice_chars
                .fetch_add(advice_chars, Ordering::Relaxed);
            crate::activity::record_bg(crate::activity::ActivityRecord {
                request: format!("shell read of `{rel}` after a read-advisor reminder (est.)"),
                response: String::new(),
                entry: crate::activity::ActivityEntry::new(
                    crate::activity::ActivityKind::Graph,
                    now,
                    crate::activity::root_key(root),
                    "read_advisor".to_string(),
                    "bypass".to_string(),
                    rel,
                    chars as usize,
                    0,
                    false, // a bypass is a miss for the advisor — flag it
                    crate::activity::Attribution::Headless,
                    None,
                    None,
                    None,
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── V17 Phase A: read-advisor verdict + snapshot LRU ────────────────────
    //
    // `should_read` itself needs an `AppHandle` (unmockable in a unit test), so
    // its verdict/re-arm/TTL/diff-threshold logic is factored into the pure
    // `read_verdict` and the snapshot store into `ReadSeenStore`; both are
    // exercised directly here. The post-compaction pass is an early return at
    // the top of `should_read` (unchanged from V11) and isn't re-tested.

    /// A `VerdictIn` with everything "neutral" — override per case.
    fn vin() -> VerdictIn {
        VerdictIn {
            seen: true,
            unchanged: false,
            ttl_expired: false,
            reminded: false,
            remind_count: 0,
            big_enough: true,
            diffs_on: true,
            have_snapshot: true,
            diff_worth_it: true,
        }
    }

    #[test]
    fn verdict_never_seen_records_and_passes() {
        let i = VerdictIn {
            seen: false,
            ..vin()
        };
        assert_eq!(read_verdict(&i), ReadAdvice::Pass { restamp: true });
    }

    #[test]
    fn verdict_unchanged_first_ask_reminds_with_outline() {
        let i = VerdictIn {
            unchanged: true,
            reminded: false,
            big_enough: true,
            ..vin()
        };
        assert_eq!(read_verdict(&i), ReadAdvice::Outline);
    }

    #[test]
    fn verdict_unchanged_small_or_reminded_passes_without_restamp() {
        // Below the min-lines floor.
        let small = VerdictIn {
            unchanged: true,
            big_enough: false,
            ..vin()
        };
        assert_eq!(read_verdict(&small), ReadAdvice::Pass { restamp: false });
        // The immediate-second-ask hatch: same file, same content, already reminded.
        let reasked = VerdictIn {
            unchanged: true,
            reminded: true,
            remind_count: 1,
            ..vin()
        };
        assert_eq!(read_verdict(&reasked), ReadAdvice::Pass { restamp: false });
    }

    #[test]
    fn verdict_unchanged_ttl_expired_restamps_and_passes() {
        let i = VerdictIn {
            unchanged: true,
            ttl_expired: true,
            ..vin()
        };
        assert_eq!(read_verdict(&i), ReadAdvice::Pass { restamp: true });
    }

    #[test]
    fn verdict_changed_with_snapshot_and_small_diff_reminds_with_diff() {
        let i = VerdictIn {
            unchanged: false,
            have_snapshot: true,
            diff_worth_it: true,
            ..vin()
        };
        assert_eq!(read_verdict(&i), ReadAdvice::Diff);
    }

    #[test]
    fn verdict_changed_but_diff_unusable_passes() {
        // Diff over 50% of the new content.
        let big = VerdictIn {
            unchanged: false,
            diff_worth_it: false,
            ..vin()
        };
        assert_eq!(read_verdict(&big), ReadAdvice::Pass { restamp: true });
        // Snapshot evicted.
        let gone = VerdictIn {
            unchanged: false,
            have_snapshot: false,
            ..vin()
        };
        assert_eq!(read_verdict(&gone), ReadAdvice::Pass { restamp: true });
        // Feature off.
        let off = VerdictIn {
            unchanged: false,
            diffs_on: false,
            ..vin()
        };
        assert_eq!(read_verdict(&off), ReadAdvice::Pass { restamp: true });
    }

    #[test]
    fn verdict_change_rearms_up_to_the_cap_then_passes() {
        // A changed re-read of an already-reminded file re-arms while under the
        // cap, then passes once at it.
        for count in 0..READ_REMIND_CAP {
            let i = VerdictIn {
                unchanged: false,
                reminded: true,
                remind_count: count,
                ..vin()
            };
            assert_eq!(
                read_verdict(&i),
                ReadAdvice::Diff,
                "count {count} still re-arms"
            );
        }
        let at_cap = VerdictIn {
            unchanged: false,
            reminded: true,
            remind_count: READ_REMIND_CAP,
            ..vin()
        };
        assert_eq!(
            read_verdict(&at_cap),
            ReadAdvice::Pass { restamp: true },
            "at cap ⇒ pass"
        );
    }

    /// V17 Phase B5: the bypass tap's skip-guard. A provable whole-file shell
    /// read is intercepted by the Bash hook (remind already recorded, or
    /// verdict-passed) so `check_bypass` must NOT also score it — but only when
    /// the shell sub-toggle is on, and only for a command the strict parser
    /// actually accepts. Residual escape routes (`sed -n`, `head`) still score.
    #[test]
    fn intercepted_whole_file_read_guards_only_provable_reads() {
        // Sub-toggle on + a provable whole-file read ⇒ skipped (intercepted).
        assert!(intercepted_whole_file_read(true, "cat src/a.rs"));
        assert!(intercepted_whole_file_read(true, "Get-Content \"a b.txt\""));
        // Sub-toggle OFF ⇒ never skipped (the Bash hook isn't installed, so the
        // command really is an un-intercepted read the canary should score).
        assert!(!intercepted_whole_file_read(false, "cat src/a.rs"));
        // Residual escape routes are not provable whole-file reads ⇒ still scored.
        assert!(!intercepted_whole_file_read(true, "sed -n 5,10p f"));
        assert!(!intercepted_whole_file_read(true, "head -50 f"));
        assert!(!intercepted_whole_file_read(true, "cat a | grep x"));
    }

    #[test]
    fn capture_snapshot_respects_min_lines_and_entry_cap() {
        // Below min-lines ⇒ no snapshot.
        assert!(capture_snapshot("a\nb\n", 10).is_none());
        // At/above min-lines and under the byte cap ⇒ snapshot kept.
        let content: String = "line\n".repeat(20);
        assert!(capture_snapshot(&content, 10).is_some());
        // Over the per-entry byte cap ⇒ no snapshot even with enough lines.
        let huge = "x\n".repeat(SNAP_ENTRY_MAX); // ~2·SNAP_ENTRY_MAX bytes
        assert!(capture_snapshot(&huge, 1).is_none());
    }

    #[test]
    fn read_seen_lru_bounds_snapshot_bytes_and_keeps_the_observation() {
        let mut store = ReadSeenStore::default();
        // ~1 MiB per snapshot; 20 of them (~20 MiB) overruns SNAP_TOTAL_MAX (16 MiB).
        let blob: Arc<str> = Arc::from("y".repeat(1024 * 1024));
        let n = 20u64;
        for k in 0..n {
            let key = ("s".to_string(), format!("f{k}.rs"));
            store.insert(key, format!("h{k}"), k as u32, Some(blob.clone()), k);
        }
        let seen = &store.map;
        // All observations survive (nothing forgot the hash/turn); only content evicted.
        assert_eq!(seen.len() as u64, n, "every observation is retained");
        assert!(
            snapshot_bytes(seen) <= SNAP_TOTAL_MAX,
            "snapshot bytes held under budget: {}",
            snapshot_bytes(seen)
        );
        // Running total matches the O(n) ground truth.
        assert_eq!(
            store.snap_bytes,
            snapshot_bytes(seen),
            "running total tracks snapshot_bytes"
        );
        // The oldest-touched entry lost its snapshot but kept its hash/turn.
        let oldest = seen
            .get(&("s".to_string(), "f0.rs".to_string()))
            .expect("oldest present");
        assert!(oldest.snapshot.is_none(), "oldest snapshot evicted");
        assert_eq!(oldest.hash, "h0", "evicted entry keeps its hash");
        assert_eq!(oldest.turn, 0, "evicted entry keeps its turn");
        // The newest still has its snapshot.
        let newest = seen
            .get(&("s".to_string(), format!("f{}.rs", n - 1)))
            .unwrap();
        assert!(newest.snapshot.is_some(), "newest snapshot retained");
    }

    #[test]
    fn read_seen_entry_backstop_bounds_row_count() {
        let mut store = ReadSeenStore::default();
        // Snapshot-less rows: only the entry backstop bounds these.
        for k in 0..(READ_SEEN_MAX_ENTRIES as u64 + 50) {
            let key = ("s".to_string(), format!("f{k}.rs"));
            store.insert(key, format!("h{k}"), k as u32, None, k);
        }
        let seen = &store.map;
        assert!(
            seen.len() <= READ_SEEN_MAX_ENTRIES,
            "row count bounded by the backstop: {}",
            seen.len()
        );
        // The most-recent key survives; the oldest was evicted.
        let last = READ_SEEN_MAX_ENTRIES as u64 + 49;
        assert!(seen.contains_key(&("s".to_string(), format!("f{last}.rs"))));
        assert!(!seen.contains_key(&("s".to_string(), "f0.rs".to_string())));
    }

    #[test]
    fn read_seen_running_total_matches_ground_truth_across_all_mutations() {
        // Drives the store through insert / replace / entry-cap eviction /
        // byte-budget eviction / session clear / whole clear and asserts the
        // incrementally-maintained `snap_bytes` equals the O(n) `snapshot_bytes`
        // ground truth at every step (V22: the running total must never drift).
        let mut store = ReadSeenStore::default();
        let mut touch = 0u64;
        let mut bump = || {
            let t = touch;
            touch += 1;
            t
        };
        let check = |store: &ReadSeenStore| {
            assert_eq!(
                store.snap_bytes,
                snapshot_bytes(&store.map),
                "running total drifted from snapshot_bytes"
            );
        };

        // Small (no snapshot) and large (snapshot) inserts across two sessions.
        let small: Arc<str> = Arc::from("x".repeat(64));
        let big: Arc<str> = Arc::from("y".repeat(2 * 1024 * 1024)); // 2 MiB each
        for k in 0..8u64 {
            let sid = if k % 2 == 0 { "a" } else { "b" };
            let snap = if k % 3 == 0 {
                Some(big.clone())
            } else {
                Some(small.clone())
            };
            store.insert(
                (sid.to_string(), format!("f{k}.rs")),
                format!("h{k}"),
                k as u32,
                snap,
                bump(),
            );
            check(&store);
        }
        assert!(store.snap_bytes > 0, "snapshots were recorded");

        // Replace an existing key: with a bigger snapshot, then with none.
        store.insert(
            ("a".to_string(), "f0.rs".to_string()),
            "h0b".into(),
            99,
            Some(big.clone()),
            bump(),
        );
        check(&store);
        store.insert(
            ("a".to_string(), "f0.rs".to_string()),
            "h0c".into(),
            100,
            None,
            bump(),
        );
        check(&store);

        // Force the byte-budget eviction path: pile on enough 2 MiB snapshots to
        // cross SNAP_TOTAL_MAX (16 MiB).
        for k in 100..120u64 {
            store.insert(
                ("c".to_string(), format!("f{k}.rs")),
                format!("h{k}"),
                k as u32,
                Some(big.clone()),
                bump(),
            );
            check(&store);
        }
        assert!(store.snap_bytes <= SNAP_TOTAL_MAX, "byte budget enforced");

        // Force the entry-cap eviction path: cross READ_SEEN_MAX_ENTRIES rows.
        for k in 0..(READ_SEEN_MAX_ENTRIES as u64 + 20) {
            store.insert(
                ("d".to_string(), format!("g{k}.rs")),
                format!("h{k}"),
                k as u32,
                None,
                bump(),
            );
        }
        check(&store);
        assert!(
            store.map.len() <= READ_SEEN_MAX_ENTRIES,
            "entry cap enforced"
        );

        // Session clear (drops one session's rows, some snapshotted).
        store.clear_session(Some("c"));
        check(&store);
        assert!(
            !store.map.keys().any(|(sid, _)| sid == "c"),
            "session c cleared"
        );

        // Whole clear.
        store.clear_session(None);
        check(&store);
        assert_eq!(store.snap_bytes, 0, "whole clear zeroes the running total");
        assert!(store.map.is_empty(), "whole clear empties the map");
    }

    // ── V17 Phase C: first-read tier eligibility (pure gate) ──────────────
    //
    // The digest lookup + enqueue + remind wiring in `should_read` needs an
    // `AppHandle` (unmockable), so — like Phase A's `read_verdict` — the tier's
    // GATE is factored into the pure `first_read_eligible` and exercised here;
    // the reminder TEXT is covered by `context::tests::first_read_advice_*`.

    /// A `FirstReadIn` that qualifies (300 KiB non-code whole-file read, tier at
    /// 256 KiB) — override one field per case.
    fn fin() -> FirstReadIn {
        FirstReadIn {
            first_read_kb: 256,
            content_len: 300 * 1024,
            slice: false,
            is_code: false,
        }
    }

    #[test]
    fn first_read_qualifying_is_eligible() {
        assert!(first_read_eligible(&fin()));
    }

    #[test]
    fn first_read_disabled_short_circuits() {
        // kb == 0 ⇒ tier off, regardless of everything else.
        assert!(!first_read_eligible(&FirstReadIn {
            first_read_kb: 0,
            ..fin()
        }));
    }

    #[test]
    fn first_read_under_threshold_passes() {
        // 200 KiB content vs a 256 KiB floor.
        assert!(!first_read_eligible(&FirstReadIn {
            content_len: 200 * 1024,
            ..fin()
        }));
        // Exactly at the threshold qualifies (>=).
        assert!(first_read_eligible(&FirstReadIn {
            content_len: 256 * 1024,
            ..fin()
        }));
    }

    #[test]
    fn first_read_code_file_passes() {
        assert!(!first_read_eligible(&FirstReadIn {
            is_code: true,
            ..fin()
        }));
    }

    #[test]
    fn first_read_slice_passes() {
        // offset OR limit present ⇒ deliberate slice ⇒ never substituted.
        assert!(!first_read_eligible(&FirstReadIn {
            slice: true,
            ..fin()
        }));
    }
}

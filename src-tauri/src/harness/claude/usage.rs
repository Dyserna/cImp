//! **Claude Code's usage source** — the status-line push, the two-slot push
//! file, and the [`UsageSource`] impl core reads it through.
//!
//! V40 Phase D (locked decision 19). This was `crate::usage`, an L4 capability
//! module whose first line described it as "Claude Code subscription usage
//! tracker": Anthropic's two subscription windows as *field names*, Claude
//! Code's `context_window` block mirrored field for field, and a push file
//! named after the harness. None of it is true of harnesses in general, so all
//! of it lives here now and core sees only the neutral readings at the bottom
//! of this file.
//!
//! The bottom-bar widget shows the same session (5h) / weekly (7d) quota the
//! `/usage` slash command shows. The data arrives via the **status line
//! push**: Claude Code (>= 2.1.80) includes a `rate_limits` object in the JSON
//! it pipes to the `statusLine` command —
//!
//! ```json
//! { "rate_limits": {
//!     "five_hour": { "used_percentage": 23.5, "resets_at": 1738425600 },
//!     "seven_day": { "used_percentage": 41.2, "resets_at": 1738857600 } } }
//! ```
//!
//! (`used_percentage` is 0-100; `resets_at` is Unix epoch seconds.) Our
//! `cimp --statusline` renderer (see [`super::statusline`]) extracts that
//! object on every invocation and persists it to `<exe-dir>/claude-usage-push.json`
//! via [`store_pushed_usage`]; the widget's poll reads it back through
//! [`ClaudeUsage`]'s [`UsageSource::read`]. The injected overlay also sets
//! `statusLine.refreshInterval` so pushes keep flowing while a Claude tab sits
//! idle.
//!
//! This replaced polling an undocumented account-usage endpoint, which allowed
//! only a tiny request burst before answering 429 with a multi-minute
//! `Retry-After` — the widget spent most of its life dimmed on cached data.
//! The push costs zero extra requests (Claude Code already has the numbers
//! from its API responses) and uses a documented schema. **The disabled poller
//! that used to sit at the bottom of this file is deleted** (V40 Phase D): it
//! was compiled dead code carrying a vendor OAuth URL and a reader for the
//! harness's on-disk credentials file, and "kept in case a future feature
//! needs it" is not a consumer.
//!
//! NC-3: the same payload also carries a `context_window` block (used
//! percentage, tokens in the window, and the turn's cache read/creation split)
//! plus session metadata (`session_name`, `agent.name`, `effort`, `thinking`,
//! `fast_mode`). That rides the *same* push — see [`ContextSnapshot`] — so the
//! live context bar costs no second data path and no extra polling. Per-turn
//! *historical* cache stats stay on the transcript-tap/graph path; this push
//! is the live snapshot only.
//!
//! Caveats of the push path:
//!   - `rate_limits` exists only for Claude.ai subscription auth, only after
//!     the first API response of a session, and only while a cImp-launched
//!     Claude tab runs with the statusline injection enabled. Absent/expired
//!     push data hides the widget — by design, there is no network fallback.
//!   - Every part is independently absent-able: either quota window, the
//!     context block, and each field inside it. Absent must render as
//!     *unknown*, never as 0%.
//!
//! M14 (push-file format 2): several Claude tabs push into the same file, and
//! they do not carry the same data — the subscription tab has `rate_limits`,
//! an API-key/local tab has only context. The file therefore holds **two
//! independently aged slots** (quota and context) rather than one observation:
//!   - a context write merges over the quota slot instead of replacing it, so
//!     quota disappears only when it is itself past [`HIDE_AFTER`];
//!   - the context slot is owned by the session whose reading most recently
//!     *changed* (see [`merge_push`]), so the tab actually being worked in wins
//!     over an idle tab that keeps re-pushing the same numbers;
//!   - each slot carries its own write instant, so the UI dims per section by
//!     that section's own age instead of one global timestamp.
//!
//! # The on-disk shape is frozen, the shape core sees is not
//!
//! [`UsageSnapshot`], [`UsageWindow`] and [`ContextSnapshot`] keep Claude's
//! field names because they ARE the push file, which a previously installed
//! build wrote and this one must still read. What crosses into core is
//! [`crate::harness::plugin::UsageReading`] — a declared window list, a token
//! map whose absent categories are absent, and nothing named after a
//! subscription plan.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::harness::plugin::{
    ContextReading, QuotaWindow, QuotaWindowSpec, TokenKindSpec, TokenKinds, TurnOrigin,
    TurnUsageShape, UsageReading, UsageSource,
};

/// One quota window: how much of the limit is used and when it resets.
/// `utilization` is 0–100; `resets_at` is an ISO-8601 timestamp (with tz) or
/// `null` (windows at 0% can report no reset time).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UsageWindow {
    pub utilization: f64,
    pub resets_at: Option<String>,
}

/// The session (5h) and weekly (7d) windows the UI renders, plus the live
/// context-window reading (NC-3) that rides the same push. Re-serialized to
/// the frontend and to the push file on disk.
///
/// Every field is independently optional: `rate_limits` exists only for
/// subscription auth after the first API response, `context` only for a
/// Claude Code new enough to send `context_window`. Consumers must render a
/// missing part as *unknown*, never as zero.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UsageSnapshot {
    #[serde(default)]
    pub five_hour: Option<UsageWindow>,
    #[serde(default)]
    pub seven_day: Option<UsageWindow>,
    /// Live context-window / cache reading from the same status-line payload.
    /// `None` on older payloads (and on old push files written before NC-3,
    /// which is why it defaults rather than being required).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextSnapshot>,
}

impl UsageSnapshot {
    /// True when the snapshot carries at least one quota window.
    pub fn has_rate_limits(&self) -> bool {
        self.five_hour.is_some() || self.seven_day.is_some()
    }

    /// True when there is anything at all worth rendering — quota windows or
    /// substantive context numbers. "Empty is not absent": a snapshot whose
    /// parts are all missing (or whose context carries only metadata) is
    /// absence with extra steps.
    pub fn is_substantive(&self) -> bool {
        self.has_rate_limits()
            || self
                .context
                .as_ref()
                .is_some_and(ContextSnapshot::is_substantive)
    }
}

/// Live context-window reading pulled from the status-line payload's
/// `context_window` block plus the session metadata beside it (NC-3).
///
/// Everything is `Option` on purpose — the block is walked leniently out of a
/// raw `serde_json::Value` (see [`super::statusline`]), so a reshaped or partial
/// upstream payload yields fewer fields rather than a failed parse, and the UI
/// can tell "0 tokens" apart from "not reported".
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ContextSnapshot {
    /// Percentage of the context window in use (0–100), as Claude Code
    /// computes it (input + cache over the window size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percentage: Option<f64>,
    /// Percentage still free (0–100). Reported separately upstream; kept as
    /// sent rather than derived, so a future definition change can't silently
    /// turn into a wrong number here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percentage: Option<f64>,
    /// Tokens currently occupying the window (input + cache).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_input_tokens: Option<u64>,
    /// Maximum context size in tokens (200k, or 1M with extended context).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_size: Option<u64>,
    /// `current_usage.cache_read_input_tokens` — tokens served from the
    /// prompt cache on the latest turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// `current_usage.cache_creation_input_tokens` — tokens written into the
    /// prompt cache on the latest turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    /// `current_usage.input_tokens` — uncached input tokens of the latest turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// `current_usage.output_tokens` — output tokens of the latest turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Human-readable session name, when Claude Code names the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    /// `agent.name` — the active agent/persona, when one is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Reasoning-effort setting as reported (free-form string; stringified
    /// from whatever scalar the payload carries).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Thinking setting as reported, stringified (`"on"` / `"off"` for the
    /// boolean form) — upstream has used both a flag and a level here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Fast-mode flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_mode: Option<bool>,
}

impl ContextSnapshot {
    /// True when at least one *number* is present. Metadata alone (a session
    /// name, an effort string) is not something the context bar can render, so
    /// it does not make a snapshot worth showing.
    pub fn is_substantive(&self) -> bool {
        self.used_percentage.is_some()
            || self.remaining_percentage.is_some()
            || self.total_input_tokens.is_some()
            || self.context_window_size.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_creation_tokens.is_some()
            || self.input_tokens.is_some()
            || self.output_tokens.is_some()
    }
}

/// Outcome of a push-file read. `Default` is the unavailable state (widget
/// hides).
///
/// V40 Phase D dropped its two `rate_limited` / `retry_after_secs` fields with
/// the poller that set them: they were serialized to the frontend as
/// permanently `false` / `null`, and a field that can only carry one value is
/// a contract nobody can read anything out of.
#[derive(Clone, Debug, Default)]
pub struct PushedReading {
    /// The snapshot to render. `None` when no push data exists (no Claude tab
    /// has produced one yet, or the last one is too old to be meaningful).
    pub snapshot: Option<UsageSnapshot>,
    /// True when *every* part of `snapshot` that carries data is aging —
    /// nothing in the widget is fresh. Kept as the whole-widget signal (the
    /// tooltip); per-section dimming uses the two flags below, because the
    /// quota and context slots are written by different tabs and age by their
    /// own clocks (M14).
    pub stale: bool,
    /// True when the quota slot is present and older than [`STALE_AFTER`].
    /// False when there is no quota data at all (nothing to dim).
    pub quota_stale: bool,
    /// True when the context slot is present and older than [`STALE_AFTER`].
    /// False when there is no context data at all.
    pub context_stale: bool,
}

// ---- status-line push (live path) ----------------------------------------

/// `<exe-dir>/claude-usage-push.json` — written by every `cimp --statusline`
/// invocation that sees `rate_limits`, read by the widget's poll. Sits next
/// to the portable `settings.json` like the other side-channel files.
const PUSH_FILE: &str = "claude-usage-push.json";

/// Push data older than this renders dimmed (`stale`). Three missed beats of
/// the 30s `statusLine.refreshInterval` the launch overlay injects (see
/// `tabs::config`) — one missed beat is normal scheduling jitter.
pub const STALE_AFTER: Duration = Duration::from_secs(90);

/// Push data older than this is treated as absent (widget hides). Half an
/// hour with no running Claude tab means the 5h window has drifted far from
/// the last-known numbers; showing them would be misinformation.
pub const HIDE_AFTER: Duration = Duration::from_secs(30 * 60);

/// How many sessions' change-marks the file keeps (see [`SessionMark`]).
/// Bounded so a long-lived file can't grow without limit; the oldest-seen
/// entry is evicted first, and entries older than [`HIDE_AFTER`] are pruned
/// on every write anyway.
const MAX_SESSION_MARKS: usize = 8;

/// Current on-disk format of the push file. Written into every push so a
/// reader can tell the two shapes apart without guessing:
///   - absent / `0` — the pre-M14 shape: one `written_at_ms` ages everything.
///   - `2` — per-slot instants (`quota_at_ms` / `context_at_ms`) plus the
///     context-ownership bookkeeping.
///
/// (There is no `1`: the pre-M14 files never carried a version at all, and
/// `0` is what `#[serde(default)]` yields for them.)
const PUSH_FORMAT: u32 = 2;

/// One session's context-change bookkeeping, used to decide which tab owns the
/// context slot. `sig` is a compact digest of the session's last context
/// reading (plus its activity counters); `changed_at_ms` is when that digest
/// last differed, `seen_at_ms` when the session last pushed at all.
///
/// The slot goes to the session with the most recent `changed_at_ms` — i.e.
/// the tab actually being worked in — instead of to whichever tab pushed last.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct SessionMark {
    key: String,
    sig: String,
    changed_at_ms: u64,
    seen_at_ms: u64,
}

/// On-disk shape of the push file. Flattened so it reads naturally:
/// `{"written_at_ms":…,"format":2,"five_hour":{…},"context":{…},…}`.
///
/// Two independently aged slots (M14): the quota windows and the context
/// block. `quota_at_ms` / `context_at_ms` are each slot's own write instant;
/// on a pre-M14 file they are absent and both fall back to `written_at_ms`
/// (see [`PushedUsage::quota_at`] / [`PushedUsage::context_at`]), which is
/// exactly the old "one instant ages everything" behavior.
#[derive(Serialize, Deserialize, Debug, Default)]
struct PushedUsage {
    /// Unix epoch milliseconds; the *oldest* of the present slot instants
    /// (writer's clock; reader is the same machine, so skew is not a concern).
    /// Only a format-0 reader ages by this — deliberately the oldest, so such
    /// a reader errs towards dimming/hiding rather than presenting a
    /// carried-over reading as fresh.
    written_at_ms: u64,
    /// On-disk format (see [`PUSH_FORMAT`]). `0` for pre-M14 files.
    #[serde(default)]
    format: u32,
    /// When the quota slot was last written. `None` on a pre-M14 file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quota_at_ms: Option<u64>,
    /// When the context slot was last written. `None` on a pre-M14 file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_at_ms: Option<u64>,
    /// Session key that currently owns the context slot (diagnostics; the
    /// merge recomputes the owner from `sessions` on every write).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_owner: Option<String>,
    /// Per-session context-change marks (see [`SessionMark`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sessions: Vec<SessionMark>,
    #[serde(flatten)]
    snapshot: UsageSnapshot,
}

impl PushedUsage {
    /// Write instant of the quota slot, falling back to the whole-file instant
    /// on a pre-M14 file.
    fn quota_at(&self) -> u64 {
        self.quota_at_ms.unwrap_or(self.written_at_ms)
    }

    /// Write instant of the context slot, same fallback.
    fn context_at(&self) -> u64 {
        self.context_at_ms.unwrap_or(self.written_at_ms)
    }
}

/// Everything a push knows about *who* wrote it, beyond the renderable
/// snapshot. Not persisted as such: it feeds the context-ownership decision in
/// [`merge_push`] and never reaches the UI, so it is not part of the
/// Rust↔TS field contract.
#[derive(Clone, Debug, Default)]
pub struct PushMeta {
    /// Stable per-session key from the status-line payload (`session_id`, else
    /// the transcript path, else the session name). `None` when the payload
    /// offers none of them — every tab then shares one bucket and the context
    /// slot degrades to last-writer-wins.
    pub session_key: Option<String>,
    /// Monotonic activity counters from the payload's `cost` block, folded
    /// into the session's signature so a turn that leaves the context numbers
    /// unchanged still counts as activity. `None` when absent.
    pub activity: Option<String>,
}

/// `<exe-dir>/claude-usage-push.json`, or `None` when `current_exe()` can't
/// be resolved (the push is then skipped entirely).
fn push_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(PUSH_FILE))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Persist a snapshot extracted from a status-line payload. Called from the
/// short-lived `cimp --statusline` child process, so it must never panic or
/// block for long: any failure is silently dropped (the next refresh retries
/// within seconds). The write is atomic (unique temp file + rename) because
/// several Claude tabs may push concurrently, so the reader never sees a torn
/// file.
///
/// The new push is *merged* over what the file already holds rather than
/// replacing it wholesale — see [`merge_push`] for the two-slot rules. A
/// read failure that isn't "file missing" aborts the write instead of
/// merging over a phantom empty file (the TOCTOU that would let a
/// context-only push evict a live quota reading for one beat).
pub fn store_pushed_usage(snapshot: &UsageSnapshot, meta: &PushMeta) {
    let Some(path) = push_path() else { return };
    let Ok(prev) = read_prev(&path) else {
        debug!("usage: push file unreadable; skipping this push rather than clobbering it");
        return;
    };
    let merged = merge_push(prev, snapshot, meta, now_ms());
    let Ok(json) = serde_json::to_string(&merged) else {
        return;
    };
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    if std::fs::write(&tmp, json).is_ok() {
        // Windows rename replaces an existing destination; if it still fails
        // (e.g. the reader holds the file at that instant), drop the temp and
        // let the next push try again.
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Read the file we are about to merge over.
///
/// `Ok(None)` means "there is genuinely nothing there" (no file yet, or its
/// contents no longer parse — that we do overwrite, otherwise a corrupt file
/// would wedge the widget forever). `Err(())` means the file exists but could
/// not be read: on Windows a concurrent reader/writer can hand us a sharing
/// violation, and treating that as "no previous data" is how a context-only
/// push would evict a perfectly good quota reading. One bounded retry pair
/// (2 × 5 ms) covers the sharing window; past that the caller skips the write.
fn read_prev(path: &std::path::Path) -> Result<Option<PushedUsage>, ()> {
    const ATTEMPTS: usize = 3;
    for attempt in 0..ATTEMPTS {
        match std::fs::read_to_string(path) {
            Ok(raw) => return Ok(serde_json::from_str::<PushedUsage>(&raw).ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) if attempt + 1 < ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return Err(()),
        }
    }
    Err(())
}

/// Fold a fresh push into the file's current contents.
///
/// Two slots, aged independently:
///
///   * **quota** — replaced when this push carries `rate_limits`, otherwise
///     carried over with its *own* instant until it is past [`HIDE_AFTER`].
///     A context-only push therefore never blinks the quota widget out, and
///     never restamps a quota reading as freshly observed.
///   * **context** — claimed by the session whose reading most recently
///     *changed*. Every pushing session gets a [`SessionMark`]; a push whose
///     signature differs from that session's last one bumps its
///     `changed_at_ms`. The slot's owner is the live session with the greatest
///     `changed_at_ms`, so an idle tab re-pushing identical numbers cannot
///     take the bar away from the tab being worked in. When the pushing
///     session is not the owner, the slot (content *and* instant) is left
///     alone; when it is, the content is rewritten and stamped `now` even if
///     it did not change — the session is alive and the reading really is
///     current.
///
/// With no session key in the payload every tab shares one bucket, which
/// degrades to plain last-writer-wins on the context slot (still never
/// evicting quota).
fn merge_push(
    prev: Option<PushedUsage>,
    new: &UsageSnapshot,
    meta: &PushMeta,
    now_ms: u64,
) -> PushedUsage {
    let age = |at: u64| Duration::from_millis(now_ms.saturating_sub(at));

    // ---- quota slot -------------------------------------------------------
    let (five_hour, seven_day, quota_at_ms) = if new.has_rate_limits() {
        (new.five_hour.clone(), new.seven_day.clone(), Some(now_ms))
    } else {
        match prev.as_ref().filter(|p| p.snapshot.has_rate_limits()) {
            Some(p) if age(p.quota_at()) <= HIDE_AFTER => (
                p.snapshot.five_hour.clone(),
                p.snapshot.seven_day.clone(),
                Some(p.quota_at()),
            ),
            _ => (None, None, None),
        }
    };

    // ---- context slot -----------------------------------------------------
    // "Empty is not absent": a context block carrying only metadata has
    // nothing to draw, so it neither claims the slot nor evicts what's there.
    let new_context = new
        .context
        .clone()
        .filter(ContextSnapshot::is_substantive);
    let mut sessions: Vec<SessionMark> = prev
        .as_ref()
        .map(|p| p.sessions.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|m| age(m.seen_at_ms) <= HIDE_AFTER)
        .collect();

    let mut owner: Option<String> = None;
    if let Some(ctx) = new_context.as_ref() {
        // No session id in the payload → one shared bucket (last writer wins).
        let key = meta
            .session_key
            .clone()
            .or_else(|| ctx.session_name.clone())
            .unwrap_or_default();
        let sig = context_signature(ctx, meta.activity.as_deref());
        match sessions.iter_mut().find(|m| m.key == key) {
            Some(mark) => {
                if mark.sig != sig {
                    mark.sig = sig;
                    mark.changed_at_ms = now_ms;
                }
                mark.seen_at_ms = now_ms;
            }
            None => sessions.push(SessionMark {
                key: key.clone(),
                sig,
                changed_at_ms: now_ms,
                seen_at_ms: now_ms,
            }),
        }
        // Keep the map bounded: evict least-recently-seen first.
        while sessions.len() > MAX_SESSION_MARKS {
            let Some(victim) = sessions
                .iter()
                .enumerate()
                .min_by_key(|(_, m)| m.seen_at_ms)
                .map(|(i, _)| i)
            else {
                break;
            };
            sessions.remove(victim);
        }
        // Most recently *changed* session owns the slot; ties (nothing has
        // changed yet) go to the most recent pusher.
        let winner = sessions
            .iter()
            .max_by_key(|m| (m.changed_at_ms, m.seen_at_ms))
            .map(|m| m.key.clone());
        if winner.as_deref() == Some(key.as_str()) {
            owner = Some(key);
        }
    }
    let (context, context_at_ms, context_owner) = match owner {
        Some(key) => (new_context, Some(now_ms), Some(key)),
        None => match prev
            .as_ref()
            .filter(|p| p.snapshot.context.is_some() && age(p.context_at()) <= HIDE_AFTER)
        {
            Some(p) => (
                p.snapshot.context.clone(),
                Some(p.context_at()),
                p.context_owner.clone(),
            ),
            None => (None, None, None),
        },
    };

    // A format-0 reader ages the whole file by `written_at_ms`; give it the
    // oldest present instant so it can only under-show, never present a
    // carried-over reading as fresh.
    let written_at_ms = [quota_at_ms, context_at_ms]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(now_ms);

    PushedUsage {
        written_at_ms,
        format: PUSH_FORMAT,
        quota_at_ms,
        context_at_ms,
        context_owner,
        sessions,
        snapshot: UsageSnapshot {
            five_hour,
            seven_day,
            context,
        },
    }
}

/// Compact digest of a context reading: every number that can move within a
/// session, plus the payload's activity counters when it has them. Two pushes
/// with the same digest are the same observation — the session has done
/// nothing since. Deliberately readable rather than hashed, so a stuck slot
/// can be diagnosed by opening the file.
fn context_signature(ctx: &ContextSnapshot, activity: Option<&str>) -> String {
    fn n<T: std::fmt::Display>(v: Option<T>) -> String {
        v.map(|x| x.to_string()).unwrap_or_default()
    }
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}",
        n(ctx.used_percentage),
        n(ctx.remaining_percentage),
        n(ctx.total_input_tokens),
        n(ctx.context_window_size),
        n(ctx.cache_read_tokens),
        n(ctx.cache_creation_tokens),
        n(ctx.input_tokens),
        n(ctx.output_tokens),
        activity.unwrap_or_default(),
    )
}

/// Read the current usage for the widget: the freshest status-line push,
/// aged into fresh / stale / absent. Pure local file read — never touches
/// the network.
fn pushed_usage() -> PushedReading {
    let raw = match push_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(r) => r,
        None => {
            debug!("usage: no push file; widget hides");
            return PushedReading::default();
        }
    };
    interpret_push(&raw, now_ms())
}

/// Age a raw push-file payload into a `PushedReading`. Split from
/// [`pushed_usage`] so staleness is unit-testable with an injected clock.
fn interpret_push(raw: &str, now_ms: u64) -> PushedReading {
    let pushed: PushedUsage = match serde_json::from_str(raw) {
        Ok(p) => p,
        Err(e) => {
            debug!(error = %e, "usage: push file unparseable; treating as absent");
            return PushedReading::default();
        }
    };
    // A write instant in the future (clock adjustment) counts as fresh.
    let age = |at: u64| Duration::from_millis(now_ms.saturating_sub(at));
    let quota_age = age(pushed.quota_at());
    let context_age = age(pushed.context_at());

    // Each slot expires on its own clock (M14): a context slot that kept
    // refreshing does not keep a half-hour-old quota reading on screen, and a
    // quota tab that is still pushing does not hold up an expired context one.
    let mut snapshot = pushed.snapshot;
    if quota_age > HIDE_AFTER {
        snapshot.five_hour = None;
        snapshot.seven_day = None;
    }
    if context_age > HIDE_AFTER {
        snapshot.context = None;
    }
    // An empty snapshot is absence with extra steps — never render it. A push
    // carrying *only* context numbers (API-key auth has no `rate_limits`) is
    // not empty: it renders the context bar alone.
    if !snapshot.is_substantive() {
        debug!(
            quota_age_secs = quota_age.as_secs(),
            context_age_secs = context_age.as_secs(),
            "usage: no live push data; widget hides"
        );
        return PushedReading::default();
    }
    let has_quota = snapshot.has_rate_limits();
    let has_context = snapshot
        .context
        .as_ref()
        .is_some_and(ContextSnapshot::is_substantive);
    let quota_stale = has_quota && quota_age > STALE_AFTER;
    let context_stale = has_context && context_age > STALE_AFTER;
    PushedReading {
        snapshot: Some(snapshot),
        // Whole-widget flag: only when nothing on screen is fresh.
        stale: (!has_quota || quota_stale) && (!has_context || context_stale),
        quota_stale,
        context_stale,
    }
}

/// Unix epoch seconds → ISO-8601 UTC string (the format the frontend's
/// `new Date(...)` parsing and the push file already use). Values that look
/// like epoch *milliseconds* (>= ~year 5138 when read as seconds) are
/// normalized first, as cheap insurance against the upstream field changing
/// units. `None` for out-of-range values.
pub(crate) fn epoch_secs_to_iso(n: i64) -> Option<String> {
    let secs = if n.abs() > 100_000_000_000 {
        n / 1000
    } else {
        n
    };
    Some(chrono::DateTime::from_timestamp(secs, 0)?.to_rfc3339())
}


// ── the neutral seam (locked decision 19) ──────────────────────────────────

/// Claude Code's subscription quota windows, in display order.
///
/// The ids are the push payload's own keys, so the reading joins to the
/// declaration without a translation table; the three display strings are what
/// the bottom-bar widget renders, and they live here rather than in the widget
/// because "current session (5h)" is a fact about Anthropic's plan.
const WINDOWS: &[QuotaWindowSpec] = &[
    QuotaWindowSpec {
        id: "five_hour",
        label: "current session",
        short: "(5h)",
        description: "Rolling 5-hour session quota",
    },
    QuotaWindowSpec {
        id: "seven_day",
        label: "weekly session",
        short: "(7d)",
        description: "Rolling 7-day weekly quota",
    },
];

/// The billing categories Claude Code reports a turn's tokens under.
///
/// Four, because this vendor prices cache reads and cache writes separately. A
/// harness that bills one flat input number declares one row and every
/// consumer of a reading sees one entry — never four with three zeros.
const TOKEN_KINDS: &[TokenKindSpec] = &[
    TokenKindSpec { id: "input", label: "Input" },
    TokenKindSpec { id: "cache_write", label: "Cache write" },
    TokenKindSpec { id: "cache_read", label: "Cache read" },
    TokenKindSpec { id: "output", label: "Output" },
];

/// **The one spelling of Claude's main-transcript lane.** Written verbatim into
/// `usage_stat.origin` by the transcript tap and read back verbatim by the
/// Usage donut — the value is frozen by the rows already on disk.
pub const ORIGIN_SESSION: &str = "session";

/// **The one spelling of Claude's sidechain lane** — a turn recorded in
/// `<sid>/subagents/*.jsonl`, or an inline `isSidechain:true` line in the parent
/// transcript. Same frozen-by-disk posture as [`ORIGIN_SESSION`].
pub const ORIGIN_AGENT: &str = "agent";

/// The two lanes a Claude turn can be attributed to. Both ids are persisted in
/// `usage_stat.origin`, and both are spelled ONCE ([`ORIGIN_SESSION`] /
/// [`ORIGIN_AGENT`]) so the tap, the declaration and the stored column cannot
/// drift apart.
const ORIGINS: &[TurnOrigin] = &[
    TurnOrigin { id: ORIGIN_SESSION, label: "main session", subagent: false },
    TurnOrigin { id: ORIGIN_AGENT, label: "sub-agents", subagent: true },
];

/// **The shape of a recorded Claude turn** — its four billing categories and
/// its two lanes, handed to core by
/// [`HarnessPlugin::turn_usage_shape`](crate::harness::plugin::HarnessPlugin::turn_usage_shape).
///
/// Declared beside the quota source rather than on it (V40 Phase G): what a
/// stored `usage_stat` row looks like is a different question from what the
/// status-line push reports, and only the first one has an answer for every
/// harness that records turns.
pub static TURN_SHAPE: TurnUsageShape = TurnUsageShape {
    token_kinds: TOKEN_KINDS,
    origins: ORIGINS,
};

/// Claude Code's pseudo-model, stamped on messages it fabricates locally
/// (errors, interrupts). Nobody was billed for a `<synthetic>` turn, so it must
/// not appear in "which model ran this session" — a filter that used to be two
/// string literals inside `graph/index.rs`.
const MODEL_SENTINELS: &[&str] = &["<synthetic>"];

/// The [`UsageSource`] the descriptor hands core.
pub struct ClaudeUsage;

/// The one instance.
pub static USAGE: ClaudeUsage = ClaudeUsage;

impl UsageSource for ClaudeUsage {
    fn windows(&self) -> &'static [QuotaWindowSpec] {
        WINDOWS
    }

    fn model_sentinels(&self) -> &'static [&'static str] {
        MODEL_SENTINELS
    }

    fn read(&self) -> Option<UsageReading> {
        pushed_usage().into_reading()
    }
}

impl PushedReading {
    /// This push-file reading as the neutral one core speaks.
    ///
    /// `None` when there is nothing to render at all — which is a different
    /// answer from `usage_source() == None` ("this harness has no usage
    /// source"), and the distinction is what stops a silent tab and a harness
    /// without quota from looking the same.
    fn into_reading(self) -> Option<UsageReading> {
        let snap = self.snapshot?;
        Some(UsageReading {
            windows: quota_windows(&snap),
            // Substantive-only at the neutral boundary as well as at the push
            // one: a context block that survived the merge but has no numbers
            // left to draw is absence with extra steps.
            context: snap
                .context
                .as_ref()
                .map(context_reading)
                .filter(ContextReading::is_substantive),
            stale: self.stale,
            quota_stale: self.quota_stale,
            context_stale: self.context_stale,
        })
    }
}

/// The declared windows that actually have a reading, in declared order.
///
/// A window with no reading is **omitted**, not emitted at 0: "absent is not
/// zero" is the widget's governing rule, and the only way to keep it at the
/// widget is to keep it here.
fn quota_windows(snap: &UsageSnapshot) -> Vec<QuotaWindow> {
    WINDOWS
        .iter()
        .filter_map(|spec| {
            let w = match spec.id {
                "five_hour" => snap.five_hour.as_ref(),
                "seven_day" => snap.seven_day.as_ref(),
                // Unreachable while WINDOWS and UsageSnapshot are edited
                // together; a new declared window with no field behind it
                // reports nothing rather than zero.
                _ => None,
            }?;
            Some(QuotaWindow {
                id: spec.id.to_string(),
                label: spec.label.to_string(),
                short: spec.short.to_string(),
                description: spec.description.to_string(),
                used: w.utilization,
                resets_at: w.resets_at.clone(),
            })
        })
        .collect()
}

/// The push's `context_window` block as a neutral reading.
///
/// The four per-turn counters become entries in `tokens` keyed by
/// [`TOKEN_KINDS`] ids, and a counter the payload did not carry produces **no
/// entry** — the whole point of the map over four `Option` fields is that a
/// consumer cannot read an absent category as a zero one.
fn context_reading(ctx: &ContextSnapshot) -> ContextReading {
    let mut tokens = TokenKinds::default();
    for (id, v) in [
        ("input", ctx.input_tokens),
        ("cache_write", ctx.cache_creation_tokens),
        ("cache_read", ctx.cache_read_tokens),
        ("output", ctx.output_tokens),
    ] {
        if let Some(v) = v {
            tokens.set(id, v);
        }
    }
    let mut meta = std::collections::BTreeMap::new();
    for (k, v) in [
        ("session_name", ctx.session_name.clone()),
        ("agent_name", ctx.agent_name.clone()),
        ("effort", ctx.effort.clone()),
        ("thinking", ctx.thinking.clone()),
        ("fast_mode", ctx.fast_mode.map(|b| b.to_string())),
    ] {
        if let Some(v) = v.filter(|s| !s.is_empty()) {
            meta.insert(k.to_string(), v);
        }
    }
    ContextReading {
        used_percentage: ctx.used_percentage,
        remaining_percentage: ctx.remaining_percentage,
        total_input_tokens: ctx.total_input_tokens,
        context_window_size: ctx.context_window_size,
        tokens,
        meta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The declared windows and the fixture render identically to the
    /// pre-V40 widget** — the numbers AND the labels.
    ///
    /// This is the regression the whole of locked decision 19 has to survive:
    /// `five_hour` / `seven_day` stopped being field names on a core struct and
    /// became declared rows, so the risk is a row that renders in the wrong
    /// order, under the wrong label, or with a percentage read off the wrong
    /// window. The fixture is the exact push-file shape a shipped build wrote.
    #[test]
    fn the_reading_carries_the_same_numbers_and_labels_the_widget_drew_before() {
        let now = 1_000_000_000u64;
        let raw = format!(
            r#"{{"written_at_ms":{now},"five_hour":{{"utilization":23.5,"resets_at":"2026-08-04T12:00:00+00:00"}},"seven_day":{{"utilization":41.2,"resets_at":null}}}}"#
        );
        let reading = interpret_push(&raw, now)
            .into_reading()
            .expect("a fresh push has a reading");

        assert_eq!(reading.windows.len(), 2, "both windows report");
        let five = &reading.windows[0];
        assert_eq!(five.id, "five_hour");
        assert_eq!(five.label, "current session");
        assert_eq!(five.short, "(5h)");
        assert_eq!(five.description, "Rolling 5-hour session quota");
        assert_eq!(five.used, 23.5);
        assert_eq!(five.resets_at.as_deref(), Some("2026-08-04T12:00:00+00:00"));

        let seven = &reading.windows[1];
        assert_eq!(seven.id, "seven_day");
        assert_eq!(seven.label, "weekly session");
        assert_eq!(seven.short, "(7d)");
        assert_eq!(seven.description, "Rolling 7-day weekly quota");
        assert_eq!(seven.used, 41.2);
        assert_eq!(seven.resets_at, None);

        assert!(!reading.stale);
        assert!(!reading.quota_stale);
    }

    /// A window the push does not carry is **absent from the list**, not
    /// present at zero — the rule the widget's hollow "not reported" track
    /// depends on, now enforced one layer earlier.
    #[test]
    fn an_unreported_window_is_absent_rather_than_zero() {
        let now = 1_000_000_000u64;
        let raw = format!(
            r#"{{"written_at_ms":{now},"five_hour":{{"utilization":0.0,"resets_at":null}}}}"#
        );
        let reading = interpret_push(&raw, now).into_reading().expect("present");
        assert_eq!(reading.windows.len(), 1);
        assert_eq!(reading.windows[0].id, "five_hour");
        assert_eq!(
            reading.windows[0].used, 0.0,
            "a REPORTED zero is still reported — only absence is absent"
        );
    }

    /// The context block's per-turn counters land under the DECLARED token
    /// category ids, and a counter the payload did not carry produces **no
    /// entry** — which is the whole reason `tokens` is a map and not four
    /// `Option` fields.
    #[test]
    fn token_categories_are_declared_and_absent_means_absent() {
        let ctx = ContextSnapshot {
            used_percentage: Some(12.5),
            cache_read_tokens: Some(700),
            input_tokens: Some(10),
            // No cache_creation_tokens, no output_tokens.
            ..Default::default()
        };
        let reading = context_reading(&ctx);
        assert_eq!(reading.tokens.get("cache_read"), Some(700));
        assert_eq!(reading.tokens.get("input"), Some(10));
        assert_eq!(reading.tokens.get("cache_write"), None);
        assert_eq!(reading.tokens.get("output"), None);
        assert_eq!(reading.used_percentage, Some(12.5));

        // Every key it can emit is one the source DECLARES, or a UI has a
        // number with no label for it.
        let declared: Vec<&str> = TURN_SHAPE.token_kinds.iter().map(|k| k.id).collect();
        for key in reading.tokens.ids() {
            assert!(declared.contains(&key), "undeclared category `{key}`");
        }
    }

    /// The persisted `usage_stat.origin` wire strings are exactly what this
    /// harness declares as its turn origins, and the tap writes those same
    /// constants.
    ///
    /// The column is written by the reader and read back by the Usage donut, so
    /// a declared origin that no row can carry (or a stored value nothing
    /// declares) is a lane with no label at one end or no data at the other.
    /// V40 Phase G: there is no `UsageOrigin` enum to round-trip through any
    /// more — the id IS the column — so what this pins instead is that the two
    /// ids are still exactly the two strings already on disk in every user's
    /// graph, and that the fan-out flag names the sidechain lane.
    #[test]
    fn every_declared_origin_round_trips_the_persisted_column() {
        let declared: Vec<&str> = TURN_SHAPE.origins.iter().map(|o| o.id).collect();
        assert_eq!(declared, vec!["session", "agent"]);
        assert_eq!(ORIGIN_SESSION, "session");
        assert_eq!(ORIGIN_AGENT, "agent");
        assert_eq!(TURN_SHAPE.main_origin(), Some(ORIGIN_SESSION));
        assert_eq!(TURN_SHAPE.subagent_origin(), Some(ORIGIN_AGENT));
    }

    fn push_json(written_at_ms: u64) -> String {
        format!(
            r#"{{"written_at_ms":{written_at_ms},"five_hour":{{"utilization":23.5,"resets_at":"2026-08-04T12:00:00+00:00"}},"seven_day":{{"utilization":41.2,"resets_at":null}}}}"#
        )
    }

    #[test]
    fn fresh_push_renders_undimmed() {
        let now = 1_000_000_000;
        let r = interpret_push(&push_json(now - STALE_AFTER.as_millis() as u64 / 2), now);
        let snap = r.snapshot.expect("snapshot present");
        assert_eq!(snap.five_hour.unwrap().utilization, 23.5);
        assert_eq!(snap.seven_day.unwrap().utilization, 41.2);
        assert!(!r.stale);
    }

    #[test]
    fn aging_push_flags_stale() {
        let now = 1_000_000_000;
        let age = STALE_AFTER.as_millis() as u64 + 1_000;
        let r = interpret_push(&push_json(now - age), now);
        assert!(r.snapshot.is_some());
        assert!(r.stale);
    }

    #[test]
    fn expired_push_hides() {
        let now = 10_000_000_000;
        let age = HIDE_AFTER.as_millis() as u64 + 1_000;
        let r = interpret_push(&push_json(now - age), now);
        assert!(r.snapshot.is_none());
        assert!(!r.stale);
    }

    #[test]
    fn future_write_instant_counts_as_fresh() {
        // Clock adjusted backwards between write and read: saturating age = 0.
        let r = interpret_push(&push_json(2_000_000_000), 1_000_000_000);
        assert!(r.snapshot.is_some());
        assert!(!r.stale);
    }

    #[test]
    fn empty_snapshot_is_treated_as_absent() {
        let raw = r#"{"written_at_ms":999999500,"five_hour":null,"seven_day":null}"#;
        let r = interpret_push(raw, 1_000_000_000);
        assert!(r.snapshot.is_none());
    }

    #[test]
    fn context_only_push_is_present() {
        // API-key auth: no rate_limits at all, but the context block is real
        // data — the old "both windows null ⇒ absent" rule would have dropped it.
        let raw = r#"{"written_at_ms":999999500,"five_hour":null,"seven_day":null,
            "context":{"used_percentage":12.5,"total_input_tokens":25004,"context_window_size":200000}}"#;
        let r = interpret_push(raw, 1_000_000_000);
        let snap = r.snapshot.expect("context-only snapshot is present");
        let ctx = snap.context.expect("context block");
        assert_eq!(ctx.used_percentage, Some(12.5));
        assert!(snap.five_hour.is_none());
        assert!(!r.stale);
    }

    #[test]
    fn context_only_push_ages_like_rate_limits() {
        let raw = r#"{"written_at_ms":0,"context":{"used_percentage":12.5}}"#;
        let stale_now = STALE_AFTER.as_millis() as u64 + 1_000;
        assert!(interpret_push(raw, stale_now).stale);
        let hidden_now = HIDE_AFTER.as_millis() as u64 + 1_000;
        assert!(interpret_push(raw, hidden_now).snapshot.is_none());
    }

    #[test]
    fn metadata_only_context_is_not_substantive() {
        // A context block with no numbers has nothing to render.
        let raw =
            r#"{"written_at_ms":999999500,"context":{"session_name":"refactor","effort":"high"}}"#;
        assert!(interpret_push(raw, 1_000_000_000).snapshot.is_none());
    }

    /// A snapshot with only context numbers (no quota windows), tagged with a
    /// distinguishable `used_percentage` so ownership tests can tell the
    /// writers apart.
    fn context_only(used: f64) -> UsageSnapshot {
        UsageSnapshot {
            context: Some(ContextSnapshot {
                used_percentage: Some(used),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn quota_only(util: f64) -> UsageSnapshot {
        UsageSnapshot {
            five_hour: Some(UsageWindow {
                utilization: util,
                resets_at: None,
            }),
            ..Default::default()
        }
    }

    fn meta(session: &str) -> PushMeta {
        PushMeta {
            session_key: Some(session.to_string()),
            activity: None,
        }
    }

    fn parse(raw: &str) -> PushedUsage {
        serde_json::from_str(raw).expect("push file parses")
    }

    #[test]
    fn quota_push_replaces_quota_and_stamps_it_now() {
        let now = 1_000_000_000;
        let prev = parse(&push_json(now - 60_000));
        let merged = merge_push(Some(prev), &quota_only(5.0), &meta("s1"), now);
        assert_eq!(merged.snapshot.five_hour.unwrap().utilization, 5.0);
        assert_eq!(merged.quota_at_ms, Some(now));
        assert_eq!(merged.format, PUSH_FORMAT);
    }

    #[test]
    fn context_push_carries_quota_forward_with_its_own_instant() {
        // M14: claude-local (no rate_limits) pushing context must neither
        // evict the quota reading nor restamp it as freshly observed.
        let now = 1_000_000_000;
        let quota_at = now - 10_000;
        let prev = parse(&push_json(quota_at));
        let merged = merge_push(Some(prev), &context_only(10.0), &meta("local"), now);
        assert_eq!(merged.snapshot.five_hour.expect("quota kept").utilization, 23.5);
        assert_eq!(merged.quota_at_ms, Some(quota_at));
        assert_eq!(merged.context_at_ms, Some(now));
        // The legacy whole-file instant is the oldest slot, so a format-0
        // reader can only under-show.
        assert_eq!(merged.written_at_ms, quota_at);
    }

    #[test]
    fn context_push_after_stale_after_still_keeps_quota() {
        // The old rule evicted quota wholesale once it passed STALE_AFTER,
        // bypassing the documented 30-minute HIDE_AFTER window.
        let now = 1_000_000_000;
        let quota_at = now - (STALE_AFTER.as_millis() as u64 + 60_000);
        let merged = merge_push(
            Some(parse(&push_json(quota_at))),
            &context_only(10.0),
            &meta("local"),
            now,
        );
        assert!(merged.snapshot.five_hour.is_some());
        assert_eq!(merged.quota_at_ms, Some(quota_at));
        // …and it does expire once past HIDE_AFTER.
        let expired_at = now - (HIDE_AFTER.as_millis() as u64 + 1_000);
        let merged = merge_push(
            Some(parse(&push_json(expired_at))),
            &context_only(10.0),
            &meta("local"),
            now,
        );
        assert!(merged.snapshot.five_hour.is_none());
        assert_eq!(merged.quota_at_ms, None);
    }

    #[test]
    fn the_session_that_keeps_changing_owns_the_context_slot() {
        // M14: an idle `claude` tab re-pushing identical numbers must not take
        // the context bar away from the `claude-local` tab being worked in.
        let mut file = merge_push(None, &context_only(10.0), &meta("idle"), 1_000);
        // The working tab arrives and changes on every beat.
        file = merge_push(Some(file), &context_only(50.0), &meta("work"), 2_000);
        assert_eq!(file.context_owner.as_deref(), Some("work"));
        // The idle tab pushes the same reading it had before: no claim.
        file = merge_push(Some(file), &context_only(10.0), &meta("idle"), 3_000);
        assert_eq!(file.context_owner.as_deref(), Some("work"));
        assert_eq!(
            file.snapshot.context.as_ref().unwrap().used_percentage,
            Some(50.0)
        );
        // The context instant belongs to the owner's last write, not to the
        // idle tab's beat.
        assert_eq!(file.context_at_ms, Some(2_000));
        // The working tab moves again and re-stamps its own slot.
        file = merge_push(Some(file), &context_only(51.0), &meta("work"), 4_000);
        assert_eq!(file.context_at_ms, Some(4_000));
        assert_eq!(
            file.snapshot.context.unwrap().used_percentage,
            Some(51.0)
        );
    }

    #[test]
    fn an_owner_that_stops_pushing_hands_the_slot_over() {
        // Known-idle tab first, so its later beats are "unchanged" rather than
        // first observations.
        let mut file = merge_push(None, &context_only(10.0), &meta("idle"), 1_000);
        file = merge_push(Some(file), &context_only(50.0), &meta("work"), 2_000);
        assert_eq!(file.context_owner.as_deref(), Some("work"));
        // Idle keeps beating with the same reading — no takeover.
        file = merge_push(Some(file), &context_only(10.0), &meta("idle"), 3_000);
        assert_eq!(file.context_owner.as_deref(), Some("work"));
        // The working tab goes away; once its mark ages past HIDE_AFTER the
        // remaining session takes the slot.
        let t = 2_000 + HIDE_AFTER.as_millis() as u64 + 1;
        file = merge_push(Some(file), &context_only(10.0), &meta("idle"), t);
        assert_eq!(file.context_owner.as_deref(), Some("idle"));
        assert_eq!(file.snapshot.context.unwrap().used_percentage, Some(10.0));
    }

    #[test]
    fn activity_counters_count_as_change_even_when_numbers_repeat() {
        let idle = PushMeta {
            session_key: Some("work".into()),
            activity: Some("1000/0.5".into()),
        };
        let busy = PushMeta {
            session_key: Some("work".into()),
            activity: Some("2000/0.9".into()),
        };
        let mut file = merge_push(None, &context_only(50.0), &meta("other"), 1_000);
        file = merge_push(Some(file), &context_only(10.0), &idle, 2_000);
        // Same context numbers, but the cost block moved: still a change, so
        // "work" reclaims the slot over an idle competitor.
        file = merge_push(Some(file), &context_only(60.0), &meta("other"), 3_000);
        file = merge_push(Some(file), &context_only(10.0), &busy, 4_000);
        assert_eq!(file.context_owner.as_deref(), Some("work"));
    }

    #[test]
    fn metadata_only_context_neither_claims_nor_evicts() {
        let mut file = merge_push(None, &context_only(50.0), &meta("work"), 1_000);
        let metadata_only = UsageSnapshot {
            context: Some(ContextSnapshot {
                session_name: Some("other".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        file = merge_push(Some(file), &metadata_only, &meta("other"), 2_000);
        assert_eq!(file.context_owner.as_deref(), Some("work"));
        assert_eq!(
            file.snapshot.context.unwrap().used_percentage,
            Some(50.0)
        );
    }

    #[test]
    fn session_marks_stay_bounded() {
        let mut file = merge_push(None, &context_only(1.0), &meta("s0"), 1_000);
        for i in 1..(MAX_SESSION_MARKS + 4) {
            file = merge_push(
                Some(file),
                &context_only(i as f64),
                &meta(&format!("s{i}")),
                1_000 + i as u64,
            );
        }
        assert_eq!(file.sessions.len(), MAX_SESSION_MARKS);
    }

    #[test]
    fn merging_over_a_pre_m14_file_upgrades_the_format() {
        // A file written before the two-slot split has no per-slot instants:
        // both fall back to `written_at_ms`, and the merge writes format 2.
        let now = 1_000_000_000;
        let legacy = parse(&push_json(now - 20_000));
        assert_eq!(legacy.format, 0);
        assert_eq!(legacy.quota_at(), legacy.written_at_ms);
        assert_eq!(legacy.context_at(), legacy.written_at_ms);
        let merged = merge_push(Some(legacy), &context_only(10.0), &meta("local"), now);
        assert_eq!(merged.format, PUSH_FORMAT);
        assert_eq!(merged.quota_at_ms, Some(now - 20_000));
    }

    #[test]
    fn no_session_key_degrades_to_last_writer_wins() {
        let anon = PushMeta::default();
        let mut file = merge_push(None, &context_only(10.0), &anon, 1_000);
        file = merge_push(Some(file), &context_only(50.0), &anon, 2_000);
        assert_eq!(
            file.snapshot.context.unwrap().used_percentage,
            Some(50.0)
        );
    }

    #[test]
    fn garbage_push_file_is_treated_as_absent() {
        let r = interpret_push("not json", 1_000_000_000);
        assert!(r.snapshot.is_none());
        assert!(!r.stale);
    }

    #[test]
    fn epoch_conversion_seconds_and_milliseconds() {
        // 2025-02-01T16:00:00Z per the docs' example payload.
        assert_eq!(
            epoch_secs_to_iso(1_738_425_600).as_deref(),
            Some("2025-02-01T16:00:00+00:00")
        );
        // Same instant expressed in ms normalizes to the same ISO string.
        assert_eq!(
            epoch_secs_to_iso(1_738_425_600_000).as_deref(),
            Some("2025-02-01T16:00:00+00:00")
        );
    }

    #[test]
    fn push_file_roundtrip() {
        let pushed = PushedUsage {
            written_at_ms: 42,
            format: PUSH_FORMAT,
            quota_at_ms: Some(42),
            context_at_ms: Some(42),
            context_owner: Some("sess-1".into()),
            sessions: vec![SessionMark {
                key: "sess-1".into(),
                sig: "12.5||||20000||||".into(),
                changed_at_ms: 42,
                seen_at_ms: 42,
            }],
            snapshot: UsageSnapshot {
                five_hour: Some(UsageWindow {
                    utilization: 7.0,
                    resets_at: Some("2026-08-04T12:00:00+00:00".into()),
                }),
                seven_day: None,
                context: Some(ContextSnapshot {
                    used_percentage: Some(12.5),
                    cache_read_tokens: Some(20_000),
                    session_name: Some("refactor".into()),
                    ..Default::default()
                }),
            },
        };
        let json = serde_json::to_string(&pushed).unwrap();
        // Flattened shape: snapshot fields sit at the top level.
        assert!(json.contains("\"written_at_ms\":42"));
        assert!(json.contains("\"five_hour\":{"));
        assert!(json.contains("\"context\":{"));
        // Self-describing: a reader can tell the two-slot shape from the
        // pre-M14 one without guessing.
        assert!(json.contains("\"format\":2"));
        assert!(json.contains("\"quota_at_ms\":42"));
        assert!(json.contains("\"context_at_ms\":42"));
        // Absent context fields are omitted rather than written as nulls.
        assert!(!json.contains("\"fast_mode\""));
        let back: PushedUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.written_at_ms, 42);
        assert_eq!(back.snapshot.five_hour.unwrap().utilization, 7.0);
        let ctx = back.snapshot.context.unwrap();
        assert_eq!(ctx.cache_read_tokens, Some(20_000));
        assert_eq!(ctx.session_name.as_deref(), Some("refactor"));
        assert!(ctx.fast_mode.is_none());
    }

    #[test]
    fn pre_nc3_push_file_still_parses() {
        // Files written before the context block existed must keep working.
        let raw = r#"{"written_at_ms":42,"five_hour":{"utilization":7.0,"resets_at":null},"seven_day":null}"#;
        let back: PushedUsage = serde_json::from_str(raw).unwrap();
        assert!(back.snapshot.context.is_none());
        assert_eq!(back.snapshot.five_hour.as_ref().unwrap().utilization, 7.0);
        // …and pre-M14 files (no per-slot instants, no format) age exactly as
        // they used to: one instant for the whole file.
        assert_eq!(back.format, 0);
        assert_eq!(back.quota_at(), 42);
        assert_eq!(back.context_at(), 42);
        assert!(back.sessions.is_empty());
        let r = interpret_push(raw, 42 + STALE_AFTER.as_millis() as u64 + 1_000);
        assert!(r.snapshot.is_some());
        assert!(r.stale && r.quota_stale && !r.context_stale);
    }

    #[test]
    fn slots_age_and_expire_independently() {
        let now = 10_000_000_000;
        let fresh = now - 5_000;
        let old = now - (STALE_AFTER.as_millis() as u64 + 10_000);
        // Fresh context over an aging quota reading: only the quota dims, and
        // the widget as a whole is not "stale" (something on it is live).
        let raw = format!(
            r#"{{"written_at_ms":{old},"format":2,"quota_at_ms":{old},"context_at_ms":{fresh},
                "five_hour":{{"utilization":23.5,"resets_at":null}},
                "context":{{"used_percentage":12.5}}}}"#
        );
        let r = interpret_push(&raw, now);
        let snap = r.snapshot.expect("both slots present");
        assert!(snap.five_hour.is_some() && snap.context.is_some());
        assert!(r.quota_stale);
        assert!(!r.context_stale);
        assert!(!r.stale);

        // Once the quota slot passes HIDE_AFTER it drops out on its own,
        // leaving the live context bar alone.
        let expired = now - (HIDE_AFTER.as_millis() as u64 + 1_000);
        let raw = format!(
            r#"{{"written_at_ms":{expired},"format":2,"quota_at_ms":{expired},"context_at_ms":{fresh},
                "five_hour":{{"utilization":23.5,"resets_at":null}},
                "context":{{"used_percentage":12.5}}}}"#
        );
        let r = interpret_push(&raw, now);
        let snap = r.snapshot.expect("context survives the quota expiry");
        assert!(snap.five_hour.is_none());
        assert_eq!(snap.context.unwrap().used_percentage, Some(12.5));
        assert!(!r.quota_stale && !r.context_stale && !r.stale);
    }
}

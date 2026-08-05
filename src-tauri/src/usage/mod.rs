//! Claude Code subscription usage tracker.
//!
//! The bottom-bar widget shows the same session (5h) / weekly (7d) quota the
//! `/usage` slash command shows. The data arrives via the **status line
//! push**: Claude Code (≥ 2.1.80) includes a `rate_limits` object in the JSON
//! it pipes to the `statusLine` command —
//!
//! ```json
//! { "rate_limits": {
//!     "five_hour": { "used_percentage": 23.5, "resets_at": 1738425600 },
//!     "seven_day": { "used_percentage": 41.2, "resets_at": 1738857600 } } }
//! ```
//!
//! (`used_percentage` is 0–100; `resets_at` is Unix epoch seconds.) Our
//! `cimp --statusline` renderer (see `crate::statusline`) extracts that
//! object on every invocation and persists it to `<exe-dir>/claude-usage-push.json`
//! via [`store_pushed_usage`]; the widget's poll reads it back through
//! [`pushed_usage`]. The injected overlay also sets `statusLine.refreshInterval`
//! so pushes keep flowing while a Claude tab sits idle.
//!
//! This replaces polling the undocumented `api.anthropic.com/api/oauth/usage`
//! endpoint, which allows only a tiny request burst before answering 429 with
//! a multi-minute `Retry-After` — the widget spent most of its life dimmed on
//! cached data. The push costs zero extra requests (Claude Code already has
//! the numbers from its API responses) and uses a documented schema. The old
//! poller is kept, disabled, in [`endpoint_poll`].
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

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::debug;

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
/// raw `serde_json::Value` (see `crate::statusline`), so a reshaped or partial
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

/// Outcome of a usage read, serialized to the frontend. `rate_limited` /
/// `retry_after_secs` are legacy fields from the endpoint-poll era (see
/// [`endpoint_poll`]); the push path never sets them, but the shape is kept so
/// the frontend contract is unchanged. `Default` is the unavailable state
/// (widget hides).
#[derive(Serialize, Clone, Debug, Default)]
pub struct UsageResult {
    /// The snapshot to render. `None` when no push data exists (no Claude tab
    /// has produced one yet, or the last one is too old to be meaningful).
    pub snapshot: Option<UsageSnapshot>,
    /// Legacy: true when the endpoint poller hit a 429. Always false now.
    pub rate_limited: bool,
    /// Legacy: parsed `Retry-After` from a 429. Always `None` now.
    pub retry_after_secs: Option<u64>,
    /// True when `snapshot` is aging — the last push is older than
    /// [`STALE_AFTER`] (the Claude tab likely closed or went quiet). The UI
    /// dims the numbers to signal they may be out of date.
    pub stale: bool,
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

/// On-disk shape of the push file: the snapshot plus the write instant used
/// for staleness. Flattened so the file reads naturally:
/// `{"written_at_ms":…,"five_hour":{…},"seven_day":{…},"context":{…}}`.
/// One instant ages the whole file — parts are never merged across pushes
/// (see [`should_write`]).
#[derive(Serialize, Deserialize, Debug)]
struct PushedUsage {
    /// Unix epoch milliseconds at write time (writer's clock; reader is the
    /// same machine, so skew is not a concern).
    written_at_ms: u64,
    #[serde(flatten)]
    snapshot: UsageSnapshot,
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
/// block: any failure is silently dropped (the next refresh retries within
/// seconds). The write is atomic (unique temp file + rename) because several
/// Claude tabs may push concurrently — last writer wins, and the reader never
/// sees a torn file.
///
/// Quota-less pushes yield to fresh quota-carrying ones — see [`should_write`].
pub fn store_pushed_usage(snapshot: &UsageSnapshot) {
    let Some(path) = push_path() else { return };
    // Only a quota-less push has to look at what it would overwrite; the
    // common case stays a single write.
    if !snapshot.has_rate_limits() {
        let prev = std::fs::read_to_string(&path).ok();
        if !should_write(snapshot, prev.as_deref(), now_ms()) {
            return;
        }
    }
    let pushed = PushedUsage {
        written_at_ms: now_ms(),
        snapshot: snapshot.clone(),
    };
    let Ok(json) = serde_json::to_string(&pushed) else {
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

/// Should this push overwrite the file whose current contents are `prev_raw`?
///
/// The push file holds **one observation**: a single `written_at_ms` ages
/// everything in it, so parts are never merged across pushes (a carried-over
/// quota reading stamped with a new write instant would read as fresh when it
/// is not). The only rule needed on top of "last writer wins" is that a push
/// *without* quota data must not evict a *fresh* push that has it — otherwise
/// `claude-local` (API-key auth, no `rate_limits`) pushing context every 30s
/// would blink the quota widget out between the `claude` tab's pushes.
///
/// So: write unless this is a quota-less push and the file already holds a
/// quota-carrying push younger than [`STALE_AFTER`]. Context data then simply
/// rides the quota tab's own pushes, which carry it too.
fn should_write(new: &UsageSnapshot, prev_raw: Option<&str>, now_ms: u64) -> bool {
    if new.has_rate_limits() {
        return true;
    }
    let Some(prev) = prev_raw.and_then(|raw| serde_json::from_str::<PushedUsage>(raw).ok()) else {
        return true;
    };
    if !prev.snapshot.has_rate_limits() {
        return true;
    }
    Duration::from_millis(now_ms.saturating_sub(prev.written_at_ms)) > STALE_AFTER
}

/// Read the current usage for the widget: the freshest status-line push,
/// aged into fresh / stale / absent. Pure local file read — never touches
/// the network.
pub fn pushed_usage() -> UsageResult {
    let raw = match push_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(r) => r,
        None => {
            debug!("usage: no push file; widget hides");
            return UsageResult::default();
        }
    };
    interpret_push(&raw, now_ms())
}

/// Age a raw push-file payload into a `UsageResult`. Split from
/// [`pushed_usage`] so staleness is unit-testable with an injected clock.
fn interpret_push(raw: &str, now_ms: u64) -> UsageResult {
    let pushed: PushedUsage = match serde_json::from_str(raw) {
        Ok(p) => p,
        Err(e) => {
            debug!(error = %e, "usage: push file unparseable; treating as absent");
            return UsageResult::default();
        }
    };
    // An empty snapshot is absence with extra steps — never render it. A push
    // carrying *only* context numbers (API-key auth has no `rate_limits`) is
    // not empty: it renders the context bar alone.
    if !pushed.snapshot.is_substantive() {
        return UsageResult::default();
    }
    // A write instant in the future (clock adjustment) counts as fresh.
    let age = Duration::from_millis(now_ms.saturating_sub(pushed.written_at_ms));
    if age > HIDE_AFTER {
        debug!(
            age_secs = age.as_secs(),
            "usage: push data expired; widget hides"
        );
        return UsageResult::default();
    }
    UsageResult {
        snapshot: Some(pushed.snapshot),
        rate_limited: false,
        retry_after_secs: None,
        stale: age > STALE_AFTER,
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

// ---- legacy endpoint poller (DISABLED 2026-08-04) ------------------------
//
// The original data source: polling the undocumented
// `GET https://api.anthropic.com/api/oauth/usage` endpoint with the OAuth
// bearer token from `~/.claude/.credentials.json`, plus a last-good on-disk
// cache (`<exe-dir>/usage-cache.json`) served — flagged stale — through the
// endpoint's aggressive 429 rate-limiting. Replaced by the status-line push
// above and no longer called from anywhere; kept compiling (not bit-rotting)
// in case a future feature needs an on-demand pull of account data again.
// To resurrect: call `endpoint_poll::fetch_usage()` from
// `ipc::commands::get_claude_usage` and restore the frontend's 429 backoff
// (see UsageMeter.svelte history around v0.49.x).
#[allow(dead_code)]
mod endpoint_poll {
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use serde::Deserialize;
    use tracing::{debug, warn};

    use super::{UsageResult, UsageSnapshot, UsageWindow};

    const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
    const OAUTH_BETA: &str = "oauth-2025-04-20";
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// Reusable HTTP client for the usage poll. Built once and shared so we
    /// don't spin up a fresh connection pool / TLS config on every poll tick.
    /// The bearer token is supplied per-request, so the client itself is
    /// stateless and safe to reuse.
    fn usage_client() -> &'static reqwest::Client {
        static USAGE_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        USAGE_CLIENT.get_or_init(|| {
            reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|e| {
                    warn!(error = %e, "usage: failed to build HTTP client; using default");
                    reqwest::Client::new()
                })
        })
    }

    /// Raw shape of the endpoint response — only the two fields we consume.
    /// `#[serde(default)]` so missing windows deserialize to `None` rather
    /// than failing the whole parse if the endpoint shape shifts.
    #[derive(Deserialize, Default)]
    struct UsageResponse {
        #[serde(default)]
        five_hour: Option<UsageWindow>,
        #[serde(default)]
        seven_day: Option<UsageWindow>,
    }

    // The endpoint allows a small burst, then 429s with a multi-minute
    // `Retry-After`. Without a fallback the widget shows bare placeholders
    // whenever a poll is throttled — so the last successful snapshot is kept
    // in memory and on disk (`<exe-dir>/usage-cache.json`) and served,
    // flagged stale, on any non-200 that isn't a clean logged-out.

    /// `<exe-dir>/usage-cache.json` — the poller's last-good persistence.
    fn cache_path() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        Some(exe.parent()?.join("usage-cache.json"))
    }

    /// Process-wide last-good snapshot, lazily hydrated from disk.
    fn cache_slot() -> &'static Mutex<Option<UsageSnapshot>> {
        static CACHE: OnceLock<Mutex<Option<UsageSnapshot>>> = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(load_cache_from_disk()))
    }

    fn load_cache_from_disk() -> Option<UsageSnapshot> {
        let path = cache_path()?;
        let raw = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn cached_snapshot() -> Option<UsageSnapshot> {
        cache_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn store_snapshot(snapshot: &UsageSnapshot) {
        *cache_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(snapshot.clone());
        if let (Some(path), Ok(json)) = (cache_path(), serde_json::to_string(snapshot)) {
            if let Err(e) = std::fs::write(&path, json) {
                debug!(error = %e, "usage: failed to persist last-good snapshot");
            }
        }
    }

    /// Build the "serve the cached snapshot, flagged stale" result for a 429
    /// / transient failure. When there's no cache yet (cold start),
    /// `snapshot` is `None` and `stale` is false.
    fn stale_result(rate_limited: bool, retry_after_secs: Option<u64>) -> UsageResult {
        let snapshot = cached_snapshot();
        let stale = snapshot.is_some();
        UsageResult {
            snapshot,
            rate_limited,
            retry_after_secs,
            stale,
        }
    }

    /// Credentials file path: `<home>/.claude/.credentials.json`. Resolves
    /// the home dir from `USERPROFILE` on Windows (falling back to
    /// `HOMEDRIVE`+`HOMEPATH`), `HOME` elsewhere.
    fn credentials_path() -> Option<PathBuf> {
        let home = if cfg!(windows) {
            std::env::var_os("USERPROFILE").or_else(|| {
                match (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH")) {
                    (Some(drive), Some(path)) => {
                        let mut h = drive;
                        h.push(path);
                        Some(h)
                    }
                    _ => None,
                }
            })
        } else {
            std::env::var_os("HOME")
        }?;
        Some(
            PathBuf::from(home)
                .join(".claude")
                .join(".credentials.json"),
        )
    }

    /// Read the OAuth access token from the credentials file. The token is
    /// refreshed by Claude Code itself, so it is re-read on every fetch
    /// rather than cached. `None` on any failure (file missing, not logged
    /// in, malformed JSON).
    fn read_access_token() -> Option<String> {
        let path = credentials_path()?;
        let raw = std::fs::read_to_string(&path).ok()?;
        let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
        parsed
            .get("claudeAiOauth")
            .and_then(|o| o.get("accessToken"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
    }

    /// Fetch the current usage from the OAuth endpoint. Returns:
    ///   - `snapshot: Some` on a successful 200.
    ///   - `rate_limited: true` (+ optional `retry_after_secs`) on a 429 —
    ///     the caller keeps the widget visible and waits the cooldown.
    ///   - the default "unavailable" state on no-token / network error /
    ///     other non-2xx / parse failure.
    pub async fn fetch_usage() -> UsageResult {
        let token = match read_access_token() {
            Some(t) => t,
            None => {
                debug!("usage: no Claude OAuth token; skipping fetch");
                return UsageResult::default();
            }
        };

        let client = usage_client();

        let resp = match client
            .get(USAGE_URL)
            .bearer_auth(&token)
            .header("anthropic-beta", OAUTH_BETA)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // Transient network/transport error — keep the last-good.
                debug!(error = %e, "usage: request failed");
                return stale_result(false, None);
            }
        };

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Honor Retry-After (delta-seconds form) when present; the
            // HTTP-date form parses as None and the caller falls back to its
            // own cooldown.
            let retry_after_secs = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok());
            debug!(?retry_after_secs, "usage: 429 rate limited");
            return stale_result(true, retry_after_secs);
        }

        if !status.is_success() {
            // 401 = stale token (Claude Code refreshes it), anything else =
            // endpoint hiccup — both transient, so keep the last-good.
            debug!(%status, "usage: non-success response");
            return stale_result(false, None);
        }

        match resp.json::<UsageResponse>().await {
            Ok(parsed) => {
                debug!(
                    five_hour = parsed.five_hour.as_ref().map(|w| w.utilization),
                    seven_day = parsed.seven_day.as_ref().map(|w| w.utilization),
                    "usage: fetched"
                );
                let snapshot = UsageSnapshot {
                    five_hour: parsed.five_hour,
                    seven_day: parsed.seven_day,
                    // The endpoint only ever served quota windows; context
                    // data exists on the status-line path alone.
                    context: None,
                };
                store_snapshot(&snapshot);
                UsageResult {
                    snapshot: Some(snapshot),
                    rate_limited: false,
                    retry_after_secs: None,
                    stale: false,
                }
            }
            Err(e) => {
                warn!(error = %e, "usage: response parse failed");
                stale_result(false, None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!r.rate_limited);
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

    /// A snapshot with only context numbers (no quota windows).
    fn context_only() -> UsageSnapshot {
        UsageSnapshot {
            context: Some(ContextSnapshot {
                used_percentage: Some(10.0),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn quota_push_always_writes() {
        let with_quota = UsageSnapshot {
            five_hour: Some(UsageWindow {
                utilization: 5.0,
                resets_at: None,
            }),
            ..Default::default()
        };
        // Even over a fresh existing quota push.
        assert!(should_write(
            &with_quota,
            Some(&push_json(1_000_000_000)),
            1_000_000_000
        ));
    }

    #[test]
    fn context_only_push_yields_to_a_fresh_quota_push() {
        // claude-local (no rate_limits) must not blink the quota widget out
        // between the claude tab's pushes.
        let now = 1_000_000_000;
        assert!(!should_write(
            &context_only(),
            Some(&push_json(now - 10_000)),
            now
        ));
    }

    #[test]
    fn context_only_push_writes_when_nothing_fresh_to_protect() {
        let now = 1_000_000_000;
        // No file yet.
        assert!(should_write(&context_only(), None, now));
        // Unparseable file.
        assert!(should_write(&context_only(), Some("not json"), now));
        // Existing push has no quota data of its own.
        let quota_less = r#"{"written_at_ms":999999500,"context":{"used_percentage":1.0}}"#;
        assert!(should_write(&context_only(), Some(quota_less), now));
        // Existing quota push has gone stale.
        let stale = push_json(now - STALE_AFTER.as_millis() as u64 - 1_000);
        assert!(should_write(&context_only(), Some(&stale), now));
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
        assert_eq!(back.snapshot.five_hour.unwrap().utilization, 7.0);
    }
}

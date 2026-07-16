//! Claude Code subscription usage tracker.
//!
//! Claude Code (run inside this app) authenticates with an OAuth token stored
//! in `~/.claude/.credentials.json`. There is an **undocumented** Anthropic
//! endpoint that returns the same session (5h) / weekly (7d) quota the
//! `/usage` slash command shows:
//!
//! ```text
//! GET https://api.anthropic.com/api/oauth/usage
//! Authorization: Bearer <claudeAiOauth.accessToken>
//! anthropic-beta: oauth-2025-04-20
//! ```
//!
//! Response (fields we don't use omitted):
//! ```json
//! { "five_hour": { "utilization": 7.0, "resets_at": "2026-..." },
//!   "seven_day": { "utilization": 1.0, "resets_at": "2026-..." } }
//! ```
//!
//! `utilization` is a 0–100 percentage; `resets_at` is ISO-8601 with timezone.
//!
//! Caveats:
//!   - Undocumented endpoint; it may change or disappear without notice.
//!   - The token is refreshed by Claude Code itself while it runs in this app,
//!     so we re-read the credentials file on every fetch rather than caching
//!     the token. When the user is logged out / the token is stale the endpoint
//!     returns a non-200 and we return `None` so the UI hides the widget.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Reusable HTTP client for the usage poll. Built once and shared so we don't
/// spin up a fresh connection pool / TLS config on every poll tick (the widget
/// polls on an interval for the whole session). The bearer token is supplied
/// per-request, so the client itself is stateless and safe to reuse.
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

/// One quota window: how much of the limit is used and when it resets.
/// `utilization` is 0–100; `resets_at` is an ISO-8601 timestamp (with tz) or
/// `null` (the endpoint reports null resets for windows at 0%).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UsageWindow {
    pub utilization: f64,
    pub resets_at: Option<String>,
}

/// The session (5h) and weekly (7d) windows the UI renders. Re-serialized to
/// the frontend; the other fields the endpoint returns (`seven_day_sonnet`,
/// `extra_usage`, …) are intentionally dropped. `Deserialize` is derived too
/// so a last-good snapshot can be re-hydrated from the on-disk cache.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UsageSnapshot {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
}

/// Outcome of a usage fetch. Distinguishes a rate-limit (429, transient — keep
/// the widget visible and retry after the server's cooldown) from a genuine
/// unavailable (no token / network error — the widget hides if it never had
/// data). `Default` is the unavailable state.
#[derive(Serialize, Clone, Debug, Default)]
pub struct UsageResult {
    /// The snapshot to render. On a 200 it's the freshly fetched one; on a
    /// 429 / transient failure it's the persisted last-good snapshot (with
    /// `stale` set) so the widget keeps showing real numbers instead of
    /// reverting to placeholders. `None` only when we've never had a good
    /// read (cold start) or the user is logged out.
    pub snapshot: Option<UsageSnapshot>,
    /// True when the endpoint returned 429 Too Many Requests.
    pub rate_limited: bool,
    /// Parsed `Retry-After` (whole seconds) from a 429, when the server sent
    /// the delta-seconds form. `None` → caller uses its own cooldown.
    pub retry_after_secs: Option<u64>,
    /// True when `snapshot` is the cached last-good rather than a fresh read —
    /// the endpoint was rate-limited or transiently unavailable. The UI dims
    /// the numbers to signal they may be out of date.
    pub stale: bool,
}

/// Raw shape of the endpoint response — only the two fields we consume.
/// `#[serde(default)]` so missing windows deserialize to `None` rather than
/// failing the whole parse if the endpoint shape shifts.
#[derive(Deserialize, Default)]
struct UsageResponse {
    #[serde(default)]
    five_hour: Option<UsageWindow>,
    #[serde(default)]
    seven_day: Option<UsageWindow>,
}

// ---- last-good snapshot cache -------------------------------------------
//
// The usage endpoint is aggressively rate-limited (a small burst budget, then
// 429 with a multi-minute `Retry-After`). Without a fallback the widget shows
// bare placeholders whenever a poll is throttled — and a cold start that keeps
// losing the burst budget never escapes that state. We therefore keep the last
// successful snapshot both in memory and on disk (`<exe-dir>/usage-cache.json`)
// so a restart shows real (stale) numbers immediately, and serve it — flagged
// stale — on any non-200 that isn't a clean logged-out.

/// `<exe-dir>/usage-cache.json` — sits next to the portable `settings.json`.
/// `None` when `current_exe()` can't be resolved (the cache is then skipped).
fn cache_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("usage-cache.json"))
}

/// Process-wide last-good snapshot, lazily hydrated from disk on first access.
fn cache_slot() -> &'static Mutex<Option<UsageSnapshot>> {
    static CACHE: OnceLock<Mutex<Option<UsageSnapshot>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(load_cache_from_disk()))
}

/// Read the persisted snapshot from disk. `None` on any failure (absent file,
/// malformed JSON) — the cache simply starts empty.
fn load_cache_from_disk() -> Option<UsageSnapshot> {
    let path = cache_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Return a clone of the cached last-good snapshot, if any (hydrating from disk
/// on first call). The lock is only poisoned if a holder panicked while writing;
/// we recover the guard rather than propagate, since a stale read is harmless.
fn cached_snapshot() -> Option<UsageSnapshot> {
    cache_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Store a freshly fetched snapshot as the new last-good, in memory and on
/// disk. Disk write failures are logged but never surfaced — the in-memory
/// copy still serves this session.
fn store_snapshot(snapshot: &UsageSnapshot) {
    *cache_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(snapshot.clone());
    if let (Some(path), Ok(json)) = (cache_path(), serde_json::to_string(snapshot)) {
        if let Err(e) = std::fs::write(&path, json) {
            debug!(error = %e, "usage: failed to persist last-good snapshot");
        }
    }
}

/// Build the "serve the cached snapshot, flagged stale" result for a 429 /
/// transient failure. When there's no cache yet (cold start), `snapshot` is
/// `None` and `stale` is false — the widget falls back to placeholders.
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

/// Credentials file path: `<home>/.claude/.credentials.json`. Resolves the
/// home dir from `USERPROFILE` on Windows (falling back to `HOMEDRIVE`+
/// `HOMEPATH`, which some domain/enterprise profiles set instead), `HOME`
/// elsewhere.
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

/// Read the OAuth access token from the credentials file. Returns `None` on
/// any failure (file missing, not logged in, malformed JSON) — the caller
/// treats that as "no usage to show".
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

/// Fetch the current usage. Returns a `UsageResult`:
///   - `snapshot: Some` on a successful 200.
///   - `rate_limited: true` (+ optional `retry_after_secs`) on a 429 — the
///     caller keeps the widget visible and waits the server's cooldown.
///   - the default (all-`None`/false) "unavailable" state on no-token / network
///     error / other non-2xx / parse failure — the caller hides the widget if
///     it never had data.
///
/// Failures are logged for diagnostics but never surfaced as errors.
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
            // Transient network/transport error — keep the last-good on screen.
            debug!(error = %e, "usage: request failed");
            return stale_result(false, None);
        }
    };

    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Honor Retry-After (delta-seconds form) when present; the HTTP-date
        // form parses as None and the caller falls back to its own cooldown.
        let retry_after_secs = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        debug!(?retry_after_secs, "usage: 429 rate limited");
        // Serve the last-good snapshot (flagged stale) so the widget keeps
        // showing real numbers through the cooldown instead of placeholders.
        return stale_result(true, retry_after_secs);
    }

    if !status.is_success() {
        // 401 = stale token (Claude Code refreshes it), anything else = endpoint
        // hiccup — both transient, so keep the last-good on screen.
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
            };
            // Persist as the new last-good for future 429s / restarts.
            store_snapshot(&snapshot);
            UsageResult {
                snapshot: Some(snapshot),
                rate_limited: false,
                retry_after_secs: None,
                stale: false,
            }
        }
        Err(e) => {
            // Unexpected shape — treat as a transient hiccup and keep last-good.
            warn!(error = %e, "usage: response parse failed");
            stale_result(false, None)
        }
    }
}

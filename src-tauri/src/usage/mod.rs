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
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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
/// `extra_usage`, …) are intentionally dropped.
#[derive(Serialize, Clone, Debug)]
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
    /// Present on a successful (200) fetch.
    pub snapshot: Option<UsageSnapshot>,
    /// True when the endpoint returned 429 Too Many Requests.
    pub rate_limited: bool,
    /// Parsed `Retry-After` (whole seconds) from a 429, when the server sent
    /// the delta-seconds form. `None` → caller uses its own cooldown.
    pub retry_after_secs: Option<u64>,
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

/// Credentials file path: `<home>/.claude/.credentials.json`. Resolves the
/// home dir from `USERPROFILE` on Windows, `HOME` elsewhere.
fn credentials_path() -> Option<PathBuf> {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    }?;
    Some(PathBuf::from(home).join(".claude").join(".credentials.json"))
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
/// Failures are logged for diagnostics but never surfaced as errors.
pub async fn fetch_usage() -> UsageResult {
    let token = match read_access_token() {
        Some(t) => t,
        None => {
            debug!("usage: no Claude OAuth token; skipping fetch");
            return UsageResult::default();
        }
    };

    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "usage: failed to build HTTP client");
            return UsageResult::default();
        }
    };

    let resp = match client
        .get(USAGE_URL)
        .bearer_auth(&token)
        .header("anthropic-beta", OAUTH_BETA)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            debug!(error = %e, "usage: request failed");
            return UsageResult::default();
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
        return UsageResult {
            snapshot: None,
            rate_limited: true,
            retry_after_secs,
        };
    }

    if !status.is_success() {
        // 401 = logged out / stale token; anything else = endpoint hiccup.
        debug!(%status, "usage: non-success response");
        return UsageResult::default();
    }

    match resp.json::<UsageResponse>().await {
        Ok(parsed) => {
            debug!(
                five_hour = parsed.five_hour.as_ref().map(|w| w.utilization),
                seven_day = parsed.seven_day.as_ref().map(|w| w.utilization),
                "usage: fetched"
            );
            UsageResult {
                snapshot: Some(UsageSnapshot {
                    five_hour: parsed.five_hour,
                    seven_day: parsed.seven_day,
                }),
                rate_limited: false,
                retry_after_secs: None,
            }
        }
        Err(e) => {
            warn!(error = %e, "usage: response parse failed");
            UsageResult::default()
        }
    }
}

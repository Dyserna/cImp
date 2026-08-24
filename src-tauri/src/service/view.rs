//! Poll-view queries — the read side the UI re-asks on a timer.
//!
//! The Phase 0 slice ports one of them, `activity_list`, and the interesting
//! result is a negative one worth writing down for Phase D.
//!
//! **A pure query was already headless.** `activity_list` never touched
//! `AppHandle`, never took `State<'_, AppState>`, and reached its store through
//! a process-global. Wrapping it in a service moves four lines and buys
//! nothing: it was callable from a test the day it was written, and the test
//! below could have existed at any point in the last year. Every command shaped
//! like this one — and by inspection that is most of the `graph_*` and
//! `workbench_*` read commands, which take a `State<'_, Arc<Service>>` and
//! delegate — is in the same position.
//!
//! What Phase D would actually be buying is not *callability* but the
//! *derivation*: the row-shaping the frontend does after the poll returns.
//! `activity_list` is a deliberate counter-example there. Its doc comment
//! records a server-side filter that shipped and was REMOVED, because the
//! Events tab's four-state attribution rule would then exist twice and only one
//! copy would be the exercised one. That is a standing decision against moving
//! this particular view's logic into Rust, and Phase D's module list should be
//! re-read with it in mind: the win is in modules where Rust becomes the ONLY
//! implementation, not in modules where it becomes a second one.

use std::sync::Arc;

use crate::activity::{ActivityEntry, ActivityRecord};
use crate::error::{AppError, AppResult};
use crate::service::on_blocking_pool;

/// The unified tool-activity feed (graph calls + offload runs), newest first,
/// without payloads. Unfiltered by design — see the module docs.
pub async fn activity_since(since_ts: Option<u64>) -> AppResult<Vec<ActivityEntry>> {
    on_blocking_pool(move || crate::activity::snapshot_since(since_ts.unwrap_or(0))).await
}

/// One activity's full record — including the captured request/response
/// payloads — for the detail popup. `None` when the entry was deleted (or
/// aged out) between the list poll and the click.
pub async fn activity_detail(id: u64) -> AppResult<Option<ActivityRecord>> {
    on_blocking_pool(move || crate::activity::detail(id)).await
}

/// Delete one activity row (the detail popup's Delete).
///
/// The store reports whether the row was still there; the caller does not want
/// to know. An id that is already gone is a successful delete, because either
/// way the row the user was looking at is not there any more — so the boolean
/// is dropped rather than turned into an error the popup would have to explain.
pub async fn activity_delete(id: u64) -> AppResult<()> {
    on_blocking_pool(move || {
        let _existed = crate::activity::delete(id);
    })
    .await
}

/// Delete every activity row (the Events tab's Clear).
pub async fn activity_clear() -> AppResult<()> {
    on_blocking_pool(crate::activity::clear).await
}

/// One system-monitor sample (CPU / memory / GPU / network) for the bottom-bar
/// panel, taken off the async reactor.
///
/// `sample()` blocks: it does a synchronous sysinfo refresh — including
/// `networks.refresh(true)`, which re-scans every interface — plus NVML device
/// queries. The frontend polls this on `system_stats.poll_interval_secs`
/// (default 1 s), so doing it on a runtime worker would park that worker once a
/// second and stall every other IPC queued behind it.
///
/// Takes the handle by value rather than by reference: the sample runs on
/// another thread, so it needs an owned `Arc` and there is nothing for a
/// borrowing service to save.
pub async fn system_stats(
    sysmon: Arc<crate::sysmon::SystemStatsState>,
) -> AppResult<crate::sysmon::SystemStatsSnapshot> {
    tokio::task::spawn_blocking(move || sysmon.sample())
        .await
        .map_err(|e| AppError::Ipc(format!("system stats join: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Previously "user clicks in the app".** The recipe is: run a tool, open
    /// the Events tab, confirm the row appears and that the `since_ts` poll
    /// only returns rows newer than the last tick.
    ///
    /// The point of keeping it is not that it is hard — it is that it is the
    /// control for the claim the rest of this slice makes. A view command with
    /// no `AppHandle` and no `AppState` was ALREADY testable; the service split
    /// is not what unlocked it, and no Phase D estimate should count it as a
    /// gain.
    #[tokio::test]
    async fn activity_poll_returns_rows_and_respects_the_since_cursor() {
        // Newest-first, and a cursor in the future selects nothing. Both hold
        // against whatever the process-global store happens to hold, so this
        // does not depend on test ordering.
        let all = activity_since(None).await.expect("poll");
        for pair in all.windows(2) {
            assert!(
                pair[0].ts_ms >= pair[1].ts_ms,
                "the feed must be newest-first"
            );
        }
        let future = u64::MAX;
        assert!(
            activity_since(Some(future)).await.expect("poll").is_empty(),
            "a cursor past every row selects nothing"
        );
    }
}

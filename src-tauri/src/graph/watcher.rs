//! V9-01 Phase D — the per-root filesystem watcher that keeps a built graph
//! live. A full rebuild ([`super::service::GraphService::spawn_rebuild`]) is a
//! one-shot snapshot; without this the index goes stale the moment a file is
//! edited. `notify` (ReadDirectoryChangesW on Windows) feeds raw events into a
//! debounce loop that coalesces a burst of saves into one incremental
//! re-index, which the service applies file-by-file (re-parse on
//! create/modify, drop rows on delete).
//!
//! The watcher handle is owned by the service (kept alive in its `watchers`
//! map); dropping it stops the OS watch, which disconnects the channel and
//! ends the debounce thread — that's the teardown path on shutdown.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::{EventKind, RecursiveMode, Watcher};
use tracing::{debug, info};

use super::service::GraphService;

/// Begin watching `root` recursively. Returns the live watcher handle, which
/// the caller must keep alive for the watch to continue. A dedicated thread
/// debounces events by `debounce` and hands each coalesced batch to
/// [`GraphService::reindex_paths`].
pub fn start(
    service: Arc<GraphService>,
    root: PathBuf,
    debounce: Duration,
) -> notify::Result<notify::RecommendedWatcher> {
    // F16: BOUNDED channel so a flood of fs events (a git checkout, a build, a
    // formatter) can't grow the queue without limit while a slow re-index holds
    // the debounce thread below. Pure-access and all-`.git` events — the dominant
    // flood source (a checkout/gc churns thousands of unindexable `.git` objects)
    // — are dropped in the callback so they never consume capacity. On a genuine
    // overflow we drop the event but flag it, and the debounce thread recovers
    // correctness with a full (idempotent) rebuild rather than a silently-missed
    // change.
    const CHANNEL_CAP: usize = 4096;
    let (tx, rx) = sync_channel::<notify::Result<notify::Event>>(CHANNEL_CAP);
    let overflow = Arc::new(AtomicBool::new(false));
    let cb_overflow = overflow.clone();
    let store_subdir = service.db_subdir();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = &res {
            // Drop events that can never change indexed content, at the source.
            if matches!(ev.kind, EventKind::Access(_)) {
                return;
            }
            // Drop events whose every path lies inside a high-churn build/VCS
            // directory that's never indexed. Filtering HERE (not only later in
            // `reindex_paths`, which respects gitignore) keeps a `cargo build`
            // or `npm install` writing thousands of files from flooding the
            // bounded channel and forcing back-to-back full rebuilds mid-build.
            // The deliberately minimal hot-path subset (see the const's docs):
            // skipping build/vendor dirs here could drop events for source that
            // legitimately lives under them, so this is NOT the broad SKIP_DIRS.
            if all_paths_skippable(&ev.paths, &store_subdir) {
                return;
            }
        }
        match tx.try_send(res) {
            Ok(()) => {}
            // Full: record it so the debounce thread does a full rebuild.
            Err(TrySendError::Full(_)) => cb_overflow.store(true, Ordering::Relaxed),
            // Disconnected: the debounce thread has exited; drop it.
            Err(TrySendError::Disconnected(_)) => {}
        }
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    // A continuous stream of events (a build writing artifacts, a formatter, a
    // git checkout) would otherwise reset the debounce timer forever and starve
    // the re-index. Cap any one batch's age so a never-quiet tree still flushes.
    // The upper clamp matters: without it a large debounce (e.g. 60s) would push
    // the forced-flush interval to ~40min, leaving the graph stale for an entire
    // build session.
    let max_batch = debounce
        .saturating_mul(40)
        .clamp(Duration::from_secs(2), Duration::from_secs(30));

    std::thread::Builder::new()
        .name("cimp-graph-watch".into())
        .spawn(move || {
            // Block for the first event of a batch. `Err` means every sender
            // dropped (the watcher was torn down) — exit the thread.
            while let Ok(first) = rx.recv() {
                let batch_start = Instant::now();
                let mut paths: HashSet<PathBuf> = HashSet::new();
                collect(first, &mut paths);

                // Drain follow-up events until the tree has been quiet for
                // `debounce`, coalescing a burst of saves into one re-index —
                // but force a flush once the batch has run for `max_batch` so a
                // never-quiet stream can't pin re-indexing indefinitely.
                loop {
                    match rx.recv_timeout(debounce) {
                        Ok(ev) => {
                            collect(ev, &mut paths);
                            if batch_start.elapsed() >= max_batch {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }

                // If the channel overflowed while we were busy, some events were
                // dropped — recover with a full (idempotent) rebuild rather than
                // risk a silently stale index. The rebuild covers everything, so
                // the incremental batch is redundant this round.
                if overflow.swap(false, Ordering::Relaxed) {
                    info!(root = %root.display(), "graph: watch channel overflowed — full rebuild to recover");
                    // Recovery, not a user request: no session-push notice.
                    service.spawn_rebuild(
                        root.clone(),
                        super::service::RebuildOrigin::Automatic,
                    );
                } else if !paths.is_empty() {
                    service.reindex_paths(&root, paths.into_iter().collect());
                }
            }
        })
        // Don't panic the process on thread exhaustion — surface it as a watch
        // error so the caller can degrade to no live re-indexing.
        .map_err(|e| notify::Error::generic(&format!("spawn graph watch thread: {e}")))?;

    Ok(watcher)
}

/// True when every path in the event lies inside a directory the watcher must
/// never react to: the hot-path skip set ([`crate::fsutil::WATCH_SKIP_DIRS`])
/// or the graph's own store subdir (`.cimp` by default). The store filter is
/// load-bearing, not an optimization: a full rebuild commits one transaction
/// per indexed file into `<root>/<subdir>/graph.db`, and SQLite's journal
/// create/write/delete cycle turns that into thousands of fs events *inside
/// the watched root*. On a large project those events overflow the bounded
/// channel while the debounce thread sits blocked on the store write-lock the
/// rebuild itself holds — and the overflow "recovery" then spawns the next
/// full rebuild, whose writes overflow the channel again: an endless
/// `building` loop. (`reindex_paths` skipping store paths later can't help;
/// the overflow fires before any batch-level filtering.)
fn all_paths_skippable(paths: &[PathBuf], store_subdir: &str) -> bool {
    !paths.is_empty()
        && paths.iter().all(|p| {
            p.components().any(|c| {
                let name = c.as_os_str().to_str().unwrap_or("");
                name == store_subdir || crate::fsutil::WATCH_SKIP_DIRS.contains(&name)
            })
        })
}

/// Fold one fs event's paths into the pending set, skipping pure access
/// (open/read) events that don't change content.
fn collect(res: notify::Result<notify::Event>, into: &mut HashSet<PathBuf>) {
    match res {
        Ok(ev) => {
            if matches!(ev.kind, EventKind::Access(_)) {
                return;
            }
            for p in ev.paths {
                // Skip VCS internals up front: a checkout/commit/gc churns
                // thousands of `.git` objects that can never be indexed, and
                // letting them into the batch just bloats the set the service
                // must walk and filter. (`target/`, `node_modules/`, etc. are
                // still handled by the gitignore pass in `reindex_paths`.)
                if p.components().any(|c| c.as_os_str() == ".git") {
                    continue;
                }
                into.insert(p);
            }
        }
        Err(e) => debug!(error = %e, "graph: watch event error (ignored)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A watched path under a project root, spelled the way the host platform
    /// spells one: `P:\proj\<tail>` on Windows, `/proj/<tail>` elsewhere.
    ///
    /// This has to be platform-shaped because [`all_paths_skippable`] works on
    /// `Path::components()`, and a hard-coded `P:\proj\.cimp\graph.db` is a
    /// SINGLE `Normal` component on Linux — no `.cimp` component exists in it,
    /// so every `assert!(all_paths_skippable(..))` below would invert while the
    /// `assert!(!...)` ones passed vacuously. The bug the assertions guard
    /// (store writes re-triggering their own rebuild forever) is not
    /// platform-specific, so the coverage should not be either.
    fn p(tail: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"P:\proj\{}", tail.replace('/', "\\")))
        } else {
            PathBuf::from(format!("/proj/{tail}"))
        }
    }

    /// Regression (2026-07-14): the graph store's own writes must be dropped at
    /// the callback. An 885-file project's rebuild emitted >4096 journal events
    /// under `.cimp\`, overflowed the channel, and re-triggered a full rebuild
    /// forever ("stuck in building").
    #[test]
    fn store_subdir_events_are_skippable() {
        assert!(all_paths_skippable(
            &[p(".cimp/graph.db"), p(".cimp/graph.db-journal")],
            ".cimp"
        ));
        // A renamed subdir setting is honored.
        assert!(all_paths_skippable(&[p(".mygraph/graph.db")], ".mygraph"));
        // ...and the default name is NOT special-cased once renamed.
        assert!(!all_paths_skippable(&[p(".cimp/graph.db")], ".mygraph"));
    }

    #[test]
    fn source_and_mixed_events_are_kept() {
        // Plain source edit: must reach the channel.
        assert!(!all_paths_skippable(&[p("src/main.rs")], ".cimp"));
        // Mixed event (rename across a skip boundary): keep it — one indexable
        // path makes the event relevant.
        assert!(!all_paths_skippable(
            &[p(".cimp/graph.db"), p("src/lib.rs")],
            ".cimp"
        ));
        // Empty path list carries no skippable evidence.
        assert!(!all_paths_skippable(&[], ".cimp"));
    }

    #[test]
    fn hot_path_skip_dirs_still_apply() {
        assert!(all_paths_skippable(&[p(".git/objects/ab/cd")], ".cimp"));
        assert!(all_paths_skippable(&[p("target/debug/foo.d")], ".cimp"));
        assert!(all_paths_skippable(&[p("node_modules/x/y.js")], ".cimp"));
        // The broad SKIP_DIRS entries deliberately do NOT apply here.
        assert!(!all_paths_skippable(&[p("dist/app.js")], ".cimp"));
    }
}

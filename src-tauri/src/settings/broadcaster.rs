//! In-memory store + broadcast channel + debounced save.
//!
//! `SettingsHandle` is a cheap-to-clone value (one Arc for the current state,
//! one for the broadcast Sender, one for the save signal). Each subsystem
//! that needs live updates clones it and calls `subscribe()` to get a
//! `broadcast::Receiver<Settings>`; on `set` the new full `Settings` struct
//! is sent to every receiver. Saves are coalesced so a slider drag doesn't
//! hammer the disk.
//!
//! Persistence is layered: a global baseline lives at `<exe-dir>/settings.json`
//! and the saver writes a diff against that baseline to the launch-dir
//! overlay file (`.cimp/config.json`). The handle keeps the global
//! snapshot resolved at startup so `set()` can compute diffs without
//! re-reading from disk; the launch_cwd is captured in `main` and threaded
//! through `init`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use crate::settings::persistence;
use crate::settings::schema::Settings;

const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// Bumped from 16 to 64 to give slow subscribers more headroom during
/// rapid edits (slider drags, theme picker, multi-keystroke compose
/// changes that touch settings). Subscribers that lag past the buffer
/// receive `RecvError::Lagged` and must call `current()` to resync —
/// the broadcaster's contract is "every receiver always sees the latest
/// state eventually," not "every receiver sees every intermediate state."
const BROADCAST_CAPACITY: usize = 64;

#[derive(Clone)]
pub struct SettingsHandle {
    inner: Arc<Mutex<Settings>>,
    tx: broadcast::Sender<Settings>,
    save_tx: mpsc::UnboundedSender<()>,
    // The global BASELINE every overlay diff is computed against. Resolved
    // from disk at startup; shared-mutable (`mutate_global`) because the
    // explicit "save/load global" flows rewrite the physical global file
    // mid-session and the baseline must track it — otherwise the next diff
    // re-pins the promoted values into the project overlay.
    global: Arc<Mutex<Settings>>,
    launch_cwd: Arc<PathBuf>,
    // Serializes (snapshot-read + persistence::save) across the debounced
    // saver and `flush()`. Both hold this across the READ as well as the
    // write, so whichever writer runs second is guaranteed to persist a
    // state at least as new as the first — without it, a saver already
    // mid-write with an older snapshot could complete its atomic rename
    // AFTER a shutdown `flush()` and clobber the newer data with stale.
    save_lock: Arc<Mutex<()>>,
}

impl SettingsHandle {
    pub fn new(initial: Settings, global: Settings, launch_cwd: PathBuf) -> Self {
        let (tx, _) = broadcast::channel::<Settings>(BROADCAST_CAPACITY);
        let (save_tx, save_rx) = mpsc::unbounded_channel::<()>();
        let inner = Arc::new(Mutex::new(initial));
        let save_lock = Arc::new(Mutex::new(()));
        let global = Arc::new(Mutex::new(global));

        spawn_saver(
            inner.clone(),
            save_rx,
            global.clone(),
            launch_cwd.clone(),
            save_lock.clone(),
        );

        Self {
            inner,
            tx,
            save_tx,
            global,
            launch_cwd: Arc::new(launch_cwd),
            save_lock,
        }
    }

    pub fn current(&self) -> Settings {
        // Recover from poisoning rather than panicking. A poisoned mutex
        // means an earlier panic occurred while the lock was held; the
        // inner Settings is still valid and cloning it lets the app keep
        // running. Panicking here cascades the original failure into a
        // second panic that unwinds the calling IPC handler.
        match self.inner.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Settings> {
        self.tx.subscribe()
    }

    /// The directory cImp was launched in — the app's notion of "this project",
    /// and the same value the overlay path and every save already use.
    ///
    /// V38 needs it as a *key*: per-project tool-plugin binary paths are stored
    /// machine-globally under the canonical project root, so the Settings window
    /// has to be able to name the project it is editing. This hands out the raw
    /// path rather than the key, so the canonicalization rule stays in exactly
    /// one place (`plugins::registry::project_key`).
    pub fn launch_cwd(&self) -> PathBuf {
        self.launch_cwd.as_ref().clone()
    }

    /// Replace the full settings struct, broadcast, and request a debounced save.
    ///
    /// Prefer [`Self::mutate`] for any read-modify-write: `current()` + `set()`
    /// is a non-atomic clone-out/replace and two concurrent callers can clobber
    /// each other (lost update). `set` is retained for a genuine wholesale
    /// replace where the caller already holds the authoritative full struct.
    #[allow(dead_code)]
    pub fn set(&self, new: Settings) {
        {
            // Recover from poisoning here too — same rationale as
            // `current()`. The slot is replaced wholesale so the prior
            // (potentially partially-mutated) state is overwritten and
            // the poisoned flag is no longer load-bearing.
            let mut g = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            *g = new.clone();
            // Broadcast while STILL HOLDING the lock: `broadcast::Sender::send`
            // is non-blocking (ring-buffer write, no await), and emitting under
            // the lock is what guarantees broadcast order matches store-update
            // order. Sent outside, two concurrent writers could deliver the
            // older state LAST, leaving every subscriber stale until the next
            // unrelated change ("sees the latest state eventually" broken).
            // A `send` failure just means "no receivers" — fine, no-op.
            let _ = self.tx.send(new);
        }
        // Save channel never fails unless the saver task has died, in which
        // case we've lost persistence — log and continue running in-memory.
        if self.save_tx.send(()).is_err() {
            tracing::warn!("settings: saver task is gone; changes will not persist");
        }
    }

    /// Atomically read-modify-write the settings under the held lock, then
    /// broadcast and request a debounced save.
    ///
    /// Unlike `current()` followed by `set()`, this composes: the inner lock
    /// is held across the whole closure, so two concurrent mutations cannot
    /// each snapshot the pre-mutation state and clobber the other's write.
    /// All settings-mutating IPC commands should funnel through this instead
    /// of the clone-out/replace pattern.
    pub fn mutate<F: FnOnce(&mut Settings)>(&self, f: F) {
        {
            let mut g = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            f(&mut g);
            // Send under the lock — see `set()` for why: broadcast order must
            // match mutation order or a subscriber's last-observed state can
            // end up stale relative to the store.
            let _ = self.tx.send(g.clone());
        }
        if self.save_tx.send(()).is_err() {
            tracing::warn!("settings: saver task is gone; changes will not persist");
        }
    }

    /// Synchronously persist the current settings, bypassing the 500ms
    /// debounce. Intended for shutdown so an edit made within the debounce
    /// window is not silently lost when the saver task is still mid-sleep.
    pub fn flush(&self) {
        // Hold the save lock across snapshot + write so an in-flight
        // debounced save can't complete after us with an older snapshot.
        let _write_guard = match self.save_lock.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let snapshot = self.current();
        let global = match self.global.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if let Err(e) = persistence::save(&snapshot, &self.launch_cwd, &global) {
            tracing::warn!(error = %e, "settings: flush save failed");
        } else {
            tracing::debug!("settings: flushed");
        }
    }

    /// Mutate the in-memory global BASELINE the overlay diff is computed
    /// against, then request a save so the overlay is recomputed against the
    /// new baseline. For the explicit "save/load global" flows only: the
    /// caller has just rewritten (or re-read) the PHYSICAL global file and
    /// the baseline must be brought in line with it — a value equal on both
    /// diff sides drops out of the project overlay. This never writes the
    /// physical global file itself.
    pub fn mutate_global<F: FnOnce(&mut Settings)>(&self, f: F) {
        {
            let mut g = match self.global.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            f(&mut g);
        }
        if self.save_tx.send(()).is_err() {
            tracing::warn!("settings: saver task is gone; changes will not persist");
        }
    }
}

fn spawn_saver(
    inner: Arc<Mutex<Settings>>,
    mut rx: mpsc::UnboundedReceiver<()>,
    global: Arc<Mutex<Settings>>,
    launch_cwd: PathBuf,
    save_lock: Arc<Mutex<()>>,
) {
    tauri::async_runtime::spawn(async move {
        while rx.recv().await.is_some() {
            // Coalesce: wait the debounce window, then drain anything that
            // arrived during it. Effectively "save 500ms after the last
            // change in a burst".
            tokio::time::sleep(SAVE_DEBOUNCE).await;
            while rx.try_recv().is_ok() {}

            // Snapshot + write under the shared save lock (see the field doc
            // on `SettingsHandle::save_lock`): guarantees the second of two
            // racing writers persists the newer-or-equal state.
            let _write_guard = match save_lock.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            // Recover from poisoning like every other lock site in this file
            // (`current()`/`set()`/`mutate()` all do). Poisoning is permanent
            // on std::sync::Mutex, so a `continue` here wouldn't skip one
            // cycle — it would silently disable persistence for the rest of
            // the process lifetime after a single panic-while-locked.
            let snapshot = match inner.lock() {
                Ok(g) => g.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            let baseline = match global.lock() {
                Ok(g) => g.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            if let Err(e) = persistence::save(&snapshot, &launch_cwd, &baseline) {
                tracing::warn!(error = %e, "settings: save failed");
            } else {
                tracing::debug!("settings: saved");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::LogLevel;

    /// Regression (2026-07 review): `set`/`mutate` used to broadcast AFTER
    /// releasing the store lock, so two racing writers could deliver the
    /// older state last — every subscriber then stayed stale until the next
    /// unrelated change, violating the "sees the latest state eventually"
    /// contract. Sending under the lock makes broadcast order match store
    /// order: once all writers finish, the last message in the channel must
    /// equal `current()`.
    #[test]
    fn last_broadcast_matches_store_after_concurrent_mutates() {
        // Unique temp launch dir so the debounced saver can't write an
        // overlay into the repo if it fires before the test ends.
        let tmp = std::env::temp_dir().join(format!("cimp-bcast-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let handle = SettingsHandle::new(Settings::default(), Settings::default(), tmp.clone());
        let mut rx = handle.subscribe();

        // 4 threads × 8 mutations = 32 messages — under BROADCAST_CAPACITY
        // (64), so nothing lags out of the ring buffer and the drain below
        // sees every send in order.
        let mut joins = Vec::new();
        for t in 0..4u8 {
            let h = handle.clone();
            joins.push(std::thread::spawn(move || {
                for i in 0..8u8 {
                    h.mutate(|s| {
                        s.logging.level = if (t + i) % 2 == 0 {
                            LogLevel::Debug
                        } else {
                            LogLevel::Warn
                        };
                    });
                }
            }));
        }
        for j in joins {
            j.join().unwrap();
        }

        let mut last = None;
        while let Ok(s) = rx.try_recv() {
            last = Some(s);
        }
        let last = last.expect("at least one broadcast was delivered");
        assert_eq!(
            serde_json::to_value(&last).unwrap(),
            serde_json::to_value(handle.current()).unwrap(),
            "the final broadcast must reflect the final store state"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }
}

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
//! overlay file (`.ccimp.custom.config.json`). The handle keeps the global
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
    // Kept so `flush()` can persist synchronously (e.g. on shutdown) without
    // routing through the debounced saver task. The saver owns its own copies.
    global: Arc<Settings>,
    launch_cwd: Arc<PathBuf>,
}

impl SettingsHandle {
    pub fn new(initial: Settings, global: Settings, launch_cwd: PathBuf) -> Self {
        let (tx, _) = broadcast::channel::<Settings>(BROADCAST_CAPACITY);
        let (save_tx, save_rx) = mpsc::unbounded_channel::<()>();
        let inner = Arc::new(Mutex::new(initial));

        spawn_saver(inner.clone(), save_rx, global.clone(), launch_cwd.clone());

        Self {
            inner,
            tx,
            save_tx,
            global: Arc::new(global),
            launch_cwd: Arc::new(launch_cwd),
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
        }
        // A `send` failure here means "no receivers" — that's fine, just a
        // no-op until something subscribes. We don't surface the error.
        let _ = self.tx.send(new);
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
        let new = {
            let mut g = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            f(&mut g);
            g.clone()
        };
        let _ = self.tx.send(new);
        if self.save_tx.send(()).is_err() {
            tracing::warn!("settings: saver task is gone; changes will not persist");
        }
    }

    /// Synchronously persist the current settings, bypassing the 500ms
    /// debounce. Intended for shutdown so an edit made within the debounce
    /// window is not silently lost when the saver task is still mid-sleep.
    pub fn flush(&self) {
        let snapshot = self.current();
        if let Err(e) = persistence::save(&snapshot, &self.launch_cwd, &self.global) {
            tracing::warn!(error = %e, "settings: flush save failed");
        } else {
            tracing::debug!("settings: flushed");
        }
    }
}

fn spawn_saver(
    inner: Arc<Mutex<Settings>>,
    mut rx: mpsc::UnboundedReceiver<()>,
    global: Settings,
    launch_cwd: PathBuf,
) {
    tauri::async_runtime::spawn(async move {
        while rx.recv().await.is_some() {
            // Coalesce: wait the debounce window, then drain anything that
            // arrived during it. Effectively "save 500ms after the last
            // change in a burst".
            tokio::time::sleep(SAVE_DEBOUNCE).await;
            while rx.try_recv().is_ok() {}

            let snapshot = match inner.lock() {
                Ok(g) => g.clone(),
                Err(e) => {
                    tracing::warn!(error = %e, "settings: lock poisoned during save");
                    continue;
                }
            };
            if let Err(e) = persistence::save(&snapshot, &launch_cwd, &global) {
                tracing::warn!(error = %e, "settings: save failed");
            } else {
                tracing::debug!("settings: saved");
            }
        }
    });
}

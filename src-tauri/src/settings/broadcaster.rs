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
//! overlay file (`.cctts.custom.config.json`). The handle keeps the global
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
const BROADCAST_CAPACITY: usize = 16;

#[derive(Clone)]
pub struct SettingsHandle {
    inner: Arc<Mutex<Settings>>,
    tx: broadcast::Sender<Settings>,
    save_tx: mpsc::UnboundedSender<()>,
}

impl SettingsHandle {
    pub fn new(initial: Settings, global: Settings, launch_cwd: PathBuf) -> Self {
        let (tx, _) = broadcast::channel::<Settings>(BROADCAST_CAPACITY);
        let (save_tx, save_rx) = mpsc::unbounded_channel::<()>();
        let inner = Arc::new(Mutex::new(initial));

        spawn_saver(inner.clone(), save_rx, global, launch_cwd);

        Self {
            inner,
            tx,
            save_tx,
        }
    }

    pub fn current(&self) -> Settings {
        self.inner.lock().expect("settings poisoned").clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Settings> {
        self.tx.subscribe()
    }

    /// Replace the full settings struct, broadcast, and request a debounced save.
    pub fn set(&self, new: Settings) {
        {
            let mut g = self.inner.lock().expect("settings poisoned");
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

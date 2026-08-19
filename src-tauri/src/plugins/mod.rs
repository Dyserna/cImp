//! V38 — the **tool plugin framework**: drop-in definition files that describe
//! tools cImp can run, discovered from a folder, requiring no rebuild.
//!
//! Design authority: `docs/MILESTONE-V38-tool-plugin-framework.md`.
//!
//! **Phase A (this module today) is discovery only.** Manifests are found,
//! parsed, validated, identified and made visible — and nothing runs them. That
//! is deliberate and is what makes Phase A shippable alone: a plugin dropped in
//! `plugins/` appears, its errors are loud, and the surface a model sees is
//! byte-for-byte unchanged (decision 5). The registry that merges manifests with
//! user state is Phase B; the pipelines that spawn anything are Phase C/D.
//!
//! Layout:
//! * [`manifest`] — the versioned schema and its validation (the parse boundary).
//! * [`loader`] — folder scan, identity rules, and the app-managed
//!   [`PluginStore`].
//! * [`events`] — the `plugin` Events lane's row builders.

pub mod events;
pub mod loader;
pub mod manifest;

use std::sync::{Arc, OnceLock};

use tauri::State;

// Only what this module's own signatures need. The rest of the vocabulary
// (LoadedPlugin, PluginError, ToolManifest, Provenance, …) stays addressed
// through its module, so a reader of a Phase B/C call site can see which layer
// a type belongs to without chasing a re-export.
pub use loader::{PluginSet, PluginStore};

/// Process-global handle to the managed [`PluginStore`], set once in `main.rs`
/// beside the state it is constructed with — the `audit::GLOBAL` seam, for the
/// same reason: Phase C/D's consumers (the offload worker's native audit tools,
/// `run_check`, `run_command`) run **outside** any Tauri command context and so
/// cannot reach a managed state through `State<_>`.
static GLOBAL: OnceLock<Arc<PluginStore>> = OnceLock::new();

/// Publish the managed store as the process global. Idempotent — there is one
/// store per launch, so a stray double-set can never swap it out from under a
/// consumer.
pub fn set_global(store: Arc<PluginStore>) {
    let _ = GLOBAL.set(store);
}

/// The process-global store, or `None` before `main.rs` has set it (a headless
/// subcommand that never builds one).
///
/// Unused in Phase A by construction: nothing outside a Tauri command context
/// reads plugins yet, because nothing RUNS one yet. It lands with `set_global`
/// rather than with its first caller so the seam and its publish site are one
/// reviewable pair — the `audit::GLOBAL` arrangement, whose consumers are the
/// same ones Phase C/D brings here.
#[allow(dead_code)]
pub fn global() -> Option<Arc<PluginStore>> {
    GLOBAL.get().cloned()
}

// ---- IPC commands --------------------------------------------------------

/// The current plugin set — what loaded and what did not. Read by the Tool
/// Plugins settings section (Phase B) on mount.
///
/// Never scans: this is a read of the state the startup scan (or the last
/// Rescan) produced, so opening settings cannot be a disk walk.
#[tauri::command]
pub fn plugins_snapshot(state: State<'_, Arc<PluginStore>>) -> Arc<PluginSet> {
    state.snapshot()
}

/// Rescan `<exe-dir>/plugins/` and return the new set — the manual **Rescan**
/// action (decision 8). Mints the scan's `plugin` Events rows as a side effect,
/// so a rejection is visible in the feed as well as in the settings pane.
///
/// On a blocking hop because it walks a directory and reads every file in it;
/// `audit_refresh_census`'s precedent.
#[tauri::command]
pub async fn plugins_rescan(
    state: State<'_, Arc<PluginStore>>,
) -> Result<Arc<PluginSet>, crate::error::AppError> {
    let store = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || store.rescan())
        .await
        .map_err(|e| crate::error::AppError::Ipc(format!("plugin rescan task failed: {e}")))
}

//! V38 — the **tool plugin framework**: drop-in definition files that describe
//! tools cImp can run, discovered from a folder, requiring no rebuild.
//!
//! Design authority: `docs/MILESTONE-V38-tool-plugin-framework.md`.
//!
//! **Phase C is where a plugin first RUNS; Phase D is where all four kinds do.**
//! Manifests are found, parsed, validated, identified, made visible (A), joined
//! with the user's configuration (B), fanned out under the two audit umbrellas
//! beside the built-in scanners (C), and — for the `check` and `command` kinds —
//! offered to `run_check` and `run_command` (D). The surface a model sees stays
//! byte-for-byte unchanged throughout (decision 5) except for `run_check`'s
//! already-project-scoped `name` enum: what plugins change is what a pipeline
//! runs, never the tool schema.
//!
//! Layout:
//! * [`manifest`] — the versioned schema and its validation (the parse boundary).
//! * [`loader`] — folder scan, identity rules, and the app-managed
//!   [`PluginStore`].
//! * [`events`] — the `plugin` Events lane's row builders.
//! * [`registry`] — Phase B's join of manifests, user state and the open
//!   project into the one answer a pipeline needs.
//! * [`posture`] — the manifest's sandbox fields, applied identically at every
//!   seam that spawns a plugin tool.

pub mod events;
pub mod loader;
pub mod manifest;
pub mod posture;
/// The manifest ⋈ user-state ⋈ project join.
///
/// Phase C is the first consumer: the audit fan-out reads `runnable_tools` for
/// the umbrella it is about to run (`run_check`'s effective set and
/// `run_command`'s allowlist follow in Phase D), so Phase B's "until its
/// pipelines exist" `allow(dead_code)` is gone. The settings pane deliberately
/// does NOT go through here: it renders the same facts from the snapshot DTO in
/// TypeScript because it needs the *unresolved* halves too (which of the two
/// flags a tool is off by).
pub mod registry;

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

/// The key this launch's project stores its per-tool binary paths under
/// (`ToolPluginsSettings::project_paths`).
///
/// The Settings window edits a machine-global map keyed by project, so it has
/// to be able to name the project it is editing — and it must name it the same
/// way every consumer will look it up, which is why this goes through
/// [`registry::project_key`] rather than the frontend joining a path itself.
/// Canonicalization touches the disk, so it belongs on this side of the wire.
#[tauri::command]
pub fn plugins_project_key(state: State<'_, crate::AppState>) -> String {
    registry::project_key(&state.settings.launch_cwd())
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

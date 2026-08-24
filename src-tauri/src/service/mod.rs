//! V42 Phase 0 — the UI-neutral service layer (sizing spike).
//!
//! Every `#[tauri::command]` in this crate is today both the *wire boundary*
//! (argument decoding, `State<'_, AppState>` extraction, error serialization)
//! and the *use case* (what the app actually does when the user clicks). That
//! makes the use case reachable only through a WebView, which is why so many
//! live-verify recipes read "user clicks in the app": there is no other caller.
//!
//! This module is the seam that separates the two. A command keeps the wire
//! contract — same name, same parameter names, same JSON error shape — and
//! delegates its body to a service here. The service takes *ordinary values*:
//! handles it can be handed in a test, and two traits ([`sink::EventSink`],
//! [`sink::OutputSink`]) standing in for the two things only a real Tauri app
//! can do, namely broadcast an event to the webviews and stream bytes down a
//! per-PTY `Channel`.
//!
//! ## Why a layer module rather than a method per domain module
//!
//! The crate is organised by domain (`tabs/` owns the live registry, `pty/`
//! owns sessions, `settings/` owns the store). A *service* is not another
//! domain — it is the list of things the UI can ask for, cutting across all of
//! them. Keeping that list in one directory means "what can the UI do?" is
//! answered by `ls src/service/`, and it keeps [`crate::tabs::registry`]
//! (runtime state: who exists, who is active) distinct from [`tabs`] here (use
//! cases: create, rename, close), which is a distinction the tab-lifecycle code
//! had lost — see [`tabs`]'s module docs.
//!
//! The sinks deliberately do **not** live in [`crate::ipc`]. `ipc` is the Tauri
//! boundary; core modules like [`crate::pty`] have to be able to *name* the
//! sink they write to, and a core module that imports the boundary it is
//! supposed to be independent of has not been decoupled from anything.
//!
//! ## Scope
//!
//! Phase 0 wrapped ten commands as a sizing spike. **Phase A1 finished the
//! surface**: all 180 `#[tauri::command]`s in the crate are now either a
//! wrapper over a use case here, or an explicit leave whose doc says why —
//! 138 and 42 respectively.
//!
//! A leave is not a gap. The criterion is that the body is one call on a handle
//! Tauri already injected, with no argument shaping, no ordering and nothing to
//! return that a test could disagree with: `graph_status` (one accessor),
//! `stt_start_recording` (one post to a capture thread), `themes_list` (a free
//! function that was headless the day it was written), the six Preview commands
//! that call one method on a Tauri `Webview`, the window effects. Three leaves
//! are deferrals rather than "nothing to test", and say so: the latch commands
//! and `injection_status` (V42 #114 owns that seam), and `plugins_rescan`
//! (its rule needs an `OffloadService`, which Phase A2 has to make
//! constructible first).
//!
//! Two leaves have a different reason again: `content_open_folder` and
//! `detection_open_rules_folder` are the crate's audited "reveal this folder"
//! spawns, and [`crate::spawn_ledger`]'s row of record names `ipc/commands.rs`
//! as the site. Moving an audited spawn to buy no testability is a worse trade
//! than leaving it where the ledger says it is.

pub mod audit;
pub mod audio;
pub mod checks;
pub mod delegation;
pub mod graph;
pub mod harness;
pub mod layout;
pub mod offload;
pub mod preview;
pub mod pty;
pub mod settings;
pub mod sink;
pub mod tabs;
pub mod usage;
pub mod view;
pub mod window;
pub mod workbench;

/// Run a synchronous, potentially slow operation on tokio's blocking pool.
///
/// An `async fn` Tauri command runs ON a runtime worker, so calling synchronous
/// store work from one parks that worker for the whole pass and starves every
/// other IPC queued behind it. Originally the activity store's file-I/O-under-a-
/// lock escape hatch in `ipc::commands`; now the shared one for any command
/// whose body blocks — notably the graph usage commands, whose Cozo passes have
/// been measured in seconds against a large store and which the Overview polls
/// on a timer.
///
/// Lives here rather than in `ipc::commands` because the service layer needs it
/// too, and a second copy of "park this off the reactor" is exactly the kind of
/// duplication V42 exists to remove.
pub(crate) async fn on_blocking_pool<T, F>(f: F) -> crate::error::AppResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| crate::error::AppError::Ipc(format!("blocking task join: {e}")))
}

/// Resolve an optional `root` IPC argument to a project directory: the given
/// path when non-blank, else the app's launch directory.
///
/// Lives here rather than in `ipc::commands` (where it is spelled
/// `resolve_graph_root`) because the services need the same answer, and a
/// second copy of "which project is this about" is exactly the kind of
/// duplication V42 exists to remove. The wire boundary keeps its own name for
/// it — one line, delegating here.
pub(crate) fn project_root(root: Option<String>) -> crate::error::AppResult<std::path::PathBuf> {
    match root {
        Some(r) if !r.trim().is_empty() => Ok(std::path::PathBuf::from(r)),
        _ => std::env::current_dir()
            .map_err(|e| crate::error::AppError::Settings(format!("cwd: {e}"))),
    }
}

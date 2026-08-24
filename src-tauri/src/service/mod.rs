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
//! Phase 0 wraps ten commands (see the issue): the tab lifecycle
//! (create shell / preview / ai, close, rename, activate, set-active,
//! request-restart), the PTY streaming path (`pty_start` / `pty_restart` /
//! `pty_rebind_channel`) and one poll view (`activity_list`). It is a real
//! increment, not a prototype — but it is deliberately not the whole surface.

pub mod pty;
pub mod settings;
pub mod sink;
pub mod tabs;
pub mod view;

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

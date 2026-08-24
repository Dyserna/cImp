//! Layout-tree and layout-preset IPC commands (V4-04).
//!
//! Wire boundary only. Every body lives on
//! [`LayoutService`](crate::service::layout::LayoutService), which needs one
//! borrowed handle and no sink — see that module for what the frontend owns,
//! why no validation happens here, and why every write is an atomic `mutate`.

use tauri::State;

use crate::error::{AppError, AppResult};
use crate::ipc::AppState;
use crate::service::layout::LayoutService;
use crate::settings::{LayoutNodePersisted, LayoutPersisted};

/// Build the layout service over this app's handles. One place, so no command
/// can drift in what it hands it.
fn layout_service(state: &AppState) -> LayoutService<'_> {
    LayoutService::new(&state.settings)
}

/// Persist the full layout tree + focused-pane id. See [`LayoutService::save`].
#[tauri::command]
pub async fn save_layout(state: State<'_, AppState>, layout: LayoutPersisted) -> AppResult<()> {
    layout_service(&state).save(layout)
}

/// Save the current layout tree under `name`, replacing a same-named preset in
/// place. See [`LayoutService::save_preset`].
#[tauri::command]
pub async fn save_layout_preset(
    state: State<'_, AppState>,
    name: String,
    tree: LayoutNodePersisted,
) -> AppResult<()> {
    layout_service(&state).save_preset(name, tree)
}

/// Restore a preset: return its tree adapted to the live tab list, ready for
/// the frontend to drop straight into the layout store. See
/// [`LayoutService::restore_preset`] for the adaptation rules and why the
/// repair walk is the load path's, not a second copy.
///
/// The hidden-tab set is the one collaborator the service cannot get for
/// itself: it is a small per-project file keyed by the LAUNCH directory, which
/// lives on `AppState`, not on the settings handle. It is read off the async
/// worker for the same reason `ui_state_get` is (see its note).
#[tauri::command]
pub async fn restore_layout_preset(
    state: State<'_, AppState>,
    name: String,
) -> AppResult<LayoutPersisted> {
    let cwd = state.launch.cwd.clone();
    let hidden =
        tauri::async_runtime::spawn_blocking(move || crate::ipc::ui_state::read_hidden_tabs(&cwd))
            .await
            .map_err(|e| AppError::Settings(format!("hidden-tab read task failed: {e}")))?;
    layout_service(&state).restore_preset(name, &hidden)
}

/// Delete a preset by name. See [`LayoutService::delete_preset`].
#[tauri::command]
pub async fn delete_layout_preset(state: State<'_, AppState>, name: String) -> AppResult<()> {
    layout_service(&state).delete_preset(name)
}

/// Rename a preset. See [`LayoutService::rename_preset`].
#[tauri::command]
pub async fn rename_layout_preset(
    state: State<'_, AppState>,
    old_name: String,
    new_name: String,
) -> AppResult<()> {
    layout_service(&state).rename_preset(old_name, new_name)
}

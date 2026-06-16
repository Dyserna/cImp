//! Settings window lifecycle. The settings window is created lazily on first
//! request and reused (focused) on subsequent opens. Both the gear icon and
//! the `open_settings` shortcut funnel through `open_settings_window`.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::error::{AppError, AppResult};

pub const SETTINGS_LABEL: &str = "settings";

pub fn open_or_focus_settings(app: &AppHandle) -> AppResult<()> {
    if let Some(existing) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = existing.show();
        let _ = existing.unminimize();
        let _ = existing.set_focus();
        return Ok(());
    }

    WebviewWindowBuilder::new(
        app,
        SETTINGS_LABEL,
        WebviewUrl::App("settings.html".into()),
    )
    .title("ccImp — Settings")
    .inner_size(970.0, 750.0)
    .min_inner_size(560.0, 480.0)
    .resizable(true)
    .build()
    .map_err(|e| AppError::Settings(format!("create settings window: {e}")))?;

    Ok(())
}

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

    let built =
        WebviewWindowBuilder::new(app, SETTINGS_LABEL, WebviewUrl::App("settings.html".into()))
            .title("cImp — Settings")
            .inner_size(1210.0, 750.0)
            .min_inner_size(560.0, 480.0)
            .resizable(true)
            // Created hidden, same as the main window (tauri.conf.json
            // `visible: false`): the WebView paints white and the TUI themes
            // strip the OS chrome after load, so revealing early flashes and
            // blinks. `settings_main.ts` shows the window after its first
            // decorations pass (with a timeout safety net).
            .visible(false)
            .build();

    if let Err(e) = built {
        // A concurrent open (gear click + `open_settings` shortcut firing
        // near-simultaneously) may have created the window between our
        // exists-check and `build()`; the duplicate-label error is then
        // expected. Re-query and focus the existing window rather than
        // surfacing a spurious error toast for what is an idempotent action.
        if let Some(existing) = app.get_webview_window(SETTINGS_LABEL) {
            let _ = existing.show();
            let _ = existing.unminimize();
            let _ = existing.set_focus();
            return Ok(());
        }
        return Err(AppError::Settings(format!("create settings window: {e}")));
    }

    Ok(())
}

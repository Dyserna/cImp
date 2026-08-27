//! #150 — WebView2 **renderer-crash** observability.
//!
//! A tab's WebView2 renderer subprocess can die on its own (the Chromium
//! sad-face) while the host process carries on perfectly happily. Until this
//! module the app was blind to it: no log line, no event, nothing to correlate a
//! user's "the pane went grey" against. WebView2 reports it as
//! `ICoreWebView2::add_ProcessFailed`, reached through
//! `tauri::Webview::with_webview` the same way `preview::capture` reaches
//! `CapturePreview` — see that module's doc for why the raw
//! `ICoreWebView2Controller` wry hands back is usable against this crate's own
//! `webview2-com` (both pinned to 0.38.2, tripwired there).
//!
//! **Strictly best-effort, and that is the contract, not a caveat.** Every
//! failure to register is a `debug!` and a silent no-op: an app that cannot
//! observe renderer crashes must still start, and a startup error here would
//! trade a diagnostic for the thing it diagnoses. The corollary is that silence
//! is not evidence of health — a build that registered nothing reports nothing.
//!
//! Non-Windows builds are a no-op: the whole mechanism is WebView2's, and the
//! Linux (webkit2gtk) equivalent is a different API with a different shape.

#[cfg(windows)]
mod windows_impl {
    use tauri::{Emitter, Manager, Webview};
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2ProcessFailedEventArgs2;
    use webview2_com::ProcessFailedEventHandler;
    use windows::core::Interface;

    use crate::service::events::WEBVIEW_PROCESS_FAILED;

    /// Register a `ProcessFailed` handler on `webview`.
    ///
    /// Idempotence is the CALLER's business: WebView2 accepts the same handler
    /// twice and would then report each failure twice. Every call site here
    /// registers once, at the point the webview is created.
    pub fn watch_process_failures(webview: &Webview) {
        let label = webview.label().to_string();
        let app = webview.app_handle().clone();
        // `with_webview` returns once the closure is QUEUED on the webview's UI
        // thread, not once it has run, so a registration failure inside it can
        // only be logged there — which is all it is worth.
        if let Err(e) = webview.with_webview(move |platform| register(platform, label, app)) {
            tracing::debug!(
                error = %e,
                "webview process-failed watch: with_webview dispatch failed; renderer crashes \
                 will not be reported for this webview"
            );
        }
    }

    /// Runs ON the webview's own UI thread (inside `with_webview`'s closure).
    fn register(platform: tauri::webview::PlatformWebview, label: String, app: tauri::AppHandle) {
        let core = match unsafe { platform.controller().CoreWebView2() } {
            Ok(core) => core,
            Err(e) => {
                tracing::debug!(
                    webview = %label,
                    error = %e,
                    "webview process-failed watch: CoreWebView2 unavailable"
                );
                return;
            }
        };

        let reported = label.clone();
        let handler = ProcessFailedEventHandler::create(Box::new(move |_sender, args| {
            let kind = args
                .as_ref()
                .and_then(|a| {
                    let mut k = Default::default();
                    unsafe { a.ProcessFailedKind(&mut k) }.ok().map(|()| k.0)
                })
                .unwrap_or(UNREPORTED_KIND);
            // `ExitCode` arrived with the `…EventArgs2` revision, so a host on
            // an older WebView2 runtime reports the kind and no code rather
            // than nothing at all.
            let exit_code = args
                .as_ref()
                .and_then(|a| a.cast::<ICoreWebView2ProcessFailedEventArgs2>().ok())
                .and_then(|a2| {
                    let mut code = 0i32;
                    unsafe { a2.ExitCode(&mut code) }.ok().map(|()| code)
                });
            tracing::error!(
                webview = %reported,
                kind = kind_word(kind),
                kind_code = kind,
                exit_code = exit_code.unwrap_or_default(),
                exit_code_known = exit_code.is_some(),
                "webview2 process failed — this webview's content is gone until it is recreated"
            );
            let _ = app.emit(
                WEBVIEW_PROCESS_FAILED,
                serde_json::json!({ "label": reported, "kind": kind_word(kind) }),
            );
            Ok(())
        }));

        let mut token = 0i64;
        if let Err(e) = unsafe { core.add_ProcessFailed(&handler, &mut token) } {
            tracing::debug!(
                webview = %label,
                error = %e,
                "webview process-failed watch: add_ProcessFailed refused"
            );
        }
    }

    /// The `kind` value used when WebView2 reported a failure but would not say
    /// which. Outside the documented 0..=9 range on purpose, so it can never
    /// collide with a real kind and be read as one.
    const UNREPORTED_KIND: i32 = -1;

    /// `COREWEBVIEW2_PROCESS_FAILED_KIND` as a word.
    ///
    /// A word rather than the raw number because the payload crosses to the
    /// frontend and a number there is a second table to keep in step. The
    /// numeric value still rides the log line (`kind_code`), which is what a
    /// bug report needs when this table is the thing that is out of date.
    fn kind_word(kind: i32) -> &'static str {
        match kind {
            0 => "browser_process_exited",
            1 => "render_process_exited",
            2 => "render_process_unresponsive",
            3 => "frame_render_process_exited",
            4 => "utility_process_exited",
            5 => "sandbox_helper_process_exited",
            6 => "gpu_process_exited",
            7 => "ppapi_plugin_process_exited",
            8 => "ppapi_broker_process_exited",
            9 => "unknown_process_exited",
            // A kind this build's table predates, or `UNREPORTED_KIND`. Both
            // must read as "we do not have a word for this" rather than borrow
            // one — `kind_code` carries the fact.
            _ => "unreported",
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
            COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED,
            COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED,
            COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED,
        };

        /// The word table is keyed on the SDK's own constants, not on numbers
        /// copied out of the header once.
        #[test]
        fn the_kind_words_track_the_sdk_constants() {
            assert_eq!(
                kind_word(COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED.0),
                "browser_process_exited"
            );
            assert_eq!(
                kind_word(COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED.0),
                "render_process_exited"
            );
            assert_eq!(
                kind_word(COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED.0),
                "gpu_process_exited"
            );
            assert_eq!(
                kind_word(COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED.0),
                "unknown_process_exited"
            );
            // A kind added after this build, and the "WebView2 would not say"
            // sentinel, must not borrow a word.
            assert_eq!(kind_word(10), "unreported");
            assert_eq!(kind_word(UNREPORTED_KIND), "unreported");
        }
    }
}

#[cfg(windows)]
pub use windows_impl::watch_process_failures;

#[cfg(not(windows))]
mod stub {
    /// No-op: `ProcessFailed` is a WebView2 event and there is no WebView2 here.
    pub fn watch_process_failures(_webview: &tauri::Webview) {}
}

#[cfg(not(windows))]
pub use stub::watch_process_failures;

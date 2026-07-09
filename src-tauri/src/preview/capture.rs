//! V14 Phase F: the Preview tab's Snapshot capture — WebView2's
//! `CapturePreviewAsync` COM method, reached through
//! `tauri::Webview::with_webview`. See `preview::mod`'s module doc for why
//! this is the embedded path (not the milestone's system-browser fallback):
//! wry doesn't expose `CapturePreviewAsync` itself, but the raw
//! `ICoreWebView2Controller` it hands back through `PlatformWebview` is the
//! SAME nominal type as this crate's own `webview2-com` dependency (both
//! pinned to 0.38.2 via Cargo.lock), so reaching one COM level further is a
//! single extra call, not a dependency saga.
//!
//! Rather than the usual in-memory `IStream` + `HGLOBAL`-lock byte
//! extraction, [`capture_to_png`] points `CapturePreview` at a FILE-BACKED
//! stream (`SHCreateStreamOnFileW`) so WebView2 writes the PNG straight to
//! `dest` — no manual byte copying, no `HGLOBAL` lifetime to manage.
//!
//! Non-Windows builds get a stub that errors clearly: Linux (webkit2gtk)
//! capture is explicitly "checked but not blocking" in the milestone's D0
//! spike — Windows-first, like the rest of the app.

#[cfg(windows)]
mod windows_impl {
    use std::path::Path;

    use tauri::Webview;
    use tokio::sync::oneshot;
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG;
    use webview2_com::CapturePreviewCompletedHandler;
    use windows::core::HSTRING;
    use windows::Win32::System::Com::{STGM_CREATE, STGM_WRITE};
    use windows::Win32::UI::Shell::SHCreateStreamOnFileW;

    use crate::error::{AppError, AppResult};

    /// Capture `webview`'s current viewport to a PNG at `dest`.
    ///
    /// `Webview::with_webview` dispatches its closure onto the webview's own
    /// UI thread and returns as soon as it's QUEUED there, not once it has
    /// run — so the actual COM call (and its completion callback, which also
    /// fires on that same UI thread) is bridged back to this `async fn` via
    /// a oneshot channel, the same "hop to the platform's UI thread, await
    /// the result here" shape any other cross-thread platform call in this
    /// codebase uses.
    ///
    /// TODO(spike E0): this compiles cleanly against the exact
    /// `webview2-com`/`windows` versions wry 0.55 resolves to (verified via
    /// Cargo.lock, not just "the API exists somewhere") — but producing an
    /// actually-correct PNG (right viewport bounds, CSS-pixel not
    /// device-pixel scale, timing relative to the page having finished
    /// painting) has not been exercised against a live WebView2 instance;
    /// that needs a manual pass, same posture as the rest of this phase's
    /// spike markers.
    pub async fn capture_to_png(webview: &Webview, dest: &Path) -> AppResult<()> {
        let (tx, rx) = oneshot::channel::<Result<(), String>>();
        let dest = dest.to_path_buf();

        webview
            .with_webview(move |platform| capture_now(platform, &dest, tx))
            .map_err(|e| {
                AppError::Preview(format!("capture: with_webview dispatch failed: {e}"))
            })?;

        rx.await
            .map_err(|_| {
                AppError::Preview(
                    "capture: the preview webview closed before the capture completed".into(),
                )
            })?
            .map_err(AppError::Preview)
    }

    /// Runs ON the webview's own UI thread (inside `with_webview`'s
    /// closure). Resolves `ICoreWebView2` from the controller, opens `dest`
    /// as a file-backed `IStream`, and issues the `CapturePreview` call.
    /// `tx` is resolved on EVERY path — every early return below sends an
    /// error immediately; the success path hands `tx` into the completion
    /// handler, which sends once WebView2 actually finishes writing the PNG.
    fn capture_now(
        platform: tauri::webview::PlatformWebview,
        dest: &Path,
        tx: oneshot::Sender<Result<(), String>>,
    ) {
        let path_wide = HSTRING::from(dest.to_string_lossy().as_ref());
        let stream = match unsafe {
            SHCreateStreamOnFileW(&path_wide, (STGM_CREATE | STGM_WRITE).0)
        } {
            Ok(stream) => stream,
            Err(e) => {
                let _ = tx.send(Err(format!(
                    "SHCreateStreamOnFileW({}) failed: {e}",
                    dest.display()
                )));
                return;
            }
        };

        let webview2 = match unsafe { platform.controller().CoreWebView2() } {
            Ok(w) => w,
            Err(e) => {
                let _ = tx.send(Err(format!("CoreWebView2 unavailable: {e}")));
                return;
            }
        };

        let handler = CapturePreviewCompletedHandler::create(Box::new(move |result| {
            let _ = tx.send(
                result.map_err(|e| format!("CapturePreview completion reported an error: {e}")),
            );
            Ok(())
        }));

        // If `CapturePreview` fails to even ISSUE the async op, the
        // completion handler above never fires and `tx` (already moved into
        // it) is simply dropped — the caller's `rx.await` then surfaces the
        // generic "closed before completing" error rather than this precise
        // one. Logged here so the specific COM failure isn't lost entirely.
        if let Err(e) =
            unsafe { webview2.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler) }
        {
            tracing::warn!(error = %e, "preview capture: CapturePreview failed to issue");
        }
    }
}

#[cfg(windows)]
pub use windows_impl::capture_to_png;

#[cfg(not(windows))]
mod stub {
    use std::path::Path;

    use tauri::Webview;

    use crate::error::{AppError, AppResult};

    /// Non-Windows builds have no capture path yet (see this module's doc
    /// comment) — errors clearly rather than silently producing a blank or
    /// missing file.
    pub async fn capture_to_png(_webview: &Webview, _dest: &Path) -> AppResult<()> {
        Err(AppError::Preview(
            "Preview tab snapshot capture is only implemented on Windows today".into(),
        ))
    }
}

#[cfg(not(windows))]
pub use stub::capture_to_png;

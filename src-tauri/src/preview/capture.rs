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
    use std::time::Duration;

    use tauri::Webview;
    use tokio::sync::oneshot;
    use webview2_com::CapturePreviewCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG;
    use windows::core::HSTRING;
    use windows::Win32::System::Com::{STGM_CREATE, STGM_WRITE};
    use windows::Win32::UI::Shell::SHCreateStreamOnFileW;

    use crate::error::{AppError, AppResult};

    /// V14 code-review fix (FIX 5): cap on how long `capture_to_png` waits
    /// for the completion callback. A few seconds is generous for an
    /// in-process viewport capture; if the tab closes concurrently (its
    /// webview torn down mid-capture) the completion handler may simply
    /// never run, and without a timeout the `rx.await` below hung forever —
    /// the caller's IPC promise would never resolve or reject.
    const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Bounded by [`CAPTURE_TIMEOUT`] (FIX 5) so a concurrent tab-close that
    /// prevents the completion callback from ever firing surfaces as a typed
    /// error instead of hanging the calling IPC command forever. On ANY
    /// failure path here — dispatch failure, a reported capture error, or a
    /// timeout — `dest` is removed if present (FIX 6): `preview_capture`'s
    /// caller (`crate::attach::reserve_path`) already touched an empty
    /// placeholder at `dest` (and `SHCreateStreamOnFileW` below, opened
    /// `STGM_CREATE`, would truncate/create it too), so a failure here would
    /// otherwise leave a stray 0-byte PNG behind rather than surfacing
    /// cleanly as "no snapshot".
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
        let dest_buf = dest.to_path_buf();

        if let Err(e) = webview.with_webview(move |platform| capture_now(platform, &dest_buf, tx)) {
            cleanup_stray_file(dest);
            return Err(AppError::Preview(format!(
                "capture: with_webview dispatch failed: {e}"
            )));
        }

        let result = match tokio::time::timeout(CAPTURE_TIMEOUT, rx).await {
            Ok(Ok(inner)) => inner.map_err(AppError::Preview),
            Ok(Err(_)) => Err(AppError::Preview(
                "capture: the preview webview closed before the capture completed".into(),
            )),
            Err(_) => Err(AppError::Preview(format!(
                "capture: timed out after {CAPTURE_TIMEOUT:?} waiting for the preview snapshot"
            ))),
        };

        if result.is_err() {
            cleanup_stray_file(dest);
        }
        result
    }

    /// Best-effort delete of a stray (empty or partial) capture output.
    /// Failure is logged, not propagated — we're already on an error path
    /// and a cleanup miss shouldn't mask the original error.
    fn cleanup_stray_file(dest: &Path) {
        if let Err(e) = std::fs::remove_file(dest) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %dest.display(),
                    error = %e,
                    "preview capture: failed to remove stray output file after a failed capture"
                );
            }
        }
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
        let stream =
            match unsafe { SHCreateStreamOnFileW(&path_wide, (STGM_CREATE | STGM_WRITE).0) } {
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
        if let Err(e) = unsafe {
            webview2.CapturePreview(
                COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                &stream,
                &handler,
            )
        } {
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

/// Lock-resolution tripwire for this module's load-bearing dependency pin
/// (2026-08-04 maintenance run, batch B-2).
///
/// `capture_to_png` casts the `ICoreWebView2Controller` that **wry** hands
/// back through `PlatformWebview::controller()` against **this** crate's own
/// `webview2-com` types. That only works while both resolve to the same
/// semver-compatible `webview2-com`, and the same holds for the `windows`
/// crate shared between `webview2-com` and our direct `SHCreateStreamOnFileW`
/// call. Cargo.lock is the only place that contract is actually settled — the
/// manifests just say `"0.38"` / `"0.61"`.
///
/// A drift usually surfaces as a confusing type-mismatch error deep in this
/// module (or, worse, a second `webview2-com` silently in the tree); this
/// test names the cause instead. If a Tauri/wry bump legitimately moves these
/// series, update the constants here **and** the pin rationale in
/// `Cargo.toml` in the same change.
#[cfg(test)]
mod lock_pins {
    /// Semver-compatible series (not exact patch): patch bumps are
    /// type-identical, so pinning the patch would fail spuriously.
    const WEBVIEW2_COM_SERIES: &str = "0.38.";
    const WINDOWS_SERIES: &str = "0.61.";

    /// Every `version` resolved for `pkg` in Cargo.lock, in file order.
    fn resolved_versions(lock: &str, pkg: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut current: Option<&str> = None;
        for line in lock.lines() {
            if let Some(rest) = line.strip_prefix("name = \"") {
                current = rest.strip_suffix('"');
            } else if let Some(rest) = line.strip_prefix("version = \"") {
                if current == Some(pkg) {
                    if let Some(v) = rest.strip_suffix('"') {
                        out.push(v.to_string());
                    }
                }
                current = None;
            }
        }
        out
    }

    #[test]
    fn webview2_com_and_windows_stay_on_the_pinned_series() {
        let lock = include_str!("../../Cargo.lock");

        let webview2 = resolved_versions(lock, "webview2-com");
        assert_eq!(
            webview2.len(),
            1,
            "expected exactly one webview2-com in Cargo.lock (two would make \
             wry's ICoreWebView2Controller a different nominal type than \
             preview::capture's), found {webview2:?}"
        );
        assert!(
            webview2[0].starts_with(WEBVIEW2_COM_SERIES),
            "webview2-com resolved to {} but preview::capture is written \
             against {WEBVIEW2_COM_SERIES}x",
            webview2[0]
        );

        let windows = resolved_versions(lock, "windows");
        assert!(
            windows.iter().any(|v| v.starts_with(WINDOWS_SERIES)),
            "no windows {WINDOWS_SERIES}x in Cargo.lock — preview::capture \
             shares IStream/PCWSTR with webview2-com {WEBVIEW2_COM_SERIES}x, \
             which depends on that series; found {windows:?}"
        );
    }
}

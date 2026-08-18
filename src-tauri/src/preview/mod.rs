//! V14 Phase F: the Preview tab — an embedded, localhost-scoped child
//! webview (Tauri 2's multi-webview API, backed by WebView2 on Windows)
//! rendered inside the main window, positioned over a pane's content rect by
//! the frontend (`PreviewToolbar.svelte` measures the pane body div and calls
//! [`preview_set_rect`] on every layout change).
//!
//! ## Path shipped: EMBEDDED (not the system-browser fallback)
//!
//! The milestone's E0 spike gated this phase on two things compiling cleanly
//! against this project's exact Tauri/wry versions (2.11.0 / 0.55.0):
//! 1. **Child webview in the main window** — `tauri::Window::add_child`
//!    (behind the `unstable` cargo feature) attaches a second `Webview` to
//!    the "main" window at an arbitrary logical rect. Confirmed present and
//!    stable-shaped in 2.11.0.
//! 2. **Programmatic capture** — wry does NOT expose WebView2's
//!    `CapturePreviewAsync` directly, but `PlatformWebview::controller()`
//!    (`Webview::with_webview`) hands back the raw `ICoreWebView2Controller`
//!    from the `webview2-com` crate (already a transitive dependency of wry
//!    0.55 at the exact version this crate now depends on directly, so no
//!    type-identity mismatch), whose `.CoreWebView2()` yields the
//!    `ICoreWebView2` with `.CapturePreview(...)`. Rather than extracting PNG
//!    bytes out of an in-memory `IStream` via `HGLOBAL` (the usual — fiddly —
//!    pattern), [`capture`] points `CapturePreview` at a FILE-BACKED stream
//!    (`SHCreateStreamOnFileW`), so the PNG lands on disk with no manual byte
//!    plumbing at all. See `preview::capture` for the single COM call this
//!    amounts to.
//!
//! Both compiled cleanly, so this module ships the real embedded webview
//! end-to-end rather than the milestone's documented re-scope (open in the
//! system browser + attach a hand screenshot). Runtime behavior — actual
//! coexistence with the xterm panes during drag, focus isolation, and
//! whether the captured PNG is pixel-correct — could not be exercised from
//! this environment (no live app); see the `TODO(spike E0)` markers below
//! for exactly what still needs a manual pass, same posture as the V11 D0 /
//! V12 F0 / V20 TUI spikes.
//!
//! ## Navigation policy
//!
//! [`is_allowed_preview_host`] is the single source of truth, applied at
//! THREE points: the initial [`preview_open`]/[`preview_navigate`] call (so a
//! disallowed URL is rejected before ever touching a webview), the
//! constructed webview's `on_navigation` handler (catches in-page navigation
//! — clicking a link, a redirect), and `on_new_window` (a `target="_blank"` /
//! `window.open()` — always denied and forwarded to the system browser,
//! regardless of host, since a Preview tab is explicitly not a general
//! browser with its own tab/window model).
//!
//! // KNOWN LIMITATION: this policy only polices the MAIN FRAME. `wry`
//! // exposes no subframe-navigation hook (`on_navigation` fires for
//! // top-level navigations of the page loaded in the webview, not for
//! // navigations initiated by an embedded `<iframe>`), so a policy-ALLOWED
//! // page (a localhost dev server, say) that embeds `<iframe src="https://
//! // some-remote-host">` can load and render that remote content inside the
//! // Preview tab without ever going through `is_allowed_preview_host`. This
//! // is acceptable for a localhost dev-preview surface (the threat model is
//! // "don't let the Preview tab casually browse the wider internet or reach
//! // a local network host you didn't ask for", not "sandbox untrusted
//! // third-party content") but is recorded here for a future hardening pass
//! // — e.g. revisiting this if/when wry grows subframe-navigation events, or
//! // scoping via WebView2's `NavigationStarting` at the `CoreWebView2Frame`
//! // level directly (bypassing wry) if this ever needs to be airtight. See
//! // `docs/MILESTONE-V14...`'s E0 section for the same note in context.

mod capture;

use std::collections::HashMap;
use std::sync::Mutex;

use tauri::{
    AppHandle, LogicalPosition, LogicalSize, Manager, State, Webview, WebviewBuilder, WebviewUrl,
    Window,
};
use tauri_plugin_opener::OpenerExt;
use tracing::warn;
use url::{Host, Url};

use crate::error::{AppError, AppResult};
use crate::ipc::AppState;
use crate::settings::{SettingsHandle, TabConfig};

/// Fallback URL for a freshly created Preview tab with no remembered
/// `Settings::preview_last_url` — a generic dev-server port, not tied to any
/// specific framework's default.
pub const DEFAULT_PREVIEW_URL: &str = "http://localhost:3000";

/// The only window a Preview child webview ever attaches to — cImp has one
/// main window (see `capabilities/default.json`'s `"windows"` list); the
/// Settings window never hosts a Preview pane.
const MAIN_WINDOW_LABEL: &str = "main";

/// The raw WebView2/wry webview label for a Preview tab, distinct from the
/// tab id itself so it can never collide with the app's own webviews
/// ("main", "settings") or with a future non-Preview use of the tab id as a
/// label elsewhere.
fn webview_label(tab_id: &str) -> String {
    format!("preview-{tab_id}")
}

/// One child webview per open Preview tab, keyed by tab id (the
/// `TabId::Preview` string, e.g. `"preview-<uuid>"`), managed as its own
/// Tauri state — the same convention `WorkbenchService`/`GraphService` use
/// (`main.rs`'s `.manage(...)` calls) rather than folding into `AppState`.
#[derive(Default)]
pub struct PreviewRegistry {
    entries: Mutex<HashMap<String, Webview>>,
}

impl PreviewRegistry {
    fn insert(&self, tab_id: String, webview: Webview) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tab_id, webview);
    }

    fn get(&self, tab_id: &str) -> Option<Webview> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(tab_id)
            .cloned()
    }

    fn remove(&self, tab_id: &str) -> Option<Webview> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(tab_id)
    }
}

// ── Navigation policy (pure, testable without a webview or app handle) ────

/// True when `url` may be loaded directly in a Preview tab's embedded
/// webview. `allow_remote` is `Settings::preview_allow_remote` (default
/// `false`): while off, only `localhost` / `127.0.0.1` / `::1` / RFC-1918
/// private ranges (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16) are allowed —
/// exactly the hosts a local dev server binds to. A malformed URL, or one
/// with no host at all (`file://`, `javascript:`, `about:blank`), is
/// rejected UNCONDITIONALLY — `allow_remote` widens which *hosts* are
/// trusted, it never widens what counts as a well-formed navigation target.
///
/// This is deliberately host-based, not string-prefix-based: parsing with
/// the `url` crate means userinfo tricks (`http://localhost@evil.com`) and
/// similar don't fool it — `Url::host()` returns the real host (`evil.com`
/// there), not whatever precedes an `@`.
pub fn is_allowed_preview_host(url: &str, allow_remote: bool) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host() else {
        return false;
    };
    if allow_remote {
        return true;
    }
    is_local_host(host)
}

/// `localhost` (name) or a loopback/RFC-1918 IP literal. Bare hostnames other
/// than `localhost` (e.g. a LAN mDNS name) are NOT treated as local — only an
/// actual private/loopback IP literal or the `localhost` name qualifies,
/// since a hostname alone can't be classified without a DNS lookup (which
/// this pure function deliberately never performs).
///
/// Classifies via `Url::host()`'s parsed `Host` enum rather than
/// `host_str()` + `str::parse::<IpAddr>()` — `host_str()` returns the IPv6
/// host WITH its brackets (`"[::1]"`, valid URL syntax but not valid
/// `IpAddr` syntax), so parsing that string form back to an `IpAddr` would
/// need a bracket-strip step; `Url::host()` hands back an already-parsed
/// `Ipv6Addr` with no string round-trip at all.
fn is_local_host(host: Host<&str>) -> bool {
    match host {
        Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        // RFC-1918 private (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16) or
        // loopback (127.0.0.0/8) — `Ipv4Addr::is_private`/`is_loopback` are
        // exactly these ranges (stable std, no manual bit-twiddling needed).
        Host::Ipv4(v4) => v4.is_loopback() || v4.is_private(),
        Host::Ipv6(v6) => v6.is_loopback(),
    }
}

/// V14 code-review fix (HIGH, security): scheme allowlist for anything
/// [`open_external`] is about to hand to the OS's system opener
/// (`tauri_plugin_opener::open_url`, which ultimately calls into
/// `ShellExecute`/`xdg-open`-style APIs). `is_allowed_preview_host` gates
/// which HOSTS the in-webview navigation policy trusts, but it never
/// restricted *scheme* — a `file:`, `javascript:`, `data:`, `mailto:`, or a
/// registered custom-protocol URI (`ms-msdt:...`, the Follina RCE vector)
/// has no meaningful "host" the way `http(s)` does, and some of those would
/// otherwise sail past `open_external` untouched and reach the OS protocol
/// handler. Only `http`/`https` are ever externally-openable; every other
/// scheme is dropped. Pure + unit-tested independent of any webview/app
/// handle.
pub fn is_externally_openable(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https")
}

/// Best-effort "open in the system browser" — used both for navigation
/// targets the policy rejects and for every `on_new_window` request (a
/// Preview tab has no tab/window model of its own, so ANY `target="_blank"`
/// / `window.open()` always leaves the embedded webview and goes external,
/// regardless of host). Failure is logged, not propagated — the caller
/// already denied the in-webview navigation either way.
///
/// Scheme-gated by [`is_externally_openable`] before ever reaching the OS
/// opener: an `http`/`https` URL forwards through `tauri_plugin_opener` as
/// before; anything else (a custom/registered protocol handler, `file:`,
/// `javascript:`, `data:`, ...) is silently dropped rather than handed to
/// the OS — see that function's doc comment for why.
///
/// **This is the only path to the OS opener.** The plugin also exposes
/// `open_url` to the webviews over IPC, but `capabilities/default.json` grants
/// the scope-less `opener:allow-open-url`, and the plugin refuses a URL that
/// matches no scope entry — with an empty allow list that is every URL. So the
/// frontend cannot route around this scheme gate. That is a property of a JSON
/// file rather than of this module, so it is enforced by
/// `spawn_ledger::tests::the_opener_grant_stays_scopeless`, whose doc carries
/// the evidence.
fn open_external(app: &AppHandle, url: &str) {
    if !is_externally_openable(url) {
        tracing::debug!(
            url,
            "preview: dropped non-http(s) URL instead of opening it externally"
        );
        return;
    }
    // Through the spawn gate (see `spawn_gate`): the `open` crate behind this
    // plugin call spawns the OS handler, and a spawn cImp cannot see is still a
    // spawn that can inherit the sandbox's pipe write-ends. `with_shared` is the
    // shape for a spawn that happens inside a third-party call — the closure
    // holds nothing but the call itself, and `open` detaches rather than waiting
    // on the browser.
    if let Err(e) = crate::spawn_gate::with_shared(|| app.opener().open_url(url, None::<&str>)) {
        warn!(url, error = %e, "preview: failed to open URL in the system browser");
    }
}

// ── Webview construction ───────────────────────────────────────────────

/// Build the `on_navigation` closure shared by [`preview_open`]. Reads
/// `preview_allow_remote` live on every navigation (via the cloned
/// `SettingsHandle`, not a value captured at construction time) so a
/// mid-session settings change takes effect on the Preview tab's very next
/// navigation without needing to reopen it.
///
/// TODO(spike E0): WebView2's `NavigationStarting` (what wry's
/// `on_navigation` wraps) firing for the INITIAL load as well as subsequent
/// ones is documented wry/WebView2 behavior, not something exercised here —
/// a live pass should confirm the very first navigation is actually policed
/// (not just the `preview_open`/`preview_navigate` IPC-level pre-check
/// below, which covers the common case but isn't the runtime enforcement
/// point for links clicked inside the loaded page).
fn navigation_handler(
    app: AppHandle,
    settings: SettingsHandle,
) -> impl Fn(&Url) -> bool + Send + 'static {
    move |url: &Url| {
        let allow_remote = settings.current().preview_allow_remote;
        let allowed = is_allowed_preview_host(url.as_str(), allow_remote);
        if !allowed {
            open_external(&app, url.as_str());
        }
        allowed
    }
}

/// Open (or, if already open, replace) the child webview for `tab_id` at
/// `url`, positioned at the given logical (CSS-pixel) rect. Rejects `url`
/// up front against the live navigation policy — see [`is_allowed_preview_host`]
/// — before ever constructing a webview, opening the URL externally instead.
///
/// TODO(spike E0): coexistence with the xterm panes' own child processes and
/// the portal/drag-and-drop tab-reorder system, and z-order during a tab
/// drag, are exactly what the milestone's E0 spike calls out as needing a
/// live pass — this function only establishes that the multi-webview API
/// call itself compiles and (per Tauri's own doctest) is the documented
/// shape; it does not simulate a drag or a focus fight.
// Tauri command: parameters are the `invoke` payload fields, plus the managed
// state handles Tauri injects.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn preview_open(
    app: AppHandle,
    state: State<'_, AppState>,
    registry: State<'_, PreviewRegistry>,
    tab_id: String,
    url: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> AppResult<()> {
    let settings = state.settings.clone();
    let allow_remote = settings.current().preview_allow_remote;
    if !is_allowed_preview_host(&url, allow_remote) {
        open_external(&app, &url);
        return Err(AppError::Preview(format!(
            "{url} is outside the Preview tab's localhost/RFC-1918 policy; opened in your browser instead"
        )));
    }
    let parsed = Url::parse(&url)
        .map_err(|e| AppError::Preview(format!("invalid preview URL {url}: {e}")))?;

    // Re-opening an id that's already live (a stray double-mount, or a
    // pane-recreate under HMR) replaces rather than leaks a second child
    // webview for the same tab.
    if let Some(existing) = registry.remove(&tab_id) {
        if let Err(e) = existing.close() {
            warn!(?tab_id, error = %e, "preview_open: closing stale webview failed");
        }
    }

    let window: Window = app
        .get_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| AppError::Preview("main window not found".into()))?;

    let nav_app = app.clone();
    let nav_settings = settings.clone();
    let new_window_app = app.clone();
    let builder = WebviewBuilder::new(webview_label(&tab_id), WebviewUrl::External(parsed))
        .on_navigation(navigation_handler(nav_app, nav_settings))
        // A Preview tab is not a general browser: every `window.open()` /
        // `target="_blank"` leaves the embedded pane and goes to the system
        // browser instead, regardless of host — there is no second Preview
        // pane to open it into.
        .on_new_window(move |url, _features| {
            open_external(&new_window_app, url.as_str());
            tauri::webview::NewWindowResponse::Deny
        });

    let webview = window
        .add_child(
            builder,
            LogicalPosition::new(x, y),
            LogicalSize::new(width, height),
        )
        .map_err(|e| AppError::Preview(format!("failed to attach preview webview: {e}")))?;

    registry.insert(tab_id, webview);
    Ok(())
}

/// Navigate an already-open Preview tab's webview to a new URL, subject to
/// the same up-front policy check as [`preview_open`].
#[tauri::command]
pub async fn preview_navigate(
    app: AppHandle,
    state: State<'_, AppState>,
    registry: State<'_, PreviewRegistry>,
    tab_id: String,
    url: String,
) -> AppResult<()> {
    let allow_remote = state.settings.current().preview_allow_remote;
    if !is_allowed_preview_host(&url, allow_remote) {
        open_external(&app, &url);
        return Err(AppError::Preview(format!(
            "{url} is outside the Preview tab's localhost/RFC-1918 policy; opened in your browser instead"
        )));
    }
    let parsed = Url::parse(&url)
        .map_err(|e| AppError::Preview(format!("invalid preview URL {url}: {e}")))?;
    let webview = registry
        .get(&tab_id)
        .ok_or_else(|| AppError::Preview(format!("no open preview webview for tab {tab_id}")))?;
    webview
        .navigate(parsed)
        .map_err(|e| AppError::Preview(format!("navigate failed: {e}")))
}

/// Reload the current page (used by the toolbar's reload button and Phase F4
/// auto-reload). WebView2 has no direct "reload" through wry's safe API, so
/// this re-navigates to the webview's own current URL — equivalent for a
/// Preview tab's purposes (there's no form-resubmission confirmation dialog
/// concern here the way a general browser's reload has).
#[tauri::command]
pub async fn preview_reload(registry: State<'_, PreviewRegistry>, tab_id: String) -> AppResult<()> {
    let webview = registry
        .get(&tab_id)
        .ok_or_else(|| AppError::Preview(format!("no open preview webview for tab {tab_id}")))?;
    let current = webview
        .url()
        .map_err(|e| AppError::Preview(format!("reload: could not read current url: {e}")))?;
    webview
        .navigate(current)
        .map_err(|e| AppError::Preview(format!("reload failed: {e}")))
}

/// Reposition/resize an open Preview tab's webview — called on every pane
/// layout change (split resize, window resize) so the child webview's rect
/// keeps tracking the pane body div `PreviewToolbar.svelte` measures. `x`/`y`
/// are relative to the main window's content area, in logical (CSS) pixels —
/// matching `getBoundingClientRect()` regardless of the OS display scale
/// factor, which is also what keeps a later [`preview_capture`] at
/// CSS-pixel scale rather than a HiDPI-inflated one.
#[tauri::command]
pub async fn preview_set_rect(
    registry: State<'_, PreviewRegistry>,
    tab_id: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> AppResult<()> {
    let webview = registry
        .get(&tab_id)
        .ok_or_else(|| AppError::Preview(format!("no open preview webview for tab {tab_id}")))?;
    webview
        .set_bounds(tauri::Rect {
            position: LogicalPosition::new(x, y).into(),
            size: LogicalSize::new(width, height).into(),
        })
        .map_err(|e| AppError::Preview(format!("set_rect failed: {e}")))
}

/// Hide (not destroy) a Preview tab's webview on tab-switch-away — cheaper
/// than tearing down and rebuilding the child webview on every tab flip, and
/// preserves in-page state (scroll position, form input) the way a real
/// browser tab would. Paired with [`preview_show`].
#[tauri::command]
pub async fn preview_hide(registry: State<'_, PreviewRegistry>, tab_id: String) -> AppResult<()> {
    let webview = registry
        .get(&tab_id)
        .ok_or_else(|| AppError::Preview(format!("no open preview webview for tab {tab_id}")))?;
    webview
        .hide()
        .map_err(|e| AppError::Preview(format!("hide failed: {e}")))
}

/// Show a previously-hidden Preview tab's webview on tab-switch-back.
#[tauri::command]
pub async fn preview_show(registry: State<'_, PreviewRegistry>, tab_id: String) -> AppResult<()> {
    let webview = registry
        .get(&tab_id)
        .ok_or_else(|| AppError::Preview(format!("no open preview webview for tab {tab_id}")))?;
    webview
        .show()
        .map_err(|e| AppError::Preview(format!("show failed: {e}")))
}

/// Destroy a Preview tab's webview on tab close (unlike hide/show, this is
/// permanent — a closed Preview tab that gets reopened calls [`preview_open`]
/// fresh). A missing `tab_id` is a no-op, not an error — closing an already-
/// closed (or never-opened) tab is a normal race during teardown.
#[tauri::command]
pub async fn preview_close(registry: State<'_, PreviewRegistry>, tab_id: String) -> AppResult<()> {
    destroy_if_open(&registry, &tab_id);
    Ok(())
}

/// V14 code-review fix (webview leak): the actual close-and-remove logic
/// shared by [`preview_close`] (frontend `PreviewToolbar.svelte`'s
/// `onDestroy`) AND the backend's own proactive cleanup —
/// `ipc::tab_lifecycle::close_tab` calls this when the closed tab is a
/// Preview tab, and `main.rs`'s `CloseRequested` handler drains the whole
/// registry on app exit. Without a backend-owned path, the child webview
/// was destroyed ONLY by the frontend's `onDestroy`, which a renderer
/// crash, an HMR reload, or an exception could skip entirely, leaking the
/// child webview (and whatever page/resources it holds) for the rest of
/// the process's life. Idempotent — a missing `tab_id` (already closed, or
/// never opened) is a silent no-op, so calling it from multiple paths for
/// the same tab is always safe.
pub(crate) fn destroy_if_open(registry: &PreviewRegistry, tab_id: &str) {
    let Some(webview) = registry.remove(tab_id) else {
        return;
    };
    if let Err(e) = webview.close() {
        warn!(?tab_id, error = %e, "preview: close failed");
    }
}

/// V14 code-review fix (webview leak): best-effort drain of every still-open
/// Preview webview, for `main.rs`'s `CloseRequested` handler on app exit. A
/// renderer that never ran its `onDestroy` (crash, forced quit) would
/// otherwise leave child webviews attached until the whole process tears
/// down anyway — harmless at that point, but this makes the cleanup
/// explicit and deterministic rather than "hope the OS reaps it".
pub fn close_all(registry: &PreviewRegistry) {
    let tab_ids: Vec<String> = {
        let entries = registry.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.keys().cloned().collect()
    };
    for tab_id in tab_ids {
        destroy_if_open(registry, &tab_id);
    }
}

/// Persist the toolbar's live `url`/`device_width`/`auto_reload` back onto
/// the tab's `PreviewTabConfig` (so a restart reopens with the same state),
/// and remember `url` as the project's `preview_last_url` for the next "New
/// Preview tab". A no-op (not an error) if `tab_id` names a non-Preview or
/// already-removed tab — a race during teardown, same tolerance as
/// `preview_close`.
#[tauri::command]
pub async fn preview_update_config(
    state: State<'_, AppState>,
    tab_id: String,
    url: String,
    device_width: Option<u32>,
    auto_reload: bool,
) -> AppResult<()> {
    state.settings.mutate(move |snap| {
        if let Some(TabConfig::Preview(cfg)) = snap.tabs.iter_mut().find(|t| t.id() == tab_id) {
            cfg.url = url.clone();
            cfg.device_width = device_width;
            cfg.auto_reload = auto_reload;
        }
        snap.preview_last_url = Some(url.clone());
    });
    Ok(())
}

/// Snapshot the Preview tab's current viewport to a PNG in the Phase-B
/// attach dir (`crate::attach`), for the toolbar's Snapshot → compose action.
/// Returns the saved path; the frontend then pushes it onto
/// `composeAttachments` and opens the compose overlay, exactly like a pasted
/// clipboard image.
///
/// TODO(spike E0): the capture itself (`capture::capture_to_png` on Windows)
/// compiles against the exact `webview2-com`/`windows` versions this crate's
/// wry pulls in, but producing an actual, pixel-correct, CSS-scale PNG from
/// a live WebView2 instance has not been exercised — see `preview::capture`'s
/// doc comment for the specific COM call and why file-backed capture was
/// chosen over in-memory `IStream` extraction.
#[tauri::command]
pub async fn preview_capture(
    state: State<'_, AppState>,
    registry: State<'_, PreviewRegistry>,
    tab_id: String,
) -> AppResult<std::path::PathBuf> {
    let webview = registry
        .get(&tab_id)
        .ok_or_else(|| AppError::Preview(format!("no open preview webview for tab {tab_id}")))?;
    let dest = crate::attach::reserve_path(&state.launch.launch_id)
        .map_err(|e| AppError::Preview(format!("could not reserve an attach path: {e}")))?;
    capture::capture_to_png(&webview, &dest).await?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── localhost / loopback ────────────────────────────────────────────

    #[test]
    fn localhost_name_allowed() {
        assert!(is_allowed_preview_host("http://localhost:3000", false));
        assert!(is_allowed_preview_host(
            "http://localhost:3000/path?x=1",
            false
        ));
        assert!(is_allowed_preview_host("http://LOCALHOST:8080", false));
    }

    #[test]
    fn loopback_ip_allowed() {
        assert!(is_allowed_preview_host("http://127.0.0.1:8080", false));
        assert!(is_allowed_preview_host("http://127.5.5.5", false));
        assert!(is_allowed_preview_host("http://[::1]:8080", false));
    }

    // ── RFC-1918 private ranges ─────────────────────────────────────────

    #[test]
    fn rfc1918_ranges_allowed() {
        assert!(is_allowed_preview_host("http://10.0.0.5:3000", false));
        assert!(is_allowed_preview_host("http://10.255.255.255", false));
        assert!(is_allowed_preview_host("http://172.16.0.1", false));
        assert!(is_allowed_preview_host("http://172.31.255.255", false));
        assert!(is_allowed_preview_host("http://192.168.1.50", false));
    }

    #[test]
    fn ranges_adjacent_to_rfc1918_are_not_mistaken_for_it() {
        // 172.16-31 only — 172.15 and 172.32 are ordinary public space.
        assert!(!is_allowed_preview_host("http://172.15.255.255", false));
        assert!(!is_allowed_preview_host("http://172.32.0.1", false));
        // Not private ranges at all.
        assert!(!is_allowed_preview_host("http://11.0.0.1", false));
        assert!(!is_allowed_preview_host("http://193.168.1.1", false));
    }

    // ── public hosts ────────────────────────────────────────────────────

    #[test]
    fn public_host_blocked_unless_allow_remote() {
        assert!(!is_allowed_preview_host("https://example.com", false));
        assert!(is_allowed_preview_host("https://example.com", true));
        assert!(!is_allowed_preview_host("http://8.8.8.8", false));
        assert!(is_allowed_preview_host("http://8.8.8.8", true));
    }

    #[test]
    fn bare_lan_hostname_is_not_treated_as_local() {
        // A hostname other than "localhost" can't be classified without a
        // DNS lookup, which this pure function never performs — so it needs
        // allow_remote even though it "looks" like a LAN name.
        assert!(!is_allowed_preview_host("http://my-nas.local", false));
        assert!(is_allowed_preview_host("http://my-nas.local", true));
    }

    // ── malformed input ─────────────────────────────────────────────────

    #[test]
    fn malformed_url_rejected_regardless_of_allow_remote() {
        assert!(!is_allowed_preview_host("not a url", false));
        assert!(!is_allowed_preview_host("not a url", true));
        assert!(!is_allowed_preview_host("", false));
        assert!(!is_allowed_preview_host("", true));
    }

    #[test]
    fn hostless_schemes_rejected_regardless_of_allow_remote() {
        // No `host_str()` at all — file:// and javascript: URLs must never
        // be treated as an "allowed" navigation target just because
        // allow_remote is on; that flag widens trusted HOSTS, not the
        // definition of a well-formed http(s) navigation.
        assert!(!is_allowed_preview_host("file:///etc/passwd", true));
        assert!(!is_allowed_preview_host("javascript:alert(1)", true));
        assert!(!is_allowed_preview_host("about:blank", true));
    }

    #[test]
    fn userinfo_does_not_fool_the_host_check() {
        // The url crate resolves the actual host (evil.com), not whatever
        // precedes an "@" — a naive string-prefix check could be fooled by
        // this, is_allowed_preview_host must not be.
        assert!(!is_allowed_preview_host("http://localhost@evil.com", false));
    }

    #[test]
    fn webview_label_is_distinct_from_the_tab_id() {
        assert_eq!(webview_label("preview-abc"), "preview-preview-abc");
        assert_ne!(webview_label("preview-abc"), "preview-abc");
    }

    // ── V14 code-review FIX 2 (HIGH, security): external-open scheme gate ──

    #[test]
    fn http_and_https_are_externally_openable() {
        assert!(is_externally_openable("http://example.com"));
        assert!(is_externally_openable("https://example.com/path?x=1"));
        assert!(is_externally_openable("HTTPS://Example.Com"));
    }

    #[test]
    fn custom_and_dangerous_schemes_are_never_externally_openable() {
        // The Follina-style RCE vector this fix closes: a registered OS
        // protocol handler must never be reachable from a Preview tab's
        // `window.open()` / rejected-navigation path.
        assert!(!is_externally_openable("ms-msdt:x-msdt-config?..."));
        assert!(!is_externally_openable("file:///etc/passwd"));
        assert!(!is_externally_openable("javascript:alert(1)"));
        assert!(!is_externally_openable(
            "data:text/html,<script>alert(1)</script>"
        ));
        assert!(!is_externally_openable("mailto:someone@example.com"));
        assert!(!is_externally_openable("about:blank"));
        // A hypothetical custom app scheme.
        assert!(!is_externally_openable("myapp://do-something"));
    }

    #[test]
    fn malformed_url_is_not_externally_openable() {
        assert!(!is_externally_openable("not a url"));
        assert!(!is_externally_openable(""));
    }
}

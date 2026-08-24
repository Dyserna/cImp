//! The Settings window's deep links — "open Settings, at this tab / at this
//! section".
//!
//! ## Why this is a service and the rest of the window commands are not
//!
//! Most of what `ipc::commands` does to windows is a host effect with no
//! shaping: focus the Settings window, close it, square off the main window's
//! corners for the TUI themes. Those stay at the boundary, noted there — a
//! service in front of `DwmSetWindowAttribute` is `AppHandle` with extra steps,
//! which is the same reason [`WebviewHost`](crate::service::sink::WebviewHost)
//! is one method wide.
//!
//! The deep link is different, because it is a *protocol* with a frontend twin
//! and two halves that must agree:
//!
//! * the **cold** half stores the target in a one-shot slot the Settings window
//!   drains on mount (`consume_settings_deep_link`), and
//! * the **hot** half emits `settings-deep-link` for a window that is already
//!   open.
//!
//! Both fire on every call, deliberately — either path alone loses the race —
//! so they have to name the same target, and the cold half has to encode
//! *which kind* of target it is in a bare `Option<String>`. That encoding is
//! the `section:` prefix [`SettingsDeepLink::to_section`] writes and
//! `SettingsApp.svelte` routes on. It had no test, and a prefix that drifts on
//! one side is a nudge chip that silently opens the wrong pane.

use std::sync::Mutex;

use crate::error::AppResult;
use crate::service::sink::{EventSink, EventSinkExt};

/// The `settings-deep-link` event name, and the window label it is targeted at.
/// Named here so the two halves of the protocol are spelled once.
pub const DEEP_LINK_EVENT: &str = "settings-deep-link";

/// The prefix the cold slot uses to mark a SECTION target. A bare target is a
/// tab id; `SettingsApp.svelte`'s consume path routes on exactly this string.
pub const SECTION_PREFIX: &str = "section:";

/// Opening or focusing the Settings window — the one host effect the deep-link
/// use case needs and cannot do itself.
///
/// A narrow trait for the same reason
/// [`GraphIndexHost`](crate::service::sink::GraphIndexHost) is one: the
/// implementor is a live Tauri app, so naming it concretely would make the
/// ordering below untestable. Separate from `WebviewHost` because that trait's
/// implementor is the Preview registry, which has no windows to open.
pub trait SettingsWindow: Send + Sync {
    /// Open the Settings window, or focus it if it is already open.
    fn open_or_focus(&self) -> AppResult<()>;

    /// The window's label, for the TARGETED emit below. Carried on the trait
    /// rather than read from `ipc::windows` so this module names nothing in the
    /// Tauri boundary — and the emit has to be targeted: broadcasting
    /// `settings-deep-link` would hand every webview a navigation instruction
    /// meant for one.
    fn label(&self) -> &str;
}

/// The deep-link use cases, over the one-shot slot `AppState` owns.
pub struct SettingsDeepLink<'a> {
    pending: &'a Mutex<Option<String>>,
}

impl<'a> SettingsDeepLink<'a> {
    pub fn new(pending: &'a Mutex<Option<String>>) -> Self {
        Self { pending }
    }

    /// V1.4-07 A: open the Settings window scrolled to one tab's section — the
    /// right-click "Configure tab" entry on AI tabs.
    ///
    /// Cold and hot both fire, in this order: the slot is armed BEFORE the
    /// window is opened, because a window that opens fast enough drains the
    /// slot on mount and must find the target already there. The event is sent
    /// after, for a window that was open all along and will never mount again.
    pub fn to_tab(&self, window: &dyn SettingsWindow, sink: &dyn EventSink, tab: &str) -> AppResult<()> {
        self.arm(tab.to_string());
        window.open_or_focus()?;
        let _ = sink.emit_to_window(
            window.label(),
            DEEP_LINK_EVENT,
            &serde_json::json!({ "kind": "tab", "tab_id": tab }),
        );
        Ok(())
    }

    /// V22 Phase E: open the Settings window at a top-level sidebar section
    /// (not a tab) — the Code Intelligence "suggested checks" nudge chip.
    ///
    /// Same cold/hot plumbing as [`to_tab`](Self::to_tab), with the stored
    /// target tagged [`SECTION_PREFIX`] so the consume path routes it to
    /// `activeSection` instead of a tab scroll. A section id and a tab id are
    /// both bare strings, so without the tag the cold half cannot say which it
    /// has.
    pub fn to_section(
        &self,
        window: &dyn SettingsWindow,
        sink: &dyn EventSink,
        section: &str,
    ) -> AppResult<()> {
        self.arm(format!("{SECTION_PREFIX}{section}"));
        window.open_or_focus()?;
        let _ = sink.emit_to_window(
            window.label(),
            DEEP_LINK_EVENT,
            &serde_json::json!({ "kind": "section", "section": section }),
        );
        Ok(())
    }

    /// Read and clear the pending target — pulled by `SettingsApp.svelte` on
    /// mount. `None` when nothing is pending, and a second read after a
    /// successful one is also `None`: the slot is one-shot, so re-mounting the
    /// Settings window does not re-navigate it somewhere the user has since
    /// scrolled away from.
    pub fn take(&self) -> Option<String> {
        self.pending.lock().ok().and_then(|mut g| g.take())
    }

    /// A poisoned slot is dropped rather than propagated: the hot half still
    /// fires, so the deep link degrades to "works if the window is already
    /// open" instead of refusing to open Settings at all.
    fn arm(&self, target: String) {
        if let Ok(mut slot) = self.pending.lock() {
            *slot = Some(target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::sink::testing::RecordingEventSink;

    /// A Settings window that records that it was asked to open.
    struct FakeWindow(std::sync::atomic::AtomicUsize);

    impl SettingsWindow for FakeWindow {
        fn open_or_focus(&self) -> AppResult<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }

        fn label(&self) -> &str {
            "settings"
        }
    }

    /// **Previously "right-click a tab, choose Configure tab, look at the
    /// Settings window".**
    ///
    /// Both halves fire and both name the same target, which is what makes the
    /// cold/hot pair a race-free deep link rather than two chances to be wrong.
    /// The slot is one-shot: the second read is empty, so a Settings window
    /// that re-mounts does not jump the user back.
    #[test]
    fn a_tab_deep_link_arms_the_slot_and_announces_the_same_target() {
        let pending = Mutex::new(None);
        let link = SettingsDeepLink::new(&pending);
        let window = FakeWindow(0.into());
        let sink = RecordingEventSink::default();

        link.to_tab(&window, &sink, "claude").expect("deep link");
        assert_eq!(window.0.load(std::sync::atomic::Ordering::Relaxed), 1);

        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, DEEP_LINK_EVENT);
        assert_eq!(
            events[0].window.as_deref(),
            Some("settings"),
            "a navigation instruction goes to the one window it is about"
        );
        assert_eq!(events[0].payload, r#"{"kind":"tab","tab_id":"claude"}"#);

        assert_eq!(link.take().as_deref(), Some("claude"));
        assert_eq!(link.take(), None, "the slot is one-shot");
    }

    /// **The `section:` prefix is the protocol**, and it is the only thing that
    /// tells the cold half a section from a tab — both are bare strings. A
    /// section target must therefore NOT round-trip as a tab id, which is what
    /// the second assertion pins.
    #[test]
    fn a_section_deep_link_is_tagged_so_the_cold_path_can_tell_them_apart() {
        let pending = Mutex::new(None);
        let link = SettingsDeepLink::new(&pending);
        let window = FakeWindow(0.into());
        let sink = RecordingEventSink::default();

        link.to_section(&window, &sink, "checks").expect("deep link");
        assert_eq!(
            sink.events()[0].payload,
            r#"{"kind":"section","section":"checks"}"#
        );

        let cold = link.take().expect("armed");
        assert_eq!(cold, "section:checks");
        assert!(
            cold.starts_with(SECTION_PREFIX),
            "SettingsApp.svelte routes on this prefix"
        );
        assert_ne!(cold, "checks", "an untagged target reads as a tab id");
    }
}

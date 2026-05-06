//! Notification queue + dedup + play-when-idle scheduler.
//!
//! Subscribes to the in-process `StateEvent` broadcast and the audio idle
//! `Notify`. State edges enqueue notifications (subject to the global
//! announcements toggle and active-tab filter). Idle edges trigger a drain:
//! we collapse per-tab duplicates, then dispatch each survivor as a
//! `SynthesizeNotification` TTS request in arrival order.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// Drain debounce: when a notification is enqueued and audio is currently
/// idle, wait this long before draining. Lets closely-spaced related events
/// (e.g. ClaudeOutputStopped → Idle followed shortly by a permission
/// detection on the same tab) land in the queue together so dedup can
/// collapse them to the most informative one.
const DRAIN_DEBOUNCE: Duration = Duration::from_millis(200);

use crate::audio::AudioOutput;
use crate::settings::SettingsHandle;
use crate::state::{AvatarState, StateEvent, TabId, TabKind};
use crate::tts::{ActiveTab, TtsRequest};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NotificationEvent {
    Idle,
    AwaitingPermission,
    Error,
    /// Shell-only: subprocess exited while the user was on a different tab.
    /// Phase 5 of MILESTONE-V3-01.
    Exited,
}

/// Per-kind allowlist of notification triggers. AI tabs get the v1.1 trio;
/// Shell tabs get only `Error` and the new `Exited`. Defense-in-depth: the
/// upstream code already won't generate Idle/AwaitingPermission for Shell
/// tabs (the avatar machine's per-kind gating in Phase 4 makes those edges
/// unreachable), but the explicit gate makes the rule grep-able.
fn allowed_for(kind: &TabKind, event: NotificationEvent) -> bool {
    match (kind, event) {
        (TabKind::AiTool(_), NotificationEvent::Idle) => true,
        (TabKind::AiTool(_), NotificationEvent::AwaitingPermission) => true,
        (TabKind::AiTool(_), NotificationEvent::Error) => true,
        (TabKind::AiTool(_), NotificationEvent::Exited) => false,
        (TabKind::Shell, NotificationEvent::Error) => true,
        (TabKind::Shell, NotificationEvent::Exited) => true,
        (TabKind::Shell, NotificationEvent::Idle) => false,
        (TabKind::Shell, NotificationEvent::AwaitingPermission) => false,
    }
}

#[derive(Clone, Debug)]
struct Queued {
    tab: TabId,
    #[allow(dead_code)] // retained for tracing/future use; logged on enqueue
    event: NotificationEvent,
    text: String,
    timestamp: Instant,
}

/// Spawn the notification manager on the Tauri/tokio runtime. Returns
/// nothing — failures are logged and the loop ends naturally if any of its
/// inputs close.
pub fn spawn_notification_manager(
    state_events: broadcast::Receiver<StateEvent>,
    audio: Arc<AudioOutput>,
    tts_tx: mpsc::Sender<TtsRequest>,
    settings: SettingsHandle,
    active: ActiveTab,
    initial_active: TabId,
) {
    let manager = NotificationManager::new(
        state_events,
        audio,
        tts_tx,
        settings,
        active,
        initial_active,
    );
    tauri::async_runtime::spawn(manager.run());
}

struct NotificationManager {
    state_events: broadcast::Receiver<StateEvent>,
    audio: Arc<AudioOutput>,
    tts_tx: mpsc::Sender<TtsRequest>,
    settings: SettingsHandle,
    active: ActiveTab,
    queue: Vec<Queued>,
    /// Most-recent observed avatar state per tab. The state manager already
    /// only emits StateChanged on real transitions, but it does re-emit
    /// initial Idle on startup; the cache lets us reject re-fires without
    /// queueing a phantom `idle` notification before any real activity.
    last_avatar: HashMap<TabId, AvatarState>,
    last_awaiting: HashMap<TabId, bool>,
    /// Set when something is enqueued and we haven't yet drained. The
    /// run loop selects on `sleep_until(deadline)` and drains when it
    /// fires. Cleared by both the deadline arm and the audio idle-edge
    /// arm so the next enqueue can re-schedule cleanly.
    drain_deadline: Option<Instant>,
}

impl NotificationManager {
    fn new(
        state_events: broadcast::Receiver<StateEvent>,
        audio: Arc<AudioOutput>,
        tts_tx: mpsc::Sender<TtsRequest>,
        settings: SettingsHandle,
        active: ActiveTab,
        initial_active: TabId,
    ) -> Self {
        let mut last_avatar = HashMap::new();
        let mut last_awaiting = HashMap::new();
        for t in TabId::all() {
            last_avatar.insert(t.clone(), AvatarState::Idle);
            last_awaiting.insert(t, false);
        }
        let _ = initial_active; // reserved; not currently used outside `active`.
        Self {
            state_events,
            audio,
            tts_tx,
            settings,
            active,
            queue: Vec::new(),
            last_avatar,
            last_awaiting,
            drain_deadline: None,
        }
    }

    async fn run(mut self) {
        let idle_notify = self.audio.idle_notify();
        loop {
            // `sleep_until` needs a concrete Instant; when no drain is
            // scheduled we await `pending` so this arm never fires. Copy
            // the deadline out so the async block can be `move`.
            let deadline = self.drain_deadline;
            tokio::select! {
                biased;
                evt = self.state_events.recv() => {
                    match evt {
                        Ok(e) => self.on_state_event(e),
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "notifications: state-event broadcast lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = idle_notify.notified() => {
                    self.try_drain().await;
                    self.drain_deadline = None;
                }
                _ = async move {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.try_drain().await;
                    self.drain_deadline = None;
                }
            }
        }
        debug!("notification manager: state-event channel closed; exiting");
    }

    fn on_state_event(&mut self, event: StateEvent) {
        let queued = match event {
            StateEvent::StateChanged { tab, state } => {
                let prev = self
                    .last_avatar
                    .insert(tab.clone(), state)
                    .unwrap_or(AvatarState::Idle);
                if prev == state {
                    return;
                }
                let active_tab = self.active.read().expect("active tab poisoned").clone();
                if tab == active_tab {
                    return;
                }
                let nev = match state {
                    AvatarState::Idle if prev != AvatarState::Idle => {
                        // Suppress Idle when this tab currently has a
                        // pending permission prompt — the avatar state
                        // machine drops to Idle when Claude stops printing
                        // (which is exactly what happens just before a
                        // permission prompt), so without this filter the
                        // user hears "awaiting permission" followed by
                        // "idle" for the same logical event.
                        if self.last_awaiting.get(&tab).copied().unwrap_or(false) {
                            debug!(?tab, "notifications: suppressing Idle (awaiting permission)");
                            return;
                        }
                        NotificationEvent::Idle
                    }
                    AvatarState::Error => NotificationEvent::Error,
                    _ => return,
                };
                self.try_enqueue(tab, nev)
            }
            StateEvent::AwaitingPermissionChanged { tab, awaiting } => {
                let prev = self.last_awaiting.insert(tab.clone(), awaiting).unwrap_or(false);
                if prev == awaiting || !awaiting {
                    return;
                }
                let active_tab = self.active.read().expect("active tab poisoned").clone();
                if tab == active_tab {
                    return;
                }
                self.try_enqueue(tab, NotificationEvent::AwaitingPermission)
            }
            StateEvent::TabClosedStateChanged {
                tab,
                closed,
                exit_code: _,
            } => {
                // Only the closed=true edge fires a notification; the
                // closed=false edge (restart) is a UI-only transition.
                if !closed {
                    return;
                }
                let active_tab = self.active.read().expect("active tab poisoned").clone();
                if tab == active_tab {
                    return;
                }
                self.try_enqueue(tab, NotificationEvent::Exited)
            }
            StateEvent::ActiveTabChanged { .. } | StateEvent::DoneWhileAwayChanged { .. } => {
                return;
            }
        };

        // Schedule a debounced drain. The delay lets closely-spaced
        // related events (e.g. permission detection arriving microseconds
        // after the Idle that preceded it) land in the queue together so
        // dedup can collapse them. If `drain_deadline` is already set the
        // earlier deadline stands — no point pushing it out on every event.
        if queued && self.drain_deadline.is_none() {
            self.drain_deadline = Some(Instant::now() + DRAIN_DEBOUNCE);
        }
    }

    fn try_enqueue(&mut self, tab: TabId, event: NotificationEvent) -> bool {
        let settings = self.settings.current();
        if !settings.behavior.announcements_enabled {
            return false;
        }
        if !allowed_for(&tab.kind(), event) {
            debug!(?tab, ?event, "notifications: dropped (disallowed for kind)");
            return false;
        }
        let text = notification_text(&settings, &tab, event);
        if text.is_empty() {
            // Per design: empty text disables the (tab, event) announcement.
            return false;
        }
        debug!(?tab, ?event, "notifications: queued");
        self.queue.push(Queued {
            tab,
            event,
            text,
            timestamp: Instant::now(),
        });
        true
    }

    async fn try_drain(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.audio.is_playing() {
            // Notify fired earlier but new playback started before we ran;
            // wait for the next idle edge.
            return;
        }
        let drained = std::mem::take(&mut self.queue);
        let surviving = dedup_per_tab(&drained);
        info!(
            queued = drained.len(),
            playing = surviving.len(),
            "notifications: draining"
        );
        for n in surviving {
            let req = TtsRequest::SynthesizeNotification {
                tab: n.tab,
                text: n.text,
            };
            if let Err(e) = self.tts_tx.send(req).await {
                warn!(error = %e, "notifications: tts channel closed");
                return;
            }
        }
    }
}

fn notification_text(
    settings: &crate::settings::Settings,
    tab: &TabId,
    event: NotificationEvent,
) -> String {
    match tab {
        TabId::Claude | TabId::Aider => {
            let tab_settings = if matches!(tab, TabId::Claude) {
                &settings.tabs.claude
            } else {
                &settings.tabs.aider
            };
            match event {
                NotificationEvent::Idle => tab_settings.notifications.idle.clone(),
                NotificationEvent::AwaitingPermission => {
                    tab_settings.notifications.awaiting_permission.clone()
                }
                NotificationEvent::Error => tab_settings.notifications.error.clone(),
                NotificationEvent::Exited => String::new(), // disallowed; defensive
            }
        }
        TabId::Shell(_) => {
            // M1 has a single hardcoded Shell tab (`shell-1`); the interim
            // settings field carries its strings. M3 swaps to per-tab
            // lookup against the unified `tabs` array. The `{code}`
            // placeholder is intentionally NOT interpolated yet — M4
            // delivers that.
            match event {
                NotificationEvent::Error => settings.shell_1_tmp.notifications.error.clone(),
                NotificationEvent::Exited => settings.shell_1_tmp.notifications.exited.clone(),
                NotificationEvent::Idle | NotificationEvent::AwaitingPermission => String::new(),
            }
        }
    }
}

/// Per-tab dedup at play-time: keep only the most recent notification per
/// tab, in the order each tab first appeared in the queue. Preserves the
/// "first-arriving tab plays first" semantic while collapsing repeats from
/// the same tab to whichever announcement is freshest.
fn dedup_per_tab(queue: &[Queued]) -> Vec<Queued> {
    let mut latest: HashMap<TabId, &Queued> = HashMap::new();
    for q in queue {
        latest
            .entry(q.tab.clone())
            .and_modify(|cur| {
                if q.timestamp > cur.timestamp {
                    *cur = q;
                }
            })
            .or_insert(q);
    }
    let mut out = Vec::with_capacity(latest.len());
    let mut emitted: HashSet<TabId> = HashSet::new();
    for q in queue {
        if emitted.insert(q.tab.clone()) {
            if let Some(winner) = latest.get(&q.tab) {
                out.push((*winner).clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(tab: TabId, event: NotificationEvent, text: &str, t_ms: u64) -> Queued {
        Queued {
            tab,
            event,
            text: text.to_string(),
            // Tests only compare via `>` on Instant, so we anchor everything
            // to a single base and offset by ms.
            timestamp: Instant::now() + std::time::Duration::from_millis(t_ms),
        }
    }

    #[test]
    fn dedup_keeps_only_latest_per_single_tab() {
        let queue = vec![
            q(TabId::Claude, NotificationEvent::Idle, "first", 0),
            q(TabId::Claude, NotificationEvent::AwaitingPermission, "second", 10),
            q(TabId::Claude, NotificationEvent::Error, "third", 20),
        ];
        let out = dedup_per_tab(&queue);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "third");
    }

    #[test]
    fn dedup_preserves_first_appearing_tab_order() {
        let queue = vec![
            q(TabId::Aider, NotificationEvent::Idle, "aider-old", 0),
            q(TabId::Claude, NotificationEvent::Idle, "claude-old", 5),
            q(TabId::Aider, NotificationEvent::Error, "aider-new", 10),
            q(TabId::Claude, NotificationEvent::Error, "claude-new", 15),
        ];
        let out = dedup_per_tab(&queue);
        assert_eq!(out.len(), 2);
        // Aider first because it appeared first in the queue.
        assert_eq!(out[0].tab, TabId::Aider);
        assert_eq!(out[0].text, "aider-new");
        assert_eq!(out[1].tab, TabId::Claude);
        assert_eq!(out[1].text, "claude-new");
    }

    #[test]
    fn dedup_passes_single_through_unchanged() {
        let queue = vec![q(TabId::Claude, NotificationEvent::Idle, "only", 0)];
        let out = dedup_per_tab(&queue);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "only");
    }

    #[test]
    fn dedup_handles_empty_queue() {
        let out = dedup_per_tab(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn allowlist_ai_tabs_get_v1_trio_only() {
        let kind = TabKind::AiTool(crate::state::AiToolKind::ClaudeCode);
        assert!(allowed_for(&kind, NotificationEvent::Idle));
        assert!(allowed_for(&kind, NotificationEvent::AwaitingPermission));
        assert!(allowed_for(&kind, NotificationEvent::Error));
        assert!(!allowed_for(&kind, NotificationEvent::Exited));
    }

    #[test]
    fn allowlist_shell_tabs_get_error_and_exited() {
        let kind = TabKind::Shell;
        assert!(allowed_for(&kind, NotificationEvent::Error));
        assert!(allowed_for(&kind, NotificationEvent::Exited));
        assert!(!allowed_for(&kind, NotificationEvent::Idle));
        assert!(!allowed_for(&kind, NotificationEvent::AwaitingPermission));
    }
}

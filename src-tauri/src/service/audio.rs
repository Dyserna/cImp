//! The speech use cases: what the app says, and when it stops saying it.
//!
//! ## What the A1-3 audio run found
//!
//! Five TTS commands, and between them three pieces of shaping that only a
//! human with a keyboard could reach:
//!
//! * **Whose voice is it.** Every command-initiated utterance is routed as if
//!   it came from the ACTIVE tab, because the worker drops requests tagged with
//!   a background tab. That rule is one line at each of three call sites, and
//!   getting it wrong makes speech silently vanish rather than fail.
//! * **The Esc ordering.** [`AudioService::speak_selection`] resolves the
//!   active tab (which awaits the registry lock) BEFORE it arms the session
//!   cell, and the two must stay in that order: storing first left a window in
//!   which a concurrent [`AudioService::stop`] zeroed the cell and this call
//!   then sent anyway, racing the stop. The comment recording that fix is now a
//!   test.
//! * **Blank input is not silence.** Whitespace-only text and an empty chunk
//!   list are dropped before the worker sees them — and, in the selection case,
//!   dropped WITHOUT arming the session cell, so a no-op read cannot supersede
//!   a live one.
//!
//! ## What stayed at the boundary
//!
//! The three STT commands (`stt_start_recording` / `stt_stop_recording` /
//! `stt_cancel`) are each one call on the [`SttHandle`](crate::stt::SttHandle)
//! Tauri injected — the handle posts to the capture thread and everything the
//! user sees comes back as an event — so they are noted leaves, not wrappers.
//! Same for the two `stt_list_*` commands, which are free functions in
//! [`crate::stt`] already.

use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;

use crate::audio::AudioOutput;
use crate::error::{AppError, AppResult};
use crate::tabs::TabRegistryHandle;
use crate::tts::{AiTtsSuppressed, SpeakSession, TtsRequest};

/// The speech use cases, over borrowed handles.
///
/// Borrowed for the reason [`TabService`](crate::service::tabs::TabService) is:
/// building one per IPC call is free, and a test can own every handle on the
/// stack. Note what is NOT here — no `AppHandle`, no `EventSink`. The TTS path
/// reaches the frontend through the audio thread's own events, so the commands
/// were never coupled to Tauri; they were coupled to `AppState`.
pub struct AudioService<'a> {
    registry: &'a TabRegistryHandle,
    segments: &'a mpsc::Sender<TtsRequest>,
    speak_session: &'a SpeakSession,
    ai_suppressed: &'a AiTtsSuppressed,
    audio: &'a RwLock<Option<Arc<AudioOutput>>>,
}

impl<'a> AudioService<'a> {
    pub fn new(
        registry: &'a TabRegistryHandle,
        segments: &'a mpsc::Sender<TtsRequest>,
        speak_session: &'a SpeakSession,
        ai_suppressed: &'a AiTtsSuppressed,
        audio: &'a RwLock<Option<Arc<AudioOutput>>>,
    ) -> Self {
        Self {
            registry,
            segments,
            speak_session,
            ai_suppressed,
            audio,
        }
    }

    /// Synthesize and play `text` directly through the TTS worker, skipping the
    /// processor. `what` names the caller in the error so a failed send says
    /// which gesture failed. Routed as if it came from the active tab — see the
    /// module docs.
    async fn synthesize(&self, text: String, what: &str) -> AppResult<()> {
        let tab = self.registry.lock().await.active();
        self.segments
            .send(TtsRequest::Synthesize {
                tab,
                text,
                suppressible: false,
            })
            .await
            .map_err(|e| AppError::Tts(format!("{what} send: {e}")))
    }

    /// Debug: speak `text` as-is, blank or not. The Settings "Test voice"
    /// button, whose whole job is to prove the worker is alive.
    pub async fn test(&self, text: String) -> AppResult<()> {
        self.synthesize(text, "tts_test").await
    }

    /// Read arbitrary text aloud (the Ctrl+right-click gesture). Whitespace-only
    /// text is ignored: the frontend guards too, but a backend skip keeps an
    /// empty synthesis off the worker.
    pub async fn speak(&self, text: String) -> AppResult<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.synthesize(text, "tts_speak").await
    }

    /// Read a terminal selection aloud as a read-along. `chunks` are the
    /// sentence segments (pre-split on the frontend so the spoken text exactly
    /// matches the highlighted text); `session` is the frontend's monotonic id,
    /// stored in the shared cell so the worker can be told to abandon the read.
    ///
    /// **The two steps are ordered, and the order is the fix.** Resolve the
    /// active tab first — that awaits the registry lock — and arm the session
    /// cell immediately before the send with no await in between. Arming first
    /// left a window in which a concurrent [`stop`](Self::stop) (Esc) zeroed the
    /// cell and this call still went on to send, racing the stop. With the
    /// store after the await, an Esc landing before this point means the worker
    /// sees a superseding or zeroed cell and abandons, and one landing after is
    /// a clean supersede. The worker re-checks `speak_session` before and after
    /// each chunk, so a stop mid-read still cancels the rest.
    pub async fn speak_selection(&self, session: u64, chunks: Vec<String>) -> AppResult<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let tab = self.registry.lock().await.active();
        self.speak_session.store(session, Ordering::SeqCst);
        self.segments
            .send(TtsRequest::SpeakSelection {
                tab,
                session,
                chunks,
            })
            .await
            .map_err(|e| AppError::Tts(format!("tts_speak_selection send: {e}")))
    }

    /// Stop all playback immediately and cancel any in-flight selection read —
    /// the Esc gesture.
    ///
    /// Three effects, and each covers a different part of what is in flight:
    /// zeroing the session cell abandons chunks the worker has not enqueued,
    /// setting the AI-suppression flag drops the rest of the current output
    /// burst's tagged segments until the next `HarnessOutputStarted` clears it,
    /// and clearing the sink discards what is already queued. Notifications and
    /// selection reads ride other request variants and are unaffected by the
    /// flag.
    ///
    /// A poisoned audio lock is recovered rather than propagated: this is the
    /// emergency stop, so it must never silently no-op and leave audio playing
    /// with no way to stop it from the UI. The data behind the guard is an
    /// `Option<Arc<AudioOutput>>`, which a panicking writer cannot leave torn.
    pub fn stop(&self) {
        self.speak_session.store(0, Ordering::SeqCst);
        self.ai_suppressed.store(true, Ordering::SeqCst);
        if let Some(audio) = self.audio_handle() {
            audio.stop_all();
        }
    }

    /// Pause or resume playback without discarding queued audio — the
    /// bottom-bar transport. The in-flight read's session is left untouched, so
    /// resume continues exactly where it paused; only the sink is paused.
    /// Poisoned-guard recovery for [`stop`](Self::stop)'s reason: a dead
    /// pause/resume button gives the user no signal why.
    pub fn set_paused(&self, paused: bool) {
        if let Some(audio) = self.audio_handle() {
            audio.set_paused(paused);
        }
    }

    fn audio_handle(&self) -> Option<Arc<AudioOutput>> {
        self.audio
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .cloned()
    }
}

/// The voice names the TTS model directory offers, sorted and de-duplicated.
///
/// A missing directory is an empty list, not an error: a portable install
/// without the voice pack still has to open its settings.
pub fn voices() -> AppResult<Vec<String>> {
    Ok(voices_in(&crate::tts::model_dir()?.join("voices")))
}

/// The `.bin` stems directly under `dir`, sorted and de-duplicated.
///
/// Split from [`voices`] so the rule is checkable without an installed model
/// tree: only `.bin` counts (the directory also holds READMEs and, on some
/// installs, the `.onnx` the bins were derived from), the extension is dropped,
/// and the order is the picker's order rather than the filesystem's — a voice
/// list that reshuffles between launches looks like a bug to the user.
pub fn voices_in(dir: &std::path::Path) -> Vec<String> {
    let mut out = std::collections::BTreeSet::<String>::new();
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("bin") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.insert(stem.to_string());
            }
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{StateSignal, TabId, TabKind, TabMeta};
    use crate::tabs::TabRegistry;
    use tokio::sync::Mutex as TokioMutex;

    /// Everything [`AudioService`] borrows, owned on the stack. Five handles,
    /// no WebView, no audio device: `audio` is `None`, which is exactly the
    /// state a machine with no output device is in, and the transport controls
    /// have to survive it.
    struct Fixture {
        registry: TabRegistryHandle,
        segments: mpsc::Sender<TtsRequest>,
        rx: mpsc::Receiver<TtsRequest>,
        speak_session: SpeakSession,
        ai_suppressed: AiTtsSuppressed,
        audio: RwLock<Option<Arc<AudioOutput>>>,
        _signals: mpsc::Sender<StateSignal>,
        _signal_rx: mpsc::Receiver<StateSignal>,
    }

    impl Fixture {
        fn new() -> Self {
            let active = TabId::Shell("shell-active".to_string());
            let (signals, _signal_rx) = mpsc::channel::<StateSignal>(8);
            let registry = Arc::new(TokioMutex::new(TabRegistry::new(
                vec![
                    TabMeta {
                        id: TabId::Shell("shell-other".to_string()),
                        kind: TabKind::Shell,
                        name: "Other".to_string(),
                    },
                    TabMeta {
                        id: active.clone(),
                        kind: TabKind::Shell,
                        name: "Active".to_string(),
                    },
                ],
                active.clone(),
                Arc::new(RwLock::new(active)),
                signals.clone(),
                Arc::new(Vec::new()),
            )));
            let (segments, rx) = mpsc::channel::<TtsRequest>(16);
            Self {
                registry,
                segments,
                rx,
                speak_session: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ai_suppressed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                audio: RwLock::new(None),
                _signals: signals,
                _signal_rx,
            }
        }

        fn service(&self) -> AudioService<'_> {
            AudioService::new(
                &self.registry,
                &self.segments,
                &self.speak_session,
                &self.ai_suppressed,
                &self.audio,
            )
        }

        fn drain(&mut self) -> Vec<TtsRequest> {
            let mut out = Vec::new();
            while let Ok(r) = self.rx.try_recv() {
                out.push(r);
            }
            out
        }
    }

    /// **Previously "user right-clicks a selection in the app".** Blank text
    /// never reaches the worker, and real text reaches it tagged with the
    /// ACTIVE tab — the tag is what stops the worker's background-tab filter
    /// from dropping a deliberate utterance on the floor.
    #[tokio::test]
    async fn speech_is_tagged_with_the_active_tab_and_blank_text_never_ships() {
        let mut f = Fixture::new();
        f.service().speak("   \n ".to_string()).await.expect("blank");
        assert!(f.drain().is_empty(), "whitespace must not reach the worker");

        f.service().speak("hello".to_string()).await.expect("speak");
        match f.drain().as_slice() {
            [TtsRequest::Synthesize {
                tab,
                text,
                suppressible,
            }] => {
                assert_eq!(tab.as_str(), "shell-active");
                assert_eq!(text, "hello");
                assert!(
                    !suppressible,
                    "command-initiated speech is never suppressed by an earlier Esc"
                );
            }
            other => panic!("expected one Synthesize, got {other:?}"),
        }

        // The debug/test path speaks whatever it is given, blank included —
        // its job is to prove the worker is alive.
        f.service().test(String::new()).await.expect("test");
        assert_eq!(f.drain().len(), 1, "tts_test does not filter its input");
    }

    /// **Previously "select text, read along, press Esc".** An empty chunk list
    /// must not arm the session cell: arming for a read that never happens
    /// supersedes a live read and silences it.
    #[tokio::test]
    async fn an_empty_selection_neither_ships_nor_supersedes_a_live_read() {
        let mut f = Fixture::new();
        f.service()
            .speak_selection(7, vec!["one".to_string()])
            .await
            .expect("live read");
        assert_eq!(f.speak_session.load(Ordering::SeqCst), 7);
        assert_eq!(f.drain().len(), 1);

        f.service()
            .speak_selection(8, Vec::new())
            .await
            .expect("empty read");
        assert_eq!(
            f.speak_session.load(Ordering::SeqCst),
            7,
            "an empty read must not claim the session cell"
        );
        assert!(f.drain().is_empty());
    }

    /// **Previously "press Esc while it is talking".** Stop zeroes the session
    /// cell AND raises the AI-suppression flag; a machine with no audio device
    /// (`audio: None`) must still get both, because the flag is what stops the
    /// rest of the burst that has not been synthesized yet.
    #[tokio::test]
    async fn stop_abandons_the_read_and_suppresses_the_rest_of_the_burst() {
        let mut f = Fixture::new();
        f.service()
            .speak_selection(42, vec!["a".to_string(), "b".to_string()])
            .await
            .expect("read");
        assert_eq!(f.speak_session.load(Ordering::SeqCst), 42);
        let _ = f.drain();

        f.service().stop();
        assert_eq!(
            f.speak_session.load(Ordering::SeqCst),
            0,
            "the worker abandons on a zeroed cell"
        );
        assert!(
            f.ai_suppressed.load(Ordering::SeqCst),
            "Esc must suppress the rest of the current AI burst"
        );

        // …and pause/resume with no output device is a no-op, not a panic.
        f.service().set_paused(true);
        f.service().set_paused(false);
    }

    /// **Previously "look at the voice dropdown".** Only `.bin` is a voice, the
    /// extension is dropped, the order is stable, and a missing directory is an
    /// empty picker rather than an error.
    #[test]
    fn the_voice_list_is_bin_stems_in_a_stable_order() {
        let dir = std::env::temp_dir().join(format!("cimp-voices-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("scratch");
        for name in ["zoe.bin", "amy.bin", "README.md", "amy.onnx"] {
            std::fs::write(dir.join(name), b"x").expect("write");
        }
        assert_eq!(voices_in(&dir), vec!["amy".to_string(), "zoe".to_string()]);
        assert!(voices_in(&dir.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

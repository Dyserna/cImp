//! The two things only a live Tauri app can do, behind traits.
//!
//! A `#[tauri::command]` body that calls `app.emit(...)` or `channel.send(...)`
//! cannot run outside a WebView, and neither can anything that calls it. These
//! two traits are the whole of that coupling for the V42 Phase 0 slice:
//!
//! * [`EventSink`] — the app's 17 broadcast events (`app.emit`) and 3
//!   window-targeted ones (`app.emit_to`).
//! * [`OutputSink`] — one per live PTY: the `Channel<String>` the terminal
//!   bytes are base64'd into. Separate from `EventSink` on purpose; see below.
//!
//! There is a third kind of coupling the spike found, and it is worth naming
//! because a single "EventSink" would have hidden it: [`WebviewHost`], for the
//! side effects that are not events at all — destroying a Preview tab's child
//! webview, opening or focusing the Settings window. Those go through the app
//! handle too, but they are commands to the host, not notifications from it,
//! and folding them into an event trait would have made the trait a synonym for
//! `AppHandle`.
//!
//! ## Why events and PTY output are two traits
//!
//! They have opposite cost profiles. An event is rare (a tab opened, a graph
//! build finished) and its payload is a small struct, so a `Serialize` bound
//! and one JSON encode per call is free. PTY output is the app's hottest path —
//! a `Vec<u8>` every 50 ms flush tick per live tab, already paying a base64
//! encode and a `String` allocation — and it carries an opaque, already-encoded
//! payload that must not be re-serialized. One trait covering both would either
//! have made events clumsy or made terminal bytes pay for a JSON round trip.
//!
//! ## Payload encoding
//!
//! [`EventSink`]'s object-safe methods take a `&RawValue` — JSON that is
//! already text. The generic [`EventSinkExt`] wrapper does the one encode, and
//! [`TauriEventSink`] hands the bytes to Tauri verbatim (Tauri would serialize
//! to JSON anyway, so nothing is paid twice). The alternative — a
//! `serde_json::Value` parameter — costs a whole `Value` tree per emit, which
//! is nothing for `tab-created` and is emphatically not nothing for
//! `audio-amplitude`/`mic-amplitude`, which carry a sample buffer ~50 times a
//! second. Neither of those is in the Phase 0 slice; the encoding is chosen so
//! that wrapping them later is not a regression.

use serde::Serialize;
use serde_json::value::RawValue;
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, EventTarget};

/// An emit that the host refused. Carries the host's own message so a caller
/// that propagates it (`restart_shell_tab` does) can keep its wording.
#[derive(Debug)]
pub struct EmitFailed(pub String);

impl std::fmt::Display for EmitFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where the core sends its named events. Object-safe; see the module docs for
/// why the payload is pre-encoded JSON.
///
/// Callers should use [`EventSinkExt::emit`] / [`EventSinkExt::emit_to_window`]
/// rather than these directly — the ext trait does the encoding.
pub trait EventSink: Send + Sync {
    /// Broadcast to every webview (`AppHandle::emit`).
    fn emit_raw(&self, event: &str, payload: &RawValue) -> Result<(), EmitFailed>;

    /// Send to one webview window by label (`AppHandle::emit_to`).
    fn emit_to_window_raw(
        &self,
        label: &str,
        event: &str,
        payload: &RawValue,
    ) -> Result<(), EmitFailed>;
}

/// The ergonomic half of [`EventSink`]: takes any `Serialize` payload and does
/// the single JSON encode. A separate trait rather than defaulted methods
/// because a generic method is not object-safe, and `&dyn EventSink` is the
/// whole point.
pub trait EventSinkExt {
    fn emit<T: Serialize>(&self, event: &str, payload: &T) -> Result<(), EmitFailed>;
    fn emit_to_window<T: Serialize>(
        &self,
        label: &str,
        event: &str,
        payload: &T,
    ) -> Result<(), EmitFailed>;
}

impl<S: EventSink + ?Sized> EventSinkExt for S {
    fn emit<T: Serialize>(&self, event: &str, payload: &T) -> Result<(), EmitFailed> {
        let raw = encode(payload)?;
        self.emit_raw(event, &raw)
    }

    fn emit_to_window<T: Serialize>(
        &self,
        label: &str,
        event: &str,
        payload: &T,
    ) -> Result<(), EmitFailed> {
        let raw = encode(payload)?;
        self.emit_to_window_raw(label, event, &raw)
    }
}

/// A payload that will not serialize is a programming error, not a host
/// failure, but it must not panic an IPC handler either — it becomes an
/// [`EmitFailed`] like any other refusal.
fn encode<T: Serialize>(payload: &T) -> Result<Box<RawValue>, EmitFailed> {
    serde_json::value::to_raw_value(payload)
        .map_err(|e| EmitFailed(format!("payload is not serializable: {e}")))
}

/// The real sink: a Tauri app handle.
pub struct TauriEventSink {
    app: AppHandle,
}

impl TauriEventSink {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl EventSink for TauriEventSink {
    fn emit_raw(&self, event: &str, payload: &RawValue) -> Result<(), EmitFailed> {
        self.app
            .emit(event, payload)
            .map_err(|e| EmitFailed(e.to_string()))
    }

    fn emit_to_window_raw(
        &self,
        label: &str,
        event: &str,
        payload: &RawValue,
    ) -> Result<(), EmitFailed> {
        self.app
            .emit_to(EventTarget::webview_window(label), event, payload)
            .map_err(|e| EmitFailed(e.to_string()))
    }
}

/// Where a live PTY's terminal bytes go. One per session; swapped in place by
/// the renderer-flip rebind path (`ProcessorControl::ChannelChange`).
///
/// The payload is the base64 text the processor already built. `String` rather
/// than `&str` because the real implementation hands ownership straight to
/// Tauri's `Channel`, and borrowing would force a second copy there.
pub trait OutputSink: Send + Sync {
    /// Deliver one chunk. `Err` means the far end is gone; the processor task
    /// treats that as terminal and unwinds, exactly as it did when this was a
    /// bare `Channel::send`.
    fn send(&self, chunk: String) -> Result<(), OutputClosed>;
}

/// The output sink's far end is gone (webview destroyed, channel dropped).
#[derive(Debug)]
pub struct OutputClosed(pub String);

impl std::fmt::Display for OutputClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl OutputSink for Channel<String> {
    fn send(&self, chunk: String) -> Result<(), OutputClosed> {
        Channel::send(self, chunk).map_err(|e| OutputClosed(e.to_string()))
    }
}

/// The host effects that are not events: they act on the webview tree rather
/// than notifying it.
///
/// Deliberately narrow — one method, for the one effect the Phase 0 slice
/// needs. A `WebviewHost` that grew a method per `AppHandle` capability would
/// be `AppHandle` with extra steps.
pub trait WebviewHost: Send + Sync {
    /// Tear down the child webview a Preview tab owns, if it has one.
    /// Idempotent: an unknown or already-closed tab id is a no-op.
    fn destroy_preview(&self, tab_id: &str);
}

impl WebviewHost for crate::preview::PreviewRegistry {
    fn destroy_preview(&self, tab_id: &str) {
        crate::preview::destroy_if_open(self, tab_id);
    }
}

/// The warm code-graph index, as the *settings* save needs it.
///
/// A second host trait rather than another method on [`WebviewHost`], and it
/// exists for a different reason than that one. `GraphService` is not a UI
/// concern at all — it is *another domain's* capability, and naming it
/// concretely in the settings service would drag the whole code-graph surface
/// into that service's signature and into every fixture that builds one. One
/// method, for the one effect a settings save has on the index; a
/// `GraphIndexHost` that grew a method per `GraphService` capability would be
/// `GraphService` with extra steps.
///
/// (Until V42 Phase A2 there was a second, blunter reason:
/// [`GraphService::new`](crate::graph::GraphService::new) took an `AppHandle`,
/// so a service that named it was a service no test could build. That one is
/// gone — the graph service takes its collaborators as values now. The reason
/// above is the one that was always load-bearing.)
pub trait GraphIndexHost: Send + Sync {
    /// Reconcile the live index against a changed `graph.ignore`: drop
    /// newly-excluded files, index newly-included ones. Returns immediately —
    /// the walk runs on its own thread.
    fn spawn_ignore_resync(&self);
}

impl GraphIndexHost for std::sync::Arc<crate::graph::GraphService> {
    fn spawn_ignore_resync(&self) {
        crate::graph::GraphService::spawn_ignore_resync(self);
    }
}

/// In-process implementations, for tests that drive the service without a
/// WebView. Test-only on purpose: shipping them would put two unused `impl`s in
/// the binary and invite production code to depend on a recorder.
#[cfg(test)]
pub mod testing {
    use std::sync::Mutex;

    use super::*;

    /// One event the core emitted.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RecordedEvent {
        /// Window label for a targeted emit, `None` for a broadcast.
        pub window: Option<String>,
        pub event: String,
        /// The payload as JSON text, exactly as the real sink would have sent
        /// it. Compared as text so a test asserts on the wire shape rather than
        /// on a Rust type the frontend never sees.
        pub payload: String,
    }

    /// Records every emit instead of delivering it.
    #[derive(Default)]
    pub struct RecordingEventSink {
        events: Mutex<Vec<RecordedEvent>>,
    }

    impl RecordingEventSink {
        /// Everything emitted so far, in order.
        pub fn events(&self) -> Vec<RecordedEvent> {
            self.events.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    impl EventSink for RecordingEventSink {
        fn emit_raw(&self, event: &str, payload: &RawValue) -> Result<(), EmitFailed> {
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(RecordedEvent {
                    window: None,
                    event: event.to_string(),
                    payload: payload.get().to_string(),
                });
            Ok(())
        }

        fn emit_to_window_raw(
            &self,
            label: &str,
            event: &str,
            payload: &RawValue,
        ) -> Result<(), EmitFailed> {
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(RecordedEvent {
                    window: Some(label.to_string()),
                    event: event.to_string(),
                    payload: payload.get().to_string(),
                });
            Ok(())
        }
    }

    /// Collects the base64 chunks a PTY processor would have sent to a webview.
    #[derive(Default)]
    pub struct RecordingOutputSink {
        chunks: Mutex<Vec<String>>,
    }

    impl RecordingOutputSink {
        pub fn chunks(&self) -> Vec<String> {
            self.chunks.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        /// The chunks decoded back to bytes and concatenated — what the
        /// terminal would actually have rendered.
        pub fn decoded(&self) -> Vec<u8> {
            use base64::prelude::*;
            self.chunks()
                .iter()
                .flat_map(|c| BASE64_STANDARD.decode(c).unwrap_or_default())
                .collect()
        }
    }

    impl OutputSink for RecordingOutputSink {
        fn send(&self, chunk: String) -> Result<(), OutputClosed> {
            self.chunks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(chunk);
            Ok(())
        }
    }

    /// An output sink whose far end is already gone. Lets a test drive the
    /// processor's send-failed unwind without destroying a real webview.
    pub struct ClosedOutputSink;

    impl OutputSink for ClosedOutputSink {
        fn send(&self, _chunk: String) -> Result<(), OutputClosed> {
            Err(OutputClosed("test sink is closed".to_string()))
        }
    }

    /// A host with no webviews. Every Preview teardown is a no-op, which is
    /// also what the real host does for a tab that never opened one.
    pub struct NoWebviews;

    impl WebviewHost for NoWebviews {
        fn destroy_preview(&self, _tab_id: &str) {}
    }

    /// A graph index that counts the resyncs it was asked for instead of
    /// walking anything. The count is the assertion: the `graph.ignore` edge is
    /// "did the save decide a resync was needed", and a test that walked a real
    /// index would be testing the walker instead.
    #[derive(Default)]
    pub struct NoGraphIndex {
        resyncs: std::sync::atomic::AtomicUsize,
    }

    impl NoGraphIndex {
        pub fn resyncs(&self) -> usize {
            self.resyncs.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl GraphIndexHost for NoGraphIndex {
        fn spawn_ignore_resync(&self) {
            self.resyncs
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn broadcast_and_targeted_emits_are_distinguishable() {
        let sink = RecordingEventSink::default();
        sink.emit("tab-created", &serde_json::json!({ "tab": "shell-1" }))
            .unwrap();
        sink.emit_to_window("main", "tab-restart-requested", &"shell-1")
            .unwrap();

        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].window, None);
        assert_eq!(events[0].event, "tab-created");
        assert_eq!(events[0].payload, r#"{"tab":"shell-1"}"#);
        assert_eq!(events[1].window.as_deref(), Some("main"));
        assert_eq!(events[1].payload, r#""shell-1""#);
    }

    /// The payload reaches the sink as the JSON the frontend would receive,
    /// not as a Rust value — the property that makes an event-sequence
    /// assertion a wire-contract assertion.
    #[test]
    fn payload_is_the_wire_json() {
        #[derive(serde::Serialize)]
        struct Payload<'a> {
            tab: &'a str,
            exit: &'a str,
        }
        let sink = RecordingEventSink::default();
        sink.emit(
            "pty-exit",
            &Payload {
                tab: "shell-1",
                exit: "ExitStatus { .. }",
            },
        )
        .unwrap();
        assert_eq!(
            sink.events()[0].payload,
            r#"{"tab":"shell-1","exit":"ExitStatus { .. }"}"#
        );
    }

    #[test]
    fn output_sink_records_base64_chunks() {
        use base64::prelude::*;
        let sink = RecordingOutputSink::default();
        sink.send(BASE64_STANDARD.encode(b"hello ")).unwrap();
        sink.send(BASE64_STANDARD.encode(b"world")).unwrap();
        assert_eq!(sink.chunks().len(), 2);
        assert_eq!(sink.decoded(), b"hello world");
    }

    #[test]
    fn closed_output_sink_reports_failure() {
        assert!(ClosedOutputSink.send("anything".to_string()).is_err());
    }

    /// V42 Phase 0 measurement, not an assertion — hence `#[ignore]`.
    ///
    /// The question the spike has to answer is what putting the PTY output
    /// path behind `Arc<dyn OutputSink>` costs per chunk, against what that
    /// path already pays (a base64 encode and a `String` allocation per flush
    /// tick). Run with:
    ///
    /// ```text
    /// cargo test service::sink::tests::measure -- --ignored --nocapture
    /// ```
    ///
    /// **Only meaningful under an optimising profile.** In `dev` the encode is
    /// ~10× slower than it ships and swamps everything else; the numbers below
    /// were taken with an identical harness compiled at cImp's release profile
    /// (`opt-level = "s"`, `lto`, `codegen-units = 1`), 500 k iterations:
    ///
    /// ```text
    ///   chunk    256 B   encode 161 ns   + static 164 ns   + dyn 169 ns
    ///   chunk  4 096 B   encode 1.9 µs   + static 1.9 µs   + dyn 2.0 µs
    ///   chunk 65 536 B   encode 41 µs    + static 41 µs    + dyn 38 µs
    ///   the virtual call with an empty payload: 1 ns, same as the static one
    /// ```
    ///
    /// So: **~1 ns per chunk of dispatch, no additional allocation.** The sink
    /// `Arc` is built once per PTY session (three sites: start, restart,
    /// rebind), never per chunk, and the `String` it carries is moved, not
    /// copied. At 20 flush ticks a second per live tab the abstraction costs
    /// tens of nanoseconds a second, against a base64 encode that costs
    /// microseconds a tick. The 64 KB row — where `dyn` measures *faster* than
    /// static — is the honest reading: the difference is below the noise floor.
    #[test]
    #[ignore = "measurement, not an assertion"]
    fn measure_output_sink_dispatch_overhead() {
        use base64::prelude::*;
        use std::sync::Arc;
        use std::time::Instant;

        // A representative flush-tick payload: 50 ms of a TUI repainting.
        let bytes = vec![b'x'; 4096];
        const ITERS: u32 = 200_000;

        struct Blackhole(std::sync::atomic::AtomicUsize);
        impl OutputSink for Blackhole {
            fn send(&self, chunk: String) -> Result<(), OutputClosed> {
                self.0
                    .fetch_add(chunk.len(), std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
        }

        let direct = Blackhole(0.into());
        let start = Instant::now();
        for _ in 0..ITERS {
            let encoded = BASE64_STANDARD.encode(&bytes);
            direct.send(encoded).unwrap();
        }
        let static_dispatch = start.elapsed();

        let dynamic: Arc<dyn OutputSink> = Arc::new(Blackhole(0.into()));
        let start = Instant::now();
        for _ in 0..ITERS {
            let encoded = BASE64_STANDARD.encode(&bytes);
            dynamic.send(encoded).unwrap();
        }
        let dyn_dispatch = start.elapsed();

        // And the same loop with no send at all, to size the encode alone.
        let start = Instant::now();
        let mut sink_len = 0usize;
        for _ in 0..ITERS {
            let encoded = BASE64_STANDARD.encode(&bytes);
            sink_len += encoded.len();
        }
        let encode_only = start.elapsed();
        assert!(sink_len > 0);

        println!(
            "chunk={} bytes, iters={ITERS}\n  encode only     : {:?} ({:?}/chunk)\n  + static send   : {:?} ({:?}/chunk)\n  + dyn send      : {:?} ({:?}/chunk)",
            bytes.len(),
            encode_only,
            encode_only / ITERS,
            static_dispatch,
            static_dispatch / ITERS,
            dyn_dispatch,
            dyn_dispatch / ITERS,
        );
    }
}

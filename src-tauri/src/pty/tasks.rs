use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use base64::prelude::*;
use portable_pty::Child;
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::processing::permission::{
    PatternKind, PatternTransition, PermissionDetector, PermissionPattern,
};
use crate::processing::{ProcessingEvent, ProcessingLayer};
use super::manager::ProcessorControl;
use crate::settings::SettingsHandle;
use crate::state::{StateSignal, TabId, TabKind};

/// Tail size scanned by the permission detector after each ingest/flush.
/// Large enough for multi-line prompt UIs, small enough not to cost real
/// CPU on every tick.
const PERMISSION_SCAN_TAIL: usize = 1000;

/// Tick interval for the processing flush timer. Short enough that the
/// 200ms stability and 500ms max-hold thresholds fire promptly.
const FLUSH_TICK: Duration = Duration::from_millis(50);

/// Claude-activity (avatar Thinking) detection is primarily content-based:
/// the `claude_working` pattern matches Claude's on-screen busy footer
/// (`esc to interrupt`), which is present for exactly as long as a request
/// is in flight. While that marker shows we hold `output_active` regardless
/// of the byte stream, so a thinking pause (Claude routinely emits nothing
/// for >0.5s mid-response) no longer collapses the avatar to Idle. The two
/// timers below are backstops only.
///
/// Byte-burst duration before the timer alone considers the child to be
/// generating — the fallback path for the rare response that never paints
/// the marker. Real responses sustain bytes for seconds; per-keystroke TUI
/// redraws are tens of ms, so anything shorter is churn.
const CLAUDE_BURST_MIN: Duration = Duration::from_millis(1000);

/// Quiet interval that closes a burst once the marker is gone. Only fires
/// `ClaudeOutputStopped` when `claude_working` is NOT currently matched —
/// the marker, not this timer, decides Idle while Claude is working.
const CLAUDE_QUIET: Duration = Duration::from_millis(500);

/// Grace window the `claude_working` marker must stay *continuously absent*
/// before a marker-driven session is allowed to settle to Idle. While Claude
/// orchestrates parallel sub-agents (the Task tool) its `esc to interrupt`
/// footer blinks in and out roughly once a second — it repaints between agent
/// batches and while the parent is blocked waiting on results. Each gap used
/// to trip the 500 ms `CLAUDE_QUIET` release and fire `ClaudeOutputStopped`,
/// so the avatar cycled Thinking → Idle → Thinking every second and announced
/// "idle" on each cycle. Requiring the marker to be gone for this long instead
/// coalesces those blinks: as long as the footer reappears within the window,
/// `working_last_seen` keeps refreshing and no Idle fires.
///
/// The *authoritative* hold during agent runs is now the transcript-driven
/// `agents_active` flag in the state manager (`oob::claude` tracks in-flight
/// `Task` ids); this window only has to bridge the brief gap at the very start
/// of a run before that out-of-band signal is read (transcript poll + write
/// latency, well under a second). So it can be short — kept just above that
/// latency, which also keeps the extra delay on an ordinary (no-agent)
/// completion's idle cue small. Only applies once the marker has actually been
/// seen this session; the pure byte-burst fallback still releases on
/// `CLAUDE_QUIET` alone.
const CLAUDE_MARKER_GRACE: Duration = Duration::from_millis(1200);

/// Grace window after a real PTY resize during which the byte-burst
/// activity fallback is ignored. A resize pulses SIGWINCH and the child
/// (Claude Code's TUI) repaints — a burst of bytes that would otherwise
/// trip `burst_ready` and flip the avatar Idle → Thinking → Idle, firing a
/// spurious "idle" notification. A drag refreshes the window on every
/// dimension change, so this only needs to cover the gap between successive
/// resizes plus the post-release repaint settle. The `claude_working`
/// marker path is unaffected, so genuine work during a resize still shows
/// Thinking.
const RESIZE_BURST_GRACE: Duration = Duration::from_millis(1200);

/// Safety valve: if the working marker is somehow still matched but the
/// byte stream has been completely silent this long, treat it as stale
/// (e.g. ghost footer text left in the cell grid) and release Idle. The
/// live spinner repaints its elapsed-second counter ~once/sec, so a true
/// in-flight request keeps bytes flowing well under this window.
const CLAUDE_WORKING_STALE: Duration = Duration::from_secs(6);

#[derive(serde::Serialize, Clone)]
struct PtyExitPayload {
    tab: TabId,
    exit: String,
}

/// Reader task. Lives on the blocking pool because PTY reads block on most platforms.
///
/// V1.4-04 D: the reader also feeds the per-tab scrollback ring buffer.
/// `scrollback` is shared with `PtyManager` so persistence and the
/// `pty_get_scrollback` Tauri command can snapshot the same bytes. The
/// ring is bounded at `scrollback_cap` — surplus is dropped from the
/// front via `pop_front`. Lock contention is minimal: only this reader
/// writes; the snapshot/persist paths are rare reads. `StdMutex` keeps
/// every critical section tight (no awaits) and avoids the awkward
/// blocking-pool-vs-tokio-mutex interplay.
pub fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
    scrollback: Arc<StdMutex<VecDeque<u8>>>,
    scrollback_cap: usize,
) {
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            if cancel.is_cancelled() {
                debug!("pty reader: cancelled");
                break;
            }
            match reader.read(&mut buf) {
                Ok(0) => {
                    debug!("pty reader: EOF");
                    break;
                }
                Ok(n) => {
                    if tracing::enabled!(target: "pty_in", tracing::Level::DEBUG) {
                        let pretty: String = String::from_utf8_lossy(&buf[..n])
                            .chars()
                            .map(|c| if c.is_control() && c != '\n' && c != '\r' && c != '\t' {
                                format!("\\x{:02x}", c as u32)
                            } else {
                                c.to_string()
                            })
                            .collect();
                        debug!(target: "pty_in", len = n, bytes = %pretty);
                    }
                    // V1.4-04 D: append to the cross-restart ring
                    // before forwarding. Doing it before the
                    // `blocking_send` means the ring captures every
                    // byte even if the processor is back-pressured;
                    // doing it after would risk losing bytes if the
                    // forwarder drops mid-read.
                    if scrollback_cap > 0 {
                        if let Ok(mut ring) = scrollback.lock() {
                            ring.extend(&buf[..n]);
                            crate::pty::scrollback::trim_ring(&mut ring, scrollback_cap);
                        }
                    }
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        debug!("pty reader: forwarder dropped");
                        break;
                    }
                }
                Err(e) => {
                    if cancel.is_cancelled() {
                        break;
                    }
                    warn!(error = %e, "pty reader: read error");
                    break;
                }
            }
        }
        debug!("pty reader task exiting");
    });
}

/// Processor task. Owns a `ProcessingLayer`, drives it from the PTY byte stream
/// and a steady 50 ms flush tick, and dispatches resulting events. Each
/// emitted state signal is tagged with the owning tab so the manager can
/// route it to the correct per-tab `TabState`.
///
/// Per-kind gating (Phase 3 of MILESTONE-V3-01):
/// - AI tabs: full pipeline — permission/question/working detection,
///   claude-output activity tracking, TTS dispatch.
/// - Shell tabs: bypass all three. The vte parser still runs (xterm.js needs
///   correctly-rendered bytes) but no TTS request is ever sent, no permission
///   pattern is scanned, and `ClaudeOutputStarted/Stopped` never fires for
///   them.
#[allow(clippy::too_many_arguments)]
pub fn spawn_processor(
    tab: TabId,
    mut rx: mpsc::Receiver<Vec<u8>>,
    channel: Channel<String>,
    mut control_rx: mpsc::Receiver<ProcessorControl>,
    cancel: CancellationToken,
    state_signals: mpsc::Sender<StateSignal>,
    settings: SettingsHandle,
    patterns: Arc<Vec<PermissionPattern>>,
    // V20: true when an out-of-band source authoritatively drives this tab's
    // avatar Thinking/Idle (OpenCode's event stream). When set, the byte-burst
    // activity fallback below is skipped — otherwise the fullscreen TUI's
    // startup/repaint bursts spuriously flip Idle→Thinking→Idle and fire an
    // "idle" notification. Claude keeps the fallback (its working state comes
    // from the cell-model marker, not an OOB stream).
    oob_drives_activity: bool,
) {
    tokio::spawn(async move {
        // V1.4-03: `channel` is `mut` so the `ProcessorControl::ChannelChange`
        // arm can swap it out when the JS-side xterm is recreated for a
        // renderer-flip. Bytes pending in `rx` during the swap remain there
        // (mpsc is FIFO; cancel-safe across select polls) and dispatch to
        // the new channel on the next iteration.
        let mut channel = channel;

        let kind = tab.kind();
        let is_shell = matches!(kind, TabKind::Shell);

        let mut layer = ProcessingLayer::new();
        let mut settings_rx = settings.subscribe();
        let mut tick = tokio::time::interval(FLUSH_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // Detector. AI tabs use the user-editable patterns loaded from
        // patterns.json at startup (subscription and local Claude both run
        // the same binary, so they share the pattern set); Shell tabs get
        // an empty pattern list, and the detector itself is never invoked
        // for them anyway — see `is_shell` below.
        let detector_patterns: Vec<PermissionPattern> = match kind {
            TabKind::AiTool => (*patterns).clone(),
            TabKind::Shell => Vec::new(),
        };
        let mut detector = PermissionDetector::new(detector_patterns);

        let mut output_active = false;
        let mut burst_start: Option<tokio::time::Instant> = None;
        let mut last_byte_time: Option<tokio::time::Instant> = None;
        // Set on each real PTY resize. While within `RESIZE_BURST_GRACE` of
        // this, the byte-burst activity fallback is suppressed so a
        // resize-driven TUI repaint doesn't masquerade as Claude output.
        let mut last_resize: Option<tokio::time::Instant> = None;
        // True while the `claude_working` marker ("esc to interrupt") is on
        // screen. Authoritative for the avatar's Thinking state; the byte
        // timers above are only a fallback. Updated by run_permission_check.
        let mut working_active = false;
        // Last tick the marker was observed on screen during the current
        // output session, or `None` if it has never been seen (pure byte-burst
        // fallback). Gates the Idle release by `CLAUDE_MARKER_GRACE` so the
        // footer blinking out mid-orchestration doesn't flip the avatar to
        // Idle. Reset to `None` whenever a session ends (release/stale).
        let mut working_last_seen: Option<tokio::time::Instant> = None;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!(?tab, "pty processor: cancelled");
                    break;
                }
                changed = settings_rx.recv() => {
                    match changed {
                        // V20: the layer has no live-tunable knobs anymore (TTS
                        // is out-of-band); we still drain the channel so it
                        // doesn't lag, but there's nothing to reconfigure here.
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            debug!("pty processor: settings channel closed");
                        }
                    }
                }
                ctrl = control_rx.recv() => {
                    match ctrl {
                        Some(ProcessorControl::ChannelChange(next)) => {
                            // V1.4-03: swap output channel without
                            // restarting the PTY. No replay here — bytes
                            // that arrived during the rebind window are
                            // queued in `rx` and dispatch against the
                            // new channel on the next iteration.
                            // Frontend handles visual continuity via the
                            // serialize-snapshot replay.
                            debug!(?tab, "pty processor: channel rebind");
                            channel = next;
                        }
                        Some(ProcessorControl::Resized) => {
                            // Open the grace window. See RESIZE_BURST_GRACE.
                            last_resize = Some(tokio::time::Instant::now());
                        }
                        None => {
                            // Sender dropped — only happens if PtyHandle
                            // is freed without going through `shutdown`,
                            // which would also cancel us. Defensive.
                            debug!(?tab, "pty processor: control channel closed");
                        }
                    }
                }
                maybe = rx.recv() => {
                    match maybe {
                        Some(bytes) => {
                            // Per-tab raw content capture. Disabled by
                            // default; fast-path no-op when off.
                            crate::content::write(&tab, &bytes);
                            let events = layer.ingest(&bytes);
                            let saw_terminal_bytes = events.iter().any(|e| matches!(
                                e,
                                ProcessingEvent::TerminalBytes(b) if !b.is_empty()
                            ));
                            if dispatch_events(events, &channel).await.is_err() {
                                break;
                            }
                            if saw_terminal_bytes && !is_shell {
                                let now = tokio::time::Instant::now();
                                if burst_start.is_none() {
                                    burst_start = Some(now);
                                }
                                last_byte_time = Some(now);
                                run_permission_check(
                                    &tab,
                                    &mut detector,
                                    &layer,
                                    &state_signals,
                                    &mut working_active,
                                );
                            }
                        }
                        None => {
                            debug!(?tab, "pty processor: bytes channel closed");
                            break;
                        }
                    }
                }
                _ = tick.tick() => {
                    let events = layer.flush_pending();
                    let saw_terminal_bytes = events.iter().any(|e| matches!(
                        e,
                        ProcessingEvent::TerminalBytes(b) if !b.is_empty()
                    ));
                    if dispatch_events(events, &channel).await.is_err() {
                        break;
                    }
                    if saw_terminal_bytes && !is_shell {
                        let now = tokio::time::Instant::now();
                        if burst_start.is_none() {
                            burst_start = Some(now);
                        }
                        last_byte_time = Some(now);
                        run_permission_check(
                            &tab,
                            &mut detector,
                            &layer,
                            &state_signals,
                            &mut working_active,
                        );
                    }

                    // Avatar activity (Thinking↔Idle). Content-first: the
                    // `claude_working` marker is authoritative while it's on
                    // screen; the byte timers are fallbacks. Skipped entirely
                    // when an out-of-band stream drives activity (V20: OpenCode),
                    // so a fullscreen startup/repaint burst can't fake a cycle.
                    if !is_shell && !oob_drives_activity {
                        // Track the marker's presence so the release path below
                        // can require it gone for a grace window, not just a
                        // single tick. Refreshed every tick it's on screen.
                        if working_active {
                            working_last_seen = Some(tokio::time::Instant::now());
                        }
                        if !output_active {
                            // Enter "working" on the marker, or — for a
                            // response that never paints it — a sustained
                            // byte burst.
                            //
                            // Suppress the burst path within the grace window
                            // after a resize: the TUI repaint that a resize
                            // triggers is a burst of bytes with no underlying
                            // request, and letting it flip Idle → Thinking →
                            // Idle fires a spurious "idle" notification. Clear
                            // the accumulated burst too, so churn that ends
                            // mid-grace can't trip `burst_ready` the instant
                            // the window closes. The marker path is untouched,
                            // so a real request mid-resize still shows Thinking.
                            let in_resize_grace = last_resize
                                .is_some_and(|t| t.elapsed() < RESIZE_BURST_GRACE);
                            if in_resize_grace && !working_active {
                                burst_start = None;
                                last_byte_time = None;
                            }
                            let burst_ready = burst_start
                                .is_some_and(|s| s.elapsed() >= CLAUDE_BURST_MIN);
                            if working_active || burst_ready {
                                output_active = true;
                                let _ = state_signals
                                    .try_send(StateSignal::ClaudeOutputStarted { tab: tab.clone() });
                            }
                        } else {
                            // Leave "working" only once the marker is gone and
                            // the stream has settled — a thinking pause with
                            // the marker still up must NOT release Idle. The
                            // stale guard frees a marker left ghosting in the
                            // grid with no underlying byte activity.
                            let (release, stale) = should_release_idle(
                                last_byte_time.map(|t| t.elapsed()),
                                working_last_seen.map(|t| t.elapsed()),
                                working_active,
                            );
                            if release {
                                if stale && working_active {
                                    // Looked stuck — drop the detector's
                                    // latched Working state so a genuine
                                    // resumption can re-trigger it.
                                    detector.force_clear(PatternKind::Working);
                                    working_active = false;
                                }
                                output_active = false;
                                working_last_seen = None;
                                let _ = state_signals
                                    .try_send(StateSignal::ClaudeOutputStopped { tab: tab.clone() });
                                burst_start = None;
                                last_byte_time = None;
                            }
                        }
                    }
                }
            }
        }
        debug!(?tab, "pty processor task exiting");
    });
}

/// Decide whether an active output session (`output_active == true`) should
/// settle back to Idle, i.e. fire `ClaudeOutputStopped`. Pure so the timing
/// logic is unit-testable without a running clock — the caller measures the
/// elapsed times against `Instant::now()` and passes them in.
///
/// - `since_last_byte`: time since the last terminal byte, or `None` if no
///   byte has arrived this session.
/// - `since_marker_seen`: time since the `claude_working` marker was last on
///   screen, or `None` if it was never seen this session (the pure byte-burst
///   fallback path, which releases on quiet alone).
/// - `working_active`: whether the marker is currently matched on screen.
///
/// Returns `(release, stale)`: `release` is whether to emit
/// `ClaudeOutputStopped`; `stale` flags the 6 s safety-valve path so the
/// caller can force-clear the latched Working state before releasing.
fn should_release_idle(
    since_last_byte: Option<Duration>,
    since_marker_seen: Option<Duration>,
    working_active: bool,
) -> (bool, bool) {
    let quiet = since_last_byte.is_some_and(|d| d >= CLAUDE_QUIET);
    let stale = since_last_byte.is_some_and(|d| d >= CLAUDE_WORKING_STALE);
    // Require the marker gone for the full grace window before settling to
    // Idle — bridges the ~1 Hz footer blink while Claude drives sub-agents so
    // a brief gap can't fire a spurious ClaudeOutputStopped (avatar flicker +
    // repeated "idle" announcements). Sessions that never painted the marker
    // fall back to `quiet` alone, preserving the pure byte-burst path.
    let marker_gone_long_enough = since_marker_seen.is_none_or(|d| d >= CLAUDE_MARKER_GRACE);
    let release = (!working_active && quiet && marker_gone_long_enough) || stale;
    (release, stale)
}

/// Scan the rendered tail for known prompt patterns and forward edge
/// transitions to the state manager. No-op for tabs configured with empty
/// patterns (Shell tabs).
fn run_permission_check(
    tab: &TabId,
    detector: &mut PermissionDetector,
    layer: &ProcessingLayer,
    state_signals: &mpsc::Sender<StateSignal>,
    working_active: &mut bool,
) {
    let rendered = layer.recent_rendered(PERMISSION_SCAN_TAIL);
    // Opt-in capture for pattern characterization. Enable with
    // RUST_LOG=perm_capture=debug to dump the exact rendered tail the
    // detector matches against; pick distinctive substrings from the dump
    // and add them to <exe-dir>/patterns.json.
    if tracing::enabled!(target: "perm_capture", tracing::Level::DEBUG) {
        let escaped = rendered.replace('\n', "\\n");
        tracing::debug!(target: "perm_capture", ?tab, rendered = %escaped, "perm capture");
    }
    for transition in detector.check(&rendered) {
        match transition {
            PatternTransition::Detected { kind: PatternKind::Permission, pattern_name } => {
                debug!(?tab, pattern = pattern_name, "permission prompt detected");
                let _ = state_signals
                    .try_send(StateSignal::PermissionPromptDetected { tab: tab.clone() });
            }
            PatternTransition::Resolved { kind: PatternKind::Permission, pattern_name } => {
                debug!(?tab, pattern = pattern_name, "permission prompt resolved");
                let _ = state_signals
                    .try_send(StateSignal::PermissionPromptResolved { tab: tab.clone() });
            }
            PatternTransition::Detected { kind: PatternKind::Question, pattern_name } => {
                debug!(?tab, pattern = pattern_name, "question prompt detected");
                let _ = state_signals
                    .try_send(StateSignal::QuestionPromptDetected { tab: tab.clone() });
            }
            PatternTransition::Resolved { kind: PatternKind::Question, pattern_name } => {
                debug!(?tab, pattern = pattern_name, "question prompt resolved");
                let _ = state_signals
                    .try_send(StateSignal::QuestionPromptResolved { tab: tab.clone() });
            }
            // Working transitions don't emit directly — they update the
            // marker level the activity block reads to drive ClaudeOutput
            // Started/Stopped (content-first, byte timers as fallback).
            PatternTransition::Detected { kind: PatternKind::Working, pattern_name } => {
                debug!(?tab, pattern = pattern_name, "claude working detected");
                *working_active = true;
            }
            PatternTransition::Resolved { kind: PatternKind::Working, pattern_name } => {
                debug!(?tab, pattern = pattern_name, "claude working resolved");
                *working_active = false;
            }
        }
    }
}

async fn dispatch_events(
    events: Vec<ProcessingEvent>,
    channel: &Channel<String>,
) -> Result<(), ()> {
    for ev in events {
        match ev {
            ProcessingEvent::TerminalBytes(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
                if tracing::enabled!(target: "pty_emit", tracing::Level::DEBUG) {
                    let pretty: String = String::from_utf8_lossy(&bytes)
                        .chars()
                        .map(|c| if c.is_control() && c != '\n' && c != '\r' && c != '\t' {
                            format!("\\x{:02x}", c as u32)
                        } else {
                            c.to_string()
                        })
                        .collect();
                    debug!(target: "pty_emit", len = bytes.len(), bytes = %pretty);
                }
                let encoded = BASE64_STANDARD.encode(&bytes);
                if let Err(e) = channel.send(encoded) {
                    warn!(error = %e, "pty processor: terminal channel send failed");
                    return Err(());
                }
            }
        }
    }
    Ok(())
}

/// Waiter task. Blocks on child.wait(), emits `pty-exit` on the AppHandle, and
/// cancels sibling tasks so the reader/processor unwind cleanly.
pub fn spawn_waiter(
    tab: TabId,
    mut child: Box<dyn Child + Send + Sync>,
    app: AppHandle,
    cancel: CancellationToken,
    state_signals: mpsc::Sender<StateSignal>,
) {
    tokio::task::spawn_blocking(move || {
        use std::time::{Duration, Instant};
        const POLL: Duration = Duration::from_millis(100);
        // Once cancellation is observed (shutdown/restart is killing the child)
        // keep polling only briefly: if the kill works the child reports its
        // status and we emit a clean pty-exit; if it never dies we must NOT
        // park this blocking-pool thread forever — that leaks a thread per
        // restart and eventually exhausts tokio's blocking pool, wedging all
        // spawn_blocking work (PTY writes/resizes). Give up after the grace.
        const KILL_GRACE: Duration = Duration::from_secs(5);
        let mut give_up_at: Option<Instant> = None;

        let exit = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(e) => break Err(e),
            }
            if cancel.is_cancelled() {
                let deadline = *give_up_at.get_or_insert_with(|| Instant::now() + KILL_GRACE);
                if Instant::now() >= deadline {
                    warn!(
                        ?tab,
                        "pty child did not exit after kill; abandoning waiter (process may be orphaned)"
                    );
                    debug!(?tab, "pty waiter task exiting (abandoned)");
                    return;
                }
            }
            std::thread::sleep(POLL);
        };
        cancel.cancel();
        let (exit_str, code) = match &exit {
            // `portable_pty::ExitStatus::exit_code()` is u32; the bit pattern
            // round-trips through i32 cleanly for display, and this matches
            // how shells (and the Tauri-side overlay) usually print exit
            // codes.
            Ok(status) => (format!("{:?}", status), Some(status.exit_code() as i32)),
            Err(e) => (format!("error: {}", e), None),
        };
        // Terminal signal: a dropped SubprocessExited leaves the backend and
        // frontend desynced about whether the tab's child is alive. We're on
        // the blocking pool here (can't await), so use blocking_send to apply
        // backpressure instead of try_send's silent drop on a full channel.
        let _ = state_signals.blocking_send(StateSignal::SubprocessExited { tab: tab.clone(), code });
        if let Err(e) = app.emit(
            "pty-exit",
            PtyExitPayload {
                tab: tab.clone(),
                exit: exit_str,
            },
        ) {
            warn!(error = %e, "failed to emit pty-exit");
        }
        debug!(?tab, "pty waiter task exiting");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helpers for readable elapsed-time inputs.
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn holds_thinking_while_marker_on_screen() {
        // Marker currently matched: never release regardless of byte silence,
        // short of the stale safety valve. A thinking pause with the footer up
        // must stay Thinking.
        let (release, stale) = should_release_idle(Some(ms(3000)), Some(ms(0)), true);
        assert!(!release, "marker on screen must hold Thinking");
        assert!(!stale);
    }

    #[test]
    fn brief_marker_gap_does_not_release() {
        // The footer blinked out and bytes went quiet, but the marker was on
        // screen well within the grace window — the ~1 Hz sub-agent blink. Must
        // NOT release, or the avatar flickers Idle and re-announces.
        let (release, _) = should_release_idle(Some(ms(600)), Some(ms(800)), false);
        assert!(!release, "sub-grace marker gap must not settle to Idle");
    }

    #[test]
    fn sustained_marker_absence_releases() {
        // Marker gone past the grace window and the stream is quiet — Claude is
        // genuinely done. Release to Idle.
        let (release, stale) = should_release_idle(
            Some(ms(600)),
            Some(CLAUDE_MARKER_GRACE + ms(1)),
            false,
        );
        assert!(release, "marker gone past grace + quiet must settle to Idle");
        assert!(!stale);
    }

    #[test]
    fn grace_gates_release_even_when_quiet() {
        // Quiet threshold is met but the marker vanished only just now. The
        // grace gate must still block the release.
        let (release, _) = should_release_idle(
            Some(CLAUDE_WORKING_STALE - ms(1)),
            Some(ms(10)),
            false,
        );
        assert!(!release, "grace must gate release until the marker is gone long enough");
    }

    #[test]
    fn no_marker_session_releases_on_quiet_alone() {
        // Pure byte-burst fallback (a response that never painted the footer):
        // `since_marker_seen == None` bypasses the grace, so plain quiet
        // releases — preserving pre-fix behavior for that path.
        let (release, stale) = should_release_idle(Some(CLAUDE_QUIET), None, false);
        assert!(release, "no-marker session must release on quiet alone");
        assert!(!stale);
    }

    #[test]
    fn no_marker_session_holds_while_bytes_flow() {
        // Same fallback path, but bytes are still flowing (under the quiet
        // threshold) — hold Thinking.
        let (release, _) = should_release_idle(Some(CLAUDE_QUIET - ms(1)), None, false);
        assert!(!release);
    }

    #[test]
    fn stale_safety_valve_fires_even_with_marker_active() {
        // The marker is somehow still matched (ghost footer) but bytes have
        // been silent past the stale window — the safety valve releases and
        // flags stale so the caller force-clears the latched Working state.
        let (release, stale) = should_release_idle(
            Some(CLAUDE_WORKING_STALE),
            Some(ms(0)),
            true,
        );
        assert!(release, "stale window must release even with the marker active");
        assert!(stale, "stale flag must be set so the caller clears Working");
    }

    #[test]
    fn no_bytes_yet_never_releases() {
        // Session opened but no byte has arrived — neither quiet nor stale can
        // be true, so we can't yet conclude anything. Hold.
        let (release, stale) = should_release_idle(None, Some(CLAUDE_MARKER_GRACE + ms(1)), false);
        assert!(!release);
        assert!(!stale);
    }
}

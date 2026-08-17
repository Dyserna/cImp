//! V35 Phase L — **the one place assistant prose becomes speech**, shared by
//! the L1 fallback readers and the CHP push path.
//!
//! # Why this moved out of `OobContext::speak`
//!
//! Until Phase L there was exactly one producer of spoken assistant text: the
//! per-tab fallback reader, which owned a `TtsRequest` sender, a settings
//! handle and a `CancellationToken` and did the whole composition inline. Phase
//! L adds a second producer — the `Stop` hook arriving at
//! `POST /session/assistant_text` — which owns none of those and reaches TTS
//! through `ipc::AppState` instead.
//!
//! Two producers of *the same speech* is exactly the situation in which a
//! second copy of "strip escapes, reduce markdown, segment, send per sentence,
//! re-check the toggle" drifts. So the composition lives here and both call it;
//! [`crate::harness::reader::OobContext::speak`] is now a thin wrapper that
//! supplies its own tab/sender/cancel.
//!
//! **Segmentation stays app-side** (design § 5.2, milestone locked decision 2
//! for this phase). A harness plugin sends prose; it never sends markup,
//! control sequences or sentence boundaries. Everything below the `to_speakable`
//! call is cImp's, and a push cannot change it.
//!
//! # The handoff (the double-speak edge)
//!
//! Arbitration is per capability, per tab, and it flips the instant a tab's
//! hello is recorded — which for Claude is `SessionStart`, and `SessionStart`
//! fires on `resume` and `clear` as well as at launch. So a hello CAN land
//! mid-session, with the reader having already spoken part of the turn that is
//! about to arrive as one complete `Stop` payload.
//!
//! Neither obvious answer is right. Speaking the push in full **replays** what
//! the reader already said; dropping it entirely **loses** whatever the reader
//! had not reached yet. [`ProseSource`] closes the gap at the message boundary:
//! the reader records the speakable prose it last emitted for a tab, and the
//! FIRST push after that strips it as a prefix and speaks only the remainder.
//! One `String` per tab, consumed on read, so the steady state costs a map miss.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use super::TtsRequest;
use crate::settings::{SettingsHandle, TabConfig};
use crate::state::TabId;

/// Which producer is speaking. The composition is identical for both; what
/// differs is which side of the handoff (module docs) they stand on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProseSource {
    /// A Tier-C fallback reader (`harness/<id>/read.rs`). Records the handoff.
    FallbackReader,
    /// A CHP `assistant_text` push. Consumes the handoff.
    ChpPush,
}

/// The most prose one tab's handoff slot will hold.
///
/// A handoff exists to strip a prefix off the *next* push; it is not a
/// transcript. Beyond a few kilobytes a stale entry stops being able to match
/// anything useful and starts being memory held per tab for the life of the
/// process, so the recorder keeps the tail and the matcher simply fails to
/// strip in the rare case a single reader block was larger than this.
const HANDOFF_MAX: usize = 8 * 1024;

/// Per-tab "the fallback reader last spoke this". See the module docs.
///
/// Bounded by construction: one entry per tab that has spoken, keyed by a
/// [`TabId`] the app itself minted (never a request body), each capped at
/// [`HANDOFF_MAX`].
fn handoffs() -> &'static Mutex<HashMap<String, String>> {
    static HANDOFFS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    HANDOFFS.get_or_init(Default::default)
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, String>> {
    handoffs().lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether `tab` should speak its assistant output — the per-tab
/// `tts_injection.enabled` toggle, read LIVE so a settings change takes effect
/// without relaunching the tab.
///
/// V20 repurposed this flag from "inject the `[[TTS]]` markup convention" to
/// "speak this tab's assistant prose"; the markup convention retired with the
/// scrape path. Phase L did not change what it means — only how many producers
/// consult it.
pub fn tts_enabled(settings: &SettingsHandle, tab: &TabId) -> bool {
    matches!(
        settings.current().find_tab(tab.as_str()),
        Some(TabConfig::AiTool(c)) if c.tts_injection.enabled
    )
}

/// What the fallback reader last spoke for `tab`, consumed.
fn take_handoff(tab: &TabId) -> Option<String> {
    lock().remove(tab.as_str())
}

/// Record what the fallback reader just spoke for `tab`, replacing any previous
/// entry (only the most recent one can be a prefix of the next push).
fn note_handoff(tab: &TabId, prose: &str) {
    let prose = prose.trim();
    if prose.is_empty() {
        return;
    }
    let kept = if prose.len() > HANDOFF_MAX {
        // Keep the TAIL: a push's prefix match is against the end of what the
        // reader said, since anything earlier was already a complete message.
        // Slid forward to the next char boundary rather than sliced blind — a
        // panic here would be a panic on assistant prose, i.e. on arbitrary
        // input.
        let want = prose.len() - HANDOFF_MAX;
        let start = (want..prose.len())
            .find(|i| prose.is_char_boundary(*i))
            .unwrap_or(prose.len());
        &prose[start..]
    } else {
        prose
    };
    lock().insert(tab.as_str().to_string(), kept.to_string());
}

/// Strip what the reader already spoke off the front of a pushed message.
///
/// Pure and exported to the test module below, because the property that
/// matters — *no replay and no drop* — is a statement about this function and
/// not about the channel it feeds.
fn without_replay<'a>(prose: &'a str, already_spoken: &str) -> &'a str {
    let already = already_spoken.trim();
    if already.is_empty() {
        return prose;
    }
    let trimmed = prose.trim_start();
    match trimmed.strip_prefix(already) {
        Some(rest) => rest,
        // Not a prefix — the reader's last block belonged to an earlier
        // message, or the harness's own rendering of this one differs. Speaking
        // the whole push is the safe answer of the two: a duplicated sentence
        // is an annoyance, a dropped one is data loss.
        None => prose,
    }
}

/// Segment `text` into sentences and push each onto the TTS channel as a
/// suppressible `Synthesize` request (so Esc/`tts_stop` cuts the rest of the
/// burst, exactly like the old scrape path). Markdown is reduced to speakable
/// prose first; empty/code-only input speaks nothing.
///
/// V32 Phase D — **escape hygiene at the one external-text boundary the TTS
/// path has.** `text` is assistant prose lifted from a transcript, an event
/// stream or (since Phase L) a hook payload, and an assistant that just read a
/// fetched page routinely quotes it verbatim — so a page carrying
/// `ESC ] 52 ; c ; …` (a clipboard write) or cursor-motion sequences reaches
/// this composition site intact. Stripping here rather than at each producer
/// keeps it one decision in one place, and it happens BEFORE markdown reduction
/// so a control sequence cannot alter how `to_speakable` sees fences or list
/// markers.
///
/// V32 Phase G (locked decision 16): the strip is one of the eleven switchable
/// controls, resolved at [`Scope::AppWide`] — TTS and toasts are global
/// surfaces (the global-only avatar/TTS decision), so this feature has an L1
/// and an L2 and deliberately no per-scope row. Resolved per burst rather than
/// cached: the settings handle is already read here for [`tts_enabled`], and a
/// user who turns hygiene off wants the next thing spoken to reflect it.
///
/// `cancel` is `Some` for a per-tab reader (whose task dies with the tab) and
/// `None` for the push path, which runs inside a loopback request that has
/// already bounded itself.
///
/// [`Scope::AppWide`]: crate::settings::injection::Scope::AppWide
pub async fn speak_prose(
    tab: &TabId,
    tts: &Sender<TtsRequest>,
    settings: &SettingsHandle,
    cancel: Option<&CancellationToken>,
    source: ProseSource,
    text: &str,
) {
    if !tts_enabled(settings, tab) {
        return;
    }
    let text = if crate::settings::injection::effective(
        crate::settings::injection::Feature::TerminalEscapeHygiene,
        crate::settings::injection::Scope::AppWide,
        &settings.current(),
    ) {
        crate::processing::strip_terminal_escapes(text)
    } else {
        std::borrow::Cow::Borrowed(text)
    };
    let prose = crate::processing::to_speakable(&text);
    let prose = match source {
        // The reader is authoritative until a hello says otherwise; record what
        // it says so the first push after a mid-session switchover can pick up
        // where it left off.
        ProseSource::FallbackReader => {
            note_handoff(tab, &prose);
            prose.clone()
        }
        ProseSource::ChpPush => match take_handoff(tab) {
            Some(spoken) => without_replay(&prose, &spoken).to_string(),
            None => prose.clone(),
        },
    };
    if prose.trim().is_empty() {
        return;
    }
    for sentence in crate::processing::segment_sentences(&prose) {
        // Re-check the toggle per sentence so switching TTS off mid-burst cuts
        // the rest of a long message (the doc above promises a live read), and
        // race the bounded send against the cancel token so a closing tab isn't
        // held hostage by a backed-up TTS channel.
        if cancel.is_some_and(CancellationToken::is_cancelled) || !tts_enabled(settings, tab) {
            return;
        }
        let send = tts.send(TtsRequest::Synthesize {
            tab: tab.clone(),
            text: sentence,
            suppressible: true,
        });
        match cancel {
            Some(cancel) => {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    // Bounded channel; if the worker is backed up, awaiting
                    // applies natural backpressure rather than dropping speech.
                    res = send => {
                        if res.is_err() {
                            return; // worker gone — stop feeding.
                        }
                    }
                }
            }
            None => {
                if send.await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handoff's whole contract, as a property of one pure function: the
    /// switchover replays nothing and drops nothing.
    #[test]
    fn the_handoff_replays_nothing_and_drops_nothing() {
        // The straddle: the reader spoke the first block of a turn, the hello
        // landed, and the push carries the whole turn.
        let pushed = "First block. Second block.";
        assert_eq!(
            without_replay(pushed, "First block.").trim(),
            "Second block.",
            "the remainder must still be spoken"
        );
        // Exactly what the reader already said ⇒ nothing left to say.
        assert!(without_replay("All of it.", "All of it.").trim().is_empty());
        // A handoff from an EARLIER message is not a prefix of this one — speak
        // the push whole rather than guessing.
        assert_eq!(without_replay("A new turn.", "Old turn."), "A new turn.");
        // No handoff at all is the steady state, and must be a no-op.
        assert_eq!(without_replay("Anything.", ""), "Anything.");
        assert_eq!(without_replay("Anything.", "   "), "Anything.");
    }

    /// The recorder keeps the tail and stays bounded — an entry per tab that
    /// grew without limit would be a leak keyed by something the user controls
    /// only by opening tabs.
    #[test]
    fn the_handoff_slot_is_bounded_and_keeps_the_tail() {
        let tab = TabId::from_str("prose-handoff-bound-test");
        let long = format!("{}TAIL.", "x".repeat(HANDOFF_MAX * 2));
        note_handoff(&tab, &long);
        let kept = take_handoff(&tab).expect("recorded");
        assert!(kept.len() <= HANDOFF_MAX, "kept {} bytes", kept.len());
        assert!(kept.ends_with("TAIL."), "the tail is what can match a prefix");
        // Consumed on read: only the FIRST push after a switchover strips.
        assert!(take_handoff(&tab).is_none());
        // Blank prose records nothing rather than an empty entry.
        note_handoff(&tab, "   ");
        assert!(take_handoff(&tab).is_none());
    }
}

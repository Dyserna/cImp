//! V39 Phase B — **the push half of delegation**: how one turn is typed into a
//! harness's TUI, declared per harness and looked up harness-neutrally.
//!
//! Locked decision 16 splits delegation across the plugin ladder at exactly the
//! two points the ladder already has:
//!
//! * the **read half** (the worker's reply, and the fact that its turn ended)
//!   is CHP's `assistant_text` — an L2 event with an L1 fallback reader, and
//!   nothing new;
//! * the **push half** — *what bytes make this TUI accept one multi-line
//!   request and submit it as exactly one turn* — is per harness, undocumented,
//!   and therefore L1. It lives in `harness/<id>/input.rs` as an
//!   [`InputProfile`], and everything above the seam reaches it through
//!   [`input_profile`], keyed by the tab's harness id.
//!
//! The engine (`crate::delegation`) never learns a harness name: it asks this
//! module for a profile and refuses when there is none. That refusal is the
//! whole reason the lookup returns [`Option`] — a harness directory without an
//! `input.rs` has no profile, fails the `delegation.worker` gate closed, and is
//! **not a valid worker**, which is a visible refusal rather than a task typed
//! into a tab cImp cannot drive.
//!
//! # Why this is Tier D, and what the spike covers
//!
//! Nothing upstream documents paste handling in either TUI. Both are known to
//! enable bracketed paste (the `\x1b[?2004h` mode the terminal answers with
//! `\x1b[200~ … \x1b[201~` wrappers) — cImp's own xterm side already tracks
//! that mode — but *how* a TUI treats the wrapped text, and whether a submit
//! key immediately after it lands in the same turn, is behaviour no payload
//! reveals. The registry rows `claude.input.profile` / `opencode.input.profile`
//! carry that as a [`Dep::Behavior`] with `Degradation::FailClosed`, and the
//! recorded spike outcome (`harness_versions.input_profile_status`) is what the
//! `delegation.worker` gate reads.
//!
//! [`Dep::Behavior`]: crate::harness::contract::Dep::Behavior

/// How a multi-line request is handed to a TUI.
// `Raw` is unconstructed on purpose — see its own doc comment: it exists so a
// future harness can DECLARE that it has no bracketed-paste path, rather than
// be forced to claim support it does not have. A variant that only appears once
// someone needs it is a variant nobody can find.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteMode {
    /// Wrap the text in the bracketed-paste markers (`ESC [ 200 ~` …
    /// `ESC [ 201 ~`). A TUI in bracketed-paste mode treats everything between
    /// them as *one literal insertion*: newlines land in the input buffer as
    /// newlines instead of being read as as many separate Enter presses, which
    /// is the difference between "one request" and "N truncated turns".
    Bracketed,
    /// Write the bytes as they are. Correct only for a TUI that does not enable
    /// bracketed paste — and then only for single-line requests, because every
    /// embedded newline is a submit. No harness uses this today; it exists so a
    /// future one can declare the truth rather than be forced to claim
    /// bracketed support it does not have.
    Raw,
}

/// One harness's answer to "how do I type a turn into this TUI".
///
/// Deliberately data, not a function: the engine composes the write from these
/// four facts in one place, so two harnesses cannot end up with two different
/// *orders* of the same steps. What varies per harness is the values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputProfile {
    /// Paste encoding — see [`PasteMode`].
    pub paste: PasteMode,
    /// The bytes that submit the composed input as a turn. `b"\r"` for both
    /// harnesses today (a TUI reads Enter as CR from a PTY, not LF).
    pub submit: &'static [u8],
    /// How long to wait between the paste and the submit.
    ///
    /// **Not cosmetic.** Both TUIs debounce a paste before they re-render, and
    /// a submit that arrives inside that window is processed against a buffer
    /// the TUI has not finished ingesting — the observable symptom being a turn
    /// that carries only the first line, or a `[Pasted text]` placeholder that
    /// submits anyway. The value is a floor, not a measurement (see the module
    /// docs on what is unverified).
    pub settle_ms: u64,
    /// The largest request this profile will type, in bytes.
    ///
    /// A bound, not a truncation point: the engine **refuses** an oversize task
    /// naming this limit rather than typing a prefix of it. Silently sending
    /// half a request is the one failure mode a worker cannot report — it would
    /// answer the truncated question perfectly.
    pub max_paste_bytes: usize,
}

impl InputProfile {
    /// The bytes that carry `task` into the TUI's input buffer, encoded per
    /// [`Self::paste`]. The task is passed through **verbatim** (locked
    /// decision 2a/10): no header, no marker, nothing a worker model could read
    /// as provenance.
    pub fn paste_bytes(&self, task: &str) -> Vec<u8> {
        match self.paste {
            PasteMode::Bracketed => {
                let mut out = Vec::with_capacity(task.len() + BRACKET_START.len() + BRACKET_END.len());
                out.extend_from_slice(BRACKET_START);
                out.extend_from_slice(task.as_bytes());
                out.extend_from_slice(BRACKET_END);
                out
            }
            PasteMode::Raw => task.as_bytes().to_vec(),
        }
    }

    /// Whether `task` fits this profile's paste bound. `false` ⇒ the engine
    /// refuses, naming the limit.
    pub fn fits(&self, task: &str) -> bool {
        task.len() <= self.max_paste_bytes
    }
}

/// `ESC [ 200 ~` — the start of a bracketed paste.
const BRACKET_START: &[u8] = b"\x1b[200~";
/// `ESC [ 201 ~` — the end of one.
const BRACKET_END: &[u8] = b"\x1b[201~";

/// **The harness-neutral lookup** (locked decision 16): the input profile for a
/// tab's harness id, or `None` when that harness declares none.
///
/// `id` is the CHP `agent` discriminator — the same two-word vocabulary
/// [`crate::tabs::tab_consumer`] produces from a tab's configured command, so
/// the id a tab is classified as at spawn and the id its profile is looked up
/// under can never disagree.
///
/// This is the ONE place above L1 that names the harness modules, exactly as
/// [`crate::harness::spawn`] is for readers. `None` is a first-class answer,
/// not an error: it is what makes "a harness without an `input.rs` is not a
/// valid worker" a fail-closed property instead of a panic.
pub fn input_profile(id: &str) -> Option<InputProfile> {
    match id {
        "claude" => Some(super::claude::input::input_profile()),
        "opencode" => Some(super::opencode::input::input_profile()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bracketed_paste_wraps_the_task_verbatim() {
        let p = InputProfile {
            paste: PasteMode::Bracketed,
            submit: b"\r",
            settle_ms: 10,
            max_paste_bytes: 100,
        };
        let bytes = p.paste_bytes("line one\nline two");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\u{1b}[200~line one\nline two\u{1b}[201~",
            "the task must arrive between the markers, unchanged — no header, no marker of ours"
        );
    }

    /// **The submit is never inside the paste.** A CR between the markers is
    /// literal text; the turn is submitted by the separate `submit` write after
    /// the settle. If these two were ever composed into one buffer the settle
    /// would stop existing.
    #[test]
    fn the_paste_carries_no_submit_key() {
        let p = InputProfile {
            paste: PasteMode::Bracketed,
            submit: b"\r",
            settle_ms: 10,
            max_paste_bytes: 100,
        };
        let bytes = p.paste_bytes("hello");
        assert!(!bytes.ends_with(b"\r"));
        assert_eq!(p.submit, b"\r");
    }

    #[test]
    fn raw_mode_passes_the_bytes_through() {
        let p = InputProfile {
            paste: PasteMode::Raw,
            submit: b"\r",
            settle_ms: 0,
            max_paste_bytes: 100,
        };
        assert_eq!(p.paste_bytes("hi"), b"hi".to_vec());
    }

    /// The bound refuses rather than truncates — a half-typed request is the
    /// one failure a worker cannot report.
    #[test]
    fn the_paste_bound_is_a_refusal_not_a_truncation() {
        let p = InputProfile {
            paste: PasteMode::Bracketed,
            submit: b"\r",
            settle_ms: 0,
            max_paste_bytes: 4,
        };
        assert!(p.fits("abcd"));
        assert!(!p.fits("abcde"));
        assert_eq!(
            p.paste_bytes("abcde").len(),
            5 + BRACKET_START.len() + BRACKET_END.len(),
            "encoding does not truncate; `fits` is what the engine asks first"
        );
    }

    /// **Every harness the registry knows has a profile, and an unknown id has
    /// none.** The second half is the fail-closed direction: a tab pointed at
    /// some other CLI must not inherit another harness's paste rules.
    #[test]
    fn every_registry_harness_declares_a_profile_and_nothing_else_does() {
        let ids = super::super::contract::harness_ids();
        assert!(!ids.is_empty(), "the registry names at least one harness");
        for id in &ids {
            assert!(
                input_profile(id).is_some(),
                "harness `{id}` has registry rows but no input profile — it cannot be a \
                 delegation worker, and the gate must be the thing that says so"
            );
        }
        for unknown in ["", "aider", "Claude", "claude.exe", "opencode-2"] {
            assert!(
                input_profile(unknown).is_none(),
                "`{unknown}` resolved to a profile it must not have"
            );
        }
    }

    /// Both shipped profiles submit with CR and paste bracketed. Pinned because
    /// the engine's write order (paste → settle → submit) is only correct for a
    /// profile shaped this way; a future `Raw` harness needs the engine's
    /// ordering test re-read, not just a new constant.
    #[test]
    fn the_shipped_profiles_are_bracketed_and_submit_with_cr() {
        for id in super::super::contract::harness_ids() {
            let p = input_profile(id).expect("declared above");
            assert_eq!(p.paste, PasteMode::Bracketed, "{id}");
            assert_eq!(p.submit, b"\r", "{id}");
            assert!(p.settle_ms > 0, "{id}: a zero settle is the defect this field exists for");
            assert!(p.max_paste_bytes >= 4096, "{id}: the bound must admit a real request");
        }
    }
}

//! V39 Phase B — cImp ▸ OpenCode: **how a turn is typed into its TUI**
//! (locked decision 16, the push half).
//!
//! OpenCode's TUI is a Bubble Tea (Go) app. The dependency is the same sentence
//! as Claude's, and it is equally undocumented:
//!
//! > A bracketed paste (`ESC [ 200 ~ … ESC [ 201 ~`) lands in the editor as
//! > **one literal insertion**, and a CR written after it submits that buffer
//! > as **exactly one turn**.
//!
//! That is the `opencode.input.profile` registry row: Tier **D**,
//! `Dep::Behavior`, `Degradation::FailClosed`, gated through
//! `delegation.worker`.
//!
//! # What is verified, and what is not
//!
//! **Verified from this tree.** OpenCode's TUI enables bracketed paste (private
//! mode 2004) — `src/lib/terminals.ts` passes `2004` through for both
//! harnesses while swallowing mouse tracking, and Bubble Tea's input reader
//! enables it whenever a program reads keys.
//!
//! **NOT verified here.** [`SETTLE_MS`] and [`MAX_PASTE_BYTES`] are floors
//! chosen from the failure they prevent, exactly as on the Claude side; the
//! recorded spike (`harness_versions.input_profile_status`) is the verifier.
//!
//! # Why the settle is shorter than Claude's
//!
//! Bubble Tea delivers a bracketed paste to the program as a single message
//! rather than as a stream of key events, so there is no multi-frame render to
//! outlast — only the one update. It is still non-zero, deliberately: the write
//! and the submit are two separate PTY writes, and "the previous write has been
//! read" is not something a writer can observe.
//!
//! # The read half is this harness's fallback reader, and that is not a gap
//!
//! OpenCode declares `cannot` for CHP's `assistant_text`
//! (`harness/opencode/plugin.rs`), so a delegation on an OpenCode worker is
//! completed by the `/event` SSE reader (`opencode.sse.events`) — this
//! harness's *declared* fallback since V35 Phase L (design D6), not a
//! degradation. The engine's preflight accepts either source; what it refuses
//! is a tab with neither.

use crate::harness::plugin::{InputProfile, PasteMode};

/// Milliseconds between the paste and the submit. See the module docs.
const SETTLE_MS: u64 = 80;

/// Largest request cImp will type into an OpenCode tab, in bytes.
const MAX_PASTE_BYTES: usize = 64 * 1024;

/// OpenCode's input profile.
pub fn input_profile() -> InputProfile {
    InputProfile {
        paste: PasteMode::Bracketed,
        submit: b"\r",
        settle_ms: SETTLE_MS,
        max_paste_bytes: MAX_PASTE_BYTES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_is_bracketed_with_a_nonzero_settle() {
        let p = input_profile();
        assert_eq!(p.paste, PasteMode::Bracketed);
        assert_eq!(p.submit, b"\r");
        assert!(
            p.settle_ms > 0,
            "two PTY writes are two writes; a zero settle assumes an ordering the writer cannot see"
        );
        assert!(p.max_paste_bytes >= 4096);
    }
}

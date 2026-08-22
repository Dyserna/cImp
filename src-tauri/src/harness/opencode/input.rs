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

use crate::harness::contract::{Capability, Degradation, Dep, Harness, Seam};
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


/// This harness, as the registry's own opaque id — the same value
/// `contract.rs` spells for its neutral rows, declared here because a row that
/// names its harness by hand is a row that can name the wrong one.
const HARNESS: Harness = Harness::declared("opencode");

/// The registry row this profile depends on, contributed to
/// [`crate::harness::contract::capabilities`] by
/// [`HarnessPlugin::capabilities`](crate::harness::plugin::HarnessPlugin::capabilities).
///
/// It lives here rather than in the neutral table because its CONTRACT is a
/// sentence about OpenCode's TUI, and it is the same sentence the module
/// docs above state: the row and the values it is about are one edit.
pub const CAPABILITIES: &[Capability] = &[
    Capability {
        id: "opencode.input.profile",
        harness: HARNESS,
        tier: Seam::D,
        contract: "OpenCode's TUI accepts a bracketed paste (`ESC [ 200 ~` … `ESC [ 201 ~`) as                    ONE literal insertion, and a CR written after it submits that buffer as                    exactly one turn.",
        depends_on: &[Dep::Behavior(
            "multi-line bracketed paste + submit yields exactly one turn",
        )],
        wired_in: &[
            "src-tauri/src/harness/opencode/input.rs",
            "src-tauri/src/harness/plugin.rs",
        ],
        degradation: Degradation::FailClosed,
        drift_rule: &[],
        canary: None,
        probe: None,
        waiver: Some(
            "Same class as `claude.input.profile`, same spike, same recorded outcome. Owner: V39              Phase D.",
        ),
        controls: &[],
        drift_token: None,
    },
];

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

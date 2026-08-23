//! V39 Phase B — cImp ▸ Claude Code: **how a turn is typed into its TUI**
//! (locked decision 16, the push half).
//!
//! Claude Code's TUI is an Ink (React-for-terminals) app. What cImp depends on
//! here is one behaviour, and it is not documented anywhere upstream:
//!
//! > A bracketed paste (`ESC [ 200 ~ … ESC [ 201 ~`) lands in the composer as
//! > **one literal insertion** — embedded newlines become newlines in the
//! > buffer, not submits — and a CR written after it submits that buffer as
//! > **exactly one turn**.
//!
//! That is the `claude.input.profile` registry row: Tier **D**,
//! `Dep::Behavior`, `Degradation::FailClosed`. Its spike outcome is recorded in
//! `harness_versions.input_profile_status`, and the `delegation.worker` gate
//! reads it — a recorded `"fail"` removes `delegate_task_claude` and refuses
//! preflight naming the reason, rather than typing half a request into a tab.
//!
//! # What is verified, and what is not
//!
//! **Verified from this tree.** Claude's TUI enables bracketed paste (private
//! mode 2004): `src/lib/terminals.ts`'s AI mouse control deliberately passes
//! `2004` through while swallowing the mouse-tracking modes, because both
//! harnesses set it. A terminal that has been asked for bracketed paste is one
//! that has an insertion path distinct from keystrokes.
//!
//! **NOT verified here, and the spike is the verifier.** Three values below are
//! engineering floors chosen from the failure they prevent, not measurements:
//!
//! * [`SETTLE_MS`] — Ink re-renders on a debounce, and a CR that arrives inside
//!   that window is evaluated against a buffer the composer has not finished
//!   ingesting. The observed shape of this failure (recorded in the milestone
//!   as the reason the field exists at all) is a turn carrying only the first
//!   line, or a `[Pasted text …]` placeholder that submits anyway.
//! * [`MAX_PASTE_BYTES`] — Claude's composer collapses a large paste into a
//!   placeholder; where exactly it does so is a build detail. The bound is a
//!   **refusal** point, never a truncation point.
//! * That the placeholder-collapse path still submits the FULL text rather than
//!   the placeholder. If that ever stops being true the symptom is a worker
//!   answering a question nobody asked, which is precisely why this row fails
//!   closed on a recorded spike failure instead of degrading silently.
//!
//! Nothing here is spawn-baked: the profile is read when a delegation runs, so
//! changing it needs no tab restart (locked decision 15).

use crate::harness::contract::{Capability, Degradation, Dep, Harness, Seam};
use crate::harness::plugin::{InputProfile, PasteMode};

/// Milliseconds between the paste and the submit. See the module docs — a
/// floor, not a measurement.
const SETTLE_MS: u64 = 150;

/// Largest request cImp will type into a Claude tab, in bytes. Comfortably
/// above any real delegated task and well under the size at which a composer
/// starts making its own decisions about a paste.
const MAX_PASTE_BYTES: usize = 64 * 1024;

/// Claude Code's input profile.
pub fn input_profile() -> InputProfile {
    InputProfile {
        paste: PasteMode::Bracketed,
        // CR, not LF: a TUI reads Enter off a PTY as `\r`. `\n` is a newline in
        // the composer for this harness, which is the opposite of a submit.
        submit: b"\r",
        settle_ms: SETTLE_MS,
        max_paste_bytes: MAX_PASTE_BYTES,
    }
}


/// This harness, as the registry's own opaque id — the same value
/// `contract.rs` spells for its neutral rows, declared here because a row that
/// names its harness by hand is a row that can name the wrong one.
const HARNESS: Harness = Harness::declared("claude");

/// The registry row this profile depends on, contributed to
/// [`crate::harness::contract::capabilities`] by
/// [`HarnessPlugin::capabilities`](crate::harness::plugin::HarnessPlugin::capabilities).
///
/// It lives here rather than in the neutral table because its CONTRACT is a
/// sentence about Claude Code's TUI, and it is the same sentence the module
/// docs above state: the row and the values it is about are one edit.
pub const CAPABILITIES: &[Capability] = &[
    Capability {
        id: "claude.input.profile",
        harness: HARNESS,
        tier: Seam::D,
        contract: "Claude Code's TUI accepts a bracketed paste (`ESC [ 200 ~` … `ESC [ 201 ~`) as                    ONE literal insertion — embedded newlines land in the composer as newlines,                    not as submits — and a CR written after it submits that buffer as exactly one                    turn.",
        depends_on: &[Dep::Behavior(
            "multi-line bracketed paste + submit yields exactly one turn",
        )],
        wired_in: &[
            "src-tauri/src/harness/claude/input.rs",
            "src-tauri/src/harness/plugin.rs",
        ],
        degradation: Degradation::FailClosed,
        drift_rule: &[],
        canary: None,
        probe: None,
        waiver: Some(
            "A `Dep::Behavior` no payload reveals: a TUI that split a paste into two turns would              corrupt the task SILENTLY, which is why the row fails closed instead of degrading.              Verification is the input-profile spike recorded in              `harness_versions.input_profile_status` (recipe in MAINTENANCE.md); a recorded              `\"fail\"` blocks `delegation.worker` for every harness. Owner: V39 Phase D.",
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
            p.settle_ms >= 50,
            "a settle under the TUI's own render debounce is the defect this field exists for"
        );
        assert!(p.max_paste_bytes >= 4096);
    }
}

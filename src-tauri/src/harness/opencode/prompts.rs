//! V40 Phase C, locked decision 21 — **OpenCode's TUI prompt rows.**
//!
//! Two rows, both **shipped disabled**, and that is the honest state of this
//! seam rather than an omission: `opencode --mini`'s permission and working
//! chrome has never been captured live (V19 task A4), so what is declared here
//! is a template with the capture recipe attached, not a transcription. A wrong
//! guess enabled by default would mis-fire the avatar and the permission
//! notification on every OpenCode tab, which is strictly worse than detecting
//! nothing.
//!
//! Capture recipe: run a permission-triggering action in an `opencode --mini`
//! tab with `RUST_LOG=perm_capture=debug`, take a distinctive substring from the
//! dumped tail (the footer chrome, or the `Allow`/`Deny` option line), replace
//! the placeholder and flip `disabled` to `false`.

use crate::processing::permission::{PatternKind, PatternSpec};

/// OpenCode's shipped prompt rows, in the order the seed file writes them.
pub const PATTERNS: &[PatternSpec] = &[
    // OpenCode (`opencode --mini`) permission prompt. OpenCode asks for
    // tool/edit/bash approval with its own inline footer. The exact marker
    // substring must be captured live against `opencode --mini` (the
    // alternate-screen TUI is never launched) — run a permission-triggering
    // action with `RUST_LOG=perm_capture=debug` and replace the placeholder
    // below with a distinctive substring from the dumped tail (e.g. the
    // footer chrome or the "Allow"/"Deny" option line). Shipped `disabled`
    // until characterized (V19 task A4) so a wrong guess can't mis-fire;
    // flip to `disabled: false` once the real marker is in place.
    PatternSpec {
        name: "opencode_permission",
        kind: PatternKind::Permission,
        all_of: &["<replace with a substring unique to opencode --mini's permission prompt>"],
        none_of: &[],
        disabled: true,
    },
    // OpenCode's "busy"/working footer while a request is in flight (drives
    // the avatar's Thinking-Idle state, like `claude_working`). Capture the
    // live `--mini` working chrome the same way and replace the placeholder;
    // shipped `disabled` until characterized (V19 task A4).
    PatternSpec {
        name: "opencode_working",
        kind: PatternKind::Working,
        all_of: &["<replace with a substring unique to opencode --mini's working footer>"],
        none_of: &[],
        disabled: true,
    },
];

/// OpenCode's rows in one named earlier era of the shipped `patterns.json`.
///
/// The two placeholder rows entered the seed in v0.22.0 and have not changed
/// since, so that era's rows are [`PATTERNS`] itself; nothing OpenCode shipped
/// before that exists to reconcile — v0.22.0 is the release that replaced aider
/// with OpenCode.
pub fn legacy_patterns(era: &str) -> &'static [PatternSpec] {
    match era {
        "v0.22.0..v0.49.1" => PATTERNS,
        _ => &[],
    }
}

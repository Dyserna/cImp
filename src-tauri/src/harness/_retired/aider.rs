//! **A retired harness's permission rows, and nothing else.**
//!
//! V40 Phase C, locked decision 21. Aider was cImp's third supported harness
//! until V19 replaced it with OpenCode. It has no plugin, no descriptor, no tab
//! and no code path — deliberately, because it is not a harness this build can
//! run. What survives is three permission-pattern rows, for exactly one
//! consumer: pristine-file reconciliation.
//!
//! A `patterns.json` seeded between v0.6.3 and v0.21.x contains these rows. The
//! reconciler decides a file is *pristine* (and may therefore be replaced with
//! today's better defaults) by comparing it against every set a past release
//! shipped; drop these rows and every such install's file stops comparing equal,
//! is treated as hand-edited forever, and never receives another default fix —
//! the 199221b bug, one harness over.
//!
//! So: data only, frozen, append-never. If a fourth harness is ever retired its
//! rows land beside these under the same rule.

use crate::processing::permission::{PatternKind, PatternSpec};

/// Aider's rows in one named earlier era of the shipped `patterns.json`.
///
/// Present in v0.6.3 through v0.21.x; gone from v0.22.0, which is the release
/// that replaced aider with OpenCode.
pub fn legacy_patterns(era: &str) -> &'static [PatternSpec] {
    match era {
        "v0.6.3..v0.6.x" | "v0.7.0..v0.21.x" => PATTERNS,
        _ => &[],
    }
}

/// The three rows, exactly as v0.6.3 wrote them.
const PATTERNS: &[PatternSpec] = &[
    PatternSpec {
        name: "aider_apply_edits",
        kind: PatternKind::Permission,
        all_of: &["Apply edits?", "(Y)es"],
        none_of: &[],
        disabled: false,
    },
    PatternSpec {
        name: "aider_add_to_chat",
        kind: PatternKind::Permission,
        all_of: &["Add ", " to the chat?"],
        none_of: &[],
        disabled: false,
    },
    PatternSpec {
        name: "aider_run_shell",
        kind: PatternKind::Permission,
        all_of: &["Run shell command?"],
        none_of: &[],
        disabled: false,
    },
];

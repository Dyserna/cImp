//! **Claude Code's OWN native tool ids** — the reviewed table its plugin
//! declares to core (V40 Phase A, locked decision 16: moved verbatim out of
//! `offload/toolclass.rs`'s `TABLE` and `graph/memory.rs::classify_tool`).
//!
//! This is a *harness registry*, not cImp's tool vocabulary, and it obeys the
//! opposite default from [`crate::offload::toolclass::TABLE`]. That table is
//! the set of names cImp ROUTES, where unknown ⇒ EXTERNAL because every
//! unrouted name is a proxied MCP id in disguise. This one is the set of names
//! the harness serves ITSELF; core never routes any of them, so no cImp latch
//! can block them — decision 3's honest limit, with OS-level containment left
//! to V33 and optional hook-based gating to a future phase.
//!
//! # Why the rows exist, given nothing can enforce them
//!
//! Three consumers, and each one used to read a *different* table:
//!
//! * **`mutates_fs`** — V33 Phase F's pre-tool checkpoint. The
//!   `/workbench/tool_checkpoint` route re-resolves the name a `PreToolUse`
//!   shim reported against this table, so a drifted matcher or a forged POST
//!   cannot mint a checkpoint for a tool cImp does not classify as mutating.
//! * **`memory_kind`** — the V10 memory event a tool call is recorded as. This
//!   used to be an inline `match` in `graph/memory.rs` that matched BOTH
//!   harnesses' ids at once (`Edit` next to `edit`), which the Phase K layering
//!   scan recorded as a FINDING rather than an exemption. It is per harness
//!   now, which is what the two vocabularies always were.
//! * **`class`** — declared, unconsumed for this harness today, and kept
//!   because a future hook that DOES gate a native tool needs its class from
//!   the same reviewed place rather than inventing one.
//!
//! # `class: None` is not "unclassified"
//!
//! It means cImp makes no gating claim about this name. `Read` is here for its
//! memory kind, not because anything gates it. Compare
//! [`crate::harness::opencode::tools`], where `class: Some(..)` IS the gate's
//! membership test.
//!
//! **TCB-adjacent (design § 5):** `mutates_fs` decides whether a checkpoint is
//! taken before a tool that can rewrite the tree. Flipping a row to `false`
//! removes a safety net; adding a row removes the fail-closed default that
//! covers an unrecognised name.

use crate::harness::plugin::{MemArg, NativeTool};
use crate::offload::toolclass::ToolClass;

/// Claude Code's native tool ids, as cImp classifies them.
///
/// `mutates_fs` is `true` for exactly the four names V33 Phase F put in the
/// `PreToolUse` matcher (`harness/claude/overlay.rs`'s
/// `CLAUDE_MUTATING_TOOL_MATCHER`), and
/// `overlay::tests::every_matched_claude_tool_is_classified_as_mutating` pins
/// that direction: a matched name with no `mutates_fs: true` row would hold
/// every one of its calls for a checkpoint the core then declines.
pub const CLAUDE_NATIVE_TABLE: &[NativeTool] = &[
    // ── reads: no mutation, recorded as memory `read` ───────────────────────
    NativeTool {
        name: "Read",
        class: None,
        mutates_fs: false,
        memory_kind: Some(("read", MemArg::Path)),
    },
    NativeTool {
        name: "NotebookRead",
        class: None,
        mutates_fs: false,
        memory_kind: Some(("read", MemArg::Path)),
    },
    // ── the mutation surface ────────────────────────────────────────────────
    //
    // These four carried `LocalCapability` rows in `toolclass::TABLE` (marked
    // `unrouted`, because no cImp dispatcher serves them) until V40 Phase A
    // moved them here. The class travels unchanged; so does `mutates_fs`.
    NativeTool {
        name: "Edit",
        class: Some(ToolClass::LocalCapability),
        mutates_fs: true,
        memory_kind: Some(("edit", MemArg::Path)),
    },
    NativeTool {
        name: "Write",
        class: Some(ToolClass::LocalCapability),
        mutates_fs: true,
        memory_kind: Some(("edit", MemArg::Path)),
    },
    // V33 Phase F. `MultiEdit` had no row at all, so `mutates_fs` answered
    // `false` for it — while the PostToolUse auto-check matcher has named it
    // (`"Edit|Write|MultiEdit"`) since V12, i.e. cImp already treated it as an
    // edit everywhere except in the one place that decides whether to take a
    // checkpoint. It is the tool a checkpoint is worth most for: one call
    // rewrites several files.
    NativeTool {
        name: "MultiEdit",
        class: Some(ToolClass::LocalCapability),
        mutates_fs: true,
        memory_kind: Some(("edit", MemArg::Path)),
    },
    NativeTool {
        name: "Bash",
        class: Some(ToolClass::LocalCapability),
        mutates_fs: true,
        memory_kind: Some(("query", MemArg::Command)),
    },
    // **`mutates_fs: false` is today's behaviour, recorded rather than
    // endorsed.** `NotebookEdit` writes a `.ipynb`, so on the merits it belongs
    // with the four above — but it has never had a `mutates_fs` row and it is
    // not in the `PreToolUse` matcher, so no call of it has ever reached the
    // checkpoint route. V40 Phase A is a verbatim move and does not change it;
    // widening the matcher and this flag together is the edit that would.
    NativeTool {
        name: "NotebookEdit",
        class: None,
        mutates_fs: false,
        memory_kind: Some(("edit", MemArg::Path)),
    },
    // ── structural / content queries ────────────────────────────────────────
    NativeTool {
        name: "Grep",
        class: None,
        mutates_fs: false,
        memory_kind: Some(("query", MemArg::Pattern)),
    },
    NativeTool {
        name: "Glob",
        class: None,
        mutates_fs: false,
        memory_kind: Some(("query", MemArg::Pattern)),
    },
    // ── the harness's own web tools — the EXTERNAL side of the boundary ─────
    //
    // The taint beacon's fire set (`overlay::CLAUDE_WEB_TOOL_MATCHER`). Neither
    // writes to the project tree, so neither checkpoints, and neither is a
    // memory event: a fetch has no path or pattern to record. Declared so the
    // fail-closed default in `harness::native::mutates_fs` — an undeclared name
    // is treated as mutating — does not turn a web fetch into a checkpoint.
    NativeTool {
        name: "WebFetch",
        class: Some(ToolClass::External),
        mutates_fs: false,
        memory_kind: None,
    },
    NativeTool {
        name: "WebSearch",
        class: Some(ToolClass::External),
        mutates_fs: false,
        memory_kind: None,
    },
    // Ids deliberately absent: `Task` and `TodoWrite` (orchestration and
    // bookkeeping — `classify_tool` returned `None` for both, and they are not
    // in either matcher), and every `mcp__*` id, which is a PROXIED name cImp
    // classifies through `toolclass::classify` in its own vocabulary.
];

/// Whether `name` can change files on disk, **in Claude's vocabulary**.
///
/// `false` for a name with no row here — which is not the same answer
/// [`crate::harness::native::mutates_fs`] gives a caller who could not identify
/// the harness at all. Here the set is closed and published; there, an
/// unidentifiable source fails closed.
///
/// Production reads this table through [`crate::harness::native`], which is
/// where the source is resolved; this accessor stays because two things assert
/// on it — `overlay::tests::every_matched_claude_tool_is_classified_as_mutating`
/// (the `PreToolUse` matcher must name only mutating tools) and this file's own
/// suite — and because a table with no executable statement of its contract is a
/// comment.
#[cfg_attr(not(test), allow(dead_code))]
pub fn claude_native_mutates_fs(name: &str) -> bool {
    CLAUDE_NATIVE_TABLE
        .iter()
        .find(|t| t.name == name)
        .is_some_and(|t| t.mutates_fs)
}

/// The memory event kind and target-argument key for `name`, or `None` for a
/// tool that is not recorded (`Task`, `TodoWrite`, cImp's own proxied tools —
/// those are already captured by the activity ring).
///
/// Called directly by the Claude transcript reader, which is already inside
/// this harness and needs no source resolution; every OTHER caller goes through
/// [`crate::harness::native::memory_kind`].
pub fn claude_memory_kind(name: &str) -> Option<(&'static str, MemArg)> {
    CLAUDE_NATIVE_TABLE
        .iter()
        .find(|t| t.name == name)
        .and_then(|t| t.memory_kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mutation half is exactly the four names the `PreToolUse` matcher
    /// fires on — the set V33 Phase F reviewed, restated where the checkpoint
    /// route now reads it.
    #[test]
    fn the_mutating_set_is_the_write_surface() {
        let mutating: Vec<&str> = CLAUDE_NATIVE_TABLE
            .iter()
            .filter(|t| t.mutates_fs)
            .map(|t| t.name)
            .collect();
        assert_eq!(mutating, vec!["Edit", "Write", "MultiEdit", "Bash"]);
        for n in ["Read", "NotebookRead", "Grep", "Glob", "WebFetch", "WebSearch"] {
            assert!(!claude_native_mutates_fs(n), "{n} must not checkpoint");
        }
    }

    /// The memory classification is the one that used to live in
    /// `graph/memory.rs`, and it must answer identically for every id it named.
    #[test]
    fn the_memory_kinds_are_the_ones_graph_memory_used_to_answer() {
        assert_eq!(claude_memory_kind("Read"), Some(("read", MemArg::Path)));
        assert_eq!(
            claude_memory_kind("NotebookRead"),
            Some(("read", MemArg::Path))
        );
        for n in ["Edit", "Write", "MultiEdit", "NotebookEdit"] {
            assert_eq!(claude_memory_kind(n), Some(("edit", MemArg::Path)), "{n}");
        }
        for n in ["Grep", "Glob"] {
            assert_eq!(claude_memory_kind(n), Some(("query", MemArg::Pattern)), "{n}");
        }
        assert_eq!(
            claude_memory_kind("Bash"),
            Some(("query", MemArg::Command))
        );
        // …and the ids it deliberately answered `None` for still do.
        for n in ["Task", "TodoWrite", "mcp__cimp-offload__graph_find_symbol"] {
            assert_eq!(claude_memory_kind(n), None, "{n}");
        }
    }

    /// **The two vocabularies stay disjoint.** Reading an OpenCode id through
    /// this table (or the reverse) is the drift the split exists to prevent:
    /// `edit` is unknown here and `Edit` is unknown there, so a crossed lookup
    /// disables a whole harness's seam while every test of the other one stays
    /// green.
    #[test]
    fn claudes_ids_are_not_opencodes() {
        use crate::harness::opencode::tools::OPENCODE_NATIVE_TABLE;
        for t in CLAUDE_NATIVE_TABLE {
            assert!(
                !OPENCODE_NATIVE_TABLE.iter().any(|o| o.name == t.name),
                "`{}` is in both harnesses' tables — one lookup, two vocabularies",
                t.name
            );
        }
        assert!(!claude_native_mutates_fs("edit"));
        assert!(claude_native_mutates_fs("Edit"));
    }

    /// No duplicate rows — a second row for a name would make every lookup
    /// depend on table order.
    #[test]
    fn every_row_is_unique() {
        for (i, t) in CLAUDE_NATIVE_TABLE.iter().enumerate() {
            assert!(
                !CLAUDE_NATIVE_TABLE[..i].iter().any(|p| p.name == t.name),
                "duplicate row for `{}`",
                t.name
            );
        }
    }
}

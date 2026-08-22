//! V40 Phase A, locked decision 16 — **the neutral lookup over the harnesses'
//! own tool vocabularies.**
//!
//! Every harness serves tools cImp never routes: Claude's `Edit`/`Bash`,
//! OpenCode's `edit`/`bash`. Core has to answer three questions about a name
//! that arrives on the wire — *does it mutate the tree?*, *what memory event is
//! it?*, *what class would gate it?* — and until this module those questions
//! were answered by a `match` on a harness string with **Claude's table in the
//! `_` arm** (`loopback::tool_checkpoint_is_mutating`) and by an inline
//! `match` that mixed both vocabularies at once
//! (`graph/memory.rs::classify_tool`, recorded by V35 Phase K as a FINDING
//! rather than an exemption).
//!
//! Both shapes have the same defect and it is not stylistic: a THIRD harness's
//! `edit` was not rejected, it was answered from Claude's table — `false`,
//! silently, for its entire mutation surface.
//!
//! # The rule: an unidentified source fails CLOSED
//!
//! [`mutates_fs`] answers **`true`** when the harness is unknown, or when the
//! name is one the registered plugin does not declare. "Not in Claude's table,
//! therefore safe" is exactly the inference that made the `_` arm wrong, and
//! the two directions are not symmetric in cost: a checkpoint nobody needed is
//! a wasted commit into cImp's own shadow repo, while a missed one is a
//! destructive tool call with no way back. The routes that consume this are
//! bearer-token-gated and throttled, so the wasted-commit side is bounded.
//!
//! [`memory_kind`] answers `None` for the same inputs, and that is the same
//! direction: recording nothing is the conservative answer for a bookkeeping
//! ring, and an event cImp cannot attribute to a vocabulary is an event it
//! cannot classify.

use super::plugin::{MemArg, NativeTool};
use super::registry::HarnessId;
use crate::offload::toolclass::ToolClass;

/// The row `harness` declares for `name`, if any.
fn row(harness: HarnessId, name: &str) -> Option<&'static NativeTool> {
    harness
        .plugin()?
        .native_tools()
        .iter()
        .find(|t| t.name == name)
}

/// Whether `name` can change files on disk, in `harness`'s own vocabulary.
///
/// **`None` (an unidentified source) ⇒ `true`, and an undeclared name ⇒
/// `true`** — see the module docs. Consumed by the loopback's
/// `/workbench/tool_checkpoint` route, which is the authority over whatever
/// matcher the harness-side shim was baked with.
pub fn mutates_fs(harness: Option<HarnessId>, name: &str) -> bool {
    match harness {
        Some(h) => row(h, name).is_none_or(|t| t.mutates_fs),
        None => true,
    }
}

/// The memory event kind and the argument key carrying its target, or `None`
/// for a tool that is not recorded — including every name from a source cImp
/// could not identify.
pub fn memory_kind(harness: Option<HarnessId>, name: &str) -> Option<(&'static str, MemArg)> {
    row(harness?, name)?.memory_kind
}

/// The containment class `harness` declares for `name`, or `None` when it makes
/// no gating claim about it.
///
/// Deliberately **not** [`crate::offload::toolclass::classify`]'s
/// unknown-⇒-EXTERNAL: that default is right for cImp's routed vocabulary,
/// where every unlisted name is a proxied MCP id in disguise, and wrong for a
/// harness's own registry, which is a closed published set whose members
/// include bookkeeping tools that are neither external nor local-capability.
/// Answering `External` for `todowrite` would refuse a to-do list under a LOCAL
/// latch — a partial, arbitrary gate, which the V32 E2 spike showed is worse
/// than none.
// Declared now, gated later: the hook that would REFUSE a native tool is a
// later phase, and the column has to exist before something can read it.
#[cfg_attr(not(test), allow(dead_code))]
pub fn class(harness: HarnessId, name: &str) -> Option<ToolClass> {
    row(harness, name)?.class
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(id: &str) -> HarnessId {
        HarnessId::from_id(id).unwrap_or_else(|| panic!("{id} is registered"))
    }

    /// The defect this module exists for, as an assertion: a name from a source
    /// cImp cannot identify is treated as mutating, not as Claude's.
    #[test]
    fn an_unidentified_source_fails_closed() {
        assert!(mutates_fs(None, "edit"));
        assert!(mutates_fs(None, "Read"));
        assert!(mutates_fs(None, "anything_at_all"));
        assert_eq!(memory_kind(None, "Edit"), None);
    }

    /// An identified harness answers from ITS table only — the two vocabularies
    /// are never crossed.
    #[test]
    fn each_harness_answers_in_its_own_vocabulary() {
        assert!(mutates_fs(Some(h("claude")), "Edit"));
        assert!(!mutates_fs(Some(h("claude")), "Read"));
        assert!(mutates_fs(Some(h("opencode")), "edit"));
        assert!(!mutates_fs(Some(h("opencode")), "read"));
        // …and a name the OTHER harness owns is undeclared here, so it fails
        // closed rather than being answered from the neighbour's table.
        assert!(mutates_fs(Some(h("opencode")), "Edit"));
        assert!(mutates_fs(Some(h("claude")), "edit"));
    }

    /// The memory classification `graph/memory.rs` used to do in one `match`,
    /// now split by source — and the split is what makes the answers honest:
    /// `edit` is an OpenCode id and `Edit` a Claude one, and neither harness
    /// answers for the other's.
    #[test]
    fn memory_kinds_are_per_harness() {
        assert_eq!(
            memory_kind(Some(h("claude")), "Edit"),
            Some(("edit", MemArg::Path))
        );
        assert_eq!(
            memory_kind(Some(h("opencode")), "edit"),
            Some(("edit", MemArg::Path))
        );
        assert_eq!(memory_kind(Some(h("claude")), "edit"), None);
        assert_eq!(memory_kind(Some(h("opencode")), "Edit"), None);
        // Not recorded, in either vocabulary.
        assert_eq!(memory_kind(Some(h("claude")), "TodoWrite"), None);
        assert_eq!(memory_kind(Some(h("opencode")), "todowrite"), None);
    }

    /// The class column travels with the name, and `None` keeps meaning "no
    /// gating claim" rather than "EXTERNAL" — the difference between the two
    /// tables' defaults, asserted rather than described.
    #[test]
    fn the_class_column_is_per_harness_and_none_is_not_external() {
        assert_eq!(
            class(h("claude"), "Edit"),
            Some(ToolClass::LocalCapability),
            "Claude's `Edit` kept the class its `toolclass::TABLE` row carried"
        );
        assert_eq!(class(h("opencode"), "edit"), Some(ToolClass::LocalCapability));
        assert_eq!(class(h("claude"), "WebFetch"), Some(ToolClass::External));
        // Declared, but with no gating claim — NOT `External`, which is what
        // `toolclass::classify` would answer and what would deny a bookkeeping
        // tool under a LOCAL latch.
        assert_eq!(class(h("claude"), "Read"), None);
        assert_eq!(class(h("opencode"), "todowrite"), None);
        assert_eq!(class(h("claude"), "not_a_tool"), None);
    }

    /// Every registered harness declares SOME native vocabulary. A harness that
    /// declared none would have every one of its tool calls fail closed — every
    /// call a checkpoint, no call a memory event — which is a working system
    /// only in the sense that nothing crashes.
    #[test]
    fn every_registered_harness_declares_its_natives() {
        for id in super::super::registry::all() {
            let Some(p) = id.plugin() else { continue };
            assert!(
                !p.native_tools().is_empty(),
                "{id} declares no native tools — every one of its tool calls would be treated as \
                 mutating and none as a memory event"
            );
        }
    }
}

/// The value `harness` records for one memory event's [`MemArg`], from the tool
/// call's own argument map.
///
/// The neutral half of locked decision 16's argument vocabulary: core asks for
/// "the thing this event is about" and the plugin decides which keys carry it.
/// `None` for an unidentified source, and for a payload that carries none of the
/// declared keys — recording nothing is the conservative answer for a
/// bookkeeping ring.
pub fn memory_arg(
    harness: Option<HarnessId>,
    arg: MemArg,
    args: &serde_json::Value,
) -> Option<String> {
    let keys = harness?.plugin()?.memory_arg_keys(arg);
    keys.iter()
        .find_map(|k| args.get(*k).and_then(|v| v.as_str()))
        .map(str::to_string)
}

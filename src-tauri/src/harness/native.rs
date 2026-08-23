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
pub fn class(harness: HarnessId, name: &str) -> Option<ToolClass> {
    row(harness, name)?.class
}

/// The class ANY registered harness declares for `name` — the harness-neutral
/// question [`crate::offload::toolclass::classify`] has to answer, since its
/// signature carries no source.
///
/// **V40 review finding M-7 (parity lens), and the column's first production
/// consumer.** Phase A moved Claude's four `LocalCapability` rows (`Edit`,
/// `Write`, `Bash`, `MultiEdit`) out of `toolclass::TABLE` and into
/// `native_tools()` — correctly, they are a harness's vocabulary and not
/// cImp's — but nothing then read the declaration back, so `classify("Edit")`
/// fell to the table's unknown-⇒-EXTERNAL default. That default is not a
/// neutral loss: `Latch::blocks` REFUSES `LocalCapability` under an EXTERNAL
/// latch and ADMITS `External`, and `Latch::engage` moves an open tab to
/// `External` rather than `Local` for it. So a tainted session's `Edit` stopped
/// being refused and an ordinary `Edit` started marking the tab
/// externally-contaminated. Live-verify 9a asserts the pre-V40 classes, which
/// is this.
///
/// Every harness's declarations are consulted, not one named here: the two
/// vocabularies are disjoint by construction (capitalised vs lowercase ids) and
/// a name only one harness declares is that harness's claim about it. First
/// declaration wins, and `no_two_harnesses_declare_one_native_name_differently`
/// refuses a registry where that could matter.
pub fn declared_class(name: &str) -> Option<ToolClass> {
    super::registry::all().find_map(|h| class(h, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(id: &str) -> HarnessId {
        HarnessId::from_id(id).unwrap_or_else(|| panic!("{id} is registered"))
    }

    /// **No two harnesses may declare one native name with different classes**
    /// (V40 review M-7).
    ///
    /// `declared_class` takes the FIRST declaration, so two harnesses claiming
    /// one name differently would have the registry's order decide a gate.
    /// Today the two vocabularies are disjoint (capitalised vs lowercase), and
    /// this refuses a registry where they stop being.
    #[test]
    fn no_two_harnesses_declare_one_native_name_differently() {
        let mut seen: std::collections::BTreeMap<&str, (Option<ToolClass>, &str)> =
            Default::default();
        for h in super::super::registry::all() {
            let Some(p) = h.plugin() else { continue };
            for t in p.native_tools() {
                if let Some((class, owner)) = seen.insert(t.name, (t.class, h.token())) {
                    assert_eq!(
                        class,
                        t.class,
                        "`{}` is declared by both `{owner}` and `{}` with different classes —                          `declared_class` takes the first, so the registry's ORDER would decide                          a gate",
                        t.name,
                        h.token()
                    );
                }
            }
        }
        assert!(!seen.is_empty(), "no harness declares a native tool");
    }

    /// The declared class reaches the neutral lookup — the pre-V40 answer for
    /// the four rows Phase A moved out of `toolclass::TABLE`.
    #[test]
    fn declared_class_answers_for_every_harnesss_own_vocabulary() {
        assert_eq!(
            declared_class("Edit"),
            Some(ToolClass::LocalCapability),
            "the pre-V40 class, restored"
        );
        assert_eq!(declared_class("edit"), Some(ToolClass::LocalCapability));
        // A name no harness declares has no answer here — the caller's own
        // default decides, and `toolclass::classify`'s is EXTERNAL.
        assert_eq!(declared_class("nothing_declares_this"), None);
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

    /// **`docs/HARNESS-NATIVE-TOOLS.md` is the machine-checked twin of
    /// `native_tools()`** (V40 Phase G, carried item).
    ///
    /// That document has always described what each harness serves; §§ 2 and 3
    /// are the VENDOR's surface, transcribed from their docs, and are
    /// deliberately wider than these tables. What was missing was the other
    /// half: what cImp *classifies*, which is the half that decides whether a
    /// checkpoint is taken before a call, whether the call is recorded as a
    /// memory event, and what class would gate it. That half was prose, so it
    /// drifted — the document said "the eight ids in `OPENCODE_NATIVE_TABLE`"
    /// while the table had ten. It is checked now, in both directions: a row
    /// that outlives its declaration fails as loudly as a declaration with no
    /// row.
    ///
    /// **Which section belongs to which harness is derived, not hard-coded.**
    /// The test finds every *"What cImp's plugin declares"* subsection, matches
    /// each to the harness whose vocabulary its first row is in, and asserts
    /// every registered harness has exactly one — so a harness added later
    /// needs a section here by being registered, and a section for a harness
    /// nobody registered is a failure rather than dead prose.
    #[test]
    fn the_native_tools_doc_matches_the_declared_tables() {
        const MARKER: &str = "What cImp's plugin declares";
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../docs/HARNESS-NATIVE-TOOLS.md");
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .replace("\r\n", "\n");

        // (heading, rows) for every declaring subsection in the document.
        let mut sections: Vec<(String, Vec<String>)> = Vec::new();
        let mut current: Option<(String, Vec<String>)> = None;
        for line in doc.lines().map(str::trim_end) {
            if line.starts_with("## ") || line.starts_with("### ") {
                if let Some(done) = current.take() {
                    sections.push(done);
                }
                if line.contains(MARKER) {
                    current = Some((line.to_string(), Vec::new()));
                }
            } else if let Some((_, rows)) = current.as_mut() {
                if line.starts_with("| `") {
                    rows.push(line.to_string());
                }
            }
        }
        if let Some(done) = current.take() {
            sections.push(done);
        }
        assert!(
            !sections.is_empty(),
            "docs/HARNESS-NATIVE-TOOLS.md has no \"{MARKER}\" subsection at all — the \
             machine-checked half of the document is gone, and this test would otherwise \
             pass by finding nothing"
        );

        let mut claimed = vec![false; sections.len()];
        for id in super::super::registry::all() {
            let Some(p) = id.plugin() else { continue };
            let expected: Vec<String> = p
                .native_tools()
                .iter()
                .map(|t| {
                    let class = match t.class {
                        Some(c) => format!("`{c:?}`"),
                        None => "\u{2014}".to_string(),
                    };
                    let mem = match t.memory_kind {
                        Some((kind, arg)) => {
                            format!("`{kind}` ({} arg)", format!("{arg:?}").to_lowercase())
                        }
                        None => "\u{2014}".to_string(),
                    };
                    format!(
                        "| `{}` | {class} | `{}` | {mem} |",
                        t.name, t.mutates_fs
                    )
                })
                .collect();
            let first = p.native_tools()[0].name;
            let hits: Vec<usize> = sections
                .iter()
                .enumerate()
                .filter(|(_, (_, rows))| {
                    rows.first()
                        .is_some_and(|r| r.starts_with(&format!("| `{first}` |")))
                })
                .map(|(i, _)| i)
                .collect();
            assert_eq!(
                hits.len(),
                1,
                "{id}: {} \"{MARKER}\" subsections open with `{first}` — each registered \
                 harness needs exactly one, headed so a reader can tell them apart",
                hits.len()
            );
            claimed[hits[0]] = true;
            let (heading, rows) = &sections[hits[0]];
            assert_eq!(
                rows,
                &expected,
                "{heading} has drifted from `{id}`'s `native_tools()`. Replace that table's \
                 body with:\n{}",
                expected.join("\n")
            );
        }
        let orphans: Vec<&String> = sections
            .iter()
            .zip(&claimed)
            .filter(|(_, taken)| !**taken)
            .map(|((heading, _), _)| heading)
            .collect();
        assert!(
            orphans.is_empty(),
            "these \"{MARKER}\" subsections belong to no registered harness — a retired \
             harness's table is prose that reads like a contract: {orphans:?}"
        );
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

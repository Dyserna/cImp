//! **OpenCode's OWN native tool ids** — the reviewed table the generated plugin
//! matches on (V35 Phase K: moved verbatim from `offload/toolclass.rs`, design
//! § 4's moves table).
//!
//! This is a *harness registry*, not cImp's tool vocabulary, and the two obey
//! opposite defaults — which is exactly why they now live in different
//! directories. [`crate::offload::toolclass::TABLE`] is the set of names cImp
//! ROUTES, where unknown ⇒ EXTERNAL because every unrouted name is a proxied
//! MCP id in disguise. The table below is the set of names a harness serves
//! ITSELF, where unknown ⇒ ungated because the set is closed and published and
//! most of its members are neither external nor local-capability.
//!
//! **TCB (design § 5):** the names here are what the generated plugin's Phase H
//! gate and Phase F beacon refuse on. Adding or removing a row changes a
//! security control.
//!
//! The [`ToolClass`] vocabulary is shared with `offload::toolclass` and is the
//! only thing the two tables have in common.

use crate::offload::toolclass::ToolClass;

// ── V32 Phase H — OpenCode's OWN native tool names ─────────────────────────

/// The OpenCode 1.18.13 native tool ids, classified for the Phase H gate —
/// **re-verified live against 1.18.18 on 2026-08-17**, where every
/// integration-relevant upstream file is byte-identical to 1.18.13.
///
/// # Why this is a SECOND table and not more rows in [`crate::offload::toolclass::TABLE`]
///
/// [`crate::offload::toolclass::TABLE`] is cImp's own tool vocabulary — the names cImp *routes*, where the
/// locked invariant is **unknown ⇒ EXTERNAL** because every unrouted name is a
/// proxied MCP id in disguise. That default is exactly wrong for a harness's own
/// registry, which is a closed, published set with members that are neither
/// external nor local-capability (`todowrite`, `question`, `skill`, `invalid`).
/// Folding these names into `TABLE` would make `classify("todowrite")` answer
/// `External` and the Phase H gate would refuse a bookkeeping tool under a LOCAL
/// latch — a partial, arbitrary gate, which the E2 spike showed is worse than
/// none.
///
/// So the two tables share the [`ToolClass`] vocabulary and nothing else, and
/// this one is **allowlist-only**: a name absent here is UNGATED, deliberately.
/// The class table's `Edit`/`Write`/`Bash` rows stay where they are — those are
/// *Claude's* capitalized natives, read by V33's `mutates_fs` consumer, and a
/// second namespace under the same lookup would be the drift this comment
/// exists to prevent.
///
/// Sourced from `GET /experimental/tool/ids` on the running binary
/// (`docs/HARNESS-NATIVE-TOOLS.md` §3), not from documentation. `apply_patch` is
/// load-bearing: it *replaces* `edit`/`write` on OpenAI-provider models, so a
/// list naming only `edit`/`write` would leave the whole mutation surface open
/// on exactly those tabs.
///
/// **The sourced set, re-verified live on 2026-08-17** against the installed
/// 1.18.13 (and diffed against 1.18.18, where every integration-relevant
/// upstream file is byte-identical): a default `opencode serve` answers exactly
/// `["invalid","question","bash","read","glob","grep","edit","write","task",`
/// `"webfetch","todowrite","websearch","skill","apply_patch"]`. Three more ids
/// exist in the binary behind experiment env flags and are therefore **absent
/// from that route's answer** — `execute`, `lsp` and `plan_exit`. The probe can
/// only ever see the default set, so those three are classified here from the
/// source rather than from the route: an experiment a user switches on must not
/// be the thing that opens an ungated surface. `list`, `todoread` and `patch`
/// are served by no current build (`apply_patch` superseded `patch`).
///
/// **UPSTREAM 2.0 WATCH ITEM.** `tool/shell/id.ts` pins the tool id `bash` with
/// a comment saying it will be RENAMED at opencode 2.0. When that lands, the
/// live probe reports `bash` as declared-but-not-served (a note, not a failure —
/// see `probe::tool_registry_outcome`) *and* the new name as UNCLASSIFIED (a
/// failure). Both halves are the intended reading: the next audit should expect
/// them, classify the new id here, and keep `bash` for the same
/// costs-nothing-and-closes-it reason `patch` is kept.
///
/// **`permission.ask` is declared-but-never-fires upstream** (checked
/// 2026-08-17 in the plugin `Hooks` type at both versions): it is part of the
/// published plugin API and no code path emits it. Nothing here or in
/// [`crate::harness::opencode::plugin`] may be built on it — a handler wired to
/// it would look like a permission control and never run once.
/// **V33 Phase F added the third element, `mutates_fs`** — the same axis
/// [`crate::offload::toolclass::ClassRow::mutates_fs`] carries in [`crate::offload::toolclass::TABLE`], for the same consumer (the
/// pre-tool checkpoint). It is a THIRD element rather than a second table
/// because the class and the mutation capability of one name must be declared
/// in one place; `read`/`glob`/`grep` are local capability without being
/// mutations, and `bash` is both.
pub const OPENCODE_NATIVE_TABLE: &[(&str, ToolClass, bool)] = &[
    // Local capability: private data + process execution + mutation.
    ("bash", ToolClass::LocalCapability, true),
    ("read", ToolClass::LocalCapability, false),
    ("glob", ToolClass::LocalCapability, false),
    ("grep", ToolClass::LocalCapability, false),
    ("edit", ToolClass::LocalCapability, true),
    ("write", ToolClass::LocalCapability, true),
    // Not in the 1.18.13 registry, but the plugin's own `CIMP_EDIT_TOOLS` has
    // carried it since V12 and the milestone's locked list names it. Gating a
    // name the harness does not serve costs nothing and closes it in advance.
    ("patch", ToolClass::LocalCapability, true),
    ("apply_patch", ToolClass::LocalCapability, true),
    // ── Experiment-gated ids (2026-08-17), absent from a default serve ───────
    //
    // Both exist in the installed binary but are only registered when their
    // experiment env flag is set, so `GET /experimental/tool/ids` never lists
    // them and the live probe cannot classify them for us. Gating them here is
    // the same costs-nothing move as `patch` above, with a sharper reason: a
    // user who switches an experiment on must not thereby open an UNGATED
    // mutation/exec surface. The gate is allowlist-only, so the alternative is
    // not "gated later" — it is "never gated".
    //
    // `OPENCODE_EXPERIMENTAL_CODE_MODE`: the code-mode tool. It EXECUTES code,
    // so it gets `bash`'s posture exactly — local capability, mutating — because
    // anything that can run code can rewrite the tree and reach the network.
    ("execute", ToolClass::LocalCapability, true),
    // `OPENCODE_EXPERIMENTAL_LSP_TOOL`: language-server queries over the project
    // (hover/definition/diagnostics). Reads private project data ⇒ local
    // capability; changes nothing on disk ⇒ not mutating, exactly like
    // `read`/`glob`/`grep`.
    ("lsp", ToolClass::LocalCapability, false),
    // The harness's own web tools — the EXTERNAL side of the same boundary.
    // Neither writes to the project tree, so neither checkpoints.
    ("webfetch", ToolClass::External, false),
    ("websearch", ToolClass::External, false),
    // Ids deliberately left out of this table are NOT undeclared — they are
    // listed, with their reasons, in [`OPENCODE_NATIVE_REVIEWED_UNGATED`].
];

/// Upstream tool ids that `GET /experimental/tool/ids` serves and
/// [`OPENCODE_NATIVE_TABLE`] deliberately does **not** gate, each with the
/// reason it was left out.
///
/// This is the prose that used to sit as a trailing comment inside the table,
/// made machine-readable in V35 Phase D — because the live probe
/// (`harness/probe.rs`, capability `opencode.tool_registry`) has to distinguish
/// two things a bare table diff cannot:
///
/// * an id nobody has ever looked at — **UNCLASSIFIED**, and a failure, because
///   the table is allowlist-only so it ships ungated; and
/// * an id a human looked at and consciously left ungated — a **recorded
///   decision**, which must not turn the probe permanently red. (Milestone
///   locked decision 8: a probe that cries wolf gets ignored, which is the
///   exact fate of the version tripwire this milestone exists to fix.)
///
/// The five original entries were already reviewed and written down; moving them
/// here changed no gating behavior whatsoever (the plugin builder reads
/// [`opencode_native_names`] / [`opencode_native_mutating_names`], neither of
/// which consults this list). Adding a row here IS a security decision and
/// belongs in review, exactly like adding one to the table.
pub const OPENCODE_NATIVE_REVIEWED_UNGATED: &[(&str, &str)] = &[
    (
        "task",
        "sub-agent spawn: orchestration, not a capability of its own. The E2 spike confirmed a \
         sub-agent's tool calls fire this same hook in the child session, and the plugin's tab \
         identity is process-wide (`CIMP_TAB_ID`), so the child's `bash`/`read`/`webfetch` are \
         gated at the same latch. Gating the spawn itself would refuse an orchestration primitive \
         whose dangerous leaves are already closed.",
    ),
    (
        "skill",
        "no file access, no process execution, no egress. Denying it would buy nothing and would \
         make the gate look arbitrary to the model it is talking to.",
    ),
    (
        "todowrite",
        "no file access, no process execution, no egress — see `skill`.",
    ),
    (
        "question",
        "no file access, no process execution, no egress — see `skill`.",
    ),
    (
        "invalid",
        "no file access, no process execution, no egress — see `skill`. (The harness's own \
         placeholder for an unresolvable tool call.)",
    ),
    (
        "plan_exit",
        "no file access, no process execution, no egress — see `skill`. Plan-mode bookkeeping: it \
         announces that the model wants to leave plan mode, and the mode switch itself is the \
         harness's, not a capability. Experiment-gated \
         (`OPENCODE_EXPERIMENTAL_PLAN_MODE`, plus a cli client), so it is absent from \
         `GET /experimental/tool/ids` on a default serve and reviewed here from the source — its \
         two experiment siblings, `execute` and `lsp`, ARE gated in the table above, which is the \
         difference this list exists to record.",
    ),
];

/// The class of one OpenCode native tool name, or `None` when the gate does not
/// apply to it.
///
/// `None` (not `External`) for an unknown name is the whole difference from
/// [`crate::offload::toolclass::classify`] — see [`OPENCODE_NATIVE_TABLE`].
///
/// Test-only today: production reads the table through
/// [`opencode_native_names`], because the *lookup* happens in the generated
/// plugin's JS rather than in Rust. It stays because the unknown-⇒-`None`
/// contract is the whole reason this table is separate, and a contract with no
/// executable statement is a comment.
#[cfg_attr(not(test), allow(dead_code))]
pub fn opencode_native_class(name: &str) -> Option<ToolClass> {
    OPENCODE_NATIVE_TABLE
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, c, _)| *c)
}

/// Every OpenCode native name in one class, in table order — the input the
/// plugin-source builder bakes into its `Set` literals, so the JS the gate runs
/// and the table reviewed here cannot drift.
pub fn opencode_native_names(class: ToolClass) -> Vec<&'static str> {
    OPENCODE_NATIVE_TABLE
        .iter()
        .filter(|(_, c, _)| *c == class)
        .map(|(n, _, _)| *n)
        .collect()
}

/// V33 Phase F: every OpenCode native name that can change files on disk, in
/// table order — the sibling of [`opencode_native_names`], baked into the
/// generated plugin's `CIMP_MUTATING_TOOLS` set so the JS that decides whether
/// to checkpoint reads the same reviewed table Rust does.
///
/// Cuts ACROSS the class axis rather than along it: `bash` is
/// local-capability AND mutating, `read` is local-capability and not.
pub fn opencode_native_mutating_names() -> Vec<&'static str> {
    OPENCODE_NATIVE_TABLE
        .iter()
        .filter(|(_, _, m)| *m)
        .map(|(n, _, _)| *n)
        .collect()
}

/// V33 Phase F: whether an OpenCode native `name` can change files on disk.
///
/// **Unknown ⇒ `false`**, deliberately, and NOT for [`crate::offload::toolclass::mutates_fs`]'s reason.
/// There the default is a safety floor; here it is the same allowlist-only
/// posture the rest of this table has — a name with no row is a name cImp makes
/// no claim about, and minting a checkpoint for it would be inventing one.
///
/// The consumer is the loopback's `/workbench/tool_checkpoint` route, which
/// must not read an OpenCode tool id (`edit`) through [`crate::offload::toolclass::mutates_fs`]: that
/// function answers for cImp's own vocabulary, where `edit` is an unknown name.
/// Two vocabularies, two lookups — the drift this second table exists to
/// prevent.
pub fn opencode_native_mutates_fs(name: &str) -> bool {
    OPENCODE_NATIVE_TABLE
        .iter()
        .find(|(n, _, _)| *n == name)
        .is_some_and(|(_, _, m)| *m)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── V32 Phase H — the OpenCode native-name table ───────────────────────

    /// **The whole-surface property**, which the E2 spike bought with a live
    /// probe: with only `write` gated the model created the file through `bash`,
    /// so the LOCAL side must be the harness's complete local-capability surface
    /// — `apply_patch` included, because it REPLACES `edit`/`write` on
    /// OpenAI-provider models and a list naming only those two leaves the whole
    /// mutation surface open on exactly those tabs.
    #[test]
    fn the_opencode_native_table_covers_the_whole_local_surface() {
        let local = opencode_native_names(ToolClass::LocalCapability);
        for n in [
            "bash",
            "read",
            "glob",
            "grep",
            "edit",
            "write",
            "patch",
            "apply_patch",
            // 2026-08-17, the experiment-gated pair. They are in this list for
            // the same reason the whole-surface property exists: a local
            // capability the gate does not name is one the model can reach for
            // after a taint, and an experiment flag is a user setting rather
            // than a boundary.
            "execute",
            "lsp",
        ] {
            assert!(local.contains(&n), "{n} missing from the local set");
            assert_eq!(
                opencode_native_class(n),
                Some(ToolClass::LocalCapability),
                "{n}"
            );
        }
        assert_eq!(local.len(), 10, "got: {local:?}");

        let web = opencode_native_names(ToolClass::External);
        assert_eq!(web, vec!["webfetch", "websearch"]);
        // The two sides are disjoint — one name must not be denied under both
        // latches, which would be a tool nobody can ever call.
        assert!(!web.iter().any(|n| local.contains(n)));
    }

    /// **V33 Phase F**: the mutation axis of the OpenCode table cuts ACROSS the
    /// class axis, and the checkpoint set must be exactly the mutating half —
    /// not "everything local", which would checkpoint before every `read`/
    /// `grep`, and not "edit/write", which the E2 spike already showed is
    /// incomplete (the model routes a blocked write through `bash`, and
    /// `apply_patch` replaces edit/write on OpenAI-provider models).
    #[test]
    fn the_opencode_mutating_set_is_the_write_surface_not_the_local_one() {
        let mutating = opencode_native_mutating_names();
        assert_eq!(
            mutating,
            // `execute` joined on 2026-08-17: the code-mode tool runs arbitrary
            // code, so it can rewrite the tree by definition — `bash`'s posture,
            // for `bash`'s reason.
            vec!["bash", "edit", "write", "patch", "apply_patch", "execute"],
            "the pre-tool checkpoint set changed — every member is a name that \
             can rewrite the project tree, and every non-member is one that cannot"
        );
        // Reads are local capability and do NOT mutate: the two axes are
        // independent, which is the whole reason `mutates_fs` is a column of
        // its own rather than being inferred from the class. `lsp` is the
        // 2026-08-17 addition to that half — it queries a language server about
        // the project and writes nothing.
        for n in ["read", "glob", "grep", "lsp"] {
            assert_eq!(
                opencode_native_class(n),
                Some(ToolClass::LocalCapability),
                "{n}"
            );
            assert!(!opencode_native_mutates_fs(n), "{n} must not checkpoint");
        }
        // Neither web tool writes to the tree.
        for n in opencode_native_names(ToolClass::External) {
            assert!(!opencode_native_mutating_names().contains(&n), "{n}");
        }
    }

    /// V35 Phase D: the gated table and the reviewed-but-ungated list are two
    /// disjoint halves of ONE claim — "every upstream tool id cImp has looked
    /// at". The live probe subtracts their union from
    /// `GET /experimental/tool/ids` and fails on what is left, so an id in both
    /// (is it gated or not?) or an entry with a blank reason (reviewed by
    /// whom, on what grounds?) would quietly weaken that subtraction instead of
    /// breaking it.
    #[test]
    fn the_reviewed_ungated_list_is_disjoint_from_the_gated_table() {
        for (name, reason) in OPENCODE_NATIVE_REVIEWED_UNGATED {
            assert!(
                !OPENCODE_NATIVE_TABLE.iter().any(|(n, _, _)| n == name),
                "`{name}` is both gated and recorded as deliberately ungated — pick one"
            );
            // Global principle 5: `Some("")` would satisfy a bare
            // is-it-listed check while recording nothing a reviewer can weigh.
            assert!(
                reason.trim().len() > 20,
                "`{name}`: the reason must say why gating it would buy nothing, not merely exist"
            );
        }
        let mut names: Vec<&str> = OPENCODE_NATIVE_REVIEWED_UNGATED
            .iter()
            .map(|(n, _)| *n)
            .collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate id in the reviewed list");
    }

    /// 2026-08-17: the three EXPERIMENT-GATED upstream ids are classified, and
    /// each lands on the side its capability earns.
    ///
    /// They need a test of their own precisely because the live probe cannot
    /// give them one: `GET /experimental/tool/ids` on a default serve does not
    /// list them, so the `live − (gated ∪ reviewed) = ∅` subtraction says
    /// nothing about them either way. Without this, dropping a row for
    /// `execute` would leave the code-mode tool ungated and every other check
    /// in this file green.
    #[test]
    fn the_experiment_gated_ids_are_classified_even_though_no_probe_sees_them() {
        let gated = |n: &str| OPENCODE_NATIVE_TABLE.iter().any(|(id, _, _)| *id == n);
        let reviewed = |n: &str| OPENCODE_NATIVE_REVIEWED_UNGATED.iter().any(|(id, _)| *id == n);
        for n in ["execute", "lsp", "plan_exit"] {
            assert!(
                gated(n) || reviewed(n),
                "`{n}` is an upstream tool id behind an experiment flag and is classified in \
                 NEITHER list — the table is allowlist-only, so it would ship UNGATED the moment \
                 a user sets that flag"
            );
        }
        // …and the split is the one that was reviewed: the two capabilities are
        // gated, the bookkeeping one is not.
        assert!(gated("execute") && gated("lsp"), "a capability lost its gate");
        assert!(reviewed("plan_exit"), "plan_exit is a recorded decision, not a gate");
    }
}

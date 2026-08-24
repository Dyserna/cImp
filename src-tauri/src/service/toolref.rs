//! V42 F6 (#131) — the Tools tab's tool REFERENCE lists, and the tripwires
//! that keep them honest.
//!
//! `ToolActivityView.svelte` carried two hand-written arrays — `GRAPH_TOOLS`
//! and `OFFLOAD_TOOLS` — mirroring the tool sets `graph::mcp::tools::tool_specs`
//! and `offload::tools` actually expose. Nothing joined the halves, and they
//! drifted: the Wave-0 review (#113, D1) found three graph tools missing from
//! the rendered list and hot-fixed them BY HAND, which is the same fix that had
//! already been applied once. This module ends that class the way Phase E ended
//! the settings mirror: the list lives once, on the Rust side, next to a test
//! that compares its NAMES against the real tool tables in both directions, and
//! `service::codegen` emits `src/lib/generated/tools.ts` for the frontend to
//! import.
//!
//! ## Why the prose is authored here rather than taken from the specs
//!
//! A tool has two descriptions and they are not the same text. The one in
//! [`tool_specs`](crate::graph::mcp::tools::tool_specs) is written FOR A MODEL:
//! long, imperative, full of "prefer this over grep" and schema talk. The one
//! the Tools tab renders is written FOR THE USER: one line plus an example
//! prompt they could actually type. Generating the second from the first would
//! put model-prompt prose in the UI, so the UI prose stays hand-written — and
//! moves HERE, where the set it belongs to is checked. What the tripwire pins
//! is the thing that drifted: the set of names.
//!
//! Consequence, and it is the point: a tool added to `tool_specs` with no row
//! below fails `the_graph_reference_names_exactly_the_graph_tool_surface`. The
//! author has to write the user-facing line, in the same change, or the suite
//! is red.
//!
//! ## Why build-time emission and not an IPC read
//!
//! The frontend could ask the backend for this list over IPC. Emission at build
//! time was chosen anyway:
//!
//! * the panel is static reference documentation with no live state in it — an
//!   IPC round trip buys a loading flicker and an error path for content that
//!   cannot change while the app runs;
//! * `tool_specs()` is not the whole answer either. `graph_semantic_docs` and
//!   `graph_semantic_code` come from separate spec functions and are advertised
//!   only when their settings are on, so an "advertised tools" IPC read would
//!   render a list that SHRINKS when the user turns semantic search off — the
//!   reference is meant to document the surface, not the current session's
//!   subset;
//! * it lands the artifact in the same CI diff gate as Phase E's bindings, so
//!   an uncommitted regeneration fails the build rather than shipping stale.

/// One rendered reference row — matches the frontend's `ToolRef`.
pub struct ToolRef {
    /// The tool name, as the model calls it.
    pub name: &'static str,
    /// One user-facing line.
    pub desc: &'static str,
    /// A prompt the user could type to trigger it.
    pub example: &'static str,
}

/// The `graph_*` / `context_*` tools the code graph exposes to AI tabs and the
/// offload worker.
///
/// Order is the order the panel renders. Kept in the reading order the previous
/// hand-written list had (workhorses first, analyses after, session memory
/// last) rather than sorted — the tripwire below compares SETS, so ordering is
/// a presentation choice, not a correctness one.
pub const GRAPH_TOOLS: &[ToolRef] = &[
    ToolRef {
        name: "graph_find_symbol",
        desc: "Where a symbol (function/struct/trait/…) is defined — file, line, kind. Never source text (V32 H-1); pair with graph_snippet for the body.",
        example: "Where is GraphService defined?",
    },
    ToolRef {
        name: "graph_callers",
        desc: "Which functions call the given symbol (its call sites). Impact analysis.",
        example: "What calls graphRebuild?",
    },
    ToolRef {
        name: "graph_callees",
        desc: "Which symbols are called by the given symbol.",
        example: "What does handle_call call?",
    },
    ToolRef {
        name: "graph_references",
        desc: "Every reference (use site) of a name — file, line, column.",
        example: "Find all references to ToolDef.",
    },
    ToolRef {
        name: "graph_imports",
        desc: "The modules/paths a file imports.",
        example: "What does src/offload/mcp.rs import?",
    },
    ToolRef {
        name: "graph_outline",
        desc: "Every definition in a file, in source order (a structural outline).",
        example: "Outline BackendDashboardCard.svelte.",
    },
    ToolRef {
        name: "graph_snippet",
        desc: "Fetch just one definition's body (by symbol, or file+line) instead of reading the whole file. Pair with graph_outline for big files.",
        example: "Show the body of dispatch_recorded.",
    },
    ToolRef {
        name: "graph_repo_map",
        desc: "A budget-bounded map of the most call-central files with their top signatures — orient fast at the start of a task.",
        example: "Give me a project map.",
    },
    ToolRef {
        name: "graph_transitive",
        desc: "Transitive call chain for a symbol — everything it reaches (callees) or that reaches it (callers).",
        example: "What does runOffloadTest transitively call?",
    },
    ToolRef {
        name: "graph_search_docs",
        desc: "Keyword search over docs and doc-comments; returns matching snippets.",
        example: "Search the docs for 'warm pool'.",
    },
    ToolRef {
        name: "graph_struct_search",
        desc: "Find code by AST shape via a tree-sitter query (not text).",
        example: "Find every .unwrap() in the Rust code.",
    },
    ToolRef {
        name: "graph_path",
        desc: "Shortest path between two code entities through call/import/containment edges — how does X reach Y. Each hop shows its edge kind and confidence; says so plainly when there is no path instead of inventing one.",
        example: "How does the auth handler reach the connection pool?",
    },
    ToolRef {
        name: "graph_architecture",
        desc: "A once-per-project map of the system’s shape: god nodes (the highest-degree hubs everything flows through), subsystems (cohesive file communities), and surprising connections (candidate accidental coupling). Topology only; clustering is heuristic, so treat subsystem boundaries as advisory.",
        example: "What does this codebase look like architecturally?",
    },
    ToolRef {
        name: "graph_semantic_docs",
        desc: "Meaning-based (embedding) search over docs — only when Semantic search is enabled.",
        example: "Find docs about how offload timeouts are handled.",
    },
    ToolRef {
        name: "graph_semantic_code",
        desc: "Meaning-based (embedding) search over symbol bodies — only when \"Embed code bodies\" is enabled. Returns file:line/kind/signature/distance, never the body; pair with graph_snippet.",
        example: "Find code that retries a failed network request.",
    },
    ToolRef {
        name: "graph_dead_exports",
        desc: "Candidate unused public symbols (no reference, no inbound call). Candidates only — may include false positives.",
        example: "List candidate dead exports.",
    },
    ToolRef {
        name: "graph_cycles",
        desc: "Import cycles between files (loops of files that import one another).",
        example: "Are there any import cycles?",
    },
    ToolRef {
        name: "graph_impact",
        desc: "Blast radius: what could this change break? Defaults to the working-tree diff vs HEAD; pass symbols to analyze specific names instead. include_tests appends an affected-tests block. Results are approximate (name-keyed).",
        example: "What would break if I change GraphIndex::dependents_transitive?",
    },
    ToolRef {
        name: "graph_tests_for",
        desc: "Which tests (candidates) would exercise a symbol or file if it changed — the transitive dependents tagged as tests. Candidates only — dynamic dispatch/fixtures aren't captured.",
        example: "What tests cover dependents_transitive?",
    },
    ToolRef {
        name: "graph_recent_changes",
        desc: "What's been happening lately — files ranked by git churn (touch count, then recency) with their last commit subject. File-level, 90-day window. Unavailable outside a git repo.",
        example: "What files have changed most recently?",
    },
    ToolRef {
        name: "context_recall",
        desc: "Recall this session's working set — the files it read/edited/queried and the symbols touched.",
        example: "What has this session been working on?",
    },
    ToolRef {
        name: "context_note",
        desc: "Remember a non-obvious decision/fact for this project (pin to keep it across sessions).",
        example: "Note: we chose FNV hashing for stability.",
    },
    ToolRef {
        name: "context_notes",
        desc: "List this session's notes plus every pinned note for the project.",
        example: "Show my remembered notes.",
    },
];

/// The tools the offload feature provides: the MCP tools an AI tab calls to
/// delegate, plus the native tools the local worker uses to complete the task.
///
/// `offload_batch` is here and was NOT in the hand-written list this replaced —
/// a live instance of the same D1 drift, found by writing the tripwire below.
pub const OFFLOAD_TOOLS: &[ToolRef] = &[
    ToolRef {
        name: "offload_task",
        desc: "Delegate a token-heavy subtask to the local model and get back only the synthesized result — conserving the main session’s context.",
        example: "Offload: summarize every TODO/FIXME across the repo and group them by theme.",
    },
    ToolRef {
        name: "offload_batch",
        desc: "Run several offload subtasks in parallel across the worker's slots in ONE call — separate offload_task calls are serialized by the MCP client, this one fans out. 1–16 subtasks; results come back one section each, per-subtask errors inline.",
        example: "Offload in parallel: summarize each of these four modules.",
    },
    ToolRef {
        name: "read_file",
        desc: "Worker reads a file (within the configured allowed roots).",
        example: "Read src/offload/openai.rs, lines 1–200.",
    },
    ToolRef {
        name: "list_dir",
        desc: "Worker enumerates a directory — the ground-truth answer to what files exist / how many.",
        example: "List the top-level *.md files in docs/.",
    },
    ToolRef {
        name: "code_search",
        desc: "Worker searches the codebase with ripgrep.",
        example: "Search the repo for predicted_per_second.",
    },
    ToolRef {
        name: "run_command",
        desc: "Worker runs an allowlisted, read-only command.",
        example: "Run git log --oneline -20.",
    },
    ToolRef {
        name: "run_check",
        desc: "Worker runs one of the project's configured checks (build/typecheck/lint/test) to verify a claim before stating it. Inert until checks are configured.",
        example: "Does the test suite pass? Prove it with run_check.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn names(rows: &[ToolRef]) -> BTreeSet<&'static str> {
        let set: BTreeSet<&'static str> = rows.iter().map(|r| r.name).collect();
        assert_eq!(set.len(), rows.len(), "a tool is listed twice");
        set
    }

    /// **The #113-D1 tripwire.**
    ///
    /// [`GRAPH_TOOLS`] names exactly the graph surface: every spec in
    /// `tool_specs()` plus the two semantic specs, which live in their own
    /// functions because they are advertised only when their settings are on
    /// (and which the reference documents unconditionally, saying so in the
    /// row's own text).
    ///
    /// Both directions. A tool added to `tool_specs` with no reference row is
    /// the D1 failure — three of them shipped once. A reference row naming a
    /// tool that no longer exists is the mirror image: documentation for
    /// something the user cannot call.
    #[test]
    fn the_graph_reference_names_exactly_the_graph_tool_surface() {
        use crate::graph::{semantic_code_spec, semantic_spec, tool_specs};

        let mut surface: BTreeSet<&'static str> =
            tool_specs().into_iter().map(|s| s.name).collect();
        assert!(
            surface.len() >= 15,
            "`tool_specs()` returned {} tools — a shrunken surface makes this test vacuous",
            surface.len()
        );
        surface.insert(semantic_spec().name);
        surface.insert(semantic_code_spec().name);

        let listed = names(GRAPH_TOOLS);
        let missing: Vec<_> = surface.difference(&listed).collect();
        let extra: Vec<_> = listed.difference(&surface).collect();
        assert!(
            missing.is_empty(),
            "these graph tools exist but the Tools tab's reference does not document them \
             (the #113-D1 drift, again): {missing:?} — add a row to `GRAPH_TOOLS` with a \
             user-facing line and an example prompt"
        );
        assert!(
            extra.is_empty(),
            "the reference documents graph tools that no longer exist: {extra:?}"
        );
    }

    /// [`OFFLOAD_TOOLS`] names exactly the offload surface: the two
    /// `offload_*` MCP tools plus every native worker tool, with all toggles
    /// on.
    ///
    /// The `delegate_task_<harness>` family is deliberately NOT here: those
    /// names are minted per harness tab at `tools/list` time (V39 Phase B), so
    /// there is no fixed set to document, and the Delegation surface has its
    /// own UI.
    #[test]
    fn the_offload_reference_names_exactly_the_offload_tool_surface() {
        use crate::offload::mcp::{OFFLOAD_BATCH_TOOL, OFFLOAD_TASK_TOOL};
        use crate::offload::tools::enabled_defs_inner;
        use crate::settings::OffloadToolToggles;

        // `Default` is all-on, and asserted so here: a future default flipped
        // to `false` would silently shrink what this test compares against.
        let toggles = OffloadToolToggles::default();
        assert!(
            toggles.read_file
                && toggles.list_dir
                && toggles.code_search
                && toggles.run_command
                && toggles.run_check,
            "the native-tool toggles no longer default to all-on; this test needs the FULL \
             surface, so build it explicitly instead of trusting `Default`"
        );

        let mut surface: BTreeSet<&'static str> =
            [OFFLOAD_TASK_TOOL, OFFLOAD_BATCH_TOOL].into_iter().collect();
        for def in enabled_defs_inner(&toggles, true) {
            // `ToolDef::function` owns its name; the reference rows are
            // `&'static str`, so match through the listed set rather than
            // inserting a borrowed name.
            let name = def.function.name.clone();
            let found = GRAPH_TOOLS
                .iter()
                .chain(OFFLOAD_TOOLS.iter())
                .find(|r| r.name == name);
            assert!(
                found.is_some(),
                "the offload worker advertises `{name}` but no reference row documents it"
            );
            surface.insert(found.expect("checked").name);
        }
        assert!(
            surface.len() >= 6,
            "the offload surface came back with {} tools — too few for this to mean anything",
            surface.len()
        );

        let listed = names(OFFLOAD_TOOLS);
        assert_eq!(
            listed, surface,
            "the Tools tab's offload reference and the real offload tool surface disagree"
        );
    }

    /// Every row says something, and says it in one line.
    ///
    /// A blank `desc` or `example` renders as an empty div — the panel looks
    /// fine and documents nothing, which is the "empty is not absent" failure.
    #[test]
    fn every_reference_row_is_substantive() {
        for row in GRAPH_TOOLS.iter().chain(OFFLOAD_TOOLS.iter()) {
            assert!(
                row.desc.trim().len() > 20,
                "`{}` has no real description",
                row.name
            );
            assert!(
                row.example.trim().len() > 5,
                "`{}` has no example prompt",
                row.name
            );
            assert!(
                !row.desc.contains('\n') && !row.example.contains('\n'),
                "`{}` spans lines; the panel renders each as a single line",
                row.name
            );
        }
    }
}

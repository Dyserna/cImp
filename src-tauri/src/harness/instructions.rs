//! V40 Phase E, locked decision 24 — **the text that reaches a model is a
//! declared seam.**
//!
//! Every string cImp puts in front of a harness's model used to live wherever
//! it was emitted from — a `const` beside the MCP handshake, a `const` beside
//! the spawn args, a `format!` inside a tool descriptor, a literal in a Svelte
//! module. Nothing enumerated them, so nothing could answer *"what does cImp
//! tell this harness?"*, and one of them had drifted into a harness-specific
//! claim without anybody noticing: `GRAPH_GUIDANCE` names Claude Code's
//! capitalised `Read` and `Bash` and was sent verbatim to **both** harnesses,
//! so every OpenCode session was told to prefer two tools it does not have.
//!
//! # What is in the inventory, and what is not
//!
//! The rule is **text cImp sends to a model _through a harness_**: the
//! system-prompt addendum, the MCP `instructions` block, a generated tool
//! description a harness advertises, and the text the V39 facade types into a
//! worker tab. Text that goes to cImp's own offload worker (`agent.rs`'s
//! `SYSTEM_PROMPT`) is not harness-facing and is not here.
//!
//! Two kinds of entry, and the difference is marked on the row rather than left
//! to a reader:
//!
//! * **harness-templated** ([`Instruction::neutral`] `== false`) — the text
//!   names something that is true of *this* harness: its own tool vocabulary
//!   ([`Slot::GraphGuidance`]), its label ([`Slot::DelegateContract`]), or a
//!   mechanism only it has ([`Slot::Channel`]).
//! * **neutral** — the same bytes for every harness. It is still inventoried,
//!   because the question the inventory answers is *what does the model see*,
//!   and an answer that omits the neutral half is not an answer.
//!
//! # Why the templating lives here rather than in each plugin
//!
//! The subject of the text is cImp — its graph tools, its channel, its
//! delegation contract — so a plugin that owned the prose would be a harness
//! declaring what cImp says about itself, and three harnesses would be three
//! copies drifting apart. What the plugin owns is the *vocabulary the text is
//! rendered in*: [`super::plugin::HarnessPlugin::tool_for_role`] names its own
//! read and shell tools, and the descriptor names its label.
//! [`super::plugin::HarnessPlugin::instructions`] is where the two meet, and it
//! is what a harness overrides if it ever needs to say something else.
//!
//! A tab that runs no registered harness gets [`neutral`]: the same text with
//! *descriptions* where the tool names go ("a full file read", "the shell"),
//! which is the honest rendering when cImp does not know the vocabulary — and
//! strictly better than the pre-V40 behaviour of handing it Claude's.

use std::borrow::Cow;
use std::sync::OnceLock;

use super::plugin::ToolRole;
use super::registry::HarnessId;

/// Where one instruction is delivered. **The slot, not the text, is what a
/// consumer names** — the text is this module's and may be templated per
/// harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot {
    /// The code-knowledge-graph nudge in the system-prompt addendum
    /// (`--append-system-prompt` for Claude, the managed instructions file for
    /// OpenCode). Templated with this harness's read and shell tool names.
    GraphGuidance,
    /// The semantic-search sentence appended to [`Slot::GraphGuidance`] when
    /// `graph.semantic_search` is on. Neutral: it names one `graph_*` tool.
    GraphSemantic,
    /// The MCP `instructions` block declared beside the session-push channel
    /// capability, for a harness that has an inbound MCP path
    /// ([`super::plugin::HarnessPlugin::supports_session_push`]).
    Channel,
    /// The line the compose overlay appends after the `[image] <path>` lines it
    /// types into the tab. Reaches the frontend over `harness_instructions`.
    Attachment,
    /// The pinned first sentence of every generated `delegate_task_<id>` tool
    /// description (V39 locked decision 3), templated with this harness's label.
    DelegateContract,
    /// The rest of that description. Carries a `{tab}` placeholder the caller
    /// fills with the Manual tab's current name.
    DelegateToolDetail,
    /// The sentence that makes a schema run a schema run, as the V39 facade
    /// types it into a worker tab. Neutral.
    SchemaFinal,
    /// The facade's rendering of `offload_task`'s `profile: research`. Neutral.
    FacadeResearch,
    /// The facade's rendering of `offload_task`'s `profile: code`. Neutral.
    FacadeCode,
}

impl Slot {
    /// Every slot, in delivery order. The inventory is complete by
    /// construction: [`render_with`] emits one row per entry here, and
    /// `every_harness_declares_every_slot` checks the other direction.
    ///
    /// Its consumer IS that test — production code names the slot it wants.
    /// Declared anyway, because "what is the complete list" is the question this
    /// module exists to answer, and a list that only exists inside a `vec![]`
    /// literal cannot be enumerated by anything.
    #[cfg_attr(not(test), allow(dead_code))]
    pub const ALL: &'static [Slot] = &[
        Slot::GraphGuidance,
        Slot::GraphSemantic,
        Slot::Channel,
        Slot::Attachment,
        Slot::DelegateContract,
        Slot::DelegateToolDetail,
        Slot::SchemaFinal,
        Slot::FacadeResearch,
        Slot::FacadeCode,
    ];

    /// The wire name, for the `harness_instructions` IPC map and for reports.
    pub fn id(self) -> &'static str {
        match self {
            Slot::GraphGuidance => "graph_guidance",
            Slot::GraphSemantic => "graph_semantic",
            Slot::Channel => "channel",
            Slot::Attachment => "attachment",
            Slot::DelegateContract => "delegate_contract",
            Slot::DelegateToolDetail => "delegate_tool_detail",
            Slot::SchemaFinal => "schema_final",
            Slot::FacadeResearch => "facade_research",
            Slot::FacadeCode => "facade_code",
        }
    }
}

/// One model-visible string, with the two things a reader needs to judge it:
/// where it is delivered, and whether it says anything harness-specific.
pub struct Instruction {
    pub slot: Slot,
    pub text: Cow<'static, str>,
    /// `true` when these bytes are the same for every harness. A neutral row is
    /// still inventoried — see the module docs.
    ///
    /// Read by `a_neutral_row_is_the_same_text_for_every_harness`, which is what
    /// makes the mark a claim rather than a comment: a row marked neutral that
    /// starts rendering per harness fails the build.
    #[cfg_attr(not(test), allow(dead_code))]
    pub neutral: bool,
}

// ── the texts ───────────────────────────────────────────────────────────────

/// What `{read_tool}` becomes for a tab whose harness cImp cannot identify: a
/// description where a tool name would go.
const NEUTRAL_READ: &str = "file read";

/// The `{shell_tool}` half of the same answer.
const NEUTRAL_SHELL: &str = "the shell";

/// V9-01: the system-prompt addendum telling the session the
/// code-knowledge-graph tools exist. Gated on `graph.enabled` (the tools are
/// only injected then).
///
/// **The two placeholders are the intended behaviour change of V40 Phase E.**
/// This blob used to name `Read` and `Bash` — Claude Code's capitalised tool
/// ids — and was sent unchanged to OpenCode, whose tools are `read` and `bash`.
/// Substituted through [`ToolRole`], so Claude's rendering is byte-identical to
/// the pre-V40 text and every other harness gets its own vocabulary.
///
/// Substituted with `str::replace`, never `format!`: the text contains real
/// braces (`run_check {name: …, changed_only: true}`) that a format string
/// would either eat or refuse.
const GRAPH_GUIDANCE: &str = "This project has a code knowledge graph (from the cimp-offload MCP \
server). Prefer the `graph_*` tools over grep for code-structure questions: `graph_find_symbol` \
(where a symbol is defined), `graph_callers`/`graph_callees` (call relationships), \
`graph_references`, `graph_imports`, `graph_outline` (a file's definitions), `graph_snippet` \
(fetch just one definition's body instead of reading the whole file — for files over ~300 lines \
prefer `graph_outline` → `graph_snippet` over a full {read_tool}), `graph_transitive` \
(transitive call chains), `graph_search_docs` (documentation/doc-comments), and \
`graph_struct_search` (find code by AST shape via a tree-sitter query — e.g. every `.unwrap()` or \
every function with a given parameter pattern — when text search can't express the structure). They \
return precise, token-bounded results from an index, so they're cheaper and more exact than text \
search for 'where is X defined', 'who calls X', and impact analysis. `graph_dead_exports` lists \
candidate unused public symbols and `graph_cycles` lists import cycles. For the edit→check→fix \
loop: before changing shared code run `graph_impact` (what your working-tree diff could break) and \
`graph_tests_for` (which tests cover a symbol); after edits run `run_check` for deduplicated \
diagnostics instead of a raw build dump — pass `name` (the check to run; its schema lists this \
project's configured names, and it is required when there is more than one) plus \
`changed_only:true`, e.g. `run_check {name: <one of the schema's names>, changed_only: true}` — \
including test runs: prefer a configured test check over running the test command in {shell_tool}; it \
returns failures only; `graph_recent_changes` shows what's been churning lately. This project also has \
session memory: call `context_recall` at the start of a follow-up task to reload what this session \
has been working on, and `context_note` to record a non-obvious decision (pin=true to keep it \
across sessions) so it survives into later sessions.";

/// V9-01: appended after [`Slot::GraphGuidance`] only when semantic search is
/// on (the `graph_semantic_docs` tool is advertised then).
const GRAPH_SEMANTIC_GUIDANCE: &str = " Also available: `graph_semantic_docs`, a meaning-based \
(embedding) search over the project's docs and doc-comments — use it when you want relevant \
material that may not share keywords with your query.";

/// The system-prompt `instructions` block injected alongside the channel
/// capability when `offload.session_push` is on.
///
/// It tells the model what a `<channel source="cimp-offload">` message is and
/// how to treat one. The "do not invent" clause is deliberate: a channel
/// message is a plain user-role message from the model's point of view, so
/// without it the pattern is trivially imitable in the model's own output.
const CHANNEL_INSTRUCTIONS: &str = "cimp-offload may push out-of-band notices into this session as <channel source=\"cimp-offload\"> messages — completion notices from the local toolchain (offloaded tasks, code audits, graph indexing). When one arrives, take it into account: act on it if it is relevant to the current task, otherwise acknowledge it briefly. Do not invent channel messages; only react to ones actually delivered.";

/// V14 Phase B: the line the compose overlay appends after the `[image] <path>`
/// lines, so the turn says what to do with them.
///
/// Neutral, and deliberately so: it is an instruction in English ("read these"),
/// not a tool call, and both harnesses accept a local image path dropped into
/// the prompt text as plain text (V14 milestone decision 3). It lives here
/// rather than in `compose/attachments.ts` because a model-visible string in the
/// frontend is one nothing in this inventory can see.
const ATTACHMENT_INSTRUCTION: &str = "Read the attached image file(s).";

/// **The pinned contract sentence** (V39 locked decision 3), templated with the
/// harness label.
///
/// Every generated `delegate_task_<id>` description opens with this. It is the
/// whole distinction between that tool and `offload_task`: not *what the work
/// is*, but **who decided to hand it off**. A model that read only the tool name
/// would call it whenever it wanted help; the sentence is what makes it a
/// user-directed instrument.
const DELEGATE_CONTRACT: &str = "Hand a task to an open {label} tab and return its answer. Call this ONLY when the user \
     explicitly asked for a task to be delegated to {label} (e.g. \"send this to \
     {label}\"). Never call it on your own initiative — for work you decide to offload \
     yourself, use `offload_task`, which you may call automatically whenever you judge it \
     useful.";

/// The rest of the generated description. `{tab}` is the Manual tab's current
/// name, filled by the generator (it changes when the user renames the tab, so
/// it cannot be baked in here).
const DELEGATE_TOOL_DETAIL: &str = "The tab it drives right now is \"{tab}\". The request is typed into that \
     tab exactly as you write it and its answer is read back off the same session, so \
     everything you send is visible on screen and in that harness's own transcript. The \
     worker keeps its own tools, permissions and sandbox; it is a peer, not a subprocess.";

/// The sentence that makes a schema run a schema run, spelled once.
///
/// It is a *substring* of `agent::SCHEMA_SYSTEM_PROMPT` rather than a piece it
/// is built from, because that prompt is one `const` literal and splicing it
/// would cost more than the tripwire that pins the relation
/// (`the_facade_schema_note_is_the_worker_prompts_own_sentence`). The V39 facade
/// needs exactly this sentence, and needs it to be the SAME sentence: the
/// facade's promise is that `offload_task`'s options mean the same thing
/// wherever the task lands.
pub(crate) const SCHEMA_FINAL_INSTRUCTION: &str = concat!(
    "Your final message must be a single JSON value matching the requested schema and nothing ",
    "else: no prose, no narration, no citation markers"
);

/// The facade's rendering of `profile: research` — what the taint latch would
/// have enforced, said in words, because a worker tab is a peer process whose
/// tools cImp does not filter.
const FACADE_RESEARCH: &str = concat!(
    "This is a research task: use web and document sources for it, and do not ",
    "read local files, search the code or run commands."
);

/// The facade's rendering of `profile: code`.
const FACADE_CODE: &str = concat!(
    "This is a local code task: use local file, search and command tools for it, ",
    "and do not fetch anything from the web."
);

// ── rendering ───────────────────────────────────────────────────────────────

/// The whole inventory, rendered in one vocabulary.
///
/// One row per [`Slot::ALL`] entry, in that order — which is what makes the
/// inventory complete by construction rather than by review.
fn render_with(read_tool: &str, shell_tool: &str, label: &str) -> Vec<Instruction> {
    let templated = |slot, text: String| Instruction {
        slot,
        text: Cow::Owned(text),
        neutral: false,
    };
    let neutral = |slot, text: &'static str| Instruction {
        slot,
        text: Cow::Borrowed(text),
        neutral: true,
    };
    vec![
        templated(
            Slot::GraphGuidance,
            GRAPH_GUIDANCE
                .replace("{read_tool}", read_tool)
                .replace("{shell_tool}", shell_tool),
        ),
        neutral(Slot::GraphSemantic, GRAPH_SEMANTIC_GUIDANCE),
        // Harness-specific in kind rather than in bytes: the channel is one
        // harness's mechanism (locked decision 25), and a harness with no
        // inbound MCP path is never handed this block at all.
        templated(Slot::Channel, CHANNEL_INSTRUCTIONS.to_string()),
        neutral(Slot::Attachment, ATTACHMENT_INSTRUCTION),
        templated(
            Slot::DelegateContract,
            DELEGATE_CONTRACT.replace("{label}", label),
        ),
        neutral(Slot::DelegateToolDetail, DELEGATE_TOOL_DETAIL),
        neutral(Slot::SchemaFinal, SCHEMA_FINAL_INSTRUCTION),
        neutral(Slot::FacadeResearch, FACADE_RESEARCH),
        neutral(Slot::FacadeCode, FACADE_CODE),
    ]
}

/// The inventory for `id`, in ITS vocabulary — what every shipped
/// [`super::plugin::HarnessPlugin::instructions`] returns.
///
/// A harness that declares no name for a role gets the neutral description
/// rather than another product's tool id: the fail-closed direction
/// [`super::native`] takes for the same question.
pub fn render_for(id: HarnessId) -> Vec<Instruction> {
    let name = |role| id.plugin().and_then(|p| p.tool_for_role(role));
    render_with(
        name(ToolRole::Read).unwrap_or(NEUTRAL_READ),
        name(ToolRole::Shell).unwrap_or(NEUTRAL_SHELL),
        id.label(),
    )
}

/// The inventory for a tab that runs **no registered harness** — descriptions
/// where the tool names go.
pub fn neutral() -> &'static [Instruction] {
    static CELL: OnceLock<Vec<Instruction>> = OnceLock::new();
    CELL.get_or_init(|| render_with(NEUTRAL_READ, NEUTRAL_SHELL, "any harness"))
}

/// Every instruction `harness` receives — [`neutral`] when it names none.
pub fn all_for(harness: Option<HarnessId>) -> &'static [Instruction] {
    harness
        .and_then(|h| h.plugin())
        .map(|p| p.instructions())
        .filter(|rows| !rows.is_empty())
        .unwrap_or_else(neutral)
}

/// One slot's text for `harness`.
///
/// Empty only if a plugin overrode
/// [`super::plugin::HarnessPlugin::instructions`] and dropped a slot — which
/// `every_harness_declares_every_slot` refuses to let a registered harness ship
/// with, because a consumer cannot tell "this harness says nothing here" from
/// "the text went missing".
pub fn text(harness: Option<HarnessId>, slot: Slot) -> &'static str {
    all_for(harness)
        .iter()
        .find(|i| i.slot == slot)
        .map(|i| i.text.as_ref())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(id: &str) -> HarnessId {
        HarnessId::from_id(id).unwrap_or_else(|| panic!("{id} is registered"))
    }

    /// **The inventory is complete for every registered harness.** A plugin that
    /// overrode `instructions()` and dropped a row would silently stop sending
    /// that text — the failure mode this whole module exists to make impossible,
    /// reproduced one layer down.
    #[test]
    fn every_harness_declares_every_slot() {
        for id in super::super::registry::all() {
            let rows = all_for(Some(id));
            for slot in Slot::ALL {
                let found = rows.iter().filter(|i| i.slot == *slot).count();
                assert_eq!(
                    found, 1,
                    "{id}: slot {:?} appears {found} times in its instruction inventory — every \
                     slot must appear exactly once, or a consumer gets an empty string it cannot \
                     distinguish from silence",
                    slot
                );
            }
            assert_eq!(
                rows.len(),
                Slot::ALL.len(),
                "{id}: the inventory has rows outside `Slot::ALL`"
            );
            for row in rows {
                assert!(
                    !row.text.trim().is_empty(),
                    "{id}: slot {:?} renders empty",
                    row.slot
                );
            }
        }
    }

    /// **No placeholder survives rendering.** A `{read_tool}` that reached a
    /// model would be a visible defect in the prompt, and a typo in a
    /// placeholder name is otherwise invisible — `replace` on a name nothing
    /// matches is a silent no-op.
    #[test]
    fn nothing_ships_with_an_unfilled_placeholder() {
        for rows in super::super::registry::all()
            .map(|id| all_for(Some(id)))
            .chain(std::iter::once(neutral()))
        {
            for row in rows {
                // `{tab}` is filled by the delegate-tool generator at list time
                // (the tab can be renamed between two `tools/list` calls), so it
                // is the one placeholder that legitimately survives to here.
                let text = row.text.replace("{tab}", "");
                assert!(
                    !text.contains("{read_tool}")
                        && !text.contains("{shell_tool}")
                        && !text.contains("{label}"),
                    "slot {:?} still carries an unfilled placeholder: {text}",
                    row.slot
                );
            }
        }
    }

    /// **The one intended behaviour change of Phase E**, stated as an
    /// assertion: the graph nudge names each harness's OWN tools. Claude's
    /// rendering is pinned byte-for-byte by the golden
    /// (`tabs::config::tests::the_claude_system_prompt_addendum_matches_its_golden`);
    /// this is the other direction.
    #[test]
    fn the_graph_nudge_speaks_each_harnesss_own_vocabulary() {
        let claude = text(Some(h("claude")), Slot::GraphGuidance);
        assert!(claude.contains("over a full Read)"), "{claude}");
        assert!(claude.contains("command in Bash;"), "{claude}");

        let opencode = text(Some(h("opencode")), Slot::GraphGuidance);
        assert!(opencode.contains("over a full read)"), "{opencode}");
        assert!(opencode.contains("command in bash;"), "{opencode}");

        // And a tab cImp cannot classify is told about a capability, not about
        // somebody else's tool id.
        let unknown = text(None, Slot::GraphGuidance);
        assert!(unknown.contains("over a full file read)"), "{unknown}");
        assert!(unknown.contains("command in the shell;"), "{unknown}");
    }

    /// The delegate contract names the harness the tool drives, from the
    /// descriptor's label — the V39 `HARNESS_LABELS` table's one source.
    #[test]
    fn the_delegate_contract_carries_the_descriptor_label() {
        for id in super::super::registry::all() {
            let t = text(Some(id), Slot::DelegateContract);
            assert!(
                t.contains(id.label()),
                "{id}: the pinned sentence does not name its label: {t}"
            );
        }
    }

    /// Neutral rows really are the same bytes everywhere. If one of them ever
    /// grows a harness-specific clause, this is what says so.
    #[test]
    fn a_neutral_row_is_the_same_text_for_every_harness() {
        for slot in Slot::ALL {
            let mut seen: Option<&str> = None;
            let mut is_neutral = false;
            for id in super::super::registry::all() {
                let row = all_for(Some(id))
                    .iter()
                    .find(|i| i.slot == *slot)
                    .expect("checked by every_harness_declares_every_slot");
                is_neutral = row.neutral;
                if !row.neutral {
                    continue;
                }
                match seen {
                    None => seen = Some(row.text.as_ref()),
                    Some(prev) => assert_eq!(
                        prev,
                        row.text.as_ref(),
                        "slot {slot:?} is marked neutral but renders differently per harness"
                    ),
                }
            }
            let _ = is_neutral;
        }
    }
}

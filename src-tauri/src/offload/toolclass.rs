//! V32 Phase A — the tool-class taxonomy and the taint latch built on it.
//!
//! # Why this exists
//!
//! Indirect prompt injection: a fetched web page carries instructions aimed at
//! whatever LLM reads it. The offload worker is the softest target — the local
//! model has no injection-resistance training and simultaneously holds the
//! classic lethal trifecta: private data access (`read_file`/`code_search`
//! inside the allowed roots), untrusted content (`ddg__fetch_content` page
//! bodies), and an exfiltration channel (a fetch of an attacker URL with stolen
//! data in the query string). **Both** orderings are lethal: fetch-then-read
//! (the injected page steers later reads, then exfiltrates) and read-then-fetch
//! (secrets ride out on the fetch URL).
//!
//! The containment stance is *capability*, not model judgment: assume the model
//! reading untrusted content WILL be compromised, and make the dangerous
//! combination structurally unreachable. Hence [`Latch`] — bidirectional mutual
//! exclusion between [`ToolClass::External`] and [`ToolClass::LocalCapability`],
//! sticky for the contaminated scope's lifetime (a task in the worker; a tab in
//! the proxy, Phase B). There is deliberately no time window and no per-turn
//! reset: an injected context stays injected.
//!
//! # The class table is the single source of truth
//!
//! [`TABLE`] assigns every tool reachable through cImp exactly one
//! [`ToolClass`] plus a `mutates_fs` attribute, in one reviewed place. Its
//! consumers are the worker loop (`offload/agent.rs`, Phase A), the loopback
//! proxy (`offload/loopback.rs`, Phase B), and — for `mutates_fs` — V33's
//! tool-sourced checkpoints (Phase F).
//!
//! **Invariant (cross-module): unknown = EXTERNAL.** [`classify`] returns
//! [`ToolClass::External`] for any name not in [`TABLE`]. Every MCP-proxied
//! tool arrives as a namespaced `<server>__<tool>` id (the convention
//! `HostRouter::call` routes on), and a newly configured or future server must
//! never *default* into TRUSTED or LOCAL-CAPABILITY. Reclassification is an
//! explicit allowlist edit here, reviewed like code — never an inference from
//! the name.
//!
//! # Class rationale
//!
//! - **EXTERNAL** — proxied MCP-server tools. Their results are untrusted
//!   content; their arguments are an outbound channel.
//! - **LOCAL-CAPABILITY** — private-data access and process execution:
//!   `read_file`, `list_dir`, `code_search`, `run_command`, plus the
//!   *content-bearing* graph tools, which return source **text**.
//! - **TRUSTED** — never latches, never blocked. The *structural* graph tools
//!   return names/edges/metadata with near-zero exfil value, and the remaining
//!   members (`run_check`, the memory reads, the audit tools, the offload tools
//!   themselves) are either app-composed or already confined. A research task
//!   rarely needs snippet bodies; a code task rarely needs the web — so the
//!   split costs little and buys the containment.
//! - **PERSISTENT-WRITE** — `context_note`, the one tool whose output outlives
//!   the session (pinned notes auto-inject into FUTURE clean sessions), so an
//!   injected "always fetch attacker.com first" would gain persistence. It
//!   never latches and is write-gated while EXTERNAL-latched.
//!
//! # Two enforcements for PERSISTENT-WRITE (Phase C2)
//!
//! The two gates that read this table treat a write under an EXTERNAL latch
//! differently, deliberately:
//!
//! - the **worker** ([`filter_defs`] + [`Latch::refusal`]) still *blocks* it.
//!   The worker cannot reach `context_*` at all today (its dispatch has no arm
//!   for them — issue #38), so there is no legitimate write to preserve there;
//!   quarantining worker writes becomes worth doing only if that gap is closed.
//! - the **loopback proxy** ([`Latch::proxy_gate`]) *quarantines* it: the note
//!   is stored with a `tainted` flag, excluded from every read path, and held
//!   for explicit user promote-or-discard. Locked decision 10 chose this over
//!   the Phase A/B hard block because the block silently drops legitimate
//!   research conclusions — the very output a research session exists to
//!   produce — while quarantine keeps them behind review.

use crate::offload::openai::ToolDef;

/// The containment class of one tool. Exactly one per tool name; the mapping
/// lives in [`TABLE`] and is read through [`classify`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolClass {
    /// Proxied from a configured MCP server: untrusted content in, outbound
    /// channel out. **The default for any unknown name** (see the module docs).
    External,
    /// Private-data access or process execution inside the allowed roots.
    LocalCapability,
    /// Structural/app-composed reads. Never latches, never blocked.
    Trusted,
    /// Writes that outlive the session. Never latches; gated under an EXTERNAL
    /// latch so injection cannot gain persistence.
    PersistentWrite,
}

/// One row of the class table: a tool name, its class, and whether calling it
/// can mutate the filesystem.
pub struct ClassRow {
    pub name: &'static str,
    pub class: ToolClass,
    /// Whether this tool can change files on disk. Consumed by V33 Phase F
    /// (a tool-sourced checkpoint fires before any `mutates_fs` call), which is
    /// why it lives here: a future tool declares its class AND its mutation
    /// capability in one reviewed place. Read only by [`mutates_fs`] and the
    /// tests until that consumer lands.
    #[cfg_attr(not(test), allow(dead_code))]
    pub mutates_fs: bool,
}

const fn row(name: &'static str, class: ToolClass, mutates_fs: bool) -> ClassRow {
    ClassRow {
        name,
        class,
        mutates_fs,
    }
}

/// The class table — the single source of truth. Adding a tool anywhere in cImp
/// without adding it here is safe by construction (it classifies as EXTERNAL,
/// the most restrictive class); *promoting* a tool out of EXTERNAL is the edit
/// that needs review.
pub const TABLE: &[ClassRow] = &[
    // ── LOCAL-CAPABILITY — private data + process execution ────────────────
    row("read_file", ToolClass::LocalCapability, false),
    row("list_dir", ToolClass::LocalCapability, false),
    row("code_search", ToolClass::LocalCapability, false),
    row("run_command", ToolClass::LocalCapability, true),
    // Content-bearing graph tools: these return source TEXT, which re-opens a
    // bounded exfil channel if EXTERNAL stays live.
    row("graph_snippet", ToolClass::LocalCapability, false),
    row("graph_search_docs", ToolClass::LocalCapability, false),
    row("graph_semantic_docs", ToolClass::LocalCapability, false),
    // Not enumerated in the milestone's locked table, but it is the code-body
    // sibling of `graph_semantic_docs` (advertised when `graph.embed_code_bodies`
    // is on) and returns embedded source bodies — content-bearing by the same
    // rationale. Classified here rather than left to the unknown⇒EXTERNAL
    // default, which would let a purely local graph query latch a task as
    // externally tainted.
    row("graph_semantic_code", ToolClass::LocalCapability, false),
    // ── TRUSTED — structural graph + app-composed reads ────────────────────
    row("graph_find_symbol", ToolClass::Trusted, false),
    row("graph_callers", ToolClass::Trusted, false),
    row("graph_callees", ToolClass::Trusted, false),
    row("graph_references", ToolClass::Trusted, false),
    row("graph_imports", ToolClass::Trusted, false),
    row("graph_outline", ToolClass::Trusted, false),
    row("graph_transitive", ToolClass::Trusted, false),
    row("graph_repo_map", ToolClass::Trusted, false),
    row("graph_impact", ToolClass::Trusted, false),
    row("graph_tests_for", ToolClass::Trusted, false),
    row("graph_recent_changes", ToolClass::Trusted, false),
    row("graph_dead_exports", ToolClass::Trusted, false),
    row("graph_cycles", ToolClass::Trusted, false),
    row("graph_struct_search", ToolClass::Trusted, false),
    // Also absent from the locked table but shipped in `graph::tool_specs()`
    // (V15). Both return structure only — a node path and a subsystem/god-node
    // report — so they belong with the structural set; leaving them to the
    // unknown⇒EXTERNAL default would misclassify a local structural query as
    // untrusted web content.
    row("graph_path", ToolClass::Trusted, false),
    row("graph_architecture", ToolClass::Trusted, false),
    // `run_check` runs the project's own user-vetted `checks` commands and
    // returns a bounded, parsed report. Recorded as `mutates_fs: false` per the
    // milestone's locked attribute list (only `run_command` and the harness
    // natives are true); see the V33 Phase F note — a check that builds does
    // touch a target dir, so this row is the one worth revisiting when
    // checkpoints land.
    row("run_check", ToolClass::Trusted, false),
    // Memory READS (V10). The write sibling is PERSISTENT-WRITE below.
    row("context_recall", ToolClass::Trusted, false),
    row("context_notes", ToolClass::Trusted, false),
    // V26 audit tools: the scan runs locally inside the app and the report is
    // app-composed scanner output.
    row("security_audit", ToolClass::Trusted, false),
    row("quality_audit", ToolClass::Trusted, false),
    // The offload tools themselves — a consumer delegating a subtask must not
    // be latched out of doing so (and the subtask gets its own latch).
    row("offload_task", ToolClass::Trusted, false),
    row("offload_batch", ToolClass::Trusted, false),
    // ── PERSISTENT-WRITE ───────────────────────────────────────────────────
    row("context_note", ToolClass::PersistentWrite, false),
    // ── Harness-native tools — classified, NOT enforced ─────────────────────
    // Claude Code's and OpenCode's own file/shell tools never route through a
    // cImp router, so NO cImp latch can block them: decision 3's honest limit,
    // with OS-level containment left to V33 and optional hook-based gating to
    // Phase E. These rows exist because (a) V33 Phase F reads `mutates_fs`
    // from this table for tool-sourced checkpoints, and (b) a Phase E hook
    // that does gate them needs their class from the same reviewed place.
    row("Edit", ToolClass::LocalCapability, true),
    row("Write", ToolClass::LocalCapability, true),
    row("Bash", ToolClass::LocalCapability, true),
];

/// The class of `name`. **Unknown ⇒ [`ToolClass::External`]** — the locked
/// cross-module invariant (see the module docs): a future/newly configured MCP
/// server's `<server>__<tool>` ids are unknown here and must land in the most
/// restrictive class, never be inferred into a trusted one.
pub fn classify(name: &str) -> ToolClass {
    TABLE
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.class)
        .unwrap_or(ToolClass::External)
}

/// Whether `name` can mutate the filesystem. Unknown names are `false`: an MCP
/// server's tools are outside our filesystem, and V33's checkpoint consumer
/// must not checkpoint on every web fetch.
///
/// Its consumer is V33 Phase F (tool-sourced checkpoints), which is a separate
/// milestone — the attribute is defined and tested here now so that the class
/// table stays the one place a new tool declares both facts, per the V32 spec.
#[cfg_attr(not(test), allow(dead_code))]
pub fn mutates_fs(name: &str) -> bool {
    TABLE
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.mutates_fs)
        .unwrap_or(false)
}

/// The declared shape of a task, pre-applying the latch at task start
/// (locked decision 4). Undeclared tasks start [`Latch::Open`] and latch on
/// their first EXTERNAL / LOCAL-CAPABILITY call instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Web/document research: EXTERNAL from turn 1, LOCAL-CAPABILITY never
    /// advertised.
    Research,
    /// Local code work: LOCAL-CAPABILITY from turn 1, EXTERNAL never
    /// advertised.
    Code,
}

impl Profile {
    /// Parse a caller-supplied `profile` argument. Unlike
    /// `ThinkingMode::parse`/`TierHint::parse` (which fall back to a benign
    /// default) an unrecognized value is an **error**: the schema `enum` is an
    /// upstream guarantee with a kill switch, so the value is validated
    /// post-hoc at the parse boundary and a typo must not silently degrade to
    /// "no containment profile".
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "research" => Ok(Profile::Research),
            "code" => Ok(Profile::Code),
            other => Err(format!(
                "invalid `profile` value `{other}` — expected \"research\" or \"code\" (omit the \
                 argument to let the task latch dynamically on its first tool call)"
            )),
        }
    }

    /// The canonical wire value (what the child forwards to the app).
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Research => "research",
            Profile::Code => "code",
        }
    }

    /// The latch this profile pre-applies at task start.
    pub fn latch(self) -> Latch {
        match self {
            Profile::Research => Latch::External,
            Profile::Code => Latch::Local,
        }
    }
}

/// The taint latch for one contaminated scope (a worker task in Phase A; a tab
/// in the proxy, Phase B). Sticky: once engaged it never re-opens and never
/// flips to the other side — the scope is contaminated for its lifetime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Latch {
    /// Nothing latched yet: every class is available.
    #[default]
    Open,
    /// EXTERNAL was used (or the `research` profile pre-applied it):
    /// LOCAL-CAPABILITY and PERSISTENT-WRITE are unavailable.
    External,
    /// LOCAL-CAPABILITY was used (or the `code` profile): EXTERNAL is
    /// unavailable.
    Local,
}

impl Latch {
    /// The latch a declared `profile` starts from ([`Latch::Open`] when none).
    pub fn from_profile(profile: Option<Profile>) -> Self {
        profile.map_or(Latch::Open, Profile::latch)
    }

    /// Whether `class` is currently unavailable.
    pub fn blocks(self, class: ToolClass) -> bool {
        match (self, class) {
            // TRUSTED is available under every latch, by definition.
            (_, ToolClass::Trusted) => false,
            (Latch::Open, _) => false,
            (Latch::External, ToolClass::LocalCapability | ToolClass::PersistentWrite) => true,
            (Latch::External, _) => false,
            (Latch::Local, ToolClass::External) => true,
            (Latch::Local, _) => false,
        }
    }

    /// Engage the latch for a class that is **about to execute**. TRUSTED and
    /// PERSISTENT-WRITE never latch; an already-engaged latch never changes
    /// (sticky). Returns whether the state moved, so callers can log the
    /// transition.
    ///
    /// Callers must consult [`blocks`](Self::blocks) first: a refused call must
    /// never engage or flip a latch, or a hallucinated call to an
    /// already-blocked tool could redefine the scope's taint.
    pub fn engage(&mut self, class: ToolClass) -> bool {
        let next = match (*self, class) {
            (Latch::Open, ToolClass::External) => Latch::External,
            (Latch::Open, ToolClass::LocalCapability) => Latch::Local,
            _ => *self,
        };
        let moved = next != *self;
        *self = next;
        moved
    }

    /// The fixed-string refusal for a blocked `class`, or `None` when the call
    /// may proceed. Deliberately carries **no** dynamic content: the refusal is
    /// a security boundary the model must not be able to shape or probe, and a
    /// constant string is what the tests pin.
    pub fn refusal(self, class: ToolClass) -> Option<&'static str> {
        if !self.blocks(class) {
            return None;
        }
        match class {
            ToolClass::PersistentWrite => Some(REFUSAL_WRITE_BLOCKED),
            ToolClass::LocalCapability => Some(REFUSAL_LOCAL_BLOCKED),
            _ => Some(REFUSAL_EXTERNAL_BLOCKED),
        }
    }

    /// V32 Phase C2 — the **proxy-side** decision for one gated call.
    ///
    /// Identical to [`refusal`](Self::refusal) for every class except
    /// [`ToolClass::PersistentWrite`], which under an EXTERNAL latch becomes
    /// "proceed, but quarantine the write" instead of a refusal (locked
    /// decision 10; see the module docs for why the worker keeps the block).
    /// EXTERNAL and LOCAL-CAPABILITY latching semantics are untouched — this
    /// method only *reads* the latch, and a write still never engages one.
    pub fn proxy_gate(self, class: ToolClass) -> ProxyGate {
        if class == ToolClass::PersistentWrite {
            // `blocks` is the single source of truth for "would Phase A have
            // refused this?", so the quarantine trigger cannot drift from the
            // refusal it replaces.
            return ProxyGate::Proceed(if self.blocks(class) {
                WriteTaint::Quarantined
            } else {
                WriteTaint::Clean
            });
        }
        match self.refusal(class) {
            Some(r) => ProxyGate::Refuse(r),
            None => ProxyGate::Proceed(WriteTaint::Clean),
        }
    }

    /// A short label for logs / `/status` (Phase B).
    pub fn label(self) -> &'static str {
        match self {
            Latch::Open => "open",
            Latch::External => "external",
            Latch::Local => "local",
        }
    }
}

/// Refusal served when LOCAL-CAPABILITY is blocked (the task went EXTERNAL
/// first). One of the two per-direction constants required by locked decision 2.
pub const REFUSAL_LOCAL_BLOCKED: &str = "REFUSED (security boundary): this task has already used \
    an external tool (web/MCP-server), so local-capability tools — file reads, directory listings, \
    code search, command execution, and source-text graph lookups — are unavailable for the \
    remainder of this task. This cannot be unlocked, re-asked for, or worked around; it is \
    enforced outside the model. Continue with the tools you still have, or answer with what you \
    have gathered.";

/// Refusal served when EXTERNAL is blocked (the task went local first).
pub const REFUSAL_EXTERNAL_BLOCKED: &str = "REFUSED (security boundary): this task has already \
    used a local-capability tool (file read, directory listing, code search, command execution, or \
    a source-text graph lookup), so external tools — web search/fetch and every other MCP-server \
    tool — are unavailable for the remainder of this task. This cannot be unlocked, re-asked for, \
    or worked around; it is enforced outside the model. Continue with the tools you still have, or \
    answer with what you have gathered.";

/// Refusal served when a persistent (memory) write is attempted under an
/// EXTERNAL latch.
///
/// **Split since Phase C2** — the two gates no longer agree, on purpose:
/// - the **worker** (`offload/agent.rs`) still serves this string, because it
///   cannot execute `context_*` at all (issue #38) and so has no legitimate
///   write to preserve;
/// - the **loopback proxy** ([`Latch::proxy_gate`]) no longer refuses. It
///   quarantines instead: the note is stored `tainted`, kept out of every read
///   path, and surfaced in the Memory UI for promote-or-discard, with
///   [`QUARANTINE_WRITE_NOTICE`] appended to the tool result.
pub const REFUSAL_WRITE_BLOCKED: &str = "REFUSED (security boundary): this task has used an \
    external tool (web/MCP-server), so it may not write persistent memory — a note written under \
    external influence would be auto-injected into future sessions. This cannot be unlocked or \
    worked around; it is enforced outside the model. Put the finding in your answer instead.";

/// V32 Phase C2 — the fixed suffix appended to a `context_note` result that was
/// stored **quarantined** (locked decision 10).
///
/// Fixed-string and content-free for the same reason as the refusals above: the
/// containment boundary must not be something the model can shape or probe, and
/// a constant is what the tests pin. It states the three facts the model needs
/// to plan around — the note IS saved, it is invisible until a human releases
/// it, and re-trying will not change that — so a compromised session cannot
/// read the outcome as a transient failure worth working around.
pub const QUARANTINE_WRITE_NOTICE: &str = " ⚠ QUARANTINED (security boundary): this session has \
    used an external tool (web/MCP-server), so the note was saved but held for review instead of \
    entering project memory — it will NOT be recalled or auto-injected into any session until the \
    user promotes it in cImp's Memory view. Nothing further can be done from here; do not rewrite \
    or re-save it, and include anything the user must act on now in your answer as well.";

/// V32 Phase C2 — whether a PERSISTENT-WRITE that the gate let through must be
/// stored quarantined. Threaded from [`Latch::proxy_gate`] through the loopback
/// `/graph_run` route into the memory write (`GraphIndex::mem_add_note`).
///
/// An enum rather than a `bool` because it crosses five module boundaries: at a
/// call site `WriteTaint::Clean` says what it means, where a bare `false` would
/// be one transposition away from silently un-quarantining every write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WriteTaint {
    /// Not latched EXTERNAL — the write enters memory normally.
    #[default]
    Clean,
    /// Written under an EXTERNAL latch: store it, flag it, hide it from every
    /// read path until the user promotes it.
    Quarantined,
}

impl WriteTaint {
    /// The stored `mem_note.tainted` column value.
    pub fn is_quarantined(self) -> bool {
        matches!(self, WriteTaint::Quarantined)
    }
}

/// The outcome of [`Latch::proxy_gate`]: proceed (with the taint the write must
/// carry) or refuse with a fixed string. Deliberately not a
/// `Result<WriteTaint, &str>` at the *decision* layer — the taint is not an
/// error case, and naming both arms keeps the quarantine path from reading as a
/// degraded refusal at the call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyGate {
    Proceed(WriteTaint),
    Refuse(&'static str),
}

/// The `profile` sentence + secrets warning shared by every rendering of the
/// `offload_task`/`offload_batch` description (the config-derived one in
/// `offload/mcp.rs` and the health-accurate `/describe` one in
/// `offload/service.rs`), so the two can't drift.
///
/// The warning states an accepted residual, not a solved problem: a research
/// task's prompt is visible to whatever page it fetches, and prompt
/// exfiltration cannot be blocked from inside the worker.
pub const PROFILE_TOOL_NOTE: &str = " Pass `profile` to declare the task's shape and get \
    injection containment from turn 1: `research` (web/document work — the worker never gets \
    local file/search/command tools) or `code` (local work — the worker never gets web/MCP-server \
    tools). Omit it and the worker latches on its own first tool call: once it has used one side, \
    the other is unavailable for the rest of the task. NEVER include secrets or sensitive code in \
    the task text of a research task — the task prompt is visible to whatever web content the task \
    fetches, and prompt exfiltration cannot be blocked.";

/// Drop the defs of every class `latch` blocks. Locked decision 2: enforcement
/// is **def removal**, not refusal-only — models handle an absent tool far
/// better than a refused one, and an absent def shrinks the steering surface an
/// injected page has to work with. The in-flight refusal ([`Latch::refusal`])
/// is the belt-and-braces half, for a call already in the same turn or
/// hallucinated from an earlier turn's def list.
pub fn filter_defs(defs: &[ToolDef], latch: Latch) -> Vec<ToolDef> {
    defs.iter()
        .filter(|d| !latch.blocks(classify(&d.function.name)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(name: &str) -> ToolDef {
        ToolDef::function(name, "", json!({ "type": "object" }))
    }

    fn names(defs: &[ToolDef]) -> Vec<String> {
        defs.iter().map(|d| d.function.name.clone()).collect()
    }

    /// Spot checks across every class, plus the `mutates_fs` attribute.
    #[test]
    fn classify_table_spot_checks() {
        // LOCAL-CAPABILITY: the private-data + process tools and the
        // content-bearing graph tools.
        for n in [
            "read_file",
            "list_dir",
            "code_search",
            "run_command",
            "graph_snippet",
            "graph_search_docs",
            "graph_semantic_docs",
        ] {
            assert_eq!(classify(n), ToolClass::LocalCapability, "{n}");
        }
        // TRUSTED: structural graph + app-composed reads + the offload tools.
        for n in [
            "graph_find_symbol",
            "graph_callers",
            "graph_callees",
            "graph_references",
            "graph_imports",
            "graph_outline",
            "graph_transitive",
            "graph_repo_map",
            "graph_impact",
            "graph_tests_for",
            "graph_recent_changes",
            "graph_dead_exports",
            "graph_cycles",
            "graph_struct_search",
            "run_check",
            "context_recall",
            "context_notes",
            "security_audit",
            "quality_audit",
            "offload_task",
            "offload_batch",
        ] {
            assert_eq!(classify(n), ToolClass::Trusted, "{n}");
        }
        // PERSISTENT-WRITE: exactly one member.
        assert_eq!(classify("context_note"), ToolClass::PersistentWrite);
        assert_eq!(
            TABLE
                .iter()
                .filter(|r| r.class == ToolClass::PersistentWrite)
                .count(),
            1
        );
        // EXTERNAL: the proxied servers we ship with.
        for n in ["ddg__search", "ddg__fetch_content", "context7__query-docs"] {
            assert_eq!(classify(n), ToolClass::External, "{n}");
        }
        // `mutates_fs`: true only for `run_command` and the harness natives.
        assert!(mutates_fs("run_command"));
        for n in ["Edit", "Write", "Bash"] {
            assert!(mutates_fs(n), "{n}");
        }
        for n in ["read_file", "code_search", "graph_snippet", "ddg__search"] {
            assert!(!mutates_fs(n), "{n}");
        }
        // Every row is unique — a duplicate name would make `classify` depend
        // on table order.
        for (i, r) in TABLE.iter().enumerate() {
            assert!(
                !TABLE[..i].iter().any(|p| p.name == r.name),
                "duplicate row for `{}`",
                r.name
            );
        }
    }

    /// The locked cross-module invariant: anything not in the table is
    /// EXTERNAL. A newly configured MCP server must never default into a
    /// trusted class just because nobody edited the table.
    #[test]
    fn unknown_and_namespaced_tools_default_to_external() {
        assert_eq!(classify("somenewserver__anything"), ToolClass::External);
        assert_eq!(classify("totally_unknown"), ToolClass::External);
        assert_eq!(classify(""), ToolClass::External);
        // Name-shaped near-misses must not slip into a trusted class either.
        assert_eq!(classify("graph_"), ToolClass::External);
        assert_eq!(classify("evil__read_file"), ToolClass::External);
        assert!(!mutates_fs("somenewserver__anything"));
    }

    #[test]
    fn latch_is_bidirectional_and_sticky() {
        // EXTERNAL first ⇒ local side closed, external side stays open.
        let mut l = Latch::default();
        assert!(!l.blocks(ToolClass::LocalCapability));
        assert!(l.engage(ToolClass::External));
        assert_eq!(l, Latch::External);
        assert!(l.blocks(ToolClass::LocalCapability));
        assert!(l.blocks(ToolClass::PersistentWrite));
        assert!(!l.blocks(ToolClass::External));
        // Sticky: a later LOCAL-CAPABILITY class can't flip it back.
        assert!(!l.engage(ToolClass::LocalCapability));
        assert_eq!(l, Latch::External);

        // LOCAL-CAPABILITY first ⇒ external side closed, writes still allowed.
        let mut l = Latch::default();
        assert!(l.engage(ToolClass::LocalCapability));
        assert_eq!(l, Latch::Local);
        assert!(l.blocks(ToolClass::External));
        assert!(!l.blocks(ToolClass::LocalCapability));
        assert!(!l.blocks(ToolClass::PersistentWrite));
        assert!(!l.engage(ToolClass::External));
        assert_eq!(l, Latch::Local);
    }

    #[test]
    fn trusted_is_immune_under_both_latches_and_never_latches() {
        for latch in [Latch::Open, Latch::External, Latch::Local] {
            assert!(!latch.blocks(ToolClass::Trusted), "{latch:?}");
            assert!(latch.refusal(ToolClass::Trusted).is_none(), "{latch:?}");
        }
        // A TRUSTED (or PERSISTENT-WRITE) call never engages a latch.
        let mut l = Latch::default();
        assert!(!l.engage(ToolClass::Trusted));
        assert!(!l.engage(ToolClass::PersistentWrite));
        assert_eq!(l, Latch::Open);
    }

    #[test]
    fn refusals_are_the_fixed_per_direction_constants() {
        assert_eq!(
            Latch::External.refusal(ToolClass::LocalCapability),
            Some(REFUSAL_LOCAL_BLOCKED)
        );
        assert_eq!(
            Latch::External.refusal(ToolClass::PersistentWrite),
            Some(REFUSAL_WRITE_BLOCKED)
        );
        assert_eq!(
            Latch::Local.refusal(ToolClass::External),
            Some(REFUSAL_EXTERNAL_BLOCKED)
        );
        assert!(Latch::Open.refusal(ToolClass::External).is_none());
        assert!(Latch::Open.refusal(ToolClass::LocalCapability).is_none());
        // No dynamic content: the strings must not be templated.
        for s in [
            REFUSAL_LOCAL_BLOCKED,
            REFUSAL_EXTERNAL_BLOCKED,
            REFUSAL_WRITE_BLOCKED,
        ] {
            assert!(!s.contains('{'), "refusal must be a fixed string: {s}");
        }
    }

    /// V32 Phase C2: the proxy gate quarantines a PERSISTENT-WRITE where the
    /// worker gate refuses it, and is byte-identical to the worker gate for
    /// every other class (the latching semantics of EXTERNAL and
    /// LOCAL-CAPABILITY are untouched by C2).
    #[test]
    fn proxy_gate_quarantines_writes_and_leaves_every_other_class_alone() {
        // Unlatched / LOCAL-latched: a write proceeds CLEAN.
        for latch in [Latch::Open, Latch::Local] {
            assert_eq!(
                latch.proxy_gate(ToolClass::PersistentWrite),
                ProxyGate::Proceed(WriteTaint::Clean),
                "{latch:?}"
            );
        }
        // EXTERNAL-latched: proceeds, but quarantined — NOT the Phase A/B
        // refusal, which the worker still serves from the same latch.
        assert_eq!(
            Latch::External.proxy_gate(ToolClass::PersistentWrite),
            ProxyGate::Proceed(WriteTaint::Quarantined)
        );
        assert_eq!(
            Latch::External.refusal(ToolClass::PersistentWrite),
            Some(REFUSAL_WRITE_BLOCKED),
            "the worker-side block must stay as it was"
        );
        // Every other class: the proxy gate mirrors `refusal` exactly.
        for latch in [Latch::Open, Latch::External, Latch::Local] {
            for class in [
                ToolClass::External,
                ToolClass::LocalCapability,
                ToolClass::Trusted,
            ] {
                let expected = match latch.refusal(class) {
                    Some(r) => ProxyGate::Refuse(r),
                    None => ProxyGate::Proceed(WriteTaint::Clean),
                };
                assert_eq!(latch.proxy_gate(class), expected, "{latch:?}/{class:?}");
            }
        }
        assert!(WriteTaint::Quarantined.is_quarantined());
        assert!(!WriteTaint::Clean.is_quarantined());
        assert_eq!(WriteTaint::default(), WriteTaint::Clean);
    }

    /// The quarantine notice is a fixed string, like the refusals, and states
    /// the three facts a model must not misread: stored, withheld pending a
    /// human, not retryable.
    #[test]
    fn quarantine_notice_is_a_fixed_string_stating_the_contract() {
        assert!(QUARANTINE_WRITE_NOTICE.contains("QUARANTINED (security boundary)"));
        assert!(QUARANTINE_WRITE_NOTICE.contains("saved"));
        assert!(QUARANTINE_WRITE_NOTICE.contains("until the user promotes it"));
        assert!(QUARANTINE_WRITE_NOTICE.contains("do not rewrite"));
        assert!(!QUARANTINE_WRITE_NOTICE.contains('{'));
    }

    #[test]
    fn filter_defs_removes_exactly_the_blocked_class() {
        let all = vec![
            def("read_file"),
            def("graph_snippet"),
            def("graph_outline"),
            def("ddg__fetch_content"),
            def("somenewserver__anything"),
            def("context_note"),
            def("context_recall"),
        ];
        // Open: nothing removed.
        assert_eq!(names(&filter_defs(&all, Latch::Open)).len(), all.len());
        // EXTERNAL-latched: local-capability + persistent-write defs gone,
        // external + trusted stay.
        let ext = names(&filter_defs(&all, Latch::External));
        assert_eq!(
            ext,
            [
                "graph_outline",
                "ddg__fetch_content",
                "somenewserver__anything",
                "context_recall",
            ],
            "unexpected surface under an EXTERNAL latch: {ext:?}"
        );
        // LOCAL-latched: every external def gone (including the unknown one).
        let loc = names(&filter_defs(&all, Latch::Local));
        assert!(!loc.iter().any(|n| n.contains("__")), "{loc:?}");
        assert!(loc.contains(&"read_file".to_string()));
        assert!(loc.contains(&"graph_snippet".to_string()));
        assert!(loc.contains(&"context_note".to_string()));
    }

    #[test]
    fn profile_parses_strictly_and_pre_applies_the_latch() {
        assert_eq!(Profile::parse("research").unwrap(), Profile::Research);
        assert_eq!(Profile::parse("code").unwrap(), Profile::Code);
        assert_eq!(Profile::parse(" code ").unwrap(), Profile::Code);
        // Invalid values are rejected at the parse boundary, never silently
        // defaulted — the schema enum is not trusted to have held.
        for bad in ["Research", "web", "", "auto"] {
            let err = Profile::parse(bad).unwrap_err();
            assert!(err.contains("invalid `profile`"), "{bad}: {err}");
        }
        assert_eq!(Profile::Research.latch(), Latch::External);
        assert_eq!(Profile::Code.latch(), Latch::Local);
        assert_eq!(Latch::from_profile(None), Latch::Open);
        assert_eq!(
            Latch::from_profile(Some(Profile::Research)),
            Latch::External
        );
        assert_eq!(Profile::Research.as_str(), "research");
        assert_eq!(Profile::Code.as_str(), "code");
    }

    /// The description note is the one place the `profile` sentence and the
    /// secrets warning live; both descriptions embed it (asserted in
    /// `offload/mcp.rs`), so pin its load-bearing content here.
    #[test]
    fn profile_tool_note_carries_the_secrets_warning() {
        assert!(PROFILE_TOOL_NOTE.contains("`profile`"));
        assert!(PROFILE_TOOL_NOTE.contains("research"));
        assert!(PROFILE_TOOL_NOTE.contains("code"));
        assert!(PROFILE_TOOL_NOTE.contains("NEVER include secrets"));
        assert!(PROFILE_TOOL_NOTE.contains("prompt exfiltration cannot be blocked"));
    }
}

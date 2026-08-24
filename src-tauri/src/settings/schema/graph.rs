//! The code-graph / code-intelligence block.
//!
//! Split out of `schema.rs` by V42 R10; see the module docs in `mod.rs`.

use super::*;

/// V9-01: per-project code knowledge graph configuration. The structural
/// graph (symbols/refs/calls/imports/full-text docs) needs no embedding
/// model; the `semantic_*` fields drive the optional Phase-G semantic search
/// over a remote `/v1/embeddings` endpoint. Additive `#[serde(default)]` — old
/// settings files round-trip with the feature disabled.
///
/// **V33 Phase E: this block now holds a secret** — `embedding_auth_token` —
/// so `Debug` is hand-rolled and redacts it, exactly like [`OffloadSettings`]
/// and [`ClaudeLocalSettings`]. (It read "No secrets here, so `Debug` is
/// derived" until the token landed; a derived `Debug` would print the bearer
/// token into the rolling log the first time anyone logs a settings snapshot.)
/// `graph_settings_debug_covers_every_field_and_redacts_the_token` keeps the
/// hand-rolled impl from silently omitting a future field.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct GraphSettings {
    /// Master switch. Off = no indexing, no `graph_*` tools, no monitor tab.
    pub enabled: bool,
    /// Languages to index. Each maps to a tree-sitter grammar (+ `tags.scm`,
    /// + optional stack-graphs `tsg`). Unsupported files are skipped.
    pub languages: Vec<String>,
    /// Extra ignore globs, additive to the project's `.gitignore`. Generated
    /// / vendored / minified dirs are excluded by default to keep the graph
    /// clean and the DB small.
    pub ignore: Vec<String>,
    /// Index markdown files + doc-comments as `doc_chunk` nodes linked to the
    /// code they describe (powers `graph_search_docs`).
    pub index_docs: bool,
    /// Skip files larger than this many bytes (minified bundles, blobs).
    pub max_file_bytes: u64,
    /// Debounce window for the fs watcher's re-index pass (milliseconds).
    pub watch_debounce_ms: u64,
    /// Hard cap on rows returned by any single `graph_*` query (results feed
    /// an LLM context, so they're bounded like V8's tool results).
    pub max_rows_per_query: u32,
    /// Hard cap on the snippet bytes attached to each result row.
    pub max_snippet_bytes: u32,
    /// Hard cap on the body bytes returned by `graph_snippet` (V11 Phase A).
    /// Larger than `max_snippet_bytes` because whole definition bodies are
    /// bigger than the one-line snippets attached to result rows.
    pub max_body_bytes: u32,
    /// Per-project subdirectory holding `graph.db`. Recommended git-ignored.
    pub db_subdir: String,
    /// Let the **offload worker** query the graph when it's running on a
    /// **remote** backend. The local worker always gets graph access; a remote
    /// backend (LAN *or* cloud) sends your project's code structure off this
    /// machine, so it's opt-in and off by default. The user decides per their
    /// trust in the remote (a private LAN box vs. a public cloud API). The
    /// cloud Opus session (via MCP) and a local worker are unaffected.
    pub allow_remote_worker_access: bool,

    // --- Semantic search (Phase G) ---
    /// Enable embedding-based semantic search. Default off — it needs a
    /// reachable embedding endpoint; the structural graph works without it.
    pub semantic_search: bool,
    /// OpenAI-compatible `/v1/embeddings` endpoint (e.g. a `llama-server
    /// --embedding` on a spare GPU box).
    pub embedding_endpoint: String,
    /// V33 Phase E: bearer token sent on every request to
    /// [`embedding_endpoint`](Self::embedding_endpoint) — the embeddings POST
    /// and the `/props`, `/tokenize`, `/detokenize` helpers alike. Empty (the
    /// default, and every pre-V33 settings file) = no `Authorization` header,
    /// i.e. exactly the pre-V33 behaviour.
    ///
    /// Why this one matters most: the embedding endpoint is the only LAN
    /// service whose corruption is **silent**. A poisoned `/health` fails
    /// loudly; poisoned vectors just make semantic search quietly wrong, for
    /// as long as the epoch lives. Redacted in `Debug` (see the type doc).
    pub embedding_auth_token: String,
    /// Embedding model id requested from the endpoint. Baked into the vector
    /// "epoch"; changing it forces a re-embed.
    pub embedding_model: String,
    /// Embedding vector dimension. `0` = auto-probe on the first embed. The
    /// HNSW index never mixes dimensions.
    pub embedding_dims: u32,
    /// Also embed full symbol bodies (not just docs + signatures) for
    /// semantic *code* search. Off by default — multiplies vector count.
    /// Requires `semantic_search` on (it shares the embedder + backfill pass);
    /// with `semantic_search` on, this enables the `graph_semantic_code` tool and
    /// its code-embedding pass.
    pub embed_code_bodies: bool,
    /// Number of chunks per `/v1/embeddings` request (amortizes round-trips).
    pub embedding_batch: usize,
    /// Hard per-input token budget for the embedding endpoint. `0` = auto-detect
    /// from the server's `/props` (`default_generation_settings.n_ctx`, minus a
    /// small margin), cached per endpoint for the process. Any text over the
    /// budget is truncated (via the server's own tokenizer when available)
    /// before it's sent, because a single oversized chunk makes the endpoint
    /// reject the WHOLE batch. Set it manually for a non-llama server that
    /// exposes no `/props`; with no override and no detection, texts are sent
    /// unchanged.
    pub embedding_max_tokens: u32,
    /// Project-wide cap on how many `code_chunk` rows a full rebuild keeps
    /// (a simple count cap for V1 — see `build_tree`). Bounds DB size and
    /// embedding cost on very large repos.
    pub semantic_code_max_chunks: u32,

    // --- Context injection (V10 Phase D) ---
    /// Automatically prepend a budget-bounded digest of the most relevant files
    /// to each user prompt (Claude via a UserPromptSubmit hook, OpenCode via a
    /// plugin). Off by default — it changes what the agent sees.
    pub context_injection: bool,
    /// Max characters of digest emitted per file (outline + best snippet).
    pub context_per_file_chars: u32,
    /// Total character budget for one turn's injected context across all files.
    pub context_turn_budget_chars: u32,
    /// Fold the current session's working set (Phase C memory) into the ranking
    /// so session-hot files rank first.
    pub context_include_session: bool,
    /// Minimum top-file relevance score below which nothing is injected (so
    /// meta/"hi" prompts inject nothing).
    pub context_min_score: u32,

    // --- V11 Phase B: repo map (session-start orientation) ---
    /// Character budget for the once-per-session project map (`graph_repo_map`
    /// tool, and the session-start injection when enabled).
    pub repo_map_budget_chars: u32,
    /// Prepend the project map to the first injected turn of each new session.
    /// Rides the `context_injection` master toggle AND this flag. Off by default.
    pub repo_map_on_session_start: bool,

    // --- V11 Phase C: injection dedup ---
    /// How many turns a dedup suppression lasts: a file injected in full is
    /// demoted to a one-line "unchanged" reminder on later turns until it changes
    /// or this many turns pass. `0` disables dedup (every turn re-injects).
    pub context_dedup_ttl_turns: u32,

    // --- V11 Phase D: compaction survival (Claude PreCompact) ---
    /// Feed the compactor the session's working set + pinned notes so they
    /// survive the summary (and clear dedup / mark post-compaction). Costs a few
    /// hundred chars once per compaction; still master-gated by `context_injection`.
    pub compaction_context: bool,

    // --- V11 Phase E: redundant-read advisor (opt-in; logic in Phase E) ---
    /// Intercept a `Read` of a file already read unchanged this session and
    /// answer with a cheap reminder (outline digest) instead of re-reading it.
    /// Strictly opt-in — it changes the agent's tool behaviour. Default off.
    pub read_advisor: bool,
    /// Files with fewer than this many lines always pass the advisor (a small
    /// file is cheap to re-read; the reminder isn't worth it).
    pub read_advisor_min_lines: u32,
    /// `"advise"` (remind with the outline) or `"substitute"` (also include the
    /// most relevant symbol body). Default `"advise"`. Compared post-hoc by its
    /// consumers, so an unrecognized string — and, since #48, a value of the
    /// wrong JSON type — behaves as `advise` rather than quarantining the whole
    /// settings file; see [`de_read_advisor_mode`].
    #[serde(deserialize_with = "de_read_advisor_mode")]
    pub read_advisor_mode: String,
    /// V16 Feature 5: trust TTL — after this many retrieval turns since the
    /// advisor last observed a full read of a file, a `Read` passes again
    /// (bounds how long the advisor trusts the agent's memory across context
    /// loss it can't observe: context editing, tool-result truncation).
    /// 0 = off (the pre-V16 behavior: trust for the whole session).
    pub read_advisor_ttl_turns: u32,
    /// V17 Phase A: when a file the agent already read is re-read *after it
    /// changed*, answer with a line-level unified diff against the last-read
    /// snapshot instead of passing the whole file. Exact (a diff versus the
    /// snapshot can't mislead), so it's safe on the post-edit verify loop that
    /// dominates real sessions. Default **on** — a strictly-better substitute,
    /// still master-gated by `read_advisor` and the E1 hard block. Falls back to
    /// a plain pass whenever no snapshot survives (small file / over-cap /
    /// LRU-evicted) or the rendered diff exceeds half the new content.
    pub read_advisor_diffs: bool,
    /// V17 Phase B: also intercept a whole-file shell read (`cat FILE`,
    /// `Get-Content FILE`, `type FILE`, `gc FILE`) of an already-read file via a
    /// second `PreToolUse` **Bash** matcher — the shell equivalent of the `Read`
    /// advisor. Strict: only a provable pure whole-file read of one file is
    /// intercepted (anything with a pipe/redirect/glob/second-path/partial-read
    /// verb runs untouched). Default **on**; master-gated by `read_advisor` and
    /// the E1 hard block. Off ⇒ a zero overlay delta (the Bash matcher isn't
    /// installed) and the bypass canary scores shell reads as before.
    pub read_advisor_shell: bool,
    /// V17 Phase C: first-read tier — the size (in KiB) at or above which a
    /// *first* whole-file `Read` of a **non-code** file (log, lockfile, generated
    /// JSON, data dump — no parsed symbols) is answered with the cached
    /// local-model digest + a head/tail sample instead of the full content. A
    /// separate opt-in *within* the advisor: `0` = off (the default). Only fires
    /// when a digest is already cached for the current content hash — a miss
    /// enqueues one and passes, so protection begins on the next (cross-session)
    /// encounter. A deliberate slice (`offset`/`limit`) always passes. Proposed
    /// starting value when enabled: 256.
    pub read_advisor_first_read_kb: u32,

    // --- V17 Phase E: lean tool surface ---
    /// Hide the cold-tail `graph_*` tools (`graph_cycles`, `graph_dead_exports`,
    /// `graph_struct_search`, `graph_path`, `graph_architecture`) from the tool
    /// surface advertised to the cloud session and the offload worker, trimming
    /// the tools block that's cache-written once per session. Advertisement-only:
    /// the hidden tools still ANSWER if an agent calls them by name — they're
    /// just not offered. Default off.
    pub lean_tools: bool,

    // --- V11 Phase F: local-model context digests ---
    /// For files with no useful outline (docs/configs/long scripts), have the
    /// **local** offload backend write a 3-line semantic digest, cached in
    /// `graph.db`. Off by default; needs a ready local offload backend. Never
    /// leaves the machine (local-only path).
    pub context_llm_digests: bool,

    // --- V12 Phase E: memory distillation (durable project facts) ---
    /// Distill an idle session's working set + notes into at most 3 durable
    /// `project_fact` rows via the **local-only** offload path before/instead
    /// of letting that knowledge evaporate with the session. Off by default —
    /// needs a ready local offload backend and the prompt is model-dependent
    /// (milestone Decision 3: revisit after real-session validation).
    pub memory_distillation: bool,
    /// Append **pinned** project facts (only pinned — the human-curated tier)
    /// to the launch-time guidance payload (Claude `--append-system-prompt`,
    /// OpenCode's instructions file), so durable knowledge arrives with zero
    /// tool calls. Off by default. Launch-time only: a fact pinned mid-session
    /// applies on the tab's next launch.
    pub promote_pinned_facts: bool,

    // --- V12 Phase F: proactive automation ---
    /// Auto-run the project's configured checks after an edit (`PostToolUse`
    /// hook → `/context/post_edit`) and inject only NEW/worsened diagnostics
    /// as additional context — the agent learns it broke something in the
    /// same turn instead of three turns later. Strictly opt-in — it's a
    /// behavior hook, same posture as `read_advisor`. Off by default; needs
    /// `checks` non-empty to do anything.
    pub auto_check: bool,
    /// Debounce window (seconds): edits inside this window since the last
    /// triggered run are coalesced (no new run); the run then covers
    /// everything the burst touched, since checks run against the file system
    /// state, not a specific edit.
    pub auto_check_debounce_s: u32,
    /// Minimum DIRECT inbound call count (`graph_callers`'s count) an edited
    /// file's symbol must have before the same hook appends a two-line
    /// blast-radius note (6b) — the moments an agent most needs impact
    /// analysis are exactly the moments it doesn't think to ask for it.
    pub auto_impact_min_dependents: u32,
    /// Re-run `dead_exports`/`import_cycles` after every completed index pass
    /// (bounded, read-only on the warm index — cheap) and badge the Analyses
    /// section when the counts changed. On by default — unlike the other
    /// Phase F toggles this doesn't change agent behavior, only a UI badge.
    pub analyses_auto: bool,
    /// V15 Feature 1: hop bound for `graph_path` shortest-path tracing — how far
    /// the BFS explores before giving up. Clamped 1–32 at the tool boundary.
    pub path_max_hops: u32,
    /// V15 Feature 2: max subsystems (file communities) `graph_architecture`
    /// reports, biggest first.
    pub arch_max_communities: u32,
    /// V15 Feature 2: ignore communities smaller than this in the architecture
    /// report (singletons/pairs are noise, not subsystems).
    pub arch_min_community_size: u32,
    /// V15 Feature 4 (STRETCH): master toggle for the **Graph view** live
    /// force-graph (the Tool Activity tab's "Graph view" section — formerly
    /// its own reserved tab, retired in schema v26). Off by default — it's
    /// the human-facing visual, not on any agent path.
    pub graph_viz: bool,
    /// V15 Feature 4: cap on the rendered subgraph node count so large repos
    /// stay smooth (the view is bounded orientation, never the whole graph).
    pub graph_viz_max_nodes: u32,
    /// Graph View tuning (all multipliers on the built-in behavior, `1.0` =
    /// unchanged): file-node radius. One size doesn't fit every repo — a
    /// dense monorepo wants smaller nodes/wider spacing than a 50-file tool.
    pub graph_viz_node_scale: f32,
    /// Directory-cluster size multiplier (the leash radius files orbit their
    /// folder anchor at — bigger = looser, larger folder discs).
    pub graph_viz_dir_scale: f32,
    /// Edge line-width multiplier (ambient, emphasized, highlighted and the
    /// aggregate folder↔folder edges all scale together).
    pub graph_viz_edge_width: f32,
    /// Spacing multiplier between FILE nodes (connected-pair rest length and
    /// the matching node↔node repulsion).
    pub graph_viz_node_spacing: f32,
    /// Spacing multiplier between DIRECTORY clusters (anchor↔anchor rest
    /// length and the matching cluster repulsion).
    pub graph_viz_cluster_spacing: f32,
    /// Directory-clustering tightness multiplier (the strength of the spring
    /// leashing each file to its folder anchor — higher = files hug their
    /// folder harder, lower = topology wins over directory grouping).
    pub graph_viz_cluster_strength: f32,
    /// Edge colors (`#rrggbb`): call edges and import edges. The remaining
    /// hues (highlight pulses, subsystem palette) stay built-in.
    pub graph_viz_color_call: String,
    pub graph_viz_color_import: String,
    /// Segment colors for the Code Intelligence tab's "This session"
    /// stacked-bar chart (`#rrggbb`). Edited in-place by clicking the chart's
    /// legend swatches; defaults match the original hard-coded palette.
    pub usage_color_in: String,
    pub usage_color_cache: String,
    pub usage_color_out: String,
    pub usage_color_tool: String,
    /// V16 Feature 8: the cache-write segment's color — new alongside the
    /// four above now that `cache_make` is plotted as its own segment.
    pub usage_color_write: String,
    /// **Per-lane colors, keyed by the harness's declared `TurnOrigin` id**
    /// (V40 Phase I, issue #107 item 4; schema 36 -> 37).
    ///
    /// Was a fixed pair, `usage_color_session` / `usage_color_agent` — two
    /// settings fields named after one harness's two lanes. `TurnOrigin` has
    /// been declared data since Phase D, so a harness with a third lane got the
    /// second lane's swatch (the legend `laneSeg` clamped) and no donut fill
    /// rule of its own, which painted it SVG-default black. Sparse on purpose:
    /// **absent means "the palette slot for this lane's declared position"**,
    /// so a harness's lanes are colored the day they are declared and only a
    /// lane the user actually picked a color for carries a row here.
    ///
    /// The v36 -> v37 step moves the two old fields in under the keys
    /// `"session"` and `"agent"`, so a user's picked colors survive.
    pub usage_lane_colors: std::collections::BTreeMap<String, String>,
}

impl std::fmt::Debug for GraphSettings {
    /// Hand-rolled since V33 Phase E purely to redact
    /// [`embedding_auth_token`](GraphSettings::embedding_auth_token); every
    /// other field prints exactly as the derive would.
    ///
    /// The hazard a hand-rolled `Debug` on a struct this wide introduces is
    /// *silent omission* — a field added later, never listed here, simply
    /// vanishes from every debug line. That is what
    /// `graph_settings_debug_covers_every_field_and_redacts_the_token` pins:
    /// it walks the serialized key set and requires each name to appear below.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphSettings")
            .field("enabled", &self.enabled)
            .field("languages", &self.languages)
            .field("ignore", &self.ignore)
            .field("index_docs", &self.index_docs)
            .field("max_file_bytes", &self.max_file_bytes)
            .field("watch_debounce_ms", &self.watch_debounce_ms)
            .field("max_rows_per_query", &self.max_rows_per_query)
            .field("max_snippet_bytes", &self.max_snippet_bytes)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("db_subdir", &self.db_subdir)
            .field("allow_remote_worker_access", &self.allow_remote_worker_access)
            .field("semantic_search", &self.semantic_search)
            .field("embedding_endpoint", &self.embedding_endpoint)
            // The one reason this impl exists.
            .field(
                "embedding_auth_token",
                &if self.embedding_auth_token.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("embedding_model", &self.embedding_model)
            .field("embedding_dims", &self.embedding_dims)
            .field("embed_code_bodies", &self.embed_code_bodies)
            .field("embedding_batch", &self.embedding_batch)
            .field("embedding_max_tokens", &self.embedding_max_tokens)
            .field("semantic_code_max_chunks", &self.semantic_code_max_chunks)
            .field("context_injection", &self.context_injection)
            .field("context_per_file_chars", &self.context_per_file_chars)
            .field("context_turn_budget_chars", &self.context_turn_budget_chars)
            .field("context_include_session", &self.context_include_session)
            .field("context_min_score", &self.context_min_score)
            .field("repo_map_budget_chars", &self.repo_map_budget_chars)
            .field("repo_map_on_session_start", &self.repo_map_on_session_start)
            .field("context_dedup_ttl_turns", &self.context_dedup_ttl_turns)
            .field("compaction_context", &self.compaction_context)
            .field("read_advisor", &self.read_advisor)
            .field("read_advisor_min_lines", &self.read_advisor_min_lines)
            .field("read_advisor_mode", &self.read_advisor_mode)
            .field("read_advisor_ttl_turns", &self.read_advisor_ttl_turns)
            .field("read_advisor_diffs", &self.read_advisor_diffs)
            .field("read_advisor_shell", &self.read_advisor_shell)
            .field("read_advisor_first_read_kb", &self.read_advisor_first_read_kb)
            .field("lean_tools", &self.lean_tools)
            .field("context_llm_digests", &self.context_llm_digests)
            .field("memory_distillation", &self.memory_distillation)
            .field("promote_pinned_facts", &self.promote_pinned_facts)
            .field("auto_check", &self.auto_check)
            .field("auto_check_debounce_s", &self.auto_check_debounce_s)
            .field(
                "auto_impact_min_dependents",
                &self.auto_impact_min_dependents,
            )
            .field("analyses_auto", &self.analyses_auto)
            .field("path_max_hops", &self.path_max_hops)
            .field("arch_max_communities", &self.arch_max_communities)
            .field("arch_min_community_size", &self.arch_min_community_size)
            .field("graph_viz", &self.graph_viz)
            .field("graph_viz_max_nodes", &self.graph_viz_max_nodes)
            .field("graph_viz_node_scale", &self.graph_viz_node_scale)
            .field("graph_viz_dir_scale", &self.graph_viz_dir_scale)
            .field("graph_viz_edge_width", &self.graph_viz_edge_width)
            .field("graph_viz_node_spacing", &self.graph_viz_node_spacing)
            .field("graph_viz_cluster_spacing", &self.graph_viz_cluster_spacing)
            .field(
                "graph_viz_cluster_strength",
                &self.graph_viz_cluster_strength,
            )
            .field("graph_viz_color_call", &self.graph_viz_color_call)
            .field("graph_viz_color_import", &self.graph_viz_color_import)
            .field("usage_color_in", &self.usage_color_in)
            .field("usage_color_cache", &self.usage_color_cache)
            .field("usage_color_out", &self.usage_color_out)
            .field("usage_color_tool", &self.usage_color_tool)
            .field("usage_color_write", &self.usage_color_write)
            .field("usage_lane_colors", &self.usage_lane_colors)
            .finish()
    }
}

impl GraphSettings {
    /// The per-project db subdirectory, falling back to `.cimp` when unset.
    /// Single source of truth so the service and the MCP child can't open
    /// different paths.
    pub fn effective_db_subdir(&self) -> String {
        let s = self.db_subdir.trim();
        if s.is_empty() {
            ".cimp".to_string()
        } else {
            s.to_string()
        }
    }
}

impl Default for GraphSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            // Tier-1 code languages are on by default; markup/data languages
            // (html/css/json) stay opt-in to keep a fresh index lean (V9-02).
            languages: [
                "rust",
                "typescript",
                "javascript",
                "python",
                "markdown",
                "go",
                "java",
                "c",
                "cpp",
                "csharp",
                "php",
                "bash",
                "scala",
                "ocaml",
                "ruby",
                "haskell",
                "kotlin",
                "swift",
                "sql",
                "erlang",
                "r",
                "perl",
                "ada",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            ignore: Vec::new(),
            index_docs: true,
            max_file_bytes: 1_048_576, // 1 MiB
            watch_debounce_ms: 300,
            max_rows_per_query: 100,
            max_snippet_bytes: 2_000,
            max_body_bytes: 16_384,
            db_subdir: ".cimp".to_string(),
            allow_remote_worker_access: false,
            semantic_search: false,
            embedding_endpoint: String::new(),
            embedding_auth_token: String::new(),
            embedding_model: String::new(),
            embedding_dims: 0,
            embed_code_bodies: false,
            embedding_batch: 32,
            embedding_max_tokens: 0,
            semantic_code_max_chunks: 20_000,
            context_injection: false,
            context_per_file_chars: 800,
            context_turn_budget_chars: 6_000,
            context_include_session: true,
            context_min_score: 3,
            repo_map_budget_chars: 4_000,
            repo_map_on_session_start: false,
            context_dedup_ttl_turns: 10,
            compaction_context: true,
            read_advisor: false,
            read_advisor_min_lines: 300,
            read_advisor_mode: "advise".to_string(),
            read_advisor_ttl_turns: 0,
            read_advisor_diffs: true,
            read_advisor_shell: true,
            read_advisor_first_read_kb: 0,
            lean_tools: false,
            context_llm_digests: false,
            memory_distillation: false,
            promote_pinned_facts: false,
            auto_check: false,
            auto_check_debounce_s: 5,
            auto_impact_min_dependents: 10,
            analyses_auto: true,
            path_max_hops: 8,
            arch_max_communities: 12,
            arch_min_community_size: 3,
            graph_viz: false,
            graph_viz_max_nodes: 1500,
            graph_viz_node_scale: 1.0,
            graph_viz_dir_scale: 1.0,
            graph_viz_edge_width: 1.0,
            graph_viz_node_spacing: 1.0,
            graph_viz_cluster_spacing: 1.0,
            graph_viz_cluster_strength: 1.0,
            graph_viz_color_call: "#4fb3ff".to_string(),
            graph_viz_color_import: "#ff8a3d".to_string(),
            usage_color_in: "#58a6ff".to_string(),
            usage_color_cache: "#d2a8ff".to_string(),
            usage_color_out: "#3fb950".to_string(),
            usage_color_tool: "#f0c674".to_string(),
            usage_color_write: "#e3738d".to_string(),
            usage_lane_colors: std::collections::BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_settings_debug_covers_every_field_and_redacts_the_token() {
        // V33 Phase E. Two properties in one test because they share a cause:
        // `GraphSettings` had a DERIVED `Debug` and a doc saying "No secrets
        // here" until it gained `embedding_auth_token`.
        let g = GraphSettings {
            embedding_auth_token: "sk-embed-secret".into(),
            ..GraphSettings::default()
        };
        let dbg = format!("{g:?}");
        assert!(
            !dbg.contains("sk-embed-secret"),
            "the embedding bearer token reached a Debug line: {dbg}"
        );
        assert!(dbg.contains("embedding_auth_token: \"<redacted>\""), "{dbg}");
        assert!(format!("{:?}", GraphSettings::default())
            .contains("embedding_auth_token: \"<empty>\""));

        // The cost of hand-rolling `Debug` on a struct this wide is that a
        // field added later is silently dropped from every debug line. Walk the
        // serialized key set — the same names, no serde renames in this block —
        // and require each to appear.
        let json = serde_json::to_value(&g).expect("GraphSettings serializes");
        for key in json.as_object().expect("a JSON object").keys() {
            assert!(
                dbg.contains(&format!("{key}:")),
                "the hand-rolled GraphSettings Debug omits `{key}` — add a \
                 `.field(\"{key}\", &self.{key})` line"
            );
        }
    }
}

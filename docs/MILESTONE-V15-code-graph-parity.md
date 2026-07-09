# V15 — Code Graph Parity (path tracing · architecture map · edge confidence · graph viz)

**Status:** IMPLEMENTED (2026-07-09). Phases A–E landed on `develop`: edge
confidence (schema 3→4), `graph_path`, `graph_architecture`, and the stretch
Graph View tab, with settings, tests, and the Code Intelligence UI sections.
The `Ambiguous` class is applied at query time (the only place a name's global
candidate count is visible); parse-time confidence is `Extracted` (same-file /
structural) vs `Inferred` (cross-file), never a silent upgrade. Architecture is
computed on demand (no warm-index cache in V1).
**Builds on:** V9-01 code knowledge graph (`docs/completedMilestones/MILESTONE-V9-01-code-knowledge-graph.md`), V9-02 multi-language `tags.scm` engine, V10 Context Engine (the "Code Intelligence" tab + Analyses section). Reuses the warm per-project `GraphIndex` (`graph/index.rs`), the `edge`/`ref` relations, and the existing `graph_*` MCP tool surface (`graph/mcp.rs`).

## Why

Benchmarking cImp's code intelligence against **Graphify** (an open-source multimodal knowledge-graph skill) surfaced four things it does that our graph doesn't — **and all four are pure code-graph features**, so they belong on top of the graph we already have. We are **deliberately not** copying Graphify's multimodal ingestion (docs, PDFs, diagrams, video). Mixing prose and pixels into the code graph dilutes exactly what makes ours precise. Non-code ingestion is captured as a **separate, decoupled future feature** at the end of this doc — a different store, a different tab, a different problem.

Where cImp is already **ahead** of Graphify and stays there: real embedding search (Graphify has none), Datalog call/impact/dead-export/cycle analyses, live sub-second fs-watch incrementalism, and the whole agentic inner loop (context injection, session memory, advisors). V15 closes the four code-only gaps so the structural graph is at least as expressive as Graphify's for code:

1. **Arbitrary relationship tracing** — Graphify's `shortest_path "A" "B"` answers "how does X reach Y?" across the whole graph. We only trace *transitive call chains in one direction* (`graph_transitive`). We can't answer "what is the path from `RateLimiter` to `DatabasePool`?" through mixed call/import/contains edges.
2. **Architecture overview** — Graphify surfaces **"god nodes"** (highest-degree concepts) and **communities/subsystems** (Leiden clustering) so a newcomer sees the shape of the system at a glance. Our `graph_repo_map` ranks call-central *files* but has no hub/subsystem view.
3. **Edge confidence** — Graphify tags every edge `EXTRACTED` (explicit in source) vs `INFERRED` (resolved heuristically) vs `AMBIGUOUS`. Our cross-file references are **name-keyed guesses** (`ref.resolved_id` is optional) but we present them with the same authority as an exact resolution. The agent can't tell a certain caller from a probable one.
4. **Graph visualization** — Graphify ships an interactive force-directed `graph.html`; our Code Intelligence tab is text/tables only. A *visual* of the code graph is a genuine, code-scoped gap (Feature 4 is a **stretch**, gated behind 1–3). We skip Graphify's *exports* (Mermaid/GraphML/Neo4j) deliberately — those exist to hand the graph to external tools, but our graph already lives as a queryable `graph.db` and the in-app view covers the human need; see Non-goals.

Everything stays **local + per-project** (`.cimp/graph.db`, git-ignored), opt-in where it changes behavior, matching the existing graph posture.

---

## Feature 1 — Arbitrary relationship tracing (`graph_path`)

### Goal
Answer "how is A connected to B?" for any two code entities, returning the **shortest ordered path** through the graph's edges — the query Graphify markets as `shortest_path`, and the one that turns a pile of symbols into a navigable map ("auth handler → service → repository → connection pool").

### Backend (`graph/index.rs` — new query)
`fn shortest_path(&self, from: &str, to: &str, kinds: EdgeKindMask, max_hops: usize) -> AppResult<Option<PathHit>>`

- Nodes are existing `symbol` rows (and, when `from`/`to` resolve to a file, the `file` node). `from`/`to` accept a symbol name or `file:line`, resolved with the same logic `graph_snippet` already uses.
- Edges: a **unified, direction-aware view** over the `edge` relation — `Call`, `Import`, `Contains` (exclude `Documents` by default; it's doc-linkage, not code structure). A `kinds` mask lets a caller restrict to e.g. calls-only.
- **BFS with parent-pointer path reconstruction**, reusing the pattern `transitive` already uses (`graph/index.rs:727` — a Rust-side BFS over a name-level adjacency built from a single scan, *not* Datalog recursion), extended two ways: the adjacency spans **multiple edge kinds** (Call/Import/Contains, per the `kinds` mask) instead of calls-only, and it records **parent pointers** so the first time `to` is dequeued we can walk back to `from` for the ordered path. Fewest-hop falls out of BFS order. Bounded by `path_max_hops` (default 8) so pathological repos can't blow it up. (Alternative worth a spike: Cozo ships built-in `ShortestPathBFS`/`ShortestPathDijkstra` fixed algorithms — if a Datalog-native path is cleaner than extending the Rust adjacency, prefer it; decide in Phase B.) Returns the ordered node list **plus the edge kind between each pair** and each edge's **confidence** (Feature 3), so the agent sees *how* A reaches B, not just that it does.
- Direction: default treats edges as directed (real call/import flow). A `symmetric: bool` arg falls back to an undirected walk for "are these two things related at all?" questions.

### Tool (`graph/mcp.rs::tool_specs`, both consumers)
- `graph_path { from, to, kinds?, max_hops?, symmetric? }` → `{ path: [{ node, file, line, kind, edge_to_next, confidence }], hops }` or an empty result with a reason (`no path within N hops`, `endpoint not found`). Token-bounded like every other tool; recorded in the activity ring.

Guidance addendum nudges the agent to reach for `graph_path` on "how does X talk to Y / what connects X and Y" questions instead of grepping.

### UI — Architecture section (shared with Feature 2)
A **"Trace path"** box: two symbol/file inputs (autocomplete off the `symbol` relation) → renders the resulting chain as a breadcrumb (`A → calls → B → imports → C`), each hop clickable to the file, each hop badged with its confidence.

### Edge cases
- No path → say so plainly; don't fabricate a weak link.
- Multiple equal-length shortest paths → return one, note `+N other paths of equal length` (deterministic tie-break by node id so results are stable across runs).

---

## Feature 2 — Architecture overview (god nodes · subsystems · surprising edges)

### Goal
A once-per-project, at-a-glance map of the system's shape — the newcomer/orientation view. **Code-only, no LLM, no embeddings required** (topology alone), so it works on any indexed repo regardless of the embedder.

### Backend (`graph/index.rs` — new queries + a small Rust pass)
1. **God nodes (hubs)** — highest-degree symbols and files by combined inbound `Call`+`Import` degree. Pure Datalog aggregate over `edge`; cheap. This is the "everything flows through these" list.
2. **Subsystems (communities)** — cluster the file-level `Import`+`Call` graph into cohesive groups. **No Leiden dependency**: run **label propagation** (a handful of synchronous passes) in Rust over the edge set loaded from the warm index — deterministic with a fixed seed order (id-sorted), bounded iterations, `O(edges)` per pass. Each community gets a **name with zero LLM**: the longest common path prefix of its files (`src/graph/…`), falling back to its top god-node's symbol name. Report the top `arch_max_communities` by size, each with its member count, representative files, and internal god node.
3. **Surprising edges** — edges that **cross** community boundaries (an unexpected dependency between two otherwise-separate subsystems). Cheap once communities are labeled: any `edge` whose endpoints are in different communities, ranked by how rare cross-links are between that pair. This is Graphify's "surprising connections," and it's the most useful signal for spotting accidental coupling.

Results are cached on the warm index and recomputed after an index pass **only when `analyses_auto` is on** (it already re-runs dead-exports/cycles there, `schema.rs:1150`) — architecture is the same class of derived report.

### Tools
- `graph_architecture {}` → `{ god_nodes: [...], subsystems: [{ name, size, files, hub }], surprising: [{ from, to, kind, from_subsystem, to_subsystem }] }`, all bounded by `max_rows_per_query`.
- `graph_repo_map` (existing) is **left as-is** — it's the budget-packed *injection* map; `graph_architecture` is the richer *human/analysis* view. They share the god-node computation internally.

### UI — Architecture section (new subsection of the Code Intelligence tab)
- **God nodes** table (symbol/file · degree · kind), click → file.
- **Subsystems** list (name · size · hub · sample files), collapsible.
- **Surprising connections** table (from-subsystem ✗ to-subsystem, the crossing edge), captioned as advisory ("candidate accidental coupling — verify before acting").
- Rides the existing `graph-status` event; a "Recompute" button runs it on demand on the warm index.

### Edge cases
- Tiny repos → one community; say "single cohesive module," don't invent structure.
- Label propagation is non-hierarchical and approximate — **label the section "heuristic," not authoritative**, same honesty posture as dead-exports. A future `arch_resolution` knob can expose granularity if users ask; V1 ships one fixed resolution.

---

## Feature 3 — Edge confidence (EXTRACTED · INFERRED · AMBIGUOUS)

### Goal
Make the graph **honest about what it knows**. Today a cross-file caller resolved by bare name looks identical to a same-file call the parser saw directly. Tagging each edge/reference with a confidence lets the agent (and the impact/callers tools) weight certain facts over probable ones — and it directly improves `graph_impact`, `graph_callers`, and Feature 1's paths.

### Data model (`graph/model.rs` + `graph/schema.rs`, **schema bump 3 → 4**)
Add a `confidence` field to the `ref` and `edge` relations:
- `Extracted` — resolution the parser is certain of: same-file definition, or a cross-file target reached through an **explicit, matched import path** (not just a name collision). One unambiguous candidate.
- `Inferred` — name-keyed cross-file resolution with **exactly one** candidate but no explicit import proof. Our current default cross-file behavior — now labeled as the guess it is.
- `Ambiguous` — the name resolves to **>1** candidate symbol; we picked one (or none). The agent should treat callers/callees here as a superset.

`GRAPH_SCHEMA_VERSION` bumps to `4` (Cozo has no cheap ALTER; the version bump triggers the existing full-rebuild-from-source path — `schema.rs`). Populated where references are resolved today:
- **Bespoke walkers** (`parse_rust`/`parse_js_ts`/`parse_python`, `graph/builder.rs`): same-file / imported-path hits → `Extracted`; single cross-file name hit → `Inferred`; multi-candidate → `Ambiguous`.
- **Generic `tags.scm` engine** (`graph/tags.rs`): span-attributed calls with a single resolved target → `Inferred`; multi-candidate → `Ambiguous`; local (same-file) → `Extracted`. Languages the tags can't resolve cross-file stay `Inferred`/`Ambiguous` honestly.

### Surfacing (every consumer)
- Add `confidence` to the result rows of `graph_callers`, `graph_callees`, `graph_references`, `graph_transitive`, `graph_impact`, and `graph_path`. The agent sees `caller: foo() [inferred]`.
- `graph_impact` gains an optional `min_confidence` filter (default: include all, but the summary line reports the split: "12 dependents (7 extracted, 5 inferred)"), so blast-radius can be read conservatively.
- UI: confidence badges on every callers/impact/path row; a one-line legend.

### Edge cases
- Don't over-promise `Extracted`: only claim it with real evidence (same file or a matched import). When unsure, `Inferred` is the safe, honest default — never silently upgrade.
- Re-index cost: the field is computed during the parse we already do; no extra pass. Only the schema bump forces the one-time rebuild.

---

## Feature 4 — Live graph visualization tab (2D/3D + activity) (STRETCH)

### Goal
An interactive picture of the code graph that **animates as the agent works**. Graphify ships a static `graph.html`; ours goes further with a **live-activity layer** that pulses nodes as they're read/edited/queried, which is the on-brand, demo-able bit (you watch the agentic inner loop navigate the codebase). Gated behind 1–3; ships only if they land cleanly.

### Its own tab (not a Code Intelligence section)
A **dedicated, reserved app-rendered tab** — separate from the text/table-oriented Code Intelligence tab, because it's a full-surface animated canvas with its own render loop, not another analysis panel. Follow the existing reserved-tab pattern (a stable `TabId` + a `*View.svelte`, wired the same way as the graph monitor — `src/lib/tabs/types.ts`, `state/manager.rs`, `settings/persistence.rs`). Working name: **"Graph View"** (label + icon only; keep the internal id stable). Read-only, no PTY.

### Rendering (self-contained, offline)
- **2D and 3D**, user-toggled (`2d | 3d`). Reuse [`3d-force-graph`](https://github.com/vasturiano/3d-force-graph) (three.js + d3-force-3d) for 3D and its 2D sibling / a shared force layout; both bundle as **offline** deps (no external network — matches the self-contained constraint). WebView2 gives hardware-accelerated WebGL, so this runs natively inside the app webview.
- Node **color = subsystem** (Feature 2), node **size = god-node degree** (Feature 2); edge **color = kind** (call/import/contains) and edge **dash pattern = confidence** (Feature 3) — two separate channels so they don't collide. The visual is a free consumer of the other three features.
- **Bounded subgraph, always.** Seed from top-N hubs + the neighborhood of a focus node / the currently-active nodes; **never** the whole graph for large repos (it's orientation + show, not a 500k-node hairball). Level-of-detail: cap rendered node count, expand a node's neighborhood on click, collapse on demand.

### Live-activity layer (the differentiator)
- Subscribe the tab to the **same event bus** that already feeds the activity ring + session memory. Two real event streams, no new capture: (a) `graph_*` tool calls, which land in `graph::activity` with `source` = **`claude`** (the cloud session) or **`offload`** (the local worker) — those are the *only* two source values the ring carries today (`activity.rs:24`); (b) `Read`/`Edit` events derived from the OOB transcript tail (V10 session memory), tagged with the acting agent (claude/opencode/offload). There is **no "human" source** — every graph/file event originates from an agent, so don't invent a user-pulse color.
- On each event, **pulse the corresponding node** (a brief glow/scale), colored by source (**cloud agent** — claude/opencode — vs **local offload worker**), and briefly highlight the edge traversed when it's a `graph_path`/`callers`/`callees` result. The effect: you *see* the agent walk `find_symbol → callers → the file it edits`, live.
- Fed by a new bounded IPC snapshot `{nodes, edges}` (reuse `graph.ts` plumbing) plus the existing `graph-status`/activity event stream for the animation deltas.

### Mouse interaction (explicit requirement)
Standard 3D camera controls, native to the webview (this is an **app-rendered** tab, not a PTY/AI tab, so no mouse-forwarding quirk applies — the wheel/drag reach the canvas directly):
- **Drag to orbit/rotate** the camera around the graph, **scroll wheel to zoom** (dolly), **right-drag (or two-finger) to pan**. `3d-force-graph` ships three.js orbit/trackball controls for exactly this — enable and tune damping/zoom speed rather than hand-rolling.
- **Click a node** to focus it (expand its neighborhood / re-center) and **hover** for a label tooltip (symbol · file · kind · degree). In 2D mode the same drag-pan + wheel-zoom apply (no rotation).
- Momentum/damping on so rotation feels smooth; a "reset view" / re-fit control returns to the framed default.

### Responsive sizing (explicit requirement)
The canvas **adapts to the tab's size and every resize** — it is not a fixed-size viewport:
- A `ResizeObserver` on the tab container drives `renderer.setSize(w, h)` + `camera.aspect = w/h; camera.updateProjectionMatrix()` (and the 2D layout's viewport) on **every** resize — window resize, pane split/drag, tab dock/undock, DPI/monitor change. Use the element's content-box size, not `window`.
- Handle **devicePixelRatio** for crisp rendering on high-DPI displays; clamp DPR on very large canvases to protect the frame rate.
- Debounce the resize→re-layout so a drag doesn't thrash the force simulation; re-center/re-fit the graph to the new bounds after a resize settles.
- Degenerate sizes are safe: zero/nil dimensions (tab hidden or mid-animation) pause rather than throw.

### Cost discipline (don't let a spinning scene burn GPU)
- **Pause the render loop and the force simulation when the tab isn't visible** (WebView2 throttles background timers anyway; make it explicit — stop `requestAnimationFrame` on hide, resume on show).
- Idle the simulation once it settles (`cooldownTicks`/`alpha` decay) so a static graph isn't re-integrating physics forever; kick it back alive only on activity or interaction.

### Legend (explicit requirement)
An always-visible, compact overlay (corner panel, collapsible) so the colors mean something without guessing:
- **Node colors → subsystems.** One swatch per rendered community (Feature 2) with its derived name (`src/graph/…` or its hub symbol), plus keys for the other encodings in play: **node size = god-node degree**; **edge color = kind** (call/import/contains); **edge dash = confidence** (Feature 3: solid = `Extracted`, dashed = `Inferred`, dotted = `Ambiguous`).
- **Activity sources → pulse colors.** The live-activity legend: a swatch each for **cloud agent (claude/opencode)** and **local offload worker** — the two real event sources — matching the node-pulse colors, so a glowing node is legible at a glance ("Opus just read this"). No human/user swatch (there is no human-origin graph event).
- Keep it honest and self-updating: it lists only the communities/sources actually present in the current view, and updates when the subgraph or focus changes. Colorblind-safe palette (don't rely on hue alone — pair with the size/edge-style channels already in use).

### Why stretch
Viz is the lowest-leverage of the four for the *agent* (agents don't read pictures; humans do). It earns its place only after path-tracing, architecture, and confidence — the features the agent uses — are solid. Within Feature 4, prioritize the **live-activity layer** (the genuinely useful/identity-reinforcing part) over static 3D eye-candy.

---

## Non-goals (explicit)

- **No multimodal ingestion** into the code graph — no PDFs, images, diagrams, video, Office/Google docs. Prose and pixels do not belong in a structural code graph. (See "Future feature" below.)
- **No cross-tier "one graph"** merging app code + SQL schema + infra + docs. Graphify's headline "single graph from table → handler → component" is explicitly rejected — it's the coupling we're avoiding.
- **No embeddings requirement** for Features 1–3 — they're topology/structure only and must work with the embedder off.
- **No LLM in the architecture pass** — clustering and naming are deterministic and local.
- **No graph export to external formats** (Mermaid/GraphML/Neo4j/Obsidian). The graph already exists as a queryable `graph.db`; export only serves handing it to third-party tools, which is Graphify's ecosystem play, not ours. Feature 4 covers the human "see the graph" need in-app.

---

## Settings (`GraphSettings`, new fields — `settings/schema.rs`)

- `path_max_hops: u32` (default 8) — hop bound for `graph_path`.
- `arch_max_communities: u32` (default 12) — subsystems reported by `graph_architecture`.
- `arch_min_community_size: u32` (default 3) — ignore singleton/pair clusters in the report.
- Edge confidence has **no toggle** — it's always computed and always surfaced (it's a correctness/honesty property, not a feature flag).
- Feature 4 (if built): `graph_viz: bool` (default false) master toggle for the **Graph View** tab (2D/3D + live activity). `graph_viz_max_nodes: u32` (default ~1500) caps the rendered subgraph so large repos stay smooth.

---

## Phasing

| Phase | Scope | Notes |
|---|---|---|
| **A. Edge confidence** | `confidence` on `ref`/`edge`, schema bump 3→4, populate in bespoke walkers + `tags.scm`, surface in all consuming tools + UI badges | Do first — it's a data-model change (forces one rebuild) and it improves Features 1 & 2's output. Cross-language surface. |
| **B. Path tracing** | `shortest_path` (BFS w/ parent pointers, reuses `transitive`) + `graph_path` tool + Trace-path UI box | Multi-edge-kind adjacency; consumes A's confidence field |
| **C. Architecture overview** | god-node aggregate, label-propagation communities (Rust pass), surprising-edges query, `graph_architecture` tool, Architecture UI section, hook into `analyses_auto` | The orientation win; no LLM, no embedder |
| **D. Graph View tab (STRETCH)** | dedicated reserved tab, 2D/3D force graph (`3d-force-graph`, offline), live-activity pulse off the existing event bus, `ResizeObserver`-driven responsive canvas, pause-when-hidden, bounded subgraph, `graph_viz` gate | Only if A–C land clean; lowest agent-leverage but the live layer is the demo/identity win |
| **E. Docs/settings/tests** | README/FEATURES/MAINTENANCE, settings UI, guidance addenda, unit + integration tests (path correctness, community determinism, confidence classification) | Per repo convention |

Suggested order **A → B → C → (D) → E**: land the honesty layer first (it touches the schema), then the two agent-facing features that build on it, then the human-facing visual, then docs.

## Decisions — RESOLVED

1. **Code-only scope** — ✔ No multimodal, no cross-tier merge (user directive). Non-code ingestion is a separate future feature, not part of Code Intelligence.
2. **Community algorithm** — **label propagation, deterministic, no external Leiden crate.** ✔ Approximate + fast + dependency-free beats a heavy exact clustering lib for an orientation view. `arch_resolution` deferred until asked for.
3. **Confidence is always-on** — ✔ Not a toggle; it's a correctness property. Default class when unsure is `Inferred` (honest), never a silent `Extracted`.
4. **Viz is stretch, in its own tab** — ✔ Agents use 1–3; humans use 4. Gate it behind the rest. The **Graph View is a dedicated reserved tab** (not a Code Intelligence section) — it's an animated full-surface canvas, and its **live-activity layer** (nodes pulsing as the agent reads/edits/queries, off the existing event bus) is the part worth prioritizing over static 3D. Canvas is **fully responsive** (`ResizeObserver` → renderer/camera resize on every resize + DPR-aware) and pauses when hidden.
5. **Numbering** — **V15.** ✔ Continues the post-V10 "Code Intelligence" line (V11–V14 already shipped as their own milestones). V9-03 was rejected: V9-01/V9-02 are closed in `completedMilestones/`, so inserting a V9-03 after V10–V14 exist would read as out-of-order.
6. **`graph_architecture` vs. extending `graph_repo_map`** — **separate `graph_architecture` tool.** ✔ `graph_repo_map` stays untouched: it's shipped (V11) and on the injection critical path, char-budgeted, returns a packed markdown digest. `graph_architecture` is on-demand analysis with a different output contract (structured `{god_nodes, subsystems, surprising}`) and a different budget model (`max_rows_per_query`). They share the god-node computation internally, but overloading the injection tool with a mode flag would risk the auto-injected session-start map for no real gain. **Future nicety (not V15):** let `graph_repo_map` optionally borrow subsystem labels to group its file list — an enhancement to repo_map, not a merge of the two tools.

## Cost note

Implementation fan-out (parser edits for confidence across bespoke walkers + `tags.scm`, the label-propagation pass, tool wiring) is mechanical — use **Sonnet/Haiku** for those passes and reserve **Opus** for the confidence-classification design (the `Extracted`/`Inferred`/`Ambiguous` boundary is the one judgment call) and final review. Per the standing agent-cost guidance.

---

## Future feature (deferred — separate milestone when scheduled) — Knowledge Base graph for non-code files

**This is intentionally NOT part of Code Intelligence and must never be merged into the code graph.** It is recorded here only so the boundary is explicit and the idea isn't lost.

### The idea
A **completely separate** graph/index for the files a code graph should ignore: markdown/prose docs, PDFs, diagrams and images, design notes, papers — "everything that is not code." It answers a different question (semantic recall over unstructured knowledge) with different machinery (LLM/embedding extraction, not tree-sitter structure).

### Why it stays separate — the whole point
- **Different data, different retrieval.** Code retrieval is *structural* (calls, imports, exact references, blast radius). Doc/diagram retrieval is *semantic* (meaning, similarity). Fusing them produces a graph that is worse at both — the precise call graph gets noised up with fuzzy prose edges, and the agent can no longer trust "callers of X" to be code.
- **Different trust & privacy posture.** Multimodal extraction routes content through an LLM; the code graph is parsed 100% locally. Keeping them apart keeps the code path local-only and auditable.
- **User directive.** The explicit ask: docs/diagrams/other non-code files get their **own** graph, decoupled from code intelligence — and specifically *not* Graphify's "all in one graph."

### Sketch (for the eventual milestone — not committed here)
- **Separate store**: its own DB (`.cimp/kb.db`), separate schema/version, separate rebuild — never shares `graph.db`.
- **Separate tab**: a distinct "Knowledge Base" tab, not a section of Code Intelligence.
- **Separate tool namespace**: `kb_search`, `kb_get`, … — no `graph_*` name overlap, so the agent (and the router gating) treat them as a different capability with its own opt-in and its own remote-access gate.
- **Ingestion**: markdown/prose (chunk + embed — we already embed markdown for code-doc linkage, so the embedding client is reusable), PDFs/images/diagrams via LLM extraction (**local backend preferred**, honest health-gating like semantic search). Video/audio explicitly out of an initial cut.
- **Coupling — none by default.** The only sanctioned link between the two graphs is an *explicit, optional* cross-reference (e.g. a doc that names a symbol), surfaced as a lookup — **never** merged edges in a shared store. Opt-in; off by default.
- **Status**: IDEA / deferred. Graduates to its own milestone (tentatively a later V-series entry) if and when non-code recall is prioritized. Not scheduled.

# Milestone V9-02: Multi-Language Code Graph (generic `tags.scm` extraction engine)

Status: **ALL PHASES DONE (on develop, tests green; not committed).** 22 code
languages + 6 struct-search languages. Extends
[V9-01](MILESTONE-V9-01-code-knowledge-graph.md).

## Implementation status (2026-07-01)

**Done — the generic engine + 11 code languages, all tests green (410 Rust + frontend 0 errors):**

- `graph/tags.rs` — the generic `tags.scm` engine (`parse_with_tags` + `tag_spec`
  + span-based containment/call attribution). Reuses `emit_symbol` + the
  name-based resolver; a broken parse or uncompilable query disables a language
  gracefully (never panics).
- Vendored queries under `src-tauri/queries/<lang>/tags.scm` for: **go, java, c,
  cpp, csharp, php, scala, ocaml, ruby** (from upstream, Ruby's over-broad
  bare-identifier ref pattern trimmed) and **bash, haskell** (hand-authored —
  upstream ships none).
- Grammars landed in `Cargo.toml` (all ABI-compatible with tree-sitter 0.26):
  go 0.25, java 0.23, c 0.24, cpp 0.23, c-sharp 0.23, php 0.24, bash 0.25,
  scala 0.26, ocaml 0.25, ruby 0.23, haskell 0.23, + **html 0.23, css 0.25,
  json 0.24** (registered in `language_for` → `graph_struct_search` works; no
  symbol extraction yet).
- `Lang` enum + `from_path` (full extension sets) + `tag`/`from_tag`;
  `parse_file` dispatch routes the 11 code langs through the engine; markup/data
  are struct-search-only.
- Default `languages` list (schema.rs + types.ts) now enables Tier-1 code langs;
  markup/data stay opt-in.
- Tests: `every_vendored_query_compiles` (the grammar-drift guard — proves all 11
  queries incl. hand-authored bash/haskell compile), plus Go/Ruby/Java fixtures
  asserting symbols + containment + call edges.

**Final shipped matrix — every candidate grammar built against tree-sitter 0.26
(none dropped for ABI):**

- **Full symbol + call graph (22):** rust, typescript, javascript, python
  (existing bespoke walkers) + go, java, c, cpp, csharp, php, bash, scala,
  ocaml, ruby, haskell, kotlin, swift, sql, erlang, r, perl, ada (generic
  engine). All default-on. Swift methods classify as functions (trimmed query);
  SQL is `create_table` only (other `create_*` field shapes irregular); Ada/SQL
  have no call edges; Bash/Haskell/Kotlin/SQL/Perl/Ada tags hand-authored (no
  upstream), Ruby/Swift trimmed, the rest upstream verbatim.
- **Struct-search only (6, opt-in):** html, css, json, yaml, xml, asm — grammar
  registered in `language_for`; no tags query.
- **Dropped:** Lisp (per user).

**Done beyond Phases 1–3:** Settings "Supported" hint updated; default
`languages` list carries all 22 code langs; README + FEATURES + a MAINTENANCE
"how to add a language" section written. No aggregated third-party-licenses file
exists in the repo, so no license-manifest change was needed (grammars are
MIT/Apache). Remaining polish (not blocking): markup/data symbol *anchors*
(HTML ids / CSS selectors / JSON keys), a per-language toggle UI (the field is
free-text CSV today), and migrating the original four onto the generic engine.

## Purpose

The code graph (V9-01) is per-project: it indexes whatever project a cImp tab
opens, not just cImp itself. Today it understands only **Rust, TypeScript,
JavaScript, Python, Markdown** — so a user who opens a Go service, a Java
backend, or a C++ codebase gets an empty graph and none of the `graph_*` tools
work. This milestone broadens language coverage toward the mainstream set users
actually open, then attempts a long tail of exotic languages as grammar
availability allows.

### The architectural problem this fixes

V9-01's spec said symbols/references would come from tree-sitter **`tags.scm`
queries** ("yields definitions and references across a broad language set with
**no per-language Rust code**"). The shipped implementation **diverged**: it
hand-writes a bespoke walker per language —

- `parse_rust` → `def_name_kind` (manual node-kind → `SymbolKind` match) +
  `collect_calls_in(CallSyntax::Rust)`,
- `parse_js_ts` → its own walker + `CallSyntax::Js`,
- `parse_python` → its own walker + `CallSyntax::Python`.

Each new language means a new bespoke Rust walker that knows that grammar's node
kinds. **This does not scale to ~25 languages.** A generic, query-driven
extractor is the only sane path, and it's the one V9-01 originally promised.

### Decisions locked (from discussion, 2026-07-01)

- **Architecture = generic `tags.scm` engine** (not more hand-written walkers,
  not struct-search-only). One extractor driven by a per-language tree-sitter
  tags query produces symbols + call/contains references uniformly.
- **Sequencing = mainstream first, exotic as available.** Ship the languages
  with solid, ABI-current Rust grammar crates; attempt the exotic tail
  (Ada, Erlang, R, Lisp, Perl, Assembly) and **drop any without a usable
  crate** rather than block the build.
- **Spec-first** (this document) before coding, per repo convention.
- **Existing four stay on their bespoke walkers (for now).** The hand-written
  Rust/TS/JS/Python walkers are well-tested and arguably more precise than
  generic tags. The new engine lands **alongside** them; `parse_file`'s dispatch
  gains a generic default branch for new languages. Migrating the original four
  onto the unified engine is an explicit **followup**, not this milestone — it
  keeps regression risk off the languages users already rely on.

## Architecture: the generic tags engine

A new `parse_with_tags(src, file, lang, &LangSpec, fg)` replaces "write a walker"
with "supply a query + a small kind map":

```
LangSpec {
  language:   tree_sitter::Language,   // from the grammar crate
  tags_query: &'static str,            // the grammar's queries/tags.scm (vendored)
  kind_of:    fn(&str) -> SymbolKind,  // capture suffix → SymbolKind (shared default + per-lang overrides)
  call_syntax: Option<CallSyntax>,     // reuse existing call attribution where a tags ref capture is absent
}
```

Extraction algorithm (all reusing the V9-01 `FileGraph` schema — `Symbol`,
`Edge{Contains,Calls,Import}`, `DocChunk` — and the existing **name-based**
reference resolver, so nothing downstream changes):

1. Parse with the grammar. A broken parse degrades to partial symbols, never
   panics (same guarantee as today).
2. Compile the `tags.scm` query once per language (cached). Run it over the tree.
3. **Definitions:** every `@definition.<suffix>` capture → a symbol. The paired
   `@name` capture gives the identifier text + span; the enclosing
   `@definition.*` node gives the full span. `<suffix>` → `SymbolKind` via
   `kind_of` (`function`→Function, `method`→Method, `class`→Class,
   `interface`→Interface, `struct`→Struct, `enum`→Enum, `module`/`namespace`/
   `package`→Module, `constant`→Const, `type`→TypeAlias, `field`→Field,
   `macro`→Macro, `trait`→Trait, else→Other).
4. **Containment:** walk ancestors from each definition to the nearest enclosing
   definition node → `Contains` edge (parent symbol id). This is what
   `graph_outline` and nested-symbol display read.
5. **Calls/references:** `@reference.call` captures inside a function/method body
   → a call reference attributed to the enclosing definition → `Calls` edge
   (name-only; the existing resolver binds names to ids later, exactly as the
   Rust/JS/Py paths already do). For grammars whose `tags.scm` lacks a usable
   call capture, fall back to the existing `collect_calls_in(CallSyntax::…)`
   node walk where a `CallSyntax` is defined; otherwise calls are simply absent
   for that language (defs/outline/struct-search still work).
6. **Doc-comments:** keep the existing proximity logic (leading comment lines
   attach to the next def). `@doc` captures, where a tags file provides them, are
   used preferentially.
7. **Imports:** best-effort, optional per language (a tiny secondary query or
   node list). Not all grammars express imports; absence is non-fatal.

`tags.scm` files are **vendored** under `src-tauri/queries/<lang>/tags.scm`
(sourced from each grammar's upstream repo, license-noted) and embedded via
`include_str!`. The build script already copies sibling assets (themes/palettes);
the queries ride the same mechanism or embed directly.

`graph_struct_search` comes **for free** the moment a grammar is in
`language_for()` — it runs arbitrary user queries and needs no tags file.

## Language matrix (vet each crate for tree-sitter 0.26 ABI 14/15 in Phase 1)

The single hard gate per language: a crates.io grammar that exposes a
`LanguageFn`/`LANGUAGE` compatible with the workspace's `tree-sitter = 0.26`
(ABI 14/15). Older crates pinned to ts ≤0.22 are rejected; a few may need a
vendored build of the grammar's C source. **Risk** = likelihood the crate is
missing/stale/ABI-incompatible.

### Tier 1 — Code (full graph: defs + calls + outline + struct-search)

| Language | Candidate crate | Risk |
|---|---|---|
| Go | `tree-sitter-go` | low |
| Java | `tree-sitter-java` | low |
| C | `tree-sitter-c` | low |
| C++ | `tree-sitter-cpp` | low |
| C# | `tree-sitter-c-sharp` | low |
| PHP | `tree-sitter-php` | low |
| Bash | `tree-sitter-bash` | low |
| Scala | `tree-sitter-scala` | low |
| OCaml ("Caml") | `tree-sitter-ocaml` | low |
| Ruby* | `tree-sitter-ruby` | low |
| Kotlin | `tree-sitter-kotlin` (community) | med |
| Swift | `tree-sitter-swift` (build-time gen, large) | med |
| Haskell | `tree-sitter-haskell` (tags coverage thin) | med |

\*Ruby isn't on the user's list but is trivially in-tier and commonly paired with
the rest; include only if free.

### Tier 2 — Markup / schema (struct-search + best-effort symbol anchors; **no call graph**)

| Language | Candidate crate | Symbol notion | Risk |
|---|---|---|---|
| HTML | `tree-sitter-html` | id/class/tag anchors | low |
| CSS | `tree-sitter-css` | selectors / rules | low |
| SQL | `tree-sitter-sequel` / `-sql` (variants) | table/view/function/proc defs | med |
| XML | `tree-sitter-xml` (community) | element/id anchors | med |

### Tier 3 — Data (struct-search + key indexing; **no calls, no docs**)

| Language | Candidate crate | Symbol notion | Risk |
|---|---|---|---|
| JSON | `tree-sitter-json` | top-level keys | low |
| YAML | `tree-sitter-yaml` (community) | top-level keys/anchors | med |

### Tier 4 — Exotic (attempt; drop any without a usable crate, log + document the drop)

| Language | Candidate crate | Risk |
|---|---|---|
| Erlang | `tree-sitter-erlang` (WhatsApp) | high |
| R | `tree-sitter-r` | high |
| Perl | `tree-sitter-perl` (community) | high |
| Lisp | `-commonlisp` / `-clojure` / `-scheme` (ambiguous — pick per user need) | high |
| Ada | `tree-sitter-ada` | high |
| Assembly | `tree-sitter-asm` (x86) — minimal symbol value | high |

**"all filetypes related to those languages"** → `Lang::from_path` gains the full
extension set per language, e.g. Go `.go`; Java `.java`; C `.c/.h`; C++
`.cc/.cpp/.cxx/.hpp/.hh/.hxx`; C# `.cs`; PHP `.php/.phtml`; Bash
`.sh/.bash/.zsh`; Scala `.scala/.sc`; OCaml `.ml/.mli`; Kotlin `.kt/.kts`; Swift
`.swift`; Haskell `.hs/.lhs`; HTML `.html/.htm`; CSS `.css`; SQL `.sql`; XML
`.xml/.xsd/.xsl/.svg`; JSON `.json/.jsonc`; YAML `.yaml/.yml`; Erlang
`.erl/.hrl`; R `.r/.R`; Perl `.pl/.pm`; Lisp `.lisp/.cl/.el/.clj/.scm`; Ada
`.adb/.ads`; Assembly `.asm/.s/.S`. (Exact set finalized per shipped grammar.)

## What this milestone delivers (phases)

**Phase 1 — Grammar vetting + dependency landing (the gate).**
For every candidate crate: confirm a crates.io release with ABI-14/15 / a
`LanguageFn`, add it to `Cargo.toml`, smoke-parse a fixture. Produce the **final
shipped matrix** (which languages made it, which were dropped and why). Measure
binary-size + compile-time delta and decide whether any grammar gets a Cargo
**feature gate**. Output: a green build with all surviving grammars linked.

**Phase 2 — The generic engine.**
`graph/tags.rs`: `parse_with_tags` + `LangSpec` + the shared `kind_of` default
and the capture conventions above. Vendor the `tags.scm` files under
`src-tauri/queries/<lang>/`. Wire `language_for()` for every shipped grammar
(unlocks `struct_search` immediately) and add a generic default branch to
`parse_file`'s dispatch. Unit-test on one Tier-1 fixture per family (a C-like, a
functional, a scripting) before fan-out.

**Phase 3 — Tier 1 (code) fan-out.**
Vendor + tune each Tier-1 language's tags query, add extensions to
`from_path`/`tag`/`from_tag`, add fixtures asserting expected symbols + at least
one call edge. `graph_find_symbol`/`outline`/`callers`/`callees` answer against a
real file per language.

**Phase 4 — Tiers 2–3 (markup/data).**
Minimal/anchor tags queries (or struct-search-only where no meaningful symbol
notion exists). Confirm these are **excluded from call-graph expectations** and
don't pollute `doc_chunk`s unless explicitly enabled.

**Phase 5 — Tier 4 (exotic), best-effort.**
Attempt each; ship the ones that build, drop the rest with a logged note and a
matrix entry. No exotic language blocks release.

**Phase 6 — Config + surface.**
Extend the default `languages` list (`schema.rs:874` + `types.ts:1038`) — decide
per tier whether each is on by default or opt-in (proposal: Tier 1 on, Tier 2–4
opt-in to keep a fresh index lean). The Settings language-toggle list and the
Code Graph monitor's "languages indexed" readout enumerate the new set.

**Phase 7 — Docs + licenses.**
`MAINTENANCE.md`: how to add a language now ("add crate + vendor tags.scm +
extension + fixture"), the shipped matrix, grammar versions/licenses.
`README.md`/`DESIGN.md`: updated supported-language list. Third-party-licenses:
one entry per grammar (mostly MIT).

## What this milestone does NOT do

- **Migrate Rust/TS/JS/Python onto the generic engine.** They keep their bespoke
  walkers; unifying is a followup once the engine is proven on new languages.
- **Precise (stack-graphs) cross-file resolution for the new languages.**
  Resolution stays **name-based** (the V9-01 baseline). Overload/lexical-scope
  disambiguation is a followup.
- **Guarantee call graphs for markup/data/exotic langs.** Tiers 2–4 are
  defs/struct-search/anchors only where calls aren't a meaningful concept.
- **Vendor a grammar's C source by default.** If a language needs a from-source
  build to work, that's a per-language followup decision, not an automatic step.
- **Data-flow / CFG / taint.** Unchanged from V9-01 — still out of scope.

## Test plan

- **Per language (unit):** a fixture file → asserts the expected `Symbol` rows
  (name/kind/line) and, for Tier 1, ≥1 `Calls` and `Contains` edge; re-indexing
  is idempotent; a deliberately broken file yields partial symbols, never panics.
- **Engine (unit):** `parse_with_tags` maps a known multi-capture tags match to
  the right kinds + containment; a missing `@name` capture is skipped, not
  fatal; an invalid vendored query surfaces a clear error at load (caught in a
  test that compiles every shipped `tags.scm`).
- **Dispatch (unit):** `from_path` maps every new extension to the right `Lang`
  (case-insensitive), and `lang_for` honors the configured `languages` filter.
- **Manual:** open a real Go (and Java, C++) project in a cImp tab →
  `graph_find_symbol` + `graph_callers` answer from the index; `graph_struct_search`
  runs a query against each shipped grammar; the monitor tab lists the languages.

## Files most likely touched

- `src-tauri/src/graph/model.rs` — `Lang` variants, `from_path`, `from_tag`, `tag`
  (and `SymbolKind` only if a new kind is genuinely needed — current set already
  covers Class/Interface/Field/Variant/Module).
- `src-tauri/src/graph/builder.rs` — `language_for`, `parse_file` dispatch default
  branch; `CallSyntax` variants for any new-language call fallback.
- `src-tauri/src/graph/tags.rs` — **new**: the generic engine + `LangSpec` table.
- `src-tauri/queries/<lang>/tags.scm` — **new**: vendored per-language queries.
- `src-tauri/Cargo.toml` — grammar crates (possibly feature-gated).
- `src-tauri/build.rs` — bundle the `queries/` assets if not embedded.
- `src-tauri/src/settings/schema.rs` + `src/lib/settings/types.ts` — default
  `languages` list; Settings language-toggle UI.
- `docs/` — MAINTENANCE/README/DESIGN/licenses.

## Risks and open questions

- **Grammar availability + ABI churn (the dominant risk).** tree-sitter 0.26 is
  recent; some community grammars lag. Phase 1 is explicitly the gate — the
  shipped matrix is whatever survives it; the spec assumes attrition in Tier 4.
- **Binary size + compile time.** ~20+ grammars are compiled C. If the delta is
  unacceptable for a single-binary app, feature-gate the long tail (build a
  "full languages" vs "core" binary) — decision deferred to Phase 1 measurement.
- **tags.scm quality varies.** Some upstream tags files capture only definitions
  (no call refs) or use nonstandard capture names → per-language `kind_of`
  overrides and a documented "calls unavailable" status for thin grammars.
- **Markup/data noise.** Indexing HTML/CSS/JSON/YAML as symbols risks flooding
  the graph with low-value nodes. Mitigation: Tier 2–4 opt-in by default + tight
  anchor-only queries.
- **"Lisp" is ambiguous** (Common Lisp / Scheme / Clojure / Elisp are distinct
  grammars). Needs a user decision on which dialect(s) to target.

## Followups (FUTURE-FEATURES candidates)

- Unify the original four languages onto the generic engine (delete the bespoke
  walkers once parity is proven).
- Per-language stack-graphs `tsg` precise resolution where rules exist.
- From-source grammar builds for high-value languages with no usable crate.
- Import-edge extraction parity across all languages.

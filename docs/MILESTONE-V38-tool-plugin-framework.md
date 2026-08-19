# V38 — Unified Tool Registry + Plugin Framework

**Status:** SPEC — not yet coded (2026-08-18). GitHub: #77 (investigation +
full decision log in dated comments; the closing comment there is an 11-point
index of the locked decisions this doc distills; from now on this doc is the
design authority).
**Sequencing:** builds on
[MILESTONE-V37-mcp-management.md](MILESTONE-V37-mcp-management.md) — V37 lands
first (the MCP tier and any per-tool model exposure ride V37's registry,
propagation, and screening).

## Motivation

Every tool cImp can run is hardcoded twice over: the audit/security roster is
15 `static Adapter` consts (`src-tauri/src/audit/adapters.rs`) with a
hand-written parser each (`audit/parsers.rs`), per-tool settings baked into
the versioned schema (`settings/schema.rs`, `src/lib/settings/codeAudit.ts`),
and the MCP surface fanned out over that fixed roster (`audit/mcp.rs`). Adding
a tool — or a whole language's toolchain — means code changes and a release.
This milestone replaces that with a **unified tool registry** fed by **drop-in
plugins**: definition files a user (or third party) writes, discovered from a
folder, requiring no rebuild, ever.

## Locked decisions

1. **Scope: ALL of cImp's tools.** One registry covers audit/security
   scanners, compilers/linters/test runners, and ad-hoc dev commands (git,
   svn, …). The 15 built-in audit adapters **migrate onto the framework as
   built-in plugins** — proving the interface sufficient and deleting the
   hardcoded tier.
2. **Two orthogonal dimensions — never overloaded:**
   - **Capability kind** (contract dimension, exactly one per tool; the
     plugin-developer-facing concept): `audit` / `security` → SARIF findings
     pipeline; `check` → `run_check` diagnostics; `command` → raw output via
     `run_command`. The kind decides which pipeline consumes output and what
     cImp guarantees around the spawn.
   - **Category** (management dimension; what the user sees and interacts
     with): a semantic grouping — "Java" (javac + maven + pmd + spotbugs),
     "Source control" (git, svn) — shipped by plugins, holding tools of mixed
     kinds, toggled as a unit. **Category carries zero contract weight**: a
     tool behaves identically regardless of which category presents it.
3. **Finding contract = SARIF.** Plugins mediate their tool's native output
   into SARIF (native `--sarif` flag where available; wrapper/transform
   inside the plugin where not). cImp keeps **one parse boundary**: one SARIF
   parser + envelope validation (category, tool identity, substantiveness).
   This retires the per-tool set in `audit/parsers.rs` and converges on the
   parser layer that already exists (`checks/parsers.rs`, `ParserKind::Sarif`)
   — **one parser layer serving both umbrellas**.
4. **Two-tier surface — manifest default, MCP escape hatch:**
   - **Tier 1 (default): declarative manifest.** cImp spawns the tool itself
     (existing `audit/runner.rs`-style spawn path; V33 sandboxing, spawn
     ledger/gate apply directly) and ingests SARIF from stdout or a report
     file. The manifest **rhymes with `CheckDef`** — argv template, cwd,
     transport, report file, timeout, applicability gates (extensions /
     marker files), exit-code classes — no third config shape.
   - **Tier 2 (escape hatch): MCP-backed provider** for service-shaped tools
     or existing third-party MCP servers: SARIF over `tools/call`, hosted via
     `mcp_host.rs`, classified EXTERNAL, managed under V37. **Deliberate
     bias: keep the standing-MCP population small** — tier 1 is the default
     precisely because long-running server processes are a foreseeable
     liability (supervision load, notification churn, per-process trust).
5. **Model-facing surface is unchanged**: the model calls the two
   zero-argument umbrellas (`security_audit`, `quality_audit`) and
   `run_check`/`run_command`; plugins change what the fan-out runs, never the
   harness-visible schema. Tool descriptions continue to name **no underlying
   binary** (the audit/mcp.rs invariant) — so plugin roster changes emit no
   `list_changed` noise at all. Direct per-tool model exposure is exclusively
   a V37 category decision.
6. **Tool selection (e.g. maven vs gradle both configured):** layered, with
   **no arbitration logic in cImp** — config governs *availability* (per-tool
   toggles within a category; the category toggle is the group operation),
   the **harness chooses** among enabled tools, and applicability gates
   (`pom.xml` → maven, `build.gradle` → gradle) auto-disambiguate most real
   projects. The harness exercises judgment only where the project itself is
   ambiguous — which is exactly when it should.
7. **Trust model — plugins carry NO binaries.** A plugin is definition only;
   the **user supplies every executable path**. Consequently: no approval
   flow, no hash pinning, no signing, no distribution vetting. Binary trust
   sits with the user by construction (same act as today's per-tool path
   overrides). Documented plainly: *enabling a plugin's tool = trusting the
   executable YOU pointed it at; cImp guarantees the definition is
   well-formed and its output is screened — it does not vouch for the tool.*
   What stands is only what exists or is free composition:
   - plugin output rides the existing pipelines → existing screening and
     caps apply automatically (V32 detectors; audit's 300-finding/64 KB
     report budget);
   - manifest validation reuses existing code (`checks/mod.rs` report_file /
     cwd confinement, argv substitution validation, timeouts, wedged
     backstops, output caps, V33 sandbox + ledger, EXTERNAL taxonomy
     default);
   - **no shadowing**: plugin ids can never claim built-in ids;
   - **the built-in security floor stays**: plugins add to, never replace,
     the built-in security tools (generalize the
     `security_trio_is_always_applicable` invariant) — this protects what
     `security_audit`'s output *means* against a malicious plugin that
     attacks by silently under-reporting.
8. **Discovery**: plugins live in the cImp folder's **global `plugins/`
   directory and ONLY there** (themes/palettes external-file precedent; no
   per-project plugins folder). Startup scan + a manual **Rescan** action.
   Invalid plugins are rejected **loudly**: settings entry in an error state
   + an Events error row with the reason — never silently skipped.
9. **Identity = unique (name, version), both mandatory.**
   - Exact duplicate (same name AND version) found twice → **load neither**,
     mint a name-version conflict error carrying the **exact file paths** of
     both offenders.
   - Same name, different version → both load; settings disambiguates as
     "name (version)". Tools are internally namespaced by `name@version`, so
     coexisting versions never clash; if both are enabled, that is just the
     harness-chooses rule until the user disables one.
   - **Plugin-level enable/disable** in settings — disabling a plugin
     disables all its categories/tools as a unit; stored settings are
     retained across disable/enable.
10. **Configuration scoping**: everything is global; **project-specific
    values are the tool paths, variable values, and CLI parameters only**
    (`.cimp/config.json` overlay). The manifest declares which
    variables/parameters it exposes as configurable; those declarations are
    exactly the fields the settings pane renders at global and project scope.
    Recommended and adopted: **no automatic PATH resolution** — cImp never
    picks a binary on its own (at most an explicit per-tool "resolve from
    PATH" button, which is still a user act of selection). A freshly
    discovered tool is therefore naturally inert: visible, but unrunnable
    until a path is set. Installation ≠ activation, for free.
11. **Settings UI**: a **Tool Plugins** settings category, populated
    dynamically — master-detail like Settings today (plugins list left;
    selected plugin right: its categories as sections with group toggles,
    tools with path + enabled + declared options, error states). Per-field
    scope visibility (inherited-from-global vs overridden-here, with revert
    — the `command_allowlist` lesson: never ambiguous which file an edit
    lands in). Persistence = **one stable keyed-map container** in the schema
    (plugin id → tool id → fields): plugin churn never forces a schema
    migration. Nothing here is spawn-baked (paths/enables read at invocation
    time; no `spawn_inject_sig` interaction).
12. **Events**: new `plugin` Events kind with its own retention lane
    (`offload_server` precedent) for load failures, validation errors,
    identity conflicts, rescan errors — paired with the settings error state
    so failures are visible where they happened and where they get fixed.

## The capability contract spec (deliverable, blocking)

A rigorous, plugin-author-facing spec — the `docs/CHP.md` pattern:
authoritative, versioned (manifest schema version validated at load),
**drift-tested**. For each capability kind it states:

1. **Purpose** — when an author picks this kind.
2. **Invocation model** — who spawns what, when (umbrella call / run_check /
   run_command), process lifetime, timeout + backstop behavior.
3. **Input surface** — exactly what cImp passes (argv substitutions, cwd, env
   policy, report-file path) and what a plugin never receives. Env is
   allowlisted keys only; argv substitutions validated (the living-off-the-
   land constraint: manifest + path + argv is the trust unit).
4. **Output contract** — SARIF / diagnostics / raw; validation at the parse
   boundary; explicit handling of each failure mode: schema-valid-but-wrong,
   empty-but-parseable, hang, partial output.
5. **Consumption path** — which pipeline, its caps, which detection screening
   it passes before reaching a model.
6. **Security posture** — sandbox applied, network policy, taxonomy class,
   and the plain-language statement of what trust the user extends by
   enabling a tool of this kind.

The category layer appears in this spec only to state that it carries none of
the above.

### APPROVED (2026-08-19) — the sandbox fields of the manifest

Raised by the V33 live battery, which measured what actually happens when a
real tool runs inside the OS sandbox. **Approved 2026-08-19: all three fields
(`runtime`, `sandbox`, `extra_grants`) are locked into the Phase A manifest
schema.** Detail and evidence: `MILESTONE-V33-sandboxing.md`'s rc.9
corrections block and #72.

**The finding that motivates it.** Of the 14 built-in audit tools, 7 are
single static binaries and work sandboxed today with only their own directory
granted; the other 7 need a runtime (Python, Node, a JRE, the .NET SDK, cargo)
and fail without its install tree, its state directories, or both. V33 now
carries a `RuntimeProfile` table that INFERS this from the resolved program.
Inference is the right default and the wrong contract for a plugin ecosystem:
cImp cannot infer a runtime it has never heard of, and the ratio worsens as
soon as third parties write tools (people write scanners in Python and Node).

**Proposed manifest fields, per tool:**

1. **`runtime: none | python | node | java | dotnet | go | rust | auto`** —
   selects a cImp-owned profile that supplies the grants and env pointers that
   runtime needs. `none` IS the "single static binary" statement; `auto` keeps
   V33's inference. **Deliberately NOT a `static | runtime-dependent`
   boolean:** that shape carries no actionable information (it never says
   *which* runtime), it can contradict a sibling field, and it mis-classifies a
   real third case — a project-local tool (eslint, knip resolving from
   `node_modules/.bin`) is runtime-dependent yet needs no grant at all, because
   its payload already lives inside the project root the sandbox grants.
2. **`sandbox: required | optional | unsupported`** — a DIFFERENT question:
   not what the tool needs, but what cImp does when it cannot provide it.
   `unsupported` runs the tool outside the boundary as an informed user choice
   with a visible row, instead of the mysterious failure V33 shipped before
   these rows existed. The two fields are orthogonal: a Python tool may be
   perfectly sandboxable; a static binary may need egress and declare
   `unsupported`.
3. **`extra_grants: [path]` (optional escape hatch)** — for a tool whose needs
   no profile covers. Constraints, all three load-bearing: shown to the user at
   enable time as a permission (the phone-app pattern), screened by V33's
   existing `extra_grant_refusal` rules (credential dirs, user-profile root,
   volume roots, `%SystemRoot%` are refused with a row, remaining grants still
   apply), and **global scope only** — see the warning below.

**Why a closed enum rather than free-form runtime paths.** A manifest is
attacker-controlled input once plugins are installable. `runtime: python` is a
*request* that cImp stamp an RX ACE on a runtime tree; that is safe only while
the value selects from a table cImp owns, so the worst a lying manifest
achieves is a grant the user can see named at enable time. Free-form paths
would make the manifest a grant-widening primitive — which is why
`extra_grants` is permission-prompted and screened rather than trusted.

**Declaration and inference cross-check.** Keep V33's detection as the `auto`
implementation *and* as a canary: when a manifest declares one runtime and
detection sees another, surface the mismatch rather than silently trusting
either side (V35's leading-indicator discipline; a stale declaration is
exactly the drift that discipline exists to catch).

**⚠ Security note on decision 10 (configuration scoping), independent of this
proposal.** Decision 10 places tool paths and CLI parameters in the
`.cimp/config.json` overlay. That file lives inside the project root, which
the sandbox grants FULL — so anything running in the project, including a
compromised model, can write it. Combined with this spec's own principle that
"manifest + path + argv is the trust unit", an overlay-settable binary path is
a code-execution primitive: repoint a tool's `path` and cImp runs it. V33 hit
the identical hole (a project overlay could switch the sandbox off, or name
`~/.ssh` as an extra grant) and closed it by making the whole `sandbox` block
overlay-banned and machine-global, with a write-through so the settings still
save. **DECIDED 2026-08-19 (amends decision 10): binary paths, and any field
that widens the boundary, are never overlay-settable.** Per-project binary
paths survive, but via a **machine-global per-project map** (keyed by project
root, stored alongside the global settings — outside every sandbox grant), so
a compromised repo cannot write them. `.cimp/config.json` carries variable
values and CLI parameters only; a `path` (or other banned field) appearing in
the overlay is ignored with a loud warning event, and a write-through keeps
project-scoped path edits saving to the machine-global map (the V33 `sandbox`
block treatment). Live-verify 5 stands unchanged — two projects, two paths —
the storage location moves, not the capability.

## Registry semantics

- Registry entry = plugin-declared tool + user state: executable path
  (global, per-project via the machine-global per-project map — never the
  overlay), enabled (plugin / category / tool levels), declared variable +
  CLI parameter values (global, project-overridable via the overlay).
- `command`-kind entries **feed `run_command`**: the registry entry (explicit
  path + enabled) becomes the allowlist entry and path resolution —
  superseding a separate allowlist for registered tools.
- `check`-kind entries surface as ready-made check definitions to `run_check`
  (a plugin can make "add language X" one drop: audit tools + checks in one
  category).
- Effective tool set for any pipeline = enabled ∩ applicable (gates) ∩
  path-configured.

## Failure modes (adversarial, from the #77 threat-model pass)

- Manifest tries `report_file`/cwd escape → rejected at validation (existing
  confinement).
- Tool emits schema-valid-but-wrong or empty-but-parseable SARIF → envelope
  validation + substantiveness checks decide surfaced-vs-refused; empty is
  not treated as absent (a blank artifact must not bypass the safe path).
- Tool hangs / floods → existing timeout + wedged backstop + output caps.
- Finding text carries prompt injection → existing detection screening on
  the delivery boundary (unchanged).
- Two plugins claim the same (name, version) → neither loads, loud conflict.
- Plugin claims a built-in id → rejected (no shadowing).

## Out of scope

- Approval/pinning/signing and distribution machinery (locked out — trust
  model, decision 7).
- Per-project plugin definitions (locked out — decision 8).
- Direct per-tool model exposure (V37 category decision).
- Hosting plugin MCP servers (#41 / V37 internal half).

## Phases

- **A — Manifest schema + loader**: versioned schema, folder scan, Rescan,
  identity rules, `plugin` Events kind, loud rejection. *(Shippable alone:
  plugins visible in settings, inert.)*
- **B — Registry + Tool Plugins settings UI**: keyed-map persistence,
  master-detail UI, scope-visible overrides, declared-variable rendering.
- **C — Audit integration**: SARIF parse boundary (shared parser layer),
  plugin tools join the umbrella fan-out; security-floor + no-binary-names
  invariants pinned by tests.
- **D — Check + command kinds**: check defs feed `run_check`; command entries
  feed `run_command` allowlist/path resolution.
- **E — Built-in migration**: the 15 adapters become shipped built-in
  plugins; `audit/parsers.rs` retired; `adapters.rs` reduced to the loader's
  built-in set. Baselines re-pinned.
- **F — MCP tier (after V37)**: tier-2 providers returning SARIF over
  `tools/call`, managed under V37.
- **G — Capability contract spec**: the CHP-pattern doc + drift tests
  (blocking for "milestone done", per decision — written alongside C/D, not
  after).

## Live-verify (fresh tabs)

1. Drop a valid plugin → appears in Tool Plugins after Rescan, tools inert;
   set path + enable → next `security_audit` run includes it; findings
   in the report and the Code audit UI.
2. Drop a broken manifest → error state in settings + Events row with
   reason; app otherwise unaffected.
3. Duplicate (name, version) pair → neither loads; conflict row names both
   file paths.
4. Same name, two versions → both listed as "name (x.y.z)"; disable one at
   plugin level.
5. Project override: same tool, different paths in two projects; each tab
   spawns its own project's binary (verify via spawn ledger rows).
6. `command`-kind plugin (git) → run_command resolves the registered path,
   allowlist honored, sandbox row minted as for any spawn.
7. Built-in migration regression: post-Phase-E, umbrella reports byte-match
   the pre-migration reports on a fixture repo (parser unification proven).

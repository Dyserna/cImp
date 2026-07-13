# V22 — run_check Generalization (multi-language parsers, Settings UI, auto-configure)

**Status:** IMPLEMENTED (2026-07-13). Phases A–F landed on `develop`: the
Rust↔TS drift fix + tripwire (Phase A), `CheckDef` `cwd`/`env`/`report_file`
with root-confinement (Phase B), the six new parsers `sarif`/`go`/`go-test-json`/
`dotnet`/`junit-xml`/`regex-custom` (Phase C), marker+graph language detection
with a preset catalog and PATH-validated proposals (Phase D), the
`ChecksEditor.svelte` editor with Test/Detect & configure/exposure-status and the
Code Intelligence nudge chip (Phase E), and these docs (Phase F). Tests green
(99 `checks`-module Rust tests, 320 vitest, svelte-check clean). The **live
verification** section below (Detect on this repo, scratch Go/TS projects, the
`regex-custom` no-code-change wire, and the drift-tripwire mutation) is still to
be run by hand.
**Builds on:** V12 `run_check` (`checks/mod.rs::CheckDef:35`, `ParserKind:65`,
`checks/parsers.rs::parse`, the MCP gate at `graph/mcp.rs:337`), V17's added
parsers (`CargoTest`, `JestJson`), the settings overlay model
(`settings/persistence.rs` — per-project `.cimp/config.json` diffs), the
tool-servers editor precedent in `SettingsApp.svelte` + reusable
`settings/ArrayEditor.svelte` / `EnvEditor.svelte`, the code graph's
per-language awareness (V9-02 multi-language engine), and V21 Feature 6
(worker-native `run_check`), whose discoverability note this milestone
implements.

## Why

`run_check` is one of the strongest tools in the box — it turns raw
build/test/lint dumps into bounded, deduplicated, structured diagnostics —
and it is nearly invisible in practice:

1. **No UI.** `checks` is config-file-only ("set via the `.cimp/config.json`
   overlay" — `src/lib/settings/types.ts:602`). Nobody discovers a feature
   whose only entry point is hand-editing JSON against a Rust enum.
2. **Silently gated.** With `checks` empty (the default), `run_check` is not
   advertised on any surface, and nothing anywhere says why ("run_check MCP
   tool isn't exposed in this session").
3. **Parser coverage is three ecosystems deep.** Rust (`cargo-json`,
   `cargo-test`), TS/JS (`tsc`, `eslint-json`, `jest-json`), Python tests
   (`pytest`), plus the `generic-gcc` line fallback. Go, .NET, Java/JVM, and
   every lint tool with a nonstandard shape are out — while other projects
   the graph already handles (V9-02: ~25 languages) can't use the feature.
4. **Wire-type drift already happened.** The TS `ParserKind` union
   (`types.ts:770`) is missing V17's `cargo-test` and `jest-json` — harmless
   today only because no UI reads it, i.e. gap 1 is masking gap 4.
5. **`cmd` runs with cwd = project root, full stop.** Projects with nested
   manifests (this repo: `src-tauri/Cargo.toml`) need `--manifest-path`
   hacks; monorepos need worse.

V22 closes all five and adds the piece that makes the feature genuinely
zero-effort: **detect the project's languages and propose a working checks
config automatically** — ride the code graph's language stats when a graph
exists, marker files when it doesn't, human-approved by default.

Scope posture: mainstream-first (the V9-02 rule) — first-class parsers for
the big ecosystems, **SARIF** for the long tail of modern tools, and a
**custom-regex** parser as the universal escape hatch, so *any* language's
tooling can be wired in without a cImp release.

No graph.db schema involvement. `CheckDef` gains optional fields
(`serde(default)` — old configs deserialize unchanged); the TS mirror is
updated in lockstep and pinned by a tripwire (Phase A).

---

## Phase A — Drift fix + mirror tripwire

### Goal
Fix the existing Rust↔TS drift and make the next one impossible to commit.

### Design
- Add `'cargo-test' | 'jest-json'` to the `ParserKind` union
  (`types.ts:770`).
- Tripwire (V16 pattern): a Rust unit test in `checks/mod.rs` embeds
  `src/lib/settings/types.ts` via `include_str!` and asserts every
  `ParserKind` wire name (its kebab-case serde rename) appears in the file —
  adding a Rust variant without the TS mirror fails `cargo test`. Same
  assertion for `CheckDef`'s field names once Phase B extends it.

### Tests
The tripwire *is* the test; plus it must fail if a name is removed from
types.ts (verified once by mutation during development).

---

## Phase B — `CheckDef` generalization (`cwd`, `env`)

### Goal
Make one `CheckDef` shape fit monorepos, nested manifests, and tools that
need environment setup — without per-language special cases.

### Design (`checks/mod.rs` + `checks/run`)
- `cwd: Option<String>` — relative to the project root, resolved and
  **confined** under it (reuse the confinement approach of
  `ToolCtx::confine`; absolute or escaping paths rejected at validation and
  at run time). Replaces `--manifest-path`-style workarounds generically.
- `env: Vec<(String, String)>` (default empty) — forced at spawn, same shape
  the `CommandPolicy` env mechanism uses. Debug redaction of values,
  matching the MCP-server env precedent in settings.
- Both `serde(default)`; TS mirror updated (tripwire from Phase A now covers
  the field list).
- Diagnostic paths from parsers are already normalized against a cwd
  (`parsers.rs::parse` takes `cwd`) — pass the effective cwd so `file`
  fields stay project-root-relative for the report.

### Tests
Confinement (escape/absolute rejected); env applied; paths in diagnostics
root-relative when `cwd` is nested; defaults roundtrip old JSON unchanged.

---

## Phase C — Multi-language parsers

### Goal
First-class coverage for the mainstream ecosystems cImp projects actually
use, one standard format that covers the modern long tail, and an escape
hatch for everything else.

### Design (`checks/parsers.rs`, one `ParserKind` variant each)
- **`sarif`** — SARIF 2.1 JSON on stdout (or via Phase B2 `report_file`,
  below). One parser unlocks: `ruff --output-format sarif`, `clang-tidy`,
  `golangci-lint`, `semgrep`, ESLint's SARIF formatter, CodeQL, and most
  security/lint tools shipped this decade. Map `level` → `Severity`
  (`note`→Note), take the first physical location, ignore the rest of the
  (large) envelope. This is the highest-leverage single variant.
- **`go`** — `go build` / `go vet` text: `file:line:col: message`
  (severity-less ⇒ Error; `vet` notes stay Error — they gate merges in Go
  shops). Handles the `# package` stanza headers.
- **`go-test-json`** — `go test -json` event stream: one Diag per failed
  test (`Action: "fail"` + the collected `Output` lines for that test as
  the message tail), pass/total counts in the summary line.
- **`dotnet`** — MSBuild canonical format from `dotnet build`:
  `file(line,col): error|warning CSnnnn: message` — covers C#/F#/VB in one
  regex family.
- **`junit-xml`** — the lingua franca of *test* results: Maven Surefire,
  Gradle, Kotlin, PHPUnit, and dozens of runners emit it. Requires reading
  a report **file** (these tools don't put XML on stdout), so `CheckDef`
  gains `report_file: Option<String>` (Phase B2, same confinement as
  `cwd`): when set, the parser input is that file's content after the run,
  not stdout. One failed `<testcase>` ⇒ one Diag (classname + name +
  message/first stack line).
- **`regex-custom`** — the universal escape hatch: `CheckDef` gains
  `pattern: Option<String>` (used only by this parser) — a regex with named
  groups `file`, `line`, optional `col`, optional `severity`, `message`,
  applied per line to stdout+stderr; unmatched severity defaults to Error.
  Validated at settings-save time (compile the regex, require the mandatory
  groups) so a bad pattern is a UI error, not a silent zero-diagnostics run.
- All new parsers: fixture-based tests in `parsers.rs::tests` from real tool
  output (the existing pattern), ANSI stripped first (existing
  `strip_ansi`), severities and dedup keys consistent with the V12
  machinery.

### Tests
Per-parser fixtures (including a truncated/garbage input ⇒ zero diags, no
panic); regex validation rejects missing groups; `report_file` read +
confinement + missing-file ⇒ explicit error diag, not empty success.

---

## Phase D — Language auto-detection + auto-configure

### Goal
Zero-effort setup: cImp detects what the project is written in and proposes
a working checks config; the user approves with one click.

### Design
- **Detection, two sources merged:**
  1. *Code graph language stats* when a graph is built — the V9-02 engine
     already classifies every indexed file by language; expose per-language
     file counts from the index (small addition to `graph/index.rs`'s
     existing stats surface).
  2. *Marker files* as the graph-less fallback and as tool-config evidence:
     `Cargo.toml`, `package.json` + `tsconfig.json`, `go.mod`,
     `pyproject.toml`/`setup.cfg`, `pom.xml`/`build.gradle(.kts)`,
     `*.sln`/`*.csproj`, eslint/ruff/jest/vitest config files.
- **Preset catalog** (constant table, ecosystem → candidate `CheckDef`s):
  - Rust: `cargo check --message-format=json` (`cargo-json`) + `cargo test`
    (`cargo-test`), with `cwd` pointed at the manifest directory when
    `Cargo.toml` isn't at the root (fixes this repo's case properly).
  - TS/JS: `tsc --noEmit --pretty false` (`tsc`) when tsconfig present;
    `eslint --format json .` (`eslint-json`) when eslint config present;
    jest/vitest (`jest-json`) when their config present.
  - Go: `go vet ./...` (`go`) + `go test -json ./...` (`go-test-json`).
  - Python: `ruff check --output-format sarif .` (`sarif`) when ruff
    configured, `pytest -q` (`pytest`) when pytest/tests detected.
  - .NET: `dotnet build --nologo` (`dotnet`).
  - Java/JVM: `mvn -q test` or `gradle test` with `junit-xml` +
    `report_file` at the conventional surefire/gradle report path.
- **Candidate validation before proposing:** the tool binary must resolve on
  PATH (reuse the `pty/resolve.rs` lookup) and the marker evidence must be
  present. Proposals the machine can't validate are shown greyed with the
  reason, never silently applied.
- **Surfaces:**
  - "Detect & configure" button in the Phase E editor — runs detection,
    shows the proposal list with per-item checkboxes, applies selected
    entries (merge by `name`, no duplicates).
  - Passive nudge: when a graph index completes and `checks` is empty,
    Code Intelligence shows a one-time "run_check: N suggested checks for
    this project" chip linking to the editor. Dismissal is remembered
    per project.
  - `checks_auto_configure: bool` setting, default **false**: when true,
    validated proposals are applied automatically on first index (for users
    who want the fully automatic behavior across many projects); the applied
    set is reported in the chip so it's visible, and entries it added carry
    a marker field so re-detection never fights user edits.

### Tests
Detection merge (graph stats + markers); per-ecosystem preset selection off
fixture trees; PATH-validation gates proposals; merge idempotence; auto
mode never overwrites a user-edited entry; dismissal persistence.

---

## Phase E — Settings UI (checks editor + exposure status)

### Goal
A first-class editor so the feature is discoverable and configurable without
touching JSON — and an honest status line so the exposure gate is never
silent again.

### Design (`SettingsApp.svelte` + a new `settings/ChecksEditor.svelte`)
- **Editor**, following the MCP tool-servers editor pattern: list of
  configured checks with add/edit/delete; fields: name, cmd, parser
  (dropdown of all `ParserKind`s; selecting `regex-custom` reveals the
  pattern field with live validation; selecting `junit-xml`/`sarif` reveals
  `report_file`), timeout, `cwd`, `env` (reuse `EnvEditor`). Writes go
  through the normal settings path — which already lands per-project overlay
  diffs, matching the per-project nature of `checks`.
- **Per-check "Test" button** — the #1 foot-gun killer: dry-runs the check
  through the existing `checks::run`, shows exit status, parsed diagnostic
  count, and the first few diagnostics; if the command produced output but
  the parser matched **zero** diagnostics, show an explicit "wrong parser?"
  warning instead of a green tick.
- **Exposure status line** (implements the V21 F6 note): "run_check
  exposed: MCP ✓ / offload worker ✓" when checks exist (worker line appears
  once V21 F6 ships), or "not exposed — no checks configured" with the
  Detect button right there.
- The "Detect & configure" button (Phase D) lives at the top of this editor.

### Tests
Svelte-side: parser-conditional fields, regex validation surfacing,
zero-diag warning path. Rust-side: the dry-run IPC returns the structured
result the UI renders; overlay write path exercised (existing persistence
tests extended).

---

## Phase F — Docs

README + FEATURES entry (run_check is currently undocumented outside
milestone docs); MAINTENANCE recipe for adding a parser (fixture → variant →
TS mirror → tripwire green); cross-link from the V21 spec's F6 note.

---

## Decisions (proposed defaults)

1. **SARIF + regex-custom over an ever-growing per-tool enum** — new
   ecosystems should usually cost *zero* cImp changes: modern tools speak
   SARIF; everything else fits the regex hatch. Per-tool variants are
   reserved for the mainstream shapes that are ugly to regex (MSBuild, go
   test JSON, JUnit XML).
2. **Auto-configure is propose-by-default, apply-on-opt-in** — a wrong
   auto-applied check burns tokens on every `auto_check` fire; a proposal
   chip costs one click. `checks_auto_configure` exists for fleet users who
   accept that trade.
3. **`report_file` confined like `cwd`** — parsers reading files is new
   surface; both resolve strictly under the project root.
4. **`vet`/severity-less Go lines map to Error, not Warning** — matching how
   Go toolchains gate; revisit on complaint.
5. **Numbering:** V21 just allocated; this is V22.

## Out of scope

- LSP-based live diagnostics (different architecture; `run_check` stays
  command-based and bounded).
- Watch/daemon modes and incremental check servers (`tsc --watch`,
  `cargo watch`) — one bounded run per call is the contract.
- Auto-fix application (`eslint --fix`, `cargo clippy --fix`) — `run_check`
  is read-only by design; fixes remain the agent's job.
- Per-check scheduling/CI integration — `auto_check` (V12 Phase F) already
  covers the post-edit trigger and benefits from all of this unchanged.

## Verification (live)

- This repo: "Detect & configure" proposes the two cargo checks with
  `cwd: src-tauri` — apply, `run_check` appears on the MCP surface, and
  "does the test suite pass?" via the worker (V21 F6) cites a run_check
  observation.
- A scratch Go module with a deliberate vet error and a failing test: both
  checks proposed, both parse (correct file:line, severities).
- A scratch TS project: tsc + eslint + vitest proposals gated on their
  config files actually existing.
- `regex-custom`: wire an arbitrary tool (e.g. `markdownlint`) purely from
  the UI, Test button shows parsed diags, no cImp code change.
- Editor Test button on a wrong-parser config shows the zero-diag warning.
- Drift tripwire: remove `'cargo-test'` from types.ts locally ⇒ `cargo test`
  fails.

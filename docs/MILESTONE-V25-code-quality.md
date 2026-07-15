# V25 — Code Quality (language-gated linters + Security/Quality split)

**Status:** IMPLEMENTED (2026-07-16) — all phases A–E coded on develop; live
verification per MAINTENANCE.md V25 recipes pending. Spec deviations found at
implementation (cppcheck report-file transport + exit-0-with-findings,
cargo-machete real output shape, typos JSONL fields) are documented inline in
`audit/adapters.rs` / `audit/parsers.rs` and in FEATURES.md.
**Builds on:** V23 Code Audit (adapter registry `audit/adapters.rs`, runner
`audit/runner.rs`, `CodeAuditView.svelte`, `Settings.code_audit` with the
lenient closed-enum `AuditToolId` + tripwire), V22 checks parsers
(`checks/parsers.rs::parse`, the shared `Diag`), `procutil.rs` spawn/capture,
and the ebin→PATH→override resolution from V23.

## Why

V23 gave cImp a security surface (osv-scanner + gitleaks + semgrep). The same
registry/SARIF pipeline extends to **code quality** — linters, dead-code and
unused-dependency detection, spell checking — at marginal cost per tool.
Decisions locked with the user (2026-07-15):

- **Ten quality tools** (five primary, five secondary — all in v1).
- **Language-gated:** a tool only appears in the tab and only runs when the
  project actually contains files/markers it applies to. No PMD chip in a
  Rust project.
- **Two tabs:** Security and Quality don't mix. The existing **Code Audit**
  tab keeps the three security tools; a new reserved **Code Quality** tab
  (`TabId::CodeQuality`) hosts the ten quality tools — each with its own
  tool chips, Scan button, and findings table, runnable independently.
- **External tools may be any language** (constraint clarified 2026-07-15:
  "no Python" applies to cImp's own code, not to tools it detects and
  invokes — semgrep set the precedent). Nothing is bundled; everything
  resolves ebin → PATH → per-tool override.

### The quality toolset

| Tier | Tool (`AuditToolId`) | Language gate | Output → parser | Exit w/ findings | Notes |
|---|---|---|---|---|---|
| P1 | **oxlint** (`oxlint`) | js/ts/jsx/tsx/mjs/cjs files | SARIF stdout (`--format sarif`) | 1 | single Rust binary, zero-config |
| P1 | **golangci-lint** (`golangci-lint`) | `go.mod` or `.go` files | SARIF (`run --output.sarif.path stdout`) | 1 | **v2 flag syntax** — `--out-format` is v1-only |
| P1 | **ruff** (`ruff`) | `.py` files | SARIF stdout (`check --output-format sarif`) | 1 | single Rust binary |
| P1 | **cppcheck** (`cppcheck`) | `.c/.cc/.cpp/.cxx/.h/.hpp` | SARIF (`--output-format=sarif`, cppcheck ≥ 2.16) | 0 (findings ≠ exit) | verify stdout-vs-stderr at impl; needs `--enable=warning,style` default args |
| P1 | **typos** (`typos`) | always applicable | JSONL (`--format json`) → new `TyposJsonl` parser | 2 | the only tool valuable on every project |
| S | **ESLint** (`eslint`) | eslint config marker present (`eslint.config.{js,mjs,cjs,ts}`, `.eslintrc*`) | JSON (`--format json`) → new `EslintJson` parser | 1 | resolve project `node_modules/.bin/eslint` first, then PATH; JSON avoids requiring the sarif-formatter package in the target project |
| S | **PMD** (`pmd`) | `.java` files | SARIF (`check -d <root> -R rulesets/java/quickstart.xml -f sarif`) | 4 | `pmd.bat` on Windows; needs JRE |
| S | **Roslyn analyzers** (`dotnet-analyzers`) | `*.sln` / `*.csproj` | SARIF report file (`dotnet build /p:ErrorLog=<report>,version=2.1 -nologo`) | 1 | **default-disabled** — runs a real build (writes obj/bin, restores packages); longer default timeout |
| S | **knip** (`knip`) | `package.json` | JSON (`--reporter json`) → new `KnipJson` parser | 1 | node tool; same project-local `.bin` → PATH resolution as eslint |
| S | **cargo-machete** (`cargo-machete`) | `Cargo.toml` | text → new `MacheteText` parser (line regex) | 1 | near-instant |
| opt | **semgrep-quality** (`semgrep-quality`) | any source files | SARIF (same adapter shape as V23 semgrep, `--config p/best-practices` default) | 1 | **default-disabled** — separate id so quality rulesets never pollute the Security section; registry configs need network |

Not selected (recorded so we don't relitigate): Biome (no SARIF, redundant
with oxlint), jscpd/scc/lizard (metrics-shaped, not findings-shaped),
Checkstyle (PMD covers the slot), SpotBugs (needs compiled bytecode),
clang-tidy (needs `compile_commands.json`; possible future power-user adapter).

Rust linting stays in `run_check` (clippy) — no duplicate audit adapter.

---

## Phase A — Language census + category plumbing

### Goal
A cheap, ignore-respecting scan of the project root that answers "which
languages/markers are present", plus `category` on the registry, with no UI
change yet.

### Design
- New `src-tauri/src/audit/census.rs`:
  - `pub struct Census { extensions: HashSet<String>, markers: HashSet<&'static str> }`
  - `pub fn take(root: &Path) -> Census` — walk with the same ignore
    semantics as the graph indexer (`ignore` crate: .gitignore + hidden
    dirs), **bounded**: stop at 20 000 entries or 2 s, whichever first.
    Collect lowercase extensions and hits on a fixed marker list
    (`go.mod`, `Cargo.toml`, `package.json`, `*.sln`, `*.csproj`,
    `eslint.config.*`, `.eslintrc*`). A truncated census is fine — it only
    ever *hides* tools, and 20k files is far past the point where every
    mainstream language has shown up.
  - Cached per root with a short TTL (~60 s) so tab-open and scan-start
    don't double-walk.
- `adapters.rs`:
  - `pub enum Category { Security, Quality }` on `Adapter`.
  - `pub struct Applicability { extensions: &'static [&'static str], markers: &'static [&'static str] }`
    on `Adapter`; empty slices = always applicable (v1 security tools, typos,
    semgrep-quality).
  - `pub fn applicable(&self, census: &Census) -> bool` — any extension OR
    any marker hit.
- Tests: census on a fixture tree (extensions, markers, gitignore respected,
  bound respected); applicability per adapter.

## Phase B — Registry growth + new parsers

### Goal
All 11 new `AuditToolId` variants (10 quality tools + `semgrep-quality`) with
adapters and parsers, fully unit-tested, runner untouched.

### Design
- `AuditToolId` grows the 11 ids above (lenient closed-enum + wire tripwire
  test extended — same pattern V23 established). Default `tools` vec gains
  them all: `enabled: true` except `dotnet-analyzers` and `semgrep-quality`.
- Adapter statics per the matrix. Argv details locked above; everything else
  (Transport, env, findings_exit_codes) follows the table. Windows launcher
  names (`pmd.bat`) go through the same resolution as V23's `.exe` handling.
- Node-tool resolution (eslint, knip): try `<root>/node_modules/.bin/<tool>`
  **before** ebin/PATH — quality of the project-local install beats a global
  one. New optional `project_local_bin: Option<&'static str>` on `Adapter`.
- Parsers: audit-local `enum AuditParser { Sarif, EslintJson, TyposJsonl, KnipJson, MacheteText }`
  on `Adapter`, dispatching in one function that returns `Vec<Diag>`. Sarif
  delegates to the existing shared parser; the four new ones are small,
  fixture-tested (`src-tauri/testdata/audit/*`), and clamp severities into
  the real `error|warning|note` set (V23 decision):
  - EslintJson: `severity 2→error, 1→warning`; ruleId → `Diag.code`.
  - TyposJsonl: one JSON object per line; `type: "typo"` → note-severity
    Diag `"`word` should be `correction`"`.
  - KnipJson: unused files → per-file Diag; unused exports/deps → per-item
    Diag; all warning.
  - MacheteText: `<crate> — unused dependency in <Cargo.toml path>` lines →
    warning Diags anchored to the Cargo.toml.
- Detect (`--version` probe) works unchanged for all; `dotnet-analyzers`
  probes `dotnet --version`.

## Phase C — Runner: category scans + applicability filter

### Goal
`start_scan` takes a category; only applicable+enabled tools of that category
run; events carry enough for a split UI.

### Design
- `start_scan(root, category: Category)`: filter =
  `enabled && adapter.category == category && adapter.applicable(&census)`.
  Census taken once per scan, shared across the tool set.
- Concurrency: keep the V23 invariant — **one scan at a time globally**
  (both Scan buttons disable while either runs). Cheapest correct model; a
  per-category lock is a possible later relaxation.
- `audit-status` events + `audit_snapshot` gain `category` per tool state
  and a `census: { extensions: string[], markers: string[] }` block (drives
  chip visibility without a second IPC).
- A tool that is enabled but **not applicable** is reported in the snapshot
  as `skipped_not_applicable` (distinct from disabled/not-found) so the UI
  can hide it while Settings can still explain it.
- Timeouts: per-tool override field already exists via `timeout_secs`
  (global). Add optional per-tool `timeout_secs` on `AuditToolConfig`
  (default None = global); `dotnet-analyzers` documents a recommended 1200.

## Phase D — UI: Code Quality tab + gated chips + settings

### Goal
A new reserved **Code Quality** tab alongside Code Audit; Settings groups
the grown tool list.

### Design
- New `TabId::CodeQuality` reserved tab (full V20/V23 pattern: `appViews.ts`
  keep-alive portal, `viewSection.ts` persistence, visibility gating on
  `code_audit.enabled` — one feature flag covers both tabs in v1).
- `CodeAuditView.svelte` is refactored into a category-parameterized
  component (or a shared core + two thin wrappers): Code Audit renders
  `Category::Security` (osv-scanner, gitleaks, semgrep — unchanged
  behavior), Code Quality renders `Category::Quality` (the 10 +
  semgrep-quality). Each tab has its own tool chips (only applicable tools
  shown; a muted "n tools hidden — not applicable to this project" line
  when any are gated off), Scan/Cancel button, status line, findings table,
  select-all + markdown copy. Existing behaviors (severity chips, graph ⌖
  jump, 500-findings cap per tool, not-installed → settings deep-link)
  apply per tab unchanged.
- Both tabs share the one-scan-at-a-time lock: the other tab's Scan button
  shows a "waiting — <other> scan running" state while a scan runs.
- Settings → Code Audit: tool list rendered in two groups (Security /
  Quality) with the same Detect/Browse/extra-args rows; non-applicable tools
  are NOT hidden in Settings (global config), but show the census-based
  hint when the current project gates them off.
- Scanned-artifacts coverage line stays osv-scanner-only (V23 decision).

## Phase E — Docs + release

- FEATURES.md: Quality section, tool matrix, language gating.
- MAINTENANCE.md: live-verify recipe per new tool (install → Detect → scan a
  fixture project per language → findings appear in the right section;
  gating check: PMD chip absent in this repo).
- CHANGELOG `[Unreleased]`.

---

## Decisions locked

1. All 10 tools in v1; `dotnet-analyzers` + `semgrep-quality` default-disabled.
2. Gating hides tools in the tab; Settings always lists them.
3. Security and Quality are separate **tabs** (Code Audit keeps security;
   new reserved Code Quality tab) with independent runs and findings
   tables; one scan at a time globally.
4. semgrep quality rulesets live under a separate `semgrep-quality` id —
   the Security semgrep entry stays pure SAST.
5. ESLint/knip prefer the project-local `node_modules/.bin` binary.
6. Census is bounded (20k entries / 2s) and only ever hides tools.
7. Rust lint depth stays in `run_check` (clippy); no audit adapter for it.

## Open items to verify at implementation time

- cppcheck SARIF: stdout or stderr, and exact exit semantics with findings.
- typos exit code with findings (assumed 2) and JSONL field names.
- golangci-lint v2 exact stdout-SARIF invocation on Windows.
- PMD findings exit code (assumed 4; `--no-fail-on-violation` alternative).
- knip JSON shape for unused exports (changed across majors — pin at impl).

# V23 — Code Audit (aggregated security scanning)

**Status:** SPEC (2026-07-15). Not yet coded.
**Builds on:** V22 checks parsers (`checks/parsers.rs::parse` with
`ParserKind::Sarif`, the `Diag` struct at `checks/mod.rs:220`), the reserved
dashboard tab pattern (`state/manager.rs::TabId`, `appViews.ts`,
`viewSection.ts`), the ebin→PATH command resolution (`pty/resolve.rs`) with
per-tool overrides (`ExternalToolsSettings` precedent, `schema.rs:985`), the
external-tools Settings section + `pickToolExe()` browse helper
(`SettingsApp.svelte:2500,1276`), and the `graph-status` progress-event
precedent (`graph/service.rs:59`).

## Why

cImp works on a project directory and hosts the agents that write code there,
but has no security surface at all: no dependency-vulnerability visibility,
no secrets detection, no SAST. The research (2026-07-15) picked a toolset
that fits cImp's constraints (Windows-first, no Python sidecars, no bundling,
single-binary CLIs, machine-readable output):

| Tier | Tool | Covers | Why this one |
|---|---|---|---|
| Core | **osv-scanner** (Google, Apache-2.0) | dependency CVEs **and** known-malicious packages (OSV `MAL-*` from OpenSSF malicious-packages), 19+ lockfiles/manifests incl. `Cargo.lock`, npm locks, `go.mod`, `pom.xml` (transitive via deps.dev) | single static Go binary, native Windows, SARIF |
| Core | **gitleaks** (MIT) | secrets in working tree + git history | single static Go binary, native Windows, SARIF |
| If present | **semgrep** (LGPL-2.1) | SAST of first-party code, 30+ languages | best OSS SAST but Python-based and Windows support is beta — never required, lights up when resolvable |
| Future | GuardDog (Datadog) | heuristic malware analysis of dependency source | Docker-only on Windows; no Maven; deferred |
| Future | Trivy (Aqua) | IaC misconfig, containers, licenses | dep scanning redundant with osv-scanner; only earns a slot for IaC/containers |
| Dropped | cargo-audit | Rust deps | redundant — RustSec exports to OSV in real time |
| Dropped | Bearer | SAST | no Windows binaries, no Rust |

Key properties that shape the design:

1. **SCA tools read manifests/lockfiles, not installed packages** — scans work
   on a fresh clone with no build step, from the project root (`launch_cwd`),
   which is exactly the directory cImp already operates on.
2. **All v1 tools emit SARIF** — the V22 `sarif` parser normalizes everything
   into `Diag` with zero new parsing code.
3. **Findings-present is a non-zero exit** for all three tools — the runner
   must distinguish "exit 1 with a report" (success + findings) from spawn
   failure/timeout (error).
4. **Nothing is bundled.** Tools resolve ebin → PATH with an explicit
   per-tool path override (Browse) — the same unbundled model rustnet/broot
   are moving to.

Deliverables: a **Code Audit** reserved dashboard tab (user-triggered scan,
per-tool progress, one aggregated findings table, selection + copy-to-agent),
and a **Code Audit settings category** (tool list with path/Detect/Browse,
per-tool enable + args, scan settings).

Non-goals for v1: scheduled/automatic scans, CI integration, GuardDog/Trivy
adapters (registry is data-driven so they're additive later), exposing scans
as an MCP tool to agents (future — see end), fixing findings.

---

## Phase A — Settings schema + Code Audit settings category

### Goal
A `code_audit` settings block and a Settings section where the user manages
the tool list and scan behavior. No schema-version bump.

### Design (`settings/schema.rs`)
- New `CodeAuditSettings` struct, additive `#[serde(default)]` (V8/V16
  precedent — old files round-trip):
  - `enabled: bool` (default **false**) — feature flag; gates the tab
    (mirrors `ui.tool_activity_tab` gating) and the bottom-bar entry point.
  - `tools: Vec<AuditToolConfig>` — default = the three v1 tools.
  - `timeout_secs: u64` (default 600) — per-tool wall clock.
- `AuditToolConfig { id: AuditToolId, enabled: bool, path: String,
  extra_args: Vec<String> }`
  - `id`: closed enum `osv-scanner | gitleaks | semgrep` (v1). Closed enum,
    not free-form: each id binds to a built-in adapter (Phase B). Unknown ids
    in the file are dropped with a warn (forward compat).
  - `path`: empty = resolve ebin → PATH via `pty::resolve` (the
    `ExternalToolsSettings` contract, `schema.rs:976-990`); non-empty = used
    verbatim.
  - `extra_args`: appended after the adapter's fixed argv (e.g. a custom
    semgrep `--config`).
- TS mirror in `src/lib/settings/types.ts`; extend the V22 tripwire pattern
  (a `checks/mod.rs`-style `include_str!` test) to pin `AuditToolId` wire
  names and `CodeAuditSettings` field names.

### Settings UI (`SettingsApp.svelte`)
- New `SectionId` `'code-audit'` + `SECTIONS` entry "Code Audit"
  (`SettingsApp.svelte:736-773`) — its own category as decided, not folded
  into Tools/Checks.
- Section body (inline section, external-tools precedent at
  `SettingsApp.svelte:2500-2571`):
  - Master toggle (show tab + enable feature).
  - Per-tool rows: enable checkbox · tool name + one-line role ("dependency
    vulnerabilities + known-malicious", "secrets", "SAST — requires Python,
    Windows support beta") · path input · **Detect** button · **Browse**
    button (`pickToolExe()` helper, `SettingsApp.svelte:1276`) · extra-args
    editor (reuse `ArrayEditor.svelte`).
  - **Detect**: IPC `audit_detect_tool(id)` → backend resolves via
    `pty::resolve` honoring the override, runs `<tool> --version`
    (short timeout), returns resolved path + version string or not-found.
    Result shown inline (`✓ v2.4.0 — C:\...\osv-scanner.exe` / `not found on
    PATH or ebin`). Never auto-writes the path field — display-only, so the
    stored config stays "resolve normally" unless the user browses.
  - Timeout field.

### Tests
Rust: serde default round-trip (old settings file → defaults present),
unknown-tool-id drop, tripwire. Vitest: section renders per-tool rows,
Detect result states.

---

## Phase B — Audit runner (`src-tauri/src/audit/`)

### Goal
A backend module that runs enabled tools concurrently against the project
root, normalizes SARIF into `Diag`s, and streams per-tool progress events.

### Design
- New module `src-tauri/src/audit/` (`mod.rs`, `adapters.rs`).
- **Adapter registry** (data-driven so GuardDog/Trivy are additive): per
  `AuditToolId` a static adapter describing:
  - fixed argv and report transport:
    - `osv-scanner scan source -r <root> --format sarif` → SARIF on stdout.
    - `gitleaks git <root> --report-format sarif --report-path <tmp> --exit-code 1`
      → SARIF from temp report file (gitleaks logs to stdout); fall back to
      `gitleaks dir` when `<root>` is not a git repo.
    - `semgrep scan --config auto --sarif --quiet <root>` → SARIF on stdout
      (adapter sets `PYTHONUTF8=1` in the child env per the beta-Windows
      requirement).
  - **exit-code semantics**: per-adapter set of "findings-present" exit codes
    (all three: 0 = clean, 1 = findings; anything else = tool error). This is
    the one place V22's checks model doesn't fit — `run_check` treats these
    tools' exit codes as failure; the audit runner owns this distinction.
- Temp report files go in the app scratch/temp dir, deleted after parse.
- **Parsing**: reuse `checks::parsers::parse(ParserKind::Sarif, …)`
  (`parsers.rs:32`, already `pub`) with `cwd = root` so paths normalize
  project-relative. Wrap results as
  `AuditFinding { tool: AuditToolId, diag: Diag }` — `Diag.code` carries the
  SARIF rule id, `Diag.severity` the level. If the SARIF parser currently
  drops fields the table needs (verify: rule id → `code`), extend it there so
  checks benefit too.
- **Run orchestration**: one scan = spawn all enabled+resolvable tools
  concurrently (they're independent; gitleaks finishes in seconds, semgrep in
  minutes — results stream per tool, no barrier). Single in-flight scan;
  re-trigger while running is rejected. Cancel = kill children.
- **State + events** (graph-status precedent, `graph/service.rs:59,2722`):
  - `AuditState` held in managed state: per-tool
    `{ status: idle|running|done|failed|not-installed, findings: Vec<AuditFinding>,
       duration_ms, error: Option<String>, resolved: Option<PathBuf> }`
    + `last_scan_at`, `root`.
  - Emit `audit-status` on every transition with the full snapshot
    (findings included — counts are small; if a pathological repo produces
    thousands, cap the wire payload and let the frontend fetch the rest via
    the snapshot IPC).
  - Last scan results live in managed state only (survive tab switch via the
    app-view keep-alive registry; not persisted across app restarts in v1).
- **IPC commands**: `audit_start_scan`, `audit_cancel_scan`,
  `audit_snapshot` (for mount), `audit_detect_tool` (Phase A).
- Record scan runs in the tool-activity store (`crate::activity`) —
  kind `audit`, per-kind cap like graph/offload.

### Tests
Adapter argv construction (incl. extra_args append, override path), exit-code
classification per tool, SARIF fixtures per tool (one real captured output
each) → expected `AuditFinding`s, not-installed path (resolve failure →
`not-installed`, scan proceeds with remaining tools), timeout → `failed`,
cancel kills children.

---

## Phase C — Code Audit tab

### Goal
The user-facing surface: trigger scan, watch progress, read one aggregated
findings table, select findings and copy them for Claude Code / OpenCode.

### Design
- **Reserved dashboard tab** `code-audit` — all touchpoints per the
  established pattern:
  - `state/manager.rs`: `TabId::CodeAudit` variant + `as_str`/`from_str`
    (`"code-audit"`) + `kind()` → Shell + `is_reserved_dashboard()` arm
    (`manager.rs:33-176`); `is_builtin()` follows automatically.
  - `src/lib/tabs/types.ts`: `CODE_AUDIT_TAB_ID`, `isCodeAuditTab()`,
    add to `isAppRenderedTab()` (`types.ts:49-104`).
  - `src/lib/appViews.ts`: `COMPONENTS` entry → `CodeAuditView.svelte`
    (keep-alive portal — scan keeps streaming while the tab is hidden).
  - Tab appears when `code_audit.enabled` (gating mirrors
    `ui.tool_activity_tab`); no settings-schema migration.
- **`CodeAuditView.svelte`** layout:
  - Header: project root, **Scan** / **Cancel** button, last-scan timestamp.
  - Per-tool status chips: `idle · running(spinner) · ✓ N findings ·
    ✗ error(tooltip) · not installed`. "Not installed" chip links to the
    Code Audit settings section. Results appear per tool as each finishes.
  - **Findings table** (one merged list): columns
    `☑ · severity · tool · rule · file:line · message`. Default sort:
    severity desc, then tool. Filters: severity threshold, per-tool toggle,
    text filter. `file:line` click jumps like the Workbench ⌖ pattern where
    applicable.
  - **Selection + copy** (the transfer mechanism): per-row checkboxes +
    header actions **Select all · Deselect all · Copy selected (N)**.
    "Select all" respects active filters (selects the visible set).
    Copy writes markdown to the clipboard via the tauri clipboard plugin
    (WebView2 `navigator.clipboard` is denied — established gotcha):

    ```
    ## Code audit findings (12 of 87 selected) — <root>, scanned 2026-07-15 14:02
    - [high] osv-scanner GHSA-xxxx-…: `tokio 1.38.0` vulnerable … (Cargo.lock)
    - [high] gitleaks generic-api-key: possible API key — src/lib/foo.ts:42
    - [med ] semgrep js.lang.security.audit…: … — src/SettingsApp.svelte:1291
    ```

    Paste target is a Claude Code / OpenCode tab prompt — the format is
    agent-ready (severity, rule id, file:line, message; project-relative
    paths so the agent can act on them).
  - UI state (filters, sort) via `viewSection.ts` localStorage; findings data
    always from backend snapshot on mount (`audit_snapshot`).
- Entry points: the tab itself; optionally a bottom-bar glyph
  (`ToolLaunchButton` precedent) that activates the tab — decide during
  implementation, not a spec commitment.

### Tests
Vitest: selection store (select all under filter, deselect, copy count),
markdown formatter (fixture findings → exact string), status-chip state map,
event-merge reducer (per-tool arrival order independence). Svelte-check
clean.

---

## Phase D — Polish + docs

- **Scan-coverage honesty line**: after a scan, show what was actually
  scanned ("Cargo.lock ✓ · package-lock.json ✓ · build.gradle — no lockfile,
  run `gradle dependencies --write-locks`"), sourced from osv-scanner's SARIF
  artifacts/invocation records. A "0 findings" from an unscannable ecosystem
  must not read as a clean bill of health. (Best-effort v1: list the
  lockfiles osv-scanner reports; full manifest gap analysis is future.)
- Network reality in UI copy: osv-scanner queries the OSV API / deps.dev;
  first semgrep run downloads rules — offline scans degrade, surface the
  tool's own error rather than a generic failure.
- FEATURES.md section, CHANGELOG entry, MAINTENANCE.md live-verify recipe.

### Live verification (run by hand before release)
1. Fresh clone of this repo, no tools on PATH → tab shows all three
   not-installed; Settings Detect agrees.
2. Drop `osv-scanner.exe` + `gitleaks.exe` in `ebin/` → Detect finds them;
   Scan produces findings (this repo's `Cargo.lock` + planted test secret in
   a scratch branch); exit-code 1 classified as findings, not error.
3. Verify the `MAL-*` claim: scan a scratch `package.json`+lockfile pinning a
   known-malicious package version from OpenSSF malicious-packages → finding
   appears via osv-scanner. (Research verified RustSec→OSV export; the
   MAL-in-default-scan behavior was high-confidence but unverified.)
4. `pipx install semgrep` (with `PYTHONUTF8=1`) → semgrep chip lights up,
   SARIF findings merge into the table.
5. Select 2 findings → Copy selected → paste into the Claude tab prompt;
   formatting intact, paths project-relative and clickable by the agent.
6. Cancel mid-semgrep-scan → children killed, partial results retained.
7. Timeout path: set timeout 5s, run semgrep on a large repo → `failed`
   chip with timeout error, other tools unaffected.

---

## Future (explicitly out of v1)
- **GuardDog adapter** (detect-if-present: PATH or Docker; `verify` mode
  SARIF on dependency files; no Maven coverage — pairs with, not replaces,
  osv-scanner).
- **Trivy adapter** for IaC misconfig/containers.
- **Bearer** as a Linux-only adapter once the Linux milestone lands.
- **Agent access**: expose the last scan as an MCP tool (`audit_findings`)
  or fold audit tools into checks with a `findings-exit-code` field — lets
  Claude/OpenCode read findings without the copy step. Deliberately deferred:
  the copy flow keeps the human in the loop for v1.
- Scheduled scans / scan-on-branch-switch; persistence of findings across
  restarts; suppression/baseline files (`.gitleaksignore`, semgrep
  `.semgrepignore` already respected tool-side).

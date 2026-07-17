// V23 Phase C: frontend mirror of the Rust audit runner wire shapes
// (`src-tauri/src/audit/runner.rs`). The `audit_snapshot` IPC and the
// `audit-status` event share this exact shape — the event caps each tool's
// `findings` at 500 and sets `truncated: true` when it did, but
// `total_findings` is always the TRUE total; on `truncated` the view fetches
// the full (uncapped) set via `audit_snapshot`.
//
// The settings-side audit types (`AuditToolId`, `AuditToolConfig`,
// `CodeAuditSettings`, `AuditDetectResult`) live in `../settings/types` — Phase
// A only mirrored the settings block, so the runtime snapshot/event/finding
// types are new here.

import type { AuditToolId } from '../settings/types';

export type { AuditToolId };

/// Wire severity from the Rust `checks::Severity` (serde lowercase). Ordered
/// error > warning > note for the table's default sort (see `SEVERITY_RANK`).
export type AuditSeverity = 'error' | 'warning' | 'note';

/// Per-tool lifecycle status within a scan (mirror of Rust `ToolStatus`,
/// kebab-case): `idle` (configured but not part of this scan), `running`,
/// `done` (ran to completion — `findings` authoritative even when empty),
/// `failed` (tool error / timeout / cancel), `not-installed` (no path
/// configured and not found on PATH/ebin), `path-invalid` (the CONFIGURED
/// path doesn't resolve — fix it in Settings), `skipped-not-applicable`
/// (V25: enabled but the project's census doesn't match this tool — e.g. no
/// PMD in a Rust repo; the split UI hides it in the tab while Settings still
/// lists it).
export type AuditToolStatus =
  | 'idle'
  | 'running'
  | 'done'
  | 'failed'
  | 'not-installed'
  | 'path-invalid'
  | 'skipped-not-applicable';

/// V25 Phase C: which tab/section a tool belongs to (mirror of Rust
/// `audit::adapters::Category`, serde lowercase). A scan runs one category; the
/// `audit_start_scan` command takes it and every `AuditToolState` is tagged with
/// it, so the two tabs filter the one shared snapshot to their own tools.
export type AuditCategory = 'security' | 'quality';

/// One diagnostic (mirror of Rust `checks::Diag`). `code` is the SARIF rule id;
/// `line` is 0 when the tool reported no line; `col` is `null` when absent.
export interface AuditDiag {
  severity: AuditSeverity;
  code: string | null;
  message: string;
  file: string;
  line: number;
  col: number | null;
}

/// One raw finding: a diag tagged with the tool that produced it (mirror of
/// Rust `AuditFinding`).
export interface AuditFinding {
  tool: AuditToolId;
  diag: AuditDiag;
}

/// One tool's live state within the current (or last) scan (mirror of Rust
/// `ToolState`). `error` is set for `failed`/`not-installed`; `resolved` is the
/// resolved binary path (a Windows backslash string) once resolution succeeds.
export interface AuditToolState {
  id: AuditToolId;
  /// V25 Phase C: the tool's category. Every state in one snapshot shares it
  /// (a scan runs one category); the split UI filters the shared snapshot by it.
  category: AuditCategory;
  status: AuditToolStatus;
  findings: AuditFinding[];
  duration_ms: number;
  error: string | null;
  resolved: string | null;
  /// Lockfiles / manifests this tool reported *scanning* (SARIF
  /// `runs[].artifacts`), project-relative. Populated for `osv-scanner` only;
  /// empty for the other tools / an older osv-scanner. Drives the tab's
  /// scan-coverage line so a "0 findings" run from an unscannable ecosystem
  /// isn't mistaken for a clean bill of health.
  scanned_artifacts: string[];
}

/// V25 Phase C: the scanned root's language census (mirror of Rust
/// `CensusBlock`) — the extensions/markers seen. Drives split-UI chip visibility
/// (gate a tool out of a project it doesn't apply to) off the one snapshot,
/// without a second IPC. Both lists empty before the first scan; the last scan's
/// census is retained afterward.
export interface AuditCensus {
  /// Lowercase, dot-less file extensions seen (sorted).
  extensions: string[];
  /// Marker tokens seen — `go.mod`, `Cargo.toml`, `package.json`, `*.sln`,
  /// `*.csproj`, `eslint.config`, `.eslintrc` (sorted).
  markers: string[];
}

/// The whole runner snapshot (mirror of Rust `AuditSnapshot`) — the
/// `audit_snapshot` return and the `audit-status` event payload.
export interface AuditSnapshot {
  /// Absolute project root (display string).
  root: string;
  /// Whether a scan is in flight right now.
  scanning: boolean;
  /// Epoch millis when the last scan started; `null` before the first scan.
  last_scan_at: number | null;
  /// Per-tool state, in configured order. Contains only the LAST scanned
  /// category's tools (a scan runs one category); empty before the first scan.
  /// The split UI filters by `AuditToolState.category` and renders from settings
  /// until its own category's tools appear here.
  tools: AuditToolState[];
  /// V25 Phase C: the scanned root's language census — drives chip visibility.
  census: AuditCensus;
  /// True total findings across all tools, BEFORE any wire cap.
  total_findings: number;
  /// Set when the event dropped findings to the per-tool cap; fetch the full
  /// set via `audit_snapshot`. Always `false` from the `audit_snapshot` IPC.
  truncated: boolean;
}

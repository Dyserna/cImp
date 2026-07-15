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
/// `failed` (tool error / timeout / cancel), `not-installed` (unresolvable).
export type AuditToolStatus = 'idle' | 'running' | 'done' | 'failed' | 'not-installed';

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

/// The whole runner snapshot (mirror of Rust `AuditSnapshot`) — the
/// `audit_snapshot` return and the `audit-status` event payload.
export interface AuditSnapshot {
  /// Absolute project root (display string).
  root: string;
  /// Whether a scan is in flight right now.
  scanning: boolean;
  /// Epoch millis when the last scan started; `null` before the first scan.
  last_scan_at: number | null;
  /// Per-tool state, in configured order. Empty before the first scan (render
  /// the configured tool list from settings until runtime state exists).
  tools: AuditToolState[];
  /// True total findings across all tools, BEFORE any wire cap.
  total_findings: number;
  /// Set when the event dropped findings to the per-tool cap; fetch the full
  /// set via `audit_snapshot`. Always `false` from the `audit_snapshot` IPC.
  truncated: boolean;
}

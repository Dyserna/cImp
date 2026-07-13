// V22 — `run_check` checks: frontend IPC wrappers for the Settings ChecksEditor
// (Phase E) and the Code Intelligence suggestion chip (Phase D). The backend
// commands live in `ipc/commands.rs`; the pure decision logic (conditional
// fields, test-result classification, chip derivation) lives in
// `settings/checksEditor.ts` so it stays unit-testable without a Tauri host.

import { invoke } from '@tauri-apps/api/core';
import type {
  CheckDef,
  ChecksApplySummary,
  ChecksProposal,
  ChecksSuggestion,
  ChecksTestResult,
} from './settings/types';

/// V22 Phase D: detect the project's languages/tooling and return `run_check`
/// proposals (marker + code-graph evidence, PATH-validated). `root` defaults
/// (backend side) to the launch directory.
export function checksDetect(root?: string): Promise<ChecksProposal[]> {
  return invoke<ChecksProposal[]>('checks_detect', { root: root ?? null });
}

/// V22 Phase D: merge the selected proposal checks into the project's `checks`
/// setting (by `name`, honoring the `auto`-ownership rule) through the normal
/// settings path. Returns the names actually written.
export function checksApplyProposals(
  checks: CheckDef[],
  root?: string,
): Promise<ChecksApplySummary> {
  return invoke<ChecksApplySummary>('checks_apply_proposals', {
    root: root ?? null,
    checks,
  });
}

/// V22 Phase D: the passive-nudge payload for the Code Intelligence chip — how
/// many VALID proposals exist for a project whose `checks` is empty, plus the
/// dismissed/auto-configure flags.
export function checksSuggestion(root?: string): Promise<ChecksSuggestion> {
  return invoke<ChecksSuggestion>('checks_suggestion', { root: root ?? null });
}

/// V22 Phase D: remember that the user dismissed the suggestion nudge for this
/// project (per-project overlay). Idempotent.
export function checksDismissSuggestion(): Promise<void> {
  return invoke<void>('checks_dismiss_suggestion');
}

/// V22 Phase E: dry-run one (possibly unsaved) check through `checks::run`
/// (`changed_only = false`) — exit status, parsed diagnostic count, the first
/// few diagnostics, captured output sizes, or an inline `error`.
export function checksTest(def: CheckDef, root?: string): Promise<ChecksTestResult> {
  return invoke<ChecksTestResult>('checks_test', { root: root ?? null, def });
}

/// V22 Phase E: validate a `regex-custom` pattern (live, debounced feedback) —
/// the same check the save path applies. Resolves on success; rejects with the
/// exact error string a save would produce.
export function checksValidatePattern(pattern: string): Promise<void> {
  return invoke<void>('checks_validate_pattern', { pattern });
}

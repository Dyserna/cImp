// Frontend mirror of `state::TabId`. JSON-serialized as a string —
// `"claude"` / `"aider"` for the AI builtins, or any user-managed shell ID
// (`"shell-1"` for the M1 hardcoded one). The union shape preserves
// autocomplete on the well-known IDs while leaving room for the dynamic
// shell IDs M2/M3 will introduce.

export type TabId = 'claude' | 'aider' | (string & {});

/// Type guard for shell tabs — currently every non-builtin ID is a shell.
export function isShellTab(id: TabId): boolean {
  return id !== 'claude' && id !== 'aider';
}

export const ALL_TABS: readonly TabId[] = ['claude', 'aider', 'shell-1'] as const;

/// Subset of `ALL_TABS` covering only the AI builtins. Used by call sites
/// that touch the v1.1-shaped `Settings.tabs` map (which still keys by the
/// `claude`/`aider` field names — Shell tabs live under the separate
/// `_shell_1_tmp` field until M3).
export type AiTabId = 'claude' | 'aider';
export const AI_TABS: readonly AiTabId[] = ['claude', 'aider'] as const;

export interface TabMeta {
  id: TabId;
  label: string;
}

// M3 reads this from settings. M1 hardcodes the three entries; the Shell-1
// label is the default — user renames flow through the settings field
// (`_shell_1_tmp.name`) and the Tab Bar should ideally read that, but the
// shape doesn't change so M1 keeps the static label here. (M2's right-click
// rename UI promotes this to a reactive store.)
export const TAB_META: readonly TabMeta[] = [
  { id: 'claude', label: 'Claude Code' },
  { id: 'aider', label: 'Aider' },
  { id: 'shell-1', label: 'Shell 1' },
] as const;

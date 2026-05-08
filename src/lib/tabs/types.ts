// Frontend mirror of `state::TabId`. JSON-serialized as a string —
// `"claude"` / `"claude-local"` for the AI builtins, `"shell-default-1"`
// for the reserved default Shell tab, or `shell-<uuid>` for user-created
// shell tabs. The union shape preserves autocomplete on the well-known
// IDs while leaving room for the dynamic shell IDs created at runtime.

export type TabId = 'claude' | 'claude-local' | (string & {});

/// Type guard for shell tabs — every non-AI-builtin ID is a shell.
export function isShellTab(id: TabId): boolean {
  return id !== 'claude' && id !== 'claude-local';
}

/// Subset of TabId covering only the AI builtins. Used by call sites that
/// need to iterate over just the AI tabs (e.g. the Settings window's
/// "Reset to default" wiring, which is meaningful only for AI tabs).
export type AiTabId = 'claude' | 'claude-local';
export const AI_TABS: readonly AiTabId[] = ['claude', 'claude-local'] as const;

export type TabKind = 'ai-tool' | 'shell';

export interface TabMeta {
  id: TabId;
  kind: TabKind;
  name: string;
  builtin: boolean;
}

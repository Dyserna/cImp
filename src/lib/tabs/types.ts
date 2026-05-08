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

/// True for tabs that ship with the app and cannot be closed/removed.
/// Mirrors the backend's `builtin: true` field on the `TabCreated` event;
/// useful when the event payload isn't in scope (e.g., quick lookups in
/// the title-bar or shortcut-dispatcher paths). The `shell-default-1`
/// reserved id is *not* a builtin: it's a regular closable shell that
/// ships on fresh installs.
export function isBuiltinTab(id: TabId): boolean {
  return id === 'claude' || id === 'claude-local';
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

/// Static metas for the AI builtins, used by call sites (notably the
/// SettingsApp window) that need a stable list independent of the runtime
/// `tabs` store. The names match the backend's launch-seed defaults. The
/// id field is narrowed to `AiTabId` so call sites can safely index the
/// v1.1-shaped `Settings.tabs` map.
export interface AiTabMeta extends TabMeta {
  id: AiTabId;
  builtin: true;
}
export const AI_TAB_META: readonly AiTabMeta[] = [
  { id: 'claude', kind: 'ai-tool', name: 'Claude', builtin: true },
  { id: 'claude-local', kind: 'ai-tool', name: 'Claude (local)', builtin: true },
] as const;

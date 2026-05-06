// Frontend mirror of `state::TabId`. JSON-serialized as a string —
// `"claude"` / `"aider"` for the AI builtins, or any user-managed shell ID
// (`"shell-1"` for the M1 hardcoded one, `shell-<uuid>` for user-created).
// The union shape preserves autocomplete on the well-known IDs while leaving
// room for the dynamic shell IDs M2 introduces at runtime.

export type TabId = 'claude' | 'aider' | (string & {});

/// Type guard for shell tabs — currently every non-builtin ID is a shell.
export function isShellTab(id: TabId): boolean {
  return id !== 'claude' && id !== 'aider';
}

/// True for tabs that ship with the app and cannot be closed/removed.
/// Mirrors the backend's `builtin: true` field on the `TabCreated` event;
/// useful when the event payload isn't in scope (e.g., quick lookups in
/// the title-bar or shortcut-dispatcher paths).
export function isBuiltinTab(id: TabId): boolean {
  return id === 'claude' || id === 'aider';
}

/// Subset of TabId covering only the AI builtins. Used by call sites that
/// touch the v1.1-shaped `Settings.tabs` map (which still keys by the
/// `claude`/`aider` field names — Shell tabs live under the separate
/// `_shell_1_tmp` field until M3).
export type AiTabId = 'claude' | 'aider';
export const AI_TABS: readonly AiTabId[] = ['claude', 'aider'] as const;

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
  { id: 'aider', kind: 'ai-tool', name: 'Aider', builtin: true },
] as const;

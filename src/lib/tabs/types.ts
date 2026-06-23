// Frontend mirror of `state::TabId`. JSON-serialized as a string —
// `"claude"` / `"claude-local"` / `"aider"` / `"aider-local"` for the
// AI builtins, `"shell-default-1"` for the reserved default Shell tab,
// or `shell-<uuid>` for user-created shell tabs. The union shape
// preserves autocomplete on the well-known IDs while leaving room for
// the dynamic shell IDs created at runtime.

export type TabId =
  | 'claude'
  | 'claude-local'
  | 'aider'
  | 'aider-local'
  | (string & {});

/// V8-03: the reserved id of the read-only Offload Server tab. Internally a
/// Shell-kind tab, but the frontend keys off this id to render read-only,
/// log-fed content (no PTY) and suppress shell-tab affordances.
export const OFFLOAD_SERVER_TAB_ID = 'offload-server';

/// True for the read-only Offload Server tab.
export function isOffloadTab(id: TabId): boolean {
  return id === OFFLOAD_SERVER_TAB_ID;
}

/// Type guard for shell tabs — every non-AI-builtin ID is a shell, EXCEPT the
/// Offload Server tab, which is read-only and log-fed (it must not get the
/// shell closed-overlay / restart / keystroke behaviors).
export function isShellTab(id: TabId): boolean {
  return (
    id !== 'claude' &&
    id !== 'claude-local' &&
    id !== 'aider' &&
    id !== 'aider-local' &&
    id !== OFFLOAD_SERVER_TAB_ID
  );
}

/// Subset of TabId covering only the AI builtins. Used by call sites that
/// need to iterate over just the AI tabs (e.g. the Settings window's
/// "Reset to default" wiring, which is meaningful only for AI tabs).
export type AiTabId = 'claude' | 'claude-local' | 'aider' | 'aider-local';
export const AI_TABS: readonly AiTabId[] = [
  'claude',
  'claude-local',
  'aider',
  'aider-local',
] as const;

/// Type guard for the Aider pair — useful when the Settings UI gates
/// per-tool behavior (e.g. hiding the TTS-injection rows because Aider
/// has no `--append-system-prompt` equivalent).
export function isAiderTabId(id: string): boolean {
  return id === 'aider' || id === 'aider-local';
}

export type TabKind = 'ai-tool' | 'shell';

export interface TabMeta {
  id: TabId;
  kind: TabKind;
  name: string;
  builtin: boolean;
}

// Frontend mirror of `state::TabId`. JSON-serialized as the lowercased
// variant name (`"claude"` / `"aider"`).

export type TabId = 'claude' | 'aider';

export const ALL_TABS: readonly TabId[] = ['claude', 'aider'] as const;

export interface TabMeta {
  id: TabId;
  label: string;
}

export const TAB_META: readonly TabMeta[] = [
  { id: 'claude', label: 'Claude Code' },
  { id: 'aider', label: 'Aider' },
] as const;

// Frontend mirror of `state::TabId`. JSON-serialized as a string —
// `"claude"` / `"claude-local"` / `"opencode"` for the
// AI builtins, `"shell-default-1"` for the reserved default Shell tab,
// or `shell-<uuid>` for user-created shell tabs. The union shape
// preserves autocomplete on the well-known IDs while leaving room for
// the dynamic shell IDs created at runtime.

export type TabId =
  | 'claude'
  | 'claude-local'
  | 'opencode'
  | (string & {});

/// V8-03: the reserved id of the read-only Offload Server tab. Internally a
/// Shell-kind tab, but the frontend keys off this id to render read-only,
/// log-fed content (no PTY) and suppress shell-tab affordances.
export const OFFLOAD_SERVER_TAB_ID = 'offload-server';

/// True for the read-only Offload Server tab.
export function isOffloadTab(id: TabId): boolean {
  return id === OFFLOAD_SERVER_TAB_ID;
}

/// V9-01: the reserved id of the read-only, app-rendered Code Graph monitor
/// tab. Like the Offload Server tab it's Shell-kind on the backend but the
/// frontend keys off this id to render a dashboard (no PTY).
export const GRAPH_MONITOR_TAB_ID = 'graph-monitor';

/// True for the read-only Code Graph monitor tab.
export function isGraphMonitorTab(id: TabId): boolean {
  return id === GRAPH_MONITOR_TAB_ID;
}

/// The reserved id of the singleton Note scratchpad tab. Shell-kind on the
/// backend (an ordinary closable tab), but the frontend keys off this id to
/// render the `NoteView` editor with no PTY — like the Offload/Graph tabs, its
/// behavior is decided by the id, not the kind.
export const NOTE_TAB_ID = 'note';

/// True for the Note scratchpad tab.
export function isNoteTab(id: TabId): boolean {
  return id === NOTE_TAB_ID;
}

/// V13 Phase A: the reserved id of the read-only, app-rendered Workbench tab
/// (live diff pane / checkpoint timeline / worktrees, sectioned like Code
/// Intelligence). Shell-kind on the backend — no PTY — same pattern as the
/// Code Graph monitor tab (`:27`).
export const WORKBENCH_TAB_ID = 'workbench-1';

/// True for the Workbench tab.
export function isWorkbenchTab(id: TabId): boolean {
  return id === WORKBENCH_TAB_ID;
}

/// V15 Feature 4: the reserved id of the read-only, app-rendered Graph View tab
/// (live 2D/3D force-graph of the code graph). Shell-kind on the backend — no
/// PTY — same pattern as the Code Graph monitor tab. Materialized only while
/// `graph.graph_viz` is on.
export const GRAPH_VIEW_TAB_ID = 'graph-view';

/// True for the Graph View tab.
export function isGraphViewTab(id: TabId): boolean {
  return id === GRAPH_VIEW_TAB_ID;
}

/// The reserved id of the read-only, app-rendered Tool Activity tab — the
/// unified feed of graph-tool calls + offload requests, plus the graph/offload
/// tool reference lists. Shell-kind on the backend — no PTY — same pattern as
/// the Code Graph monitor tab. Materialized while `ui.tool_activity_tab` is on
/// (default true).
export const TOOL_ACTIVITY_TAB_ID = 'tool-activity';

/// True for the Tool Activity tab.
export function isToolActivityTab(id: TabId): boolean {
  return id === TOOL_ACTIVITY_TAB_ID;
}

/// True for a Preview tab's id (`"preview-<uuid>"` — see `create_preview_tab`
/// / `TabId::Preview` on the backend). Unlike the reserved dashboards above,
/// there's no single constant to compare against (Preview is repeatable),
/// so this checks the id's shape instead — the same convention the backend's
/// own `TabId::from_str` uses to round-trip the variant.
export function isPreviewTabId(id: TabId): boolean {
  return id.startsWith('preview-');
}

/// Type guard for shell tabs — every non-AI-builtin ID is a shell, EXCEPT the
/// Offload Server, Code Graph monitor, Note, and Workbench tabs (app-rendered
/// dashboards) and Preview tabs (an embedded webview, not a PTY) — none of
/// these get the shell closed-overlay / restart / keystroke behaviors.
export function isShellTab(id: TabId): boolean {
  return (
    id !== 'claude' &&
    id !== 'claude-local' &&
    id !== 'opencode' &&
    id !== OFFLOAD_SERVER_TAB_ID &&
    id !== GRAPH_MONITOR_TAB_ID &&
    id !== NOTE_TAB_ID &&
    id !== WORKBENCH_TAB_ID &&
    id !== GRAPH_VIEW_TAB_ID &&
    id !== TOOL_ACTIVITY_TAB_ID &&
    !isPreviewTabId(id)
  );
}

/// Subset of TabId covering only the AI builtins. Used by call sites that
/// need to iterate over just the AI tabs (e.g. the Settings window's
/// "Reset to default" wiring, which is meaningful only for AI tabs).
export type AiTabId = 'claude' | 'claude-local' | 'opencode';
export const AI_TABS: readonly AiTabId[] = [
  'claude',
  'claude-local',
  'opencode',
] as const;

/// Type guard for the single OpenCode tab.
export function isOpencodeTabId(id: string): boolean {
  return id === 'opencode';
}

/// V14 Phase F: `'preview'` is a genuinely new kind (unlike the reserved
/// app-rendered dashboards above — Workbench/Graph-monitor/Note/Offload —
/// which are all `'shell'`-kind with a reserved id). Preview is
/// user-creatable and repeatable, so the frontend needs the real wire kind
/// (mirrors the Rust `TabKindWire::Preview`) rather than an id-sniffing
/// predicate.
export type TabKind = 'ai-tool' | 'shell' | 'preview';

export interface TabMeta {
  id: TabId;
  kind: TabKind;
  name: string;
  builtin: boolean;
}

// Frontend mirror of `state::TabId`. JSON-serialized as a string — a reserved
// built-in AI tab id for each harness the registry declares (see
// `../harness.ts`), `"shell-default-1"` for the reserved default Shell tab, or
// `shell-<uuid>` / `ai-<uuid>` for user-created ones.
//
// Deliberately a bare `string`: which ids are the AI builtins is the registry's
// answer, delivered over `harness_list`, and a union here would be this file
// re-declaring the roster (V40 Phase F, locked decision 7). Ask
// `isReservedAiTab` / `harnessForTab` instead of comparing.

import { isReservedAiTab } from '../harness';

export type TabId = string;

// The V8-03 "offload-server" reserved tab is retired (schema v25) — the
// dashboard lives inside the Tool Activity tab as the "Offload server"
// section now (ToolActivityView.svelte); the v24 → v25 migration drops old
// persisted entries.

/// V9-01: the reserved id of the read-only, app-rendered Code Graph monitor
/// tab. Shell-kind on the backend, but the frontend keys off this id to
/// render a dashboard (no PTY).
export const GRAPH_MONITOR_TAB_ID = 'graph-monitor';

/// True for the read-only Code Graph monitor tab.
export function isGraphMonitorTab(id: TabId): boolean {
  return id === GRAPH_MONITOR_TAB_ID;
}

/// The reserved id of the singleton Note scratchpad tab. Shell-kind on the
/// backend (an ordinary closable tab), but the frontend keys off this id to
/// render the `NoteView` editor with no PTY — like the Graph monitor tab, its
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

// The V15 "graph-view" reserved tab is retired (schema v26) — the live
// force-graph lives inside the Tool Activity tab as the "Graph view" section
// now (ToolActivityView.svelte); the v25 → v26 migration drops old persisted
// entries.

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

/// #51: the reserved id of the read-only, app-rendered Events tab — the same
/// persistent activity feed the Tool Activity tab shows, but with the #51
/// attribution columns (which tab / which session) visible and filterable.
/// Shell-kind on the backend — no PTY — same pattern as the Code Graph monitor
/// tab.
///
/// ADDITIVE: the Tool Activity tab and its Activities section stay exactly as
/// they were. The two overlap on purpose for now — the consolidation is a
/// later, separate decision.
export const EVENTS_TAB_ID = 'events';

/// True for the Events tab.
export function isEventsTab(id: TabId): boolean {
  return id === EVENTS_TAB_ID;
}

// The V23 "code-audit" reserved tab is retired (schema v27) — the Security |
// Quality audit panels live inside the Tool Activity tab as the "Code audit"
// section now (ToolActivityView.svelte); the v26 → v27 migration drops old
// persisted entries.

/// True for a Preview tab's id (`"preview-<uuid>"` — see `create_preview_tab`
/// / `TabId::Preview` on the backend). Unlike the reserved dashboards above,
/// there's no single constant to compare against (Preview is repeatable),
/// so this checks the id's shape instead — the same convention the backend's
/// own `TabId::from_str` uses to round-trip the variant.
export function isPreviewTabId(id: TabId): boolean {
  return id.startsWith('preview-');
}

/// THE single source of truth for "app-rendered, no-PTY" tabs: the reserved
/// dashboards (Code Graph monitor, Note, Workbench, Tool Activity, Events) plus
/// Preview tabs (an embedded webview). Every guard
/// that used to hand-enumerate these must call this instead — a new
/// app-rendered tab is added HERE (plus its own isXTab predicate above) and
/// nowhere else. Mirrors the Rust side's reserved-tab set.
export function isAppRenderedTab(id: TabId): boolean {
  return (
    isGraphMonitorTab(id) ||
    isNoteTab(id) ||
    isWorkbenchTab(id) ||
    isToolActivityTab(id) ||
    isEventsTab(id) ||
    isPreviewTabId(id)
  );
}

/// Type guard for shell tabs — every id that is not a reserved AI builtin is a
/// shell, EXCEPT the app-rendered tabs (see `isAppRenderedTab`) — none of these
/// get the shell closed-overlay / restart / keystroke behaviors.
///
/// V40 Phase F: "is this a reserved AI builtin?" is the registry's question now
/// (`harness.ts`), not a `!==` chain per shipped harness.
export function isShellTab(id: TabId): boolean {
  return !isReservedAiTab(id) && !isAppRenderedTab(id);
}

/// The subset of `TabId` covering the reserved AI builtins. A plain `string`
/// for the same reason `TabId` is: the roster comes from `harness_list`. Kept
/// as a named alias because the call sites that iterate *only* the AI builtins
/// (the Settings window's per-tab forms, `enabled_ai_tabs`) read better for it —
/// they get the list itself from `reservedAiTabIds($harnesses)`.
export type AiTabId = string;

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

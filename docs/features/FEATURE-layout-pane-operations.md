# Feature: Layout & Pane Operations

## Purpose

A bag of small/medium tweaks to the v1.3 layout system. Each item is independently shippable and self-contained — the group exists because they all touch the same files (`src/lib/layout/`, `src/lib/Pane.svelte`, `src/lib/LayoutNodeRenderer.svelte`, `src/lib/shortcuts/dispatcher.ts`) and share UX vocabulary (pane menus, layout-tree walks, shortcut bindings). Implement them in any order as triggers fire; do not wait for a "v1.4 layout polish" milestone.

See `FUTURE-FEATURES.md` for the full per-item rationale and trigger-to-act conditions; this doc captures the implementation strategy and shared decisions.

## Items in this group

1. **Pane numbering for `Ctrl+Alt+1..9` direct focus** — number badge on each pane, Nth-leaf shortcut.
2. **Maximize pane / Zen mode** — `Ctrl+Shift+Z` toggles a temporary single-pane view.
3. **Pane swap** — right-click a pane → "Swap with..." → pick another pane.
4. **Split ratio quick-presets** — right-click splitter → 50/50, 70/30, 30/70.
5. **Hide pane tab bar when only one tab is in the pane** — CSS-driven, with a fallback right-click target for the pane menu.
6. **Pane-level DoneWhileAway indicator** — accent border on any pane whose tabs include a `DoneWhileAway` flag.
7. **Custom keyboard shortcuts UI for layout actions** — Settings UI for binding capture, conflict detection.
8. **Pane-aware compose overlay** — target-tab dropdown locked at compose-open time.

## Shared design

### Pane enumeration

Items 1, 3, 8 all need an "Nth pane" or "list of all panes" derived from the layout tree. Add one helper in `src/lib/layout/tree.ts`:

```ts
// In-order traversal of leaves. Order is stable for a given tree shape;
// numbering shifts as the tree is mutated, which is the desired behavior
// (users orient by what's visible *now*).
export function enumeratePanes(root: LayoutNode): PaneNode[] { ... }
```

Pane numbers are *not* persisted — they're derived per render. A small `<span class="pane-number">{i+1}</span>` in `Pane.svelte`'s tab bar shows the current index. Item 1's `Ctrl+Alt+N` handler runs `enumeratePanes(root)[N-1]?.id` and dispatches focus.

### Pane context menu surface

Items 3, 4, 5 need a right-click target on the pane chrome (not the terminal). The existing pane menu already lives on the tab bar background. For item 5 (hidden tab bar), accept the small UX regression: when the bar is hidden, the right-click target falls back to the *splitter* if one is adjacent, or the user uses keyboard shortcuts. Don't try to make xterm.js's right-click area double as a pane menu — it conflicts with terminal context behavior.

### Layout-tree mutations

Items 2 (Zen) and 3 (Swap) both mutate the tree:

- **Zen**: stash the current `LayoutPersisted` snapshot in a non-persisted in-memory `zenStash` store; replace the tree with `Pane { tab_ids: focusedPane.tab_ids, active_tab_id: focusedPane.active_tab_id }`. On toggle-off (or any layout-mutating action that isn't toggle-off itself), restore from `zenStash` and clear it. Persist nothing while in Zen mode — the ephemeral state should not survive a crash.
- **Swap**: tree-level operation in `tree.ts` that takes two pane ids and swaps their `tab_ids` + `active_tab_id` (not their parent pointers — moving subtrees risks unintuitive reflows). Reuses the v1.3 "Move all tabs to" submenu pattern in `TabContextMenu.svelte` / pane menu.

### Shortcut binding surface

Items 1, 2, 7 all touch `src/lib/shortcuts/{dispatcher,parser}.ts` and the `shortcuts` settings field. Item 7 (UI) is the larger one — see its section below for the conflict-detection design. Items 1, 2 add new bindings:

- `Ctrl+Alt+1..9` — direct pane focus (binds to a parameterized action `focus_pane_n` with n=1..9, or 9 distinct entries; pick whichever is simpler in the dispatcher's current shape)
- `Ctrl+Shift+Z` — `toggle_zen_mode`

Both follow v1.3's existing convention (action name in the `shortcuts` settings field, parsed key combo, dispatched via the existing layer).

## Per-item implementation notes

### 1. Pane numbering

- Add `enumeratePanes` helper to `tree.ts`.
- `Pane.svelte`: bind `paneIndex = enumeratePanes($layoutTree).findIndex(p => p.id === paneId) + 1`. Render in tab bar corner, dim styling.
- Dispatcher: register `focus_pane_n` action; `Ctrl+Alt+N` shortcut maps to it; handler is `enumeratePanes(...)[N-1]?.id ?? null` → `setFocusedPane(id)`.
- No persistence changes.

### 2. Maximize pane / Zen mode

- New module `src/lib/layout/zen.ts` holding the stash store and `enterZen()` / `exitZen()` actions.
- `Pane.svelte` (or `LayoutNodeRenderer.svelte`) reads `$zenStash`; if set, render a single-pane layout from its stashed-active-pane tabs while Zen is active.
- Hook into `actions.ts` to call `exitZen()` from any layout-mutating action (split, drag, close-pane) — *unless* the mutating action is `exitZen` itself.
- Add `Ctrl+Shift+Z` shortcut.
- Decision: do **not** persist Zen state across launches. If the app is closed in Zen mode, it reopens with the stashed (real) layout — Zen is a transient visual mode, not a layout type.

### 3. Pane swap

- Add `swapPaneTabs(treeRoot, paneIdA, paneIdB)` to `tree.ts`. Pure tree op.
- `Pane.svelte` pane menu (or `TabContextMenu.svelte` if reusing): "Swap with..." → submenu listing `enumeratePanes(...)` minus self, labeled "Pane N (active: <tab name>)".
- Reuses M3's "Move all tabs to" submenu UI almost verbatim.

### 4. Split ratio quick-presets

- `Split.svelte`: add `oncontextmenu` on the splitter element. Show a tiny popover with 50/50, 70/30, 30/70 buttons. Each sets `split.ratio`.
- No new shortcut.
- Optional: hold `Shift` while dragging splitter to snap to 10% increments. Trivial. Defer if it adds noise.

### 5. Hide pane tab bar when single tab

- `Pane.svelte`: CSS class `single-tab` when `pane.tab_ids.length === 1`. Class hides the tab bar with `display: none`.
- Reappears automatically when tab count > 1 (Svelte reactivity).
- Document the right-click-menu-loss caveat in the README's shortcuts section. Accept the trade-off.

### 6. Pane-level DoneWhileAway indicator

- `Pane.svelte`: derive `paneHasDoneWhileAway = pane.tab_ids.some(id => $tabState[id]?.doneWhileAway)`.
- CSS class `pane-has-done-while-away` → 2px accent border (use a *different* color or position from the focused-pane indicator — e.g., focused = top, done-while-away = left edge, distinct accent).
- Auto-clears when the user focuses any tab in the pane (existing `DoneWhileAway` clear logic in `tabs/state.ts` already runs on focus).

### 7. Custom keyboard shortcuts UI for layout actions

This is the largest item in this group. Treat it as one PR but do not split into a milestone — the scope is clearly bounded.

- Settings tab "Shortcuts": list of all action names with their current binding.
- Reuse `src/lib/settings/ShortcutCapture.svelte` (already exists per the v1.2 era — verify it's the right one) for capture. If insufficient, the capture pattern is: focus an input, listen for `keydown`, normalize via `parser.ts`, debounce-commit.
- Conflict detection: on save, walk all bindings; if two map to the same combo, refuse and highlight both. OS-level conflicts (e.g., `Ctrl+Alt+Arrow` on GNOME) are *not* detectable — show a documentation note in the settings tab pointing at the README's known-conflicts table.
- Reset-to-default per binding and reset-all buttons.

### 8. Pane-aware compose overlay

- `ComposeOverlay.svelte` gains a target-tab `<select>` in the header.
- Options are `enumeratePanes(...)` flatmapped to `(pane, tab)` pairs, rendered as "Pane N — <tab name>". Default: focused pane's active tab.
- Selection is locked at compose-open time (mounted state; not reactive to focus changes during compose).
- Submit dispatches to the chosen `tabId`, not the currently-focused tab.
- `composeState.ts` gains a `targetTabId` field captured at open.

## Open questions

- **Pane numbering visibility**: corner badge always-on, or only on `Ctrl+Alt` modifier press? Always-on is simpler and the visual cost is small. Default to always-on; revisit if it feels noisy.
- **Zen + multi-window** (if multi-window ever ships): Zen is per-window. Doesn't change this design.
- **Custom shortcuts UI scope**: limit to layout-action shortcuts only (item 7's title), or extend to *all* shortcuts (TTS toggle, mute, etc.)? Recommend extending to all — the UI cost is the same, and partial coverage is confusing. Update item 7's title at implementation time if so.

## Milestone recommendation

**No milestone docs needed.** Each item is one PR's worth of work. Pick them up as triggers fire (per `FUTURE-FEATURES.md`). If 5+ items happen to land in a single sprint, write a single combined `MILESTONE-V1.4-XX-layout-polish.md` summarizing what shipped — but that's reactive bookkeeping, not pre-implementation planning.

## Files most likely touched

- `src/lib/layout/{tree,actions,store,zen}.ts` (new file: `zen.ts`)
- `src/lib/Pane.svelte`, `Split.svelte`, `LayoutNodeRenderer.svelte`
- `src/lib/ComposeOverlay.svelte`, `composeState.ts`
- `src/lib/shortcuts/{dispatcher,parser}.ts`
- `src/lib/settings/store.ts`, new Settings tab component
- `src-tauri/src/settings/schema.rs` (only for item 7, if new shortcut keys are added)

# Changelog

All notable changes to cctts are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.3.0] — 2026-05-06

### Added

- **Multi-pane layout.** The terminal area is now a recursive tree of panes
  and splits. Drag a tab to a pane edge to tear it into a new split, or to a
  pane center / tab bar to move it. Drag-and-drop uses a custom pointer-event
  handler with a 4 px threshold so clicks still register as clicks.
- **Splitter resize.** Each split has a 4 px draggable line between its two
  children (`col-resize` / `row-resize` cursor). Min-pane sizes (200 px wide,
  100 px tall) clamp during drag; window resize re-clamps visually without
  overwriting the user's stored ratio.
- **Pane-aware keyboard shortcuts.**
  - `Ctrl+\` — split focused pane horizontally with a fresh Shell tab.
  - `Ctrl+Shift+\` — split vertically with a fresh Shell tab.
  - `Ctrl+Alt+Arrow` — move focus to the geometrically-adjacent pane.
  - `Ctrl+Shift+W` — close focused pane (tabs migrate to the surviving
    sibling, then the empty pane collapses).
- **Pane right-click context menu** with Split horizontally / vertically,
  Close pane, and Move all tabs to → submenu.
- **Layout persistence.** The full layout tree and focused pane id persist to
  `settings.json` on a 250 ms debounce. Re-launching restores the exact pane
  arrangement from the previous session.
- **Named layout presets.** Save the current layout under a name from the
  Layouts popover in the bottom status bar; restore via Recent presets or the
  Manage presets dialog (with inline rename and confirm-delete).
- **Per-pane tab bar overflow.** When more tabs fit in a pane's width than
  display, the tab bar scrolls horizontally with thin scrollbars and edge-fade
  gradients. The `+` button stays pinned at the right. Activating an
  off-screen tab (via `Ctrl+N` or click) scrolls it into view.
- **Accessibility:** `role="group"` + dynamic `aria-label` on each pane
  (announces ordinal, total panes, and active tab name). `role="separator"` +
  `aria-orientation` + `aria-label="Resize panes"` on splitters. `:focus-visible`
  outlines on tabs, panes, splitters, and the new-tab button. `aria-hidden`
  on the drag ghost so screen readers don't follow it.

### Changed

- **`Ctrl+1`..`Ctrl+9` are now pane-scoped.** They switch to the Nth tab in
  the **focused pane**, not the Nth tab in the global list. This is the only
  behavior change for v1.2 users — closing or moving a tab shifts higher-
  numbered ones down by one within their pane, just as before, but the
  numbering is per-pane.
- **`Ctrl+T` and `Ctrl+W` are now pane-scoped** (new tab into focused pane,
  close active tab in focused pane).
- **Focused-pane indicator** is a 2 px top accent on the focused pane's tab
  bar (placed at the top so it doesn't merge with the active-tab underline,
  which uses the same accent color at the bottom).
- **Avatar overlay, audio playback, and the compose overlay** now route to
  the **focused pane's active tab** rather than a single global active tab.
  Switching pane focus retargets all three.

### Migrated

- v1.2 → v1.3: settings files without a `layout` key are migrated by
  synthesizing a single root pane containing every tab in order, picking
  active from `session.active_tab_id` (then dropped). A
  `settings.json.v1.2.bak` backup is written alongside before the rewrite.

### Known issues

- `Ctrl+Shift+W` may collide with WebView2's "close window" on some Windows
  configurations. If the close shortcut steals the keypress, remap
  `close_pane` to `Ctrl+Q` or `Ctrl+Alt+W` in *Settings → Shortcuts*.
- `Ctrl+Alt+Arrow` may collide with GNOME / KDE workspace switching on
  Linux. Remap `focus_pane_*` to `Ctrl+Shift+Arrow` if so.
- Tearing a tab into its own top-level window is not implemented — tabs
  always live within the single application window.
- No keyboard equivalent for moving a tab between existing panes; use drag
  or the Move all tabs to → context-menu submenu.

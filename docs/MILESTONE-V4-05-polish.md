# Milestone V4-05: Polish and Cross-Platform Validation

## Purpose

Round out v1.3 with cross-platform DnD validation, focused-pane indicator tuning, accessibility improvements, edge-case handling for the tab DOM portal, pane bar overflow when many tabs are in one pane, and README updates.

This is the smallest of the v1.3 milestones in scope but the most validation-heavy. Mouse-event behavior between WebView2 (Windows) and WebKitGTK (Linux) has known differences; M5 is where those get found and fixed.

Read `DESIGN-V4.md` and M1 through M4 first.

## What This Milestone Delivers

1. Cross-platform validation of drag-and-drop on Windows (WebView2) and Linux (WebKitGTK), with documented quirks and any necessary workarounds applied.
2. Cross-platform validation of splitter resize.
3. Cross-platform validation of all v1.3 keyboard shortcuts; OS-level conflicts documented, alternative bindings suggested.
4. Focused-pane visual indicator: tested on light and dark themes; tuned for visibility without distraction.
5. Pane tab bar overflow handling: when a single pane has more tabs than fit in its width, tabs become narrower and a scroll affordance kicks in. (Not the same as v1.2's tab bar, which was always full-width; per-pane tab bars are narrower.)
6. Accessibility: focus-visible cues for keyboard navigation; ARIA labels on the splitter, panes, and DnD ghost; basic screen-reader friendliness for the layout structure.
7. Tab DOM portal edge cases: rapid drag operations, app window resize during drag, pane collapse with active xterm.js terminal, recovery from xterm.js disposal errors.
8. README updates: new "Multi-pane Layout (v1.3+)" section documenting drag, shortcuts, presets, and known quirks.
9. Changelog entry summarizing all v1.3 changes.

## What This Milestone Does NOT Do

- No new features (this is a polish milestone).
- No tearing tabs into a new top-level window (deferred — multi-window in Tauri is a substantial separate feature).
- No animated transitions on pane operations (sudden snap is fine).
- No touch / pen DnD support (deferred).
- No drag-image custom rendering (the simple text ghost from M2 is sufficient).

## Implementation Steps

### 1. Cross-platform DnD validation

Run the M2 manual verification checklist on both platforms. Specifically check for:

- **WebView2 (Windows)**:
  - Mousedown on a tab — does the press register cleanly? WebView2 has had historical issues with element-level mouse events when the cursor is near other elements with hover effects. Verify the 4px threshold works.
  - Drag outside the application window — does the mouseup fire when the user releases outside? WebView2 should fire it; if not, fall back to a `pointerleave` cancel handler.
  - Cursor styling during drag — does `cursor: grabbing` apply globally? Set `document.body.style.cursor` not just the source element.
  - Drag ghost rendering — does it follow the cursor smoothly without lag?

- **WebKitGTK (Linux)**:
  - Same checks. WebKitGTK behaves more like Safari/WebKit.
  - Wayland-specific: under Wayland, `clientX/clientY` may behave differently for cross-window moves. Test on Wayland (most modern Linux distros default to it) and X11.
  - Drag ghost positioning: WebKitGTK may report cursor positions with subpixel precision or lag — verify the ghost stays glued to the cursor.

For any quirk found, add a fix and document it in the source file's top comment: e.g., `// WebView2: pointerleave fallback needed because mouseup outside window is unreliable.`

### 2. Cross-platform splitter validation

Run the M3 splitter manual verification on both platforms. Specifically:

- Splitter cursor (`col-resize` / `row-resize`) appears correctly on hover.
- Drag is smooth at 60Hz; no jitter.
- Min-size clamp works.
- Window resize triggers re-clamp without jitter (WebKitGTK's `ResizeObserver` may fire more aggressively than WebView2's).

### 3. Cross-platform shortcut validation

Test every v1.3 shortcut on both platforms and document:

| Shortcut | Windows status | Linux status | Workaround if blocked |
|----------|----------------|--------------|------------------------|
| `Ctrl+1`..`Ctrl+9` | | | |
| `Ctrl+T`, `Ctrl+W` | | | |
| `Ctrl+Shift+W` | (likely Webview2 close-window collision) | | Remap to `Ctrl+Q` or `Ctrl+Alt+W` |
| `Ctrl+\\` | | (Linux: usually fine) | |
| `Ctrl+Shift+\\` | | | |
| `Ctrl+Alt+Left/Right/Up/Down` | (usually fine) | (likely OS workspace switch) | Remap to `Ctrl+Shift+Left/Right/Up/Down` |
| `Ctrl+Shift+E` (compose) | | | |
| `Ctrl+Enter` (submit compose) | | | |

For each blocked shortcut, document the workaround in the README. Don't change the *defaults* — users on different distros / configurations have different conflicts; let each user remap.

### 4. Focused-pane indicator tuning

The M3 implementation specified a subtle indicator (e.g., 2px top border, soft glow). Iterate:

- Test on the app's default theme.
- Test on a dark theme (typical user preference).
- Verify the indicator is *clearly* present but not visually noisy.
- Common pitfalls: the indicator looks great on the dark theme but disappears on light backgrounds, or vice versa.
- Consider: a thin top accent line on the focused pane's tab bar at the application's accent color.

If the indicator implementation needs revision, do it here.

### 5. Pane tab bar overflow

A pane with many tabs (e.g., 8 tabs in a narrow pane) will overflow its tab bar's width. v1.2's monolithic tab bar didn't really need overflow handling because the full window width was available. v1.3's narrower per-pane tab bars hit overflow earlier.

Implementation:

- Tab elements have a min-width of ~80px and grow to fit.
- When the sum of min-widths exceeds the tab bar width, enable horizontal scroll on the tab bar:
  ```css
  .tab-bar {
      overflow-x: auto;
      scrollbar-width: thin;
  }
  ```
- A small left/right scroll-affordance (subtle gradient at the edges where content is hidden) for visual cue. Optional polish.

The `+` button (and any tab-bar-end controls) should remain visible — separate them into a fixed right-hand region of the tab bar with the scrollable tab list to the left.

### 6. Accessibility

- **`aria-label` on each pane**: e.g., "Pane 1 of 3, contains Claude and 2 more tabs." Computed dynamically from the pane's tabs.
- **`aria-label` on the splitter**: "Resize panes" with `role="separator"` and `aria-orientation="horizontal"` or `"vertical"`.
- **Focus-visible cues**: when the user tabs to a pane via keyboard navigation, the pane's focus indicator should appear distinctly. Use `:focus-visible` CSS.
- **Keyboard equivalents for common DnD operations**: M3's shortcuts (`Ctrl+\\`, `Ctrl+Shift+\\`) cover splitting; closing covers `Ctrl+Shift+W`. Moving a tab between existing panes via keyboard alone isn't covered by v1.3 shortcuts — out of scope; document as a known limitation.
- **Screen reader announcements**: when focus moves to a different pane via `Ctrl+Alt+Arrow`, announce the new pane (the `aria-label` should be picked up automatically). Test with NVDA on Windows and Orca on Linux.

### 7. Tab DOM portal stress tests

Things to verify don't break:

- Rapid drag operations: drag a tab between panes 20 times in quick succession. The tab's xterm.js stays connected to its PTY; scrollback is preserved; no orphan DOM elements accumulate.
- Window resize during drag: start a drag, resize the window, complete the drop. Drop computation uses fresh rects.
- Pane collapse with active terminal: a pane being collapsed has its active tab moved to a sibling pane (or all tabs moved). The xterm.js DOM follows. Verify no memory leaks (devtools heap snapshot before and after; counts of `.terminal-host` elements stay equal to tab count).
- xterm.js disposal: when a tab is closed, `terminals.destroyForTab` calls `term.dispose()`. Verify no exceptions thrown if dispose is called after the host element is already removed.
- Re-creation: closing and reopening (re-creating) a tab with the same name should produce a fresh xterm.js instance (verify the store map's deletion-then-recreation logic doesn't reuse the old one).

### 8. Audio routing edge cases

The audio target tab gate from M1 has a few edge cases worth verifying:

- A tab generates TTS while the user has it focused, then the user changes focus mid-playback: audio cuts immediately, remaining queue for that tab drops.
- A tab generates TTS while in a non-focused pane: synthesis runs, buffers queue, then drop at the audio target gate. No audio plays. The `DoneWhileAway` indicator on the tab strip shows.
- Two tabs in different panes finish generating around the same time: only the focused pane's tab plays. The other gets DoneWhileAway.
- Notification audio (e.g., `error` notification fires for a non-focused-pane tab): notification audio also goes through the target gate. If the user wants to hear notifications regardless of focus, that's a settings toggle worth considering — but for v1.3, consistent gating is the right default.

### 9. README updates

Add a new section "Multi-pane Layout (v1.3+)" with:

- **Splitting**: how to split a pane (drag a tab to an edge, or `Ctrl+\\` for horizontal / `Ctrl+Shift+\\` for vertical).
- **Moving tabs**: drag between panes, drop on tab bar or center.
- **Closing panes**: drag the last tab out (auto-collapses), or `Ctrl+Shift+W` on the focused pane.
- **Resizing**: drag the splitter line between panes.
- **Focus**: click any pane to focus it, or `Ctrl+Alt+Arrow` to navigate. The avatar and audio follow the focused pane.
- **Layout presets**: how to save and restore named layouts via the bottom-status-bar Layouts menu.
- **Keyboard shortcuts table**.
- **Known shortcut conflicts**: `Ctrl+Shift+W` may conflict with WebView2; `Ctrl+Alt+Arrow` may conflict with GNOME/KDE workspace switching. Suggest remappings.
- **Migration note**: existing v1.2 users get a single-pane layout on first launch; their `tabs` and per-tab settings carry over unchanged.

### 10. Changelog

Add a `CHANGELOG.md` entry (or update an existing one) summarizing v1.3:

- Multi-pane layout: free split tree, drag-and-drop tab tearing
- Splitter resize, min-pane sizes
- Pane-aware shortcuts (split, focus, close)
- `Ctrl+1`..`Ctrl+9` now scoped to focused pane (behavior change from v1.2)
- Layout persistence
- Named layout presets
- v1.2 → v1.3 settings migration

Note the `Ctrl+1`..`Ctrl+9` semantic change explicitly — it is the only behavior change for existing v1.2 users.

## Files Touched / Added

**Modified (mostly small fixes):**
- M2's drag handlers (any platform-specific workarounds found)
- M3's splitter mousedown handler (any platform fixes)
- `Pane.svelte` (focused-pane indicator tuning, ARIA labels)
- `Split.svelte` (ARIA `role="separator"` on the splitter)
- `TabBar.svelte` (overflow scroll, edge gradients, ARIA)
- `frontend/src/stores/terminals.ts` (defensive disposal handling)
- README
- CHANGELOG.md

**Added:**
- `frontend/src/components/TabBarOverflow.svelte` (if extracted)

## Edge Cases and Gotchas

- **WebKitGTK pointer events and Wayland**: under Wayland, certain pointer events may not deliver if the focus changed mid-drag. Use `pointercapture` to force the events to a specific element, falling back to window-level listeners for global capture.
- **Focus-visible polyfill**: `:focus-visible` is well-supported in modern WebView2 and WebKitGTK but check minimum versions match the project's Tauri requirements.
- **Scroll-on-focus for tab bar**: when a non-visible (overflowed) tab becomes active via `Ctrl+N`, scroll the tab bar to bring it into view. `el.scrollIntoView({ inline: "nearest", block: "nearest" })` works.
- **Drop zone overlay z-index**: ensure it's below modal dialogs (Settings, Manage Presets) so opening a dialog mid-drag (unlikely but possible) doesn't visually break.
- **Preset restore during in-progress drag**: shouldn't be possible (drag captures input), but if restored via a global shortcut while a drag is pending, cancel the drag first. Handle in the layout-store update path: if `drag.kind !== "idle"` when a layout-replace happens, set `drag.kind = "idle"` and clean up.

## Manual Verification Checklist

This is the v1.3 acceptance test. Run through it before declaring v1.3 shipped.

Cross-platform DnD:
- [ ] Run M2's full checklist on Windows. All items pass.
- [ ] Run M2's full checklist on Linux. All items pass.

Cross-platform splitter:
- [ ] Run M3's splitter checklist on Windows. Pass.
- [ ] Run M3's splitter checklist on Linux. Pass.

Cross-platform shortcuts:
- [ ] Test every v1.3 shortcut on Windows; log results.
- [ ] Test on Linux; log results. Document conflicts in README.

Visual polish:
- [ ] Focused-pane indicator visible on light theme.
- [ ] Visible on dark theme.
- [ ] Doesn't dominate visually.
- [ ] When window is small (e.g., 800×600): layout still functional, panes don't overlap, splitters draggable.
- [ ] When window is huge (4K): same.

Tab bar overflow:
- [ ] In a single narrow pane, create 10 tabs. Tab bar scrolls horizontally; `+` button stays visible at the right end.
- [ ] Click a tab not in view: it activates; tab bar scrolls to bring it in view.
- [ ] `Ctrl+5` switches to the 5th tab even if not in view; tab bar scrolls.

Accessibility:
- [ ] Run NVDA on Windows: navigate panes via Ctrl+Alt+Arrow; pane labels are announced.
- [ ] Run Orca on Linux: same.
- [ ] Splitter is announced as `separator` with orientation.

Stress test:
- [ ] Drag a tab between two panes 50 times rapidly. No errors, no orphan DOM, terminal state preserved.
- [ ] Create + close 20 Shell tabs. Memory stable; no PTY leaks (check process count).
- [ ] Create a 4-deep nested split. All panes interact correctly.
- [ ] Save 10 layout presets, restore each in turn. All restore correctly.

Migration:
- [ ] Pristine v1.2 settings file → v1.3 launch → migration runs, layout is single-pane, all v1.2 features still work.
- [ ] Re-launch v1.3 → no double-migration, no surprises.

End-to-end use case:
- [ ] Launch v1.3 from a project directory.
- [ ] Drag aider out of the tab bar to the right edge — Claude and aider now side by side.
- [ ] Drag a Shell tab to the bottom of Claude's pane — three-pane layout (Claude top-left, Shell bottom-left, aider right).
- [ ] Save as preset "Bid review."
- [ ] Reset to single pane.
- [ ] Restore "Bid review" — exact layout returns.
- [ ] Quit. Relaunch. Layout is restored.

This is the user's stated goal: dragging aider so both Claude and aider are visible. Verify it's smooth and natural.

## Done Criteria

- All 9 "What This Milestone Delivers" items work.
- All "Manual Verification Checklist" items pass on both Windows and Linux.
- README and CHANGELOG updated.
- No regression in v1.2, M1, M2, M3, or M4 behavior.
- The end-to-end use case (drag aider into a side-by-side layout, save preset, restart, restore) works smoothly without surprises.
- v1.3 ships from this point.

# V20 spike 0d — select-text TTS under fullscreen (manual, keep-vs-drop)

**Question.** In a fullscreen tab (mouse tracking ON), does cImp's
**Ctrl+right-click → speak selection** gesture still work? It needs two things,
both of which mouse tracking can break:

1. a **local** selection — under mouse tracking, plain drag goes to the app, so
   the user must hold **Shift** to bypass mouse reporting and select locally;
2. the `contextmenu` gesture + `selectionTts.ts` alt-buffer math
   (`registerMarker`/`baseY`) still firing on the alternate screen.

This is **not a gate.** Owner is willing to drop speak-selection if it's fiddly.
0d just answers keep-vs-drop, cheaply.

## How to run

Until Phase A lands, force a fullscreen tab by launching the app **without** the
inline knob:

- OpenCode: run `opencode` (no `--mini`) in a cImp tab.
- Claude: launch with `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN` **unset** for that tab.

Then, in the fullscreen tab:

1. **Plain drag** to select. Expected: the app handles it (no local highlight).
   Confirms mouse tracking is on.
2. **Shift+drag** to select. Expected: cImp/xterm makes a *local* selection
   (highlight visible, not sent to the app).
3. With a Shift-selection active, open the browser devtools console for the
   webview and run:
   ```js
   // tabId = the focused tab; grab its terminal from the registry
   __cimp_debugSelectedText?.()   // if exposed; else:
   document.activeElement && window.getSelection && console.log('dom?', '')
   ```
   Simpler: just confirm `term.getSelection()` is non-empty via any existing
   debug hook, or visually that copy-on-select / the highlight populated.
4. **Ctrl+right-click** on the selection. Expected: speech starts (and the
   receding read-along highlight paints, if enabled).

## Verdicts

- **All four work** → KEEP speak-selection in fullscreen (it's free).
- **Shift-drag selects but Ctrl+right-click doesn't speak** → small fix in
  `selectionTts.ts`/`terminals.ts`, decide by effort.
- **Shift-drag does NOT select** → DROP the gesture (Phase B.6); auto out-of-band
  TTS is the path. Record the finding and move on.

## Note

Record the result in the milestone (Phase B.6). Whatever the outcome, auto TTS
via 0a/0b is unaffected — 0d only governs the optional manual gesture.

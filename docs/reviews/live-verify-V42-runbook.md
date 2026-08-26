# V42 live-verify runbook (milestone 15 — headless core + repo-wide refactoring)

Compiled 2026-08-25 from the per-issue agent reports. Run against the first RC carrying the V42 merges.
The LV-B block (from #127) is MANDATORY and blocking for that RC. Others: work through by area; anything failing gets a fresh issue (a symptom ⇒ new issue, per standing rule).


## From #113 (wave-0 defects)
- LV-1: Settings → Tools reference lists graph_path + graph_architecture (D1).
- LV-2: Error text renders red where --text-danger-soft applies (SettingsApp validation rows; EventsView :1341 site) (D2).
- LV-3: TTS still speaks normally after D6 echo-suppression removal (keystroke path untouched: type into a Claude tab while TTS on; no echo of typed text, no regression in spoken output).

## From #114/#115 (loopback refactor)
- LV-4: MCP tools work end-to-end from a fresh tab (run_check, graph_* via cimp-offload) — discovery + dispatch exercise the moved code.
- LV-5: Delegation + hook shim round-trip (a Claude tab session with hooks: pre/post tool use rows appear; latch chip behaves).
- LV-6: /latch UI override flow still gates (taint chip → override → clears per policy) — latch.rs move.

## From #133 (Phase C ui_state)
- LV-7: First launch on a project WITH existing localStorage state → .cimp/ui_state.json appears containing the values; DevTools localStorage shows durable keys REMOVED, ephemeral keys still present.
- LV-8: No flash of default section/cards/columns on boot (Workbench opens on saved sub-tab immediately).
- LV-9: Hidden tab stays hidden across restart in project A and is NOT hidden in project B (per-project scoping — the designed behaviour change).
- LV-10: Events column widths + audit severity/hidden-tools filters survive restart; audit TEXT filter intentionally does not need to (ephemeral).
- LV-11: Toggle a card then close the window within 250 ms → reopen: state survived (pagehide flush) — KNOWN RISK: WebView2 may not fire pagehide on Tauri close; if this fails, file an issue rather than treating as blocker.
- LV-12: Settings window open + main window toggling state → no wipe of ui_state.json (write-inert unhydrated window).

## From #134 (Phase E codegen) — placeholder, fill from agent report
- LV-13: Settings window renders all sections; save round-trips (defaults now sourced from Rust).
- LV-14: (fill: any defaults-mismatch fixes that are user-visible get their own check items)

## From #128 (CSS dedup) — manual QA (no component tests exist; each family is eyes-on)
- LV-15 Status bar, all 3 theme families: button size/hover/pressed states; Record disabled+pulse; TabVisibility pip + .active; pill posture colours; ±N tabular numerals (no jitter); focus rings (2px accent modern / 1px inset tui-nippon).
- LV-16 All 7 dialogs: card width/centring/padding; backdrop click closes; Escape closes 6, does NOTHING in Restore checkpoint; Enter submits 4, does nothing in Offload + Manage presets; Manage presets Escape ladder (rename → confirm → dialog); tui/nippon buttons still render [ Cancel ] brackets no fill; Manage presets Close ring now matches other primaries (accepted delta).
- LV-17 Diff: unified row colours/markers/word-highlight/wrapping + leading whitespace; side-by-side unregressed; CheckpointDiffView in Timeline "Diff vs now", Session commits, Git graph.
- LV-18 Sub-tab strips ×4: padding/hover/active/divider/wrap; Code Audit inset 16px no gap, others 14px gap; CI badges tight; Memory/Overview refetch on click; selection persists across restart (via V42-C file).
- LV-19 Popovers ×3: open at cursor, viewport clamp near right/bottom edges, Escape + outside-click close, own-entry click does NOT close; TabContextMenu hover submenus stay open and fire.

## From #129a (SettingsApp CSS hoist)
- LV-20 INTENDED FIX: Settings → section hosting per-harness declared ext fields — fields now render as normal chrome rows (label block spacing, checkbox flex+gap, .input-with-action, hint margins). HarnessExtForm had no <style> and was written against chrome classes that scoping denied it; the hoist restores the intended look. Compare against another section for the target look.
- LV-21 Settings window overall: pixel-parity sweep of every section in tui + one modern theme (chrome hoisted to settings-chrome.css; children win source-order ties; residuals patched per agent report).
- LV-22 Settings: hover the ACTIVE Compose-template button and the ACTIVE sidebar entry — must look exactly as before (order-preservation companions in settings-chrome.css).

## From #120 (migration floor)
- LV-23 Plant a v20-stamped (or stamp-less) settings.json next to the exe → launch: file moved to *.outdated.<ts>.bak INTACT, defaults reseeded, loud error names the quarantined path; v30+ file migrates normally; fresh install unaffected.

## From #129c (section extraction)
- LV-24 Settings deep-link (cold AND hot): a settings-deep-link that targets section=tabs + a sub-tab lands on the right sub-tab (two out-of-section writers preserved).
- LV-25 Harness Verify: start a run, switch sections mid-run, come back — poll still live, button state (starting/busy) preserved (parent-owned poll).
- LV-26 Disable the AI tab you are currently viewing in Settings → Tabs: sub-tab moves as before (toggleAiTabEnabled optimistic write + rollback path).
- LV-27 Graph → ignore list commit: edit + commit while another window/tab changes a setting concurrently — no lost update (commitGraphIgnore now gated by draftSync; new behaviour).
- LV-28 Full Settings sweep across all 21 sidebar sections after the split: every section renders, saves round-trip, restart hints fire only when they should (tabs baseline machinery).

## From the tranche-2 review-fix pass
- LV-29 (T2-5) Section-state survival: offload test prompt+result+sub-tab, half-typed preset name, selected plugin + Detect results, CI sub-tab — all survive sidebar switches; closing/reopening the Settings WINDOW still starts clean.
- LV-30 (T2-3) Env editor: rows monospace, remove-× hover fully red; Tabs → harness page: declared-settings block is a group INSIDE the tab card (no card-in-card), keeping LV-20's label/checkbox/hint chrome.
- LV-31 (T2-1/T2-2) Floor edge: plant a below-floor settings.json where the move-aside FAILS (e.g. lock the backup names) → app runs read-only-ish for globals (saves refused loudly), file intact; unstamped overlay + quarantined global → loud refusal naming the file, not a silent merge.

## From #138 (Phase B layout)
- LV-32 Boot with corrupt layout (ratio 5.0/-3.0/NaN, dead tab id, duplicated tab id, bogus focused_pane_id): sane first paint, no dup chips, focus leftmost.
- LV-33 Hide 2 tabs → relaunch: stay hidden, space freed, popover checkboxes right; Show all returns them to focused pane.
- LV-34 Fresh install with pre-existing hidden set in ui_state.json: hidden tabs absent from seeded layout.
- LV-35 Splitter drag: smooth WITH Settings window open (quiet-broadcast fix); ratio survives relaunch; drag→close-within-500ms→relaunch keeps last position (close-flush).
- LV-36 Preset: save → new tab → restore (new tab lands focused pane, deleted gone, hidden stays hidden); restore mid-drag cancels cleanly; restore a deleted preset = no-op + one console warning.

## From #139 (Phase D diffWords)
- LV-37 Word-level diff highlight: DiffView unified AND side-by-side, CheckpointDiffView, WorktreesView; one CRLF file; one UNTRACKED file (all-+ hunk, every group plain add). Spans render identically to pre-port.

## From #130 (CI split + GraphView engine)
- LV-38 Code Intelligence full sweep: Overview identical to rc.10 (donuts/chart/counters/cost/advisor/sessions); populates on FIRST open (bind:this timing); per-section state survives switches (selected session+zoom+follow, scan results, trace form, architecture); "+N" badges; usage-* cards' open state survives restart; lane/segment pickers recolour live; pricing edit reprices open card; Memory "(this/last session)" label. REPEAT items 1/9/10 under tui + one external theme (the specificity claim).
- LV-39 Graph view: load/settle/auto-fit/stop-at-touch; orbit/pan/zoom-limits/momentum; hover ring + edge emphasis; click→ego+history; directory focus; live agent pulses; workbench jump incl. dropped-by-top-N file; 6 tuning knobs + 2 edge colours + cluster spacing 50×; hide/show pauses+resumes rAF.
- LV-40 Keep-alive polls ×5 (Events/Timeline/GraphIndex/GitGraph/CI): switch away 1 min → return: refresh-on-return, stays live, no double-refresh; Timeline latch action mid-flight not clobbered.

## From #127 (unified confined spawn) — MANDATORY SANDBOX BATTERY (blocking for the RC that ships b561bd9; sandbox ON)
- LV-B1 run_check sandboxed, network OFF: completes; one sandboxed row named by the CONFIGURED CHECK NAME (never cmd.exe/sh).
- LV-B2 run_check sandboxed, network ON: completes; posture note reflects network-on.
- LV-B3 security_audit with a stdout scanner (semgrep): findings whole; one sandboxed row per scanner named by PROGRAM.
- LV-B4 ReportFile scanner (gitleaks/cppcheck): SARIF written+parsed; NO denial row for the report dir.
- LV-B5 Sandboxed run_check reading outside project root (type %USERPROFILE%\.ssh\id_rsa): denied + denied row with argv+exit code; control passes with sandbox off.
- LV-B6 Low-timeout check running a forker: timed_out, no exit code, partial output, NO orphans in Task Manager.
- LV-B7 Cancel a long security_audit mid-scan: prompt return, Cancelled, no orphaned scanners.
- LV-B8 D3 eyes-on: kill the check child EXTERNALLY mid-run: check errors rather than hangs; no orphaned readers/grandchildren (only live coverage of the behaviour change).
- LV-B9 Sandbox OFF: one check + one audit run normally; one skipped row "off (user choice)" per session.
- LV-B10 On any B1-B5 failure: re-run twice before recording (mapped-drive flake).

## From the Phase-F review (2026-08-25) — the last code change of the milestone
- LV-41 (F-2, eyes-on, no test can see it): Code Intelligence → **Trace path**. Run a trace that returns a multi-hop chain (e.g. a function to a file it reaches through two calls). Every `──edge──▶` row's confidence badge must be COLOURED by value, exactly as the same badges look under **Analyses → Impact**: `extracted` neutral/dim, `inferred` amber, `ambiguous` red. A row of identical grey pills is the bug. Repeat under `tui` + one external theme (the fills moved to the shared sheet, so the specificity claim is retested).
- LV-42 (F-3): with two AI harnesses available, stand in a NON-AI tab (a shell tab, or Workbench), open Settings → Tabs and ENABLE a harness whose tab is currently off. The new tab must appear in the bar and the app must NOT switch to it — focus stays where you were. Then, standing in the harness tab you are about to remove, UNCHECK it: focus must hand off to a surviving AI tab before the tab disappears (never to a tab id that no longer exists, and never a flash of a blank pane).
- LV-43 (F-10, eyes-on): open the Code Intelligence → Cost card (or Dashboard) on an IDLE session — one that has not taken a turn for a while. Per-model rows must populate. Then, with the card open, restart the graph/index (Settings → toggle the graph off and on) so one usage fetch fails, and leave the session idle: within a few seconds the rows must come back on their own. Before this fix the card stayed on its "no per-model data" placeholder until the next turn.
- LV-44 (F-5): on a NON-git project with checkpoints ON, take a checkpoint, edit several files, and open the Diff pane. The file list's ± counts must be right, and expanding a file must still show intra-line word highlighting (the summary path stopped computing it; the file path still does). LV-37's word-diff sweep covers the git case.

**Amendment to LV-B8 (F-4).** That item is now the live half of a fix, not just an eyes-on check. Kill the check's child process EXTERNALLY mid-run (Task Manager → End task on the `sh`/`cmd.exe` the check spawned, NOT the tree): the check must error rather than hang, and — the part that changed — **no grandchild may survive**. Start a check whose command forks a long-lived worker, kill the shell, then confirm Task Manager has no orphan left behind. Previously a failed `wait()` skipped the whole-tree kill entirely.

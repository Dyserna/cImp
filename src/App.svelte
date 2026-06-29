<script lang="ts">
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import LayoutNodeRenderer from './lib/LayoutNodeRenderer.svelte';
  import StatusBar from './lib/StatusBar.svelte';
  import TuiTitleBar from './lib/TuiTitleBar.svelte';
  import AvatarOverlay from './lib/AvatarOverlay.svelte';
  import WaveformOverlay from './lib/WaveformOverlay.svelte';
  import ComposeOverlay from './lib/ComposeOverlay.svelte';
  import ErrorBanner from './lib/ErrorBanner.svelte';
  import NewShellTabDialog from './lib/dialog/NewShellTabDialog.svelte';
  import ConfigureTabDialog from './lib/dialog/ConfigureTabDialog.svelte';
  import SaveLayoutDialog from './lib/dialog/SaveLayoutDialog.svelte';
  import ManagePresetsDialog from './lib/dialog/ManagePresetsDialog.svelte';
  import Toast from './lib/Toast.svelte';
  import DragGhost from './lib/dnd/DragGhost.svelte';
  import DropZoneOverlay from './lib/dnd/DropZoneOverlay.svelte';
  import { dialogState, openNewShellTabDialog } from './lib/dialog/store';
  import { closeTab as closeTabIpc } from './lib/ipc';
  import { showToast } from './lib/toast';
  import {
    seedPerTabEntries,
    startAvatarStateListener,
    perTabAvatarState,
  } from './lib/avatarState';
  import { initSettings, settings, applySettings } from './lib/settings/store';
  import { themeRegistry } from './lib/themes/registry';
  import { openSettingsWindow, setActiveTab as setActiveTabIpc } from './lib/settings/ipc';
  import { activeTab, switchTab } from './lib/tabs/state';
  import { applyTabCreated } from './lib/tabs/store';
  import {
    applyTabCreatedToLayout,
    closeFocusedPane,
    focusedActiveTabId,
    focusedPane,
    focusPaneInDirection,
    layout,
    resetLayoutToSinglePane,
    setFocusedPaneActiveTab,
    setPaneActiveTab,
  } from './lib/layout/store';
  import { splitFocusedPaneWithNewShell } from './lib/layout/actions';
  import {
    installLayoutPersistence,
    validateAndRepairLayout,
  } from './lib/layout/persistence';
  import { createTerminal } from './lib/terminals';
  import { stopAllTts, isSelectionTtsActive, playSelectionTts } from './lib/selectionTts';
  import { listTabs } from './lib/ipc';
  import {
    configureShortcuts,
    installDispatcher,
  } from './lib/shortcuts/dispatcher';
  import {
    composeOpen,
    composeFocused,
    composeContent,
    openCompose,
    closeCompose,
    submitCompose,
  } from './lib/composeState';
  import { composeContentChanged } from './lib/ipc';
  import {
    initStt,
    startRecording,
    stopRecording,
    cancelRecording,
  } from './lib/stt';
  import { configurePushToTalk } from './lib/shortcuts/pushToTalk';

  // OS window title is set by the Rust setup hook to "<project> - cImp".
  // Mirrored into the TUI title bar via a one-shot read in onMount.
  let tuiTitle = $state('cImp');

  // Whether to render the custom TuiTitleBar: driven by the active theme's
  // `decorations` metadata (false = OS chrome hidden, we draw our own bar).
  // Derived from the registry store so it re-evaluates when the registry
  // finishes loading, not just when the theme id changes. Unknown / not-yet-
  // loaded themes default to the custom bar (matches the tui-orange fallback).
  let useCustomTitleBar = $derived(
    !($themeRegistry.find((t) => t.id === $settings.ui.theme)?.decorations ?? false),
  );

  let unsubSettings: (() => void) | undefined;
  let unsubContent: (() => void) | undefined;
  let unsubFocusedTab: (() => void) | undefined;
  let unsubActiveTabBack: (() => void) | undefined;
  let removeDebugKeys: (() => void) | undefined;
  let unsubLayoutSave: (() => void) | undefined;
  let unsubFollowAvatar: (() => void) | undefined;

  // Set to true by the cleanup returned from onMount. Checked at the
  // single `await` suspension point inside the async IIFE (and once
  // more after the post-await sync block) so an HMR / quick-reload
  // teardown that runs before the IIFE finishes can opt out of further
  // setup. The end-of-IIFE block runs the same cleanup the returned
  // teardown would, so any subscriptions installed in the post-await
  // tail get torn down immediately rather than leaking.
  let disposed = false;
  function runCleanup(): void {
    unsubSettings?.();
    unsubContent?.();
    unsubFocusedTab?.();
    unsubActiveTabBack?.();
    removeDebugKeys?.();
    unsubLayoutSave?.();
    unsubFollowAvatar?.();
    unsubSettings = undefined;
    unsubContent = undefined;
    unsubFocusedTab = undefined;
    unsubActiveTabBack = undefined;
    removeDebugKeys = undefined;
    unsubLayoutSave = undefined;
    unsubFollowAvatar = undefined;
  }

  onMount(() => {
    void (async () => {
      await initSettings();
      if (disposed) return;
      // Seed the tabs store from a synchronous snapshot before attaching
      // the avatar-state listener. Event-driven add/remove still updates
      // the store at runtime; the snapshot just guarantees the launch
      // tabs are present even if backend TabCreated emissions raced the
      // webview mount. Idempotent: events for tabs already in the store
      // overwrite name/kind in place.
      try {
        const snapshot = await listTabs();
        const persistedLayout = get(settings).layout;
        // Seed every tab's per-tab state (avatar entries, tabs store,
        // terminal DOM host) regardless of layout source. The layout
        // tree references tabs by id; the tabs store + terminals
        // registry are what actually own the per-tab data.
        snapshot.forEach((m, position) => {
          seedPerTabEntries(m.id);
          applyTabCreated({
            tab: m.id,
            kind: m.kind,
            name: m.name,
            builtin: m.builtin,
            position,
          });
          createTerminal(m.id);
        });

        if (persistedLayout) {
          // V4-04: hydrate the layout tree from settings. The integrity
          // sieve adapts the persisted shape to the live tab list —
          // dropping tabs the user deleted between launches and placing
          // newly-created tabs as orphans in the focused pane.
          const repaired = validateAndRepairLayout(
            persistedLayout,
            get(settings).tabs,
          );
          layout.set(repaired);
        } else {
          // Fresh-install / pre-V4-04 path: every tab goes into the
          // store-built single root pane via `applyTabCreatedToLayout`,
          // and the previously-active tab (if any) is restored from
          // the legacy session.active_tab_id field.
          snapshot.forEach((m) => applyTabCreatedToLayout(m.id));
          const sessionActive = get(settings).session.active_tab_id;
          if (sessionActive) {
            setFocusedPaneActiveTab(sessionActive);
          }
        }
      } catch (e) {
        console.error('list_tabs failed:', e);
      }
      if (disposed) return;
      // Install the debounced save subscription AFTER hydration so the
      // first emission (the just-set layout from settings) is the one
      // it swallows. If we install earlier, the subscription would
      // round-trip the hydrated layout straight back to the backend on
      // launch — harmless (would write the same state) but wasteful.
      unsubLayoutSave = installLayoutPersistence();
      // Start the backend state listener early — it drives both the
      // per-tab avatar cache AND the activeTab store (via the
      // ActiveTabChanged event), so it must run regardless of whether
      // the avatar overlay is mounted/visible.
      void startAvatarStateListener().catch((e) =>
        console.error('startAvatarStateListener failed:', e),
      );
      installDispatcher();
      // V6-01: register the STT event listeners (state + transcript →
      // compose overlay). Idempotent; safe even when STT is disabled —
      // the backend simply never emits until the user records.
      initStt();
      // Position-bound tab-switch handler: 1-indexed lookup against the
      // *focused pane's* tab list (V4-03 reinterpretation of v1.2's
      // global Ctrl+N). No-op when the focused pane has fewer than N
      // tabs. The closest analogs are iTerm2 and VS Code, both of which
      // scope Cmd+N / Ctrl+N to the current group / pane.
      const switchToPosition = (n: number) => () => {
        const pane = get(focusedPane);
        const target = pane.tab_ids[n - 1];
        if (!target) return;
        setPaneActiveTab(pane.id, target);
        // Mirror to the backend so audio / avatar / window-title
        // routing follows. switchTab is the v1.2 call that updates
        // session.active_tab_id and broadcasts ActiveTabChanged.
        void switchTab(target);
      };
      // Active-tab close handler. Builtins surface a transient toast
      // since closing them is rejected by the backend; the toast keeps
      // the keystroke from feeling like a no-op.
      const closeActiveTab = () => {
        if (get(dialogState).kind !== 'none') return;
        const tab = get(activeTab);
        void closeTabIpc(tab).catch((e) => {
          const wire = e as { kind?: string } | string | null;
          if (wire && typeof wire === 'object' && 'kind' in wire) {
            if (wire.kind === 'builtin-not-closable') {
              showToast('This tab cannot be closed.');
              return;
            }
          }
          console.error('close_tab failed:', e);
        });
      };
      unsubSettings = settings.subscribe((s) => {
        configureShortcuts(s.shortcuts, {
          open_compose: {
            // Default is Alt+Enter — which, while the compose sheet is open,
            // doubles as the "insert newline" key (handled in the textarea).
            // Guard so the dispatcher only consumes Alt+Enter to OPEN compose
            // when it's closed; when it's open the keystroke falls through to
            // the textarea's newline handler.
            handler: openCompose,
            active: () => !get(composeOpen),
          },
          submit_compose: {
            handler: () => {
              void submitCompose();
            },
            active: () => get(composeFocused),
          },
          cancel_compose: {
            handler: closeCompose,
            active: () => get(composeOpen),
          },
          open_settings: () => {
            void openSettingsWindow();
          },
          switch_to_tab_1: switchToPosition(1),
          switch_to_tab_2: switchToPosition(2),
          switch_to_tab_3: switchToPosition(3),
          switch_to_tab_4: switchToPosition(4),
          switch_to_tab_5: switchToPosition(5),
          switch_to_tab_6: switchToPosition(6),
          switch_to_tab_7: switchToPosition(7),
          switch_to_tab_8: switchToPosition(8),
          switch_to_tab_9: switchToPosition(9),
          new_shell_tab: openNewShellTabDialog,
          close_tab: closeActiveTab,
          focus_pane_left: () => focusPaneInDirection('left'),
          focus_pane_right: () => focusPaneInDirection('right'),
          focus_pane_up: () => focusPaneInDirection('up'),
          focus_pane_down: () => focusPaneInDirection('down'),
          split_pane_horizontal: () => {
            void splitFocusedPaneWithNewShell('horizontal');
          },
          split_pane_vertical: () => {
            void splitFocusedPaneWithNewShell('vertical');
          },
          close_pane: closeFocusedPane,
          speak_selection: () => playSelectionTts(),
          stop_tts: {
            handler: () => {
              // Only issue the stop when something is plausibly playing — a
              // selection read, or any tab whose avatar is Speaking. Avoids an
              // IPC round-trip on every Escape pressed in the terminal when no
              // TTS is in flight.
              const anySpeaking = Object.values(get(perTabAvatarState)).some(
                (s) => s === 'Speaking',
              );
              if (isSelectionTtsActive() || anySpeaking) {
                void stopAllTts();
              }
            },
            active: () => isSelectionTtsActive(),
          },
        });
        // V6-01: push-to-talk is a hold gesture, configured separately from
        // the fire-once shortcut table. Re-runs on every settings change so a
        // re-bound chord or an enable/disable toggle takes effect live.
        configurePushToTalk(s.stt.enabled, s.shortcuts.push_to_talk, {
          start: () => void startRecording(),
          stop: () => void stopRecording(),
          cancel: () => void cancelRecording(),
        });
      });
      // OS window title is set by the Rust setup hook to
      // "<project> - cImp" (project = git-root or launch-dir folder
      // name). Mirror it into the TUI in-app title bar so the chrome
      // matches. Read once — Rust set it before the webview rendered
      // and nothing in the frontend changes it after launch.
      void getCurrentWindow()
        .title()
        .then((t) => {
          if (t) tuiTitle = t;
        })
        .catch((e) => console.warn('read window title failed:', e));

      let lastNonEmpty = false;
      unsubContent = composeContent.subscribe((content) => {
        const nonEmpty = content.length > 0;
        if (nonEmpty !== lastNonEmpty) {
          lastNonEmpty = nonEmpty;
          void composeContentChanged(nonEmpty).catch((e) =>
            console.error('compose_content_changed failed:', e),
          );
        }
      });

      // Sync the focused pane's active tab to the backend's "active
      // tab" cell. The backend gates audio routing on this id (TTS
      // worker drops samples for non-active tabs), and the rest of the
      // frontend reads `activeTab` for avatar / compose / window title
      // routing. Initial value lands here so the first render reflects
      // the restored session.active_tab_id.
      let lastSyncedActive: string | null = null;
      unsubFocusedTab = focusedActiveTabId.subscribe((id) => {
        if (id === lastSyncedActive) return;
        lastSyncedActive = id;
        if (id === null) return;
        void setActiveTabIpc(id).catch((e) =>
          console.error('set_active_tab failed:', e),
        );
      });
      // Back-sync: when the backend broadcasts ActiveTabChanged,
      // reflect that into the focused pane's active-tab field so any
      // legacy v1.2-style switch path stays coherent with the layout.
      //
      // Pane-scoped guard: only mirror the broadcast when the new id
      // lives in the *currently focused* pane. The backend's
      // close-tab fallback walks the global tab list to pick the
      // previous tab; in a multi-pane layout that fallback can land
      // in a different pane than the user is operating in. Reflecting
      // those broadcasts via `setFocusedPaneActiveTab` would search
      // the tree for the new id, force the holding pane's active to
      // it, and steal focus there — yanking the user's "current
      // thread" to a tab they didn't ask for. Ignoring out-of-pane
      // broadcasts keeps the unrelated pane's active tab stable; the
      // backend's active-tab cell self-corrects on the next
      // focusedActiveTabId.subscribe push (which fires when
      // applyTabClosedFromLayout lands).
      //
      // First-emission swallow: Svelte writables fire on subscribe
      // with their current value. The activeTab store starts at the
      // backend's launch-active id (set by `ActiveTabChanged` events
      // and pre-populated to a default in `tabs/state.ts`). Treating
      // that initial firing as a "real" change would race with the
      // forward-sync above (`unsubFocusedTab` pushes the layout's
      // focused-pane active tab to the backend on its own first
      // emission). Without this guard, when the two values disagree
      // — common after V4-04 migration where session.active_tab_id
      // is dropped and the backend falls back to Claude while the
      // hydrated layout has e.g. Claude (local) active — the two
      // subscriptions ping-pong correcting each other across async
      // backend round-trips, flapping the active tab dozens of
      // times on launch.
      // "Follow avatar visibility" mode: keep tts.mute in sync with the
      // inverse of avatar.visible. Only mutates state when the two are
      // out of agreement, so the resulting applySettings broadcast (which
      // re-fires this subscriber) is a no-op and there's no loop. Lives
      // in App.svelte rather than the Settings window so the sync runs
      // continuously regardless of whether Settings is open.
      unsubFollowAvatar = settings.subscribe((s) => {
        if (!s.behavior.follow_avatar) return;
        const wantMute = !s.avatar.visible;
        if (s.tts.mute === wantMute) return;
        void applySettings({ ...s, tts: { ...s.tts, mute: wantMute } });
      });

      let activeTabFirstEmission = true;
      unsubActiveTabBack = activeTab.subscribe((t) => {
        if (activeTabFirstEmission) {
          activeTabFirstEmission = false;
          return;
        }
        const pane = get(focusedPane);
        if (!pane.tab_ids.includes(t)) return;
        if (pane.active_tab_id === t) return;
        setPaneActiveTab(pane.id, t);
      });

      // Debug shortcut: Ctrl+Shift+F3 collapses every split into a
      // single root pane. Bypasses the configurable dispatcher because
      // it's a QA / recovery hatch, not a user-facing binding. The
      // M1-era F1 / F2 split keys were retired in V4-03 — Ctrl+\ /
      // Ctrl+Shift+\ now do the user-facing split with a fresh shell.
      const onDebugKey = (e: KeyboardEvent) => {
        if (!e.ctrlKey || !e.shiftKey) return;
        if (e.code === 'F3') {
          e.preventDefault();
          resetLayoutToSinglePane();
        }
      };
      window.addEventListener('keydown', onDebugKey, true);
      removeDebugKeys = () => window.removeEventListener('keydown', onDebugKey, true);

      // Tail check: if teardown ran while we were inside the post-await
      // synchronous tail above, every `unsub*` is now set but the
      // returned cleanup has already been called once and won't fire
      // again. Run cleanup eagerly so nothing leaks.
      if (disposed) runCleanup();
    })();
    return () => {
      disposed = true;
      runCleanup();
    };
  });
</script>

<main>
  <!--
    The layout tree replaces v1.2's single TabBar + per-tab Terminal
    block. Each leaf Pane renders its own tab bar and portals in its
    active tab's xterm host from the registry. Avatar/compose/error
    overlays remain at app root, layered over the entire content area;
    they subscribe to `activeTab`-derived stores which are kept in
    sync with the focused pane's active tab.

    Custom title bar mounts for any theme whose metadata sets
    `decorations: false` (both shipped themes are TUI, so it always mounts);
    a future native-chrome theme (`decorations: true`) would keep the OS
    title bar via setDecorations(true) wired in main.ts.
  -->
  {#if useCustomTitleBar}
    <TuiTitleBar title={tuiTitle} />
  {/if}
  <div class="terminal-area">
    <LayoutNodeRenderer node={$layout.tree} />
    <AvatarOverlay />
    <WaveformOverlay />
    <ComposeOverlay />
    <ErrorBanner />
    <!--
      DnD overlays mounted here so they layer above panes but below
      modal dialogs (which render outside .terminal-area). The ghost
      and drop-zone are pointer-events: none so they never intercept
      the in-flight drag's pointermove/up.
    -->
    <DropZoneOverlay />
    <DragGhost />
  </div>
  <StatusBar />
  <NewShellTabDialog />
  <ConfigureTabDialog />
  <SaveLayoutDialog />
  <ManagePresetsDialog />
  <Toast />
</main>

<style>
  :global(html, body) {
    margin: 0;
    padding: 0;
    height: 100%;
    overflow: hidden;
  }
  main {
    position: relative;
    width: 100vw;
    height: 100vh;
    display: flex;
    flex-direction: column;
  }
  .terminal-area {
    position: relative;
    flex: 1 1 auto;
    min-height: 0;
    min-width: 0;
    overflow: hidden;
    display: flex;
  }
</style>

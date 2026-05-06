<script lang="ts">
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import LayoutNodeRenderer from './lib/LayoutNodeRenderer.svelte';
  import StatusBar from './lib/StatusBar.svelte';
  import AvatarOverlay from './lib/AvatarOverlay.svelte';
  import WaveformOverlay from './lib/WaveformOverlay.svelte';
  import ComposeOverlay from './lib/ComposeOverlay.svelte';
  import ErrorBanner from './lib/ErrorBanner.svelte';
  import AiderFirstLaunchNotice from './lib/AiderFirstLaunchNotice.svelte';
  import NewShellTabDialog from './lib/dialog/NewShellTabDialog.svelte';
  import ConfigureTabDialog from './lib/dialog/ConfigureTabDialog.svelte';
  import Toast from './lib/Toast.svelte';
  import DragGhost from './lib/dnd/DragGhost.svelte';
  import DropZoneOverlay from './lib/dnd/DropZoneOverlay.svelte';
  import { dialogState, openNewShellTabDialog } from './lib/dialog/store';
  import { closeTab as closeTabIpc } from './lib/ipc';
  import { showToast } from './lib/toast';
  import {
    avatarState,
    seedPerTabEntries,
    startAvatarStateListener,
  } from './lib/avatarState';
  import { initSettings, settings } from './lib/settings/store';
  import { openSettingsWindow, setActiveTab as setActiveTabIpc } from './lib/settings/ipc';
  import { activeTab, switchTab } from './lib/tabs/state';
  import { applyTabCreated, tabMeta } from './lib/tabs/store';
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
  import { createTerminal } from './lib/terminals';
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

  let unsubSettings: (() => void) | undefined;
  let unsubContent: (() => void) | undefined;
  let unsubTitle: (() => void) | undefined;
  let unsubFocusedTab: (() => void) | undefined;
  let unsubActiveTabBack: (() => void) | undefined;
  let removeDebugKeys: (() => void) | undefined;

  onMount(() => {
    void (async () => {
      await initSettings();
      // Seed the tabs store from a synchronous snapshot before attaching
      // the avatar-state listener. Event-driven add/remove still updates
      // the store at runtime; the snapshot just guarantees the launch
      // tabs are present even if backend TabCreated emissions raced the
      // webview mount. Idempotent: events for tabs already in the store
      // overwrite name/kind in place.
      try {
        const snapshot = await listTabs();
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
          applyTabCreatedToLayout(m.id);
        });
        // Restore the previously-active tab into the (single) root pane
        // so the avatar/audio/compose routing picks up where the user
        // left off. The backend persists session.active_tab_id, but the
        // settings store already has it loaded by this point.
        const sessionActive = get(settings).session.active_tab_id;
        if (sessionActive) {
          setFocusedPaneActiveTab(sessionActive);
        }
      } catch (e) {
        console.error('list_tabs failed:', e);
      }
      // Start the backend state listener early — it drives both the
      // per-tab avatar cache AND the activeTab store (via the
      // ActiveTabChanged event), so it must run regardless of whether
      // the avatar overlay is mounted/visible.
      void startAvatarStateListener().catch((e) =>
        console.error('startAvatarStateListener failed:', e),
      );
      installDispatcher();
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
          open_compose: openCompose,
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
        });
      });
      // Window title reflects the active tab's avatar state. Switching
      // tabs re-derives the avatar state, so this listener picks that up
      // automatically. The tab name comes from the tabs store so user
      // renames flow through immediately.
      const win = getCurrentWindow();
      unsubTitle = avatarState.subscribe((s) => {
        const tab = get(activeTab);
        const meta = tabMeta(tab);
        const tabLabel = meta?.name ?? tab;
        const label = s === 'Idle' ? tabLabel : `${tabLabel} — ${s}`;
        void win.setTitle(label).catch((e) =>
          console.warn('setTitle failed:', e),
        );
      });

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
      unsubActiveTabBack = activeTab.subscribe((t) => {
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
    })();
    return () => {
      unsubSettings?.();
      unsubContent?.();
      unsubTitle?.();
      unsubFocusedTab?.();
      unsubActiveTabBack?.();
      removeDebugKeys?.();
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
  -->
  <div class="terminal-area">
    <LayoutNodeRenderer node={$layout.tree} />
    <AvatarOverlay />
    <WaveformOverlay />
    <ComposeOverlay />
    <ErrorBanner />
    <AiderFirstLaunchNotice />
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

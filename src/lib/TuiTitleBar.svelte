<script lang="ts">
  // Custom TUI-styled window title bar. Mounted whenever the active theme
  // sets `decorations: false` — true for the built-in `tui` theme and the
  // shipped disk themes. A future native-chrome theme would use the OS bar
  // via setDecorations(true).
  //
  // The bar is a single 22px-tall row with a drag region in the middle
  // (data-tauri-drag-region — Tauri natively turns mousedown into a
  // window drag) and minimize/maximize/close buttons on the right.
  // Glyphs are box-drawing-style ASCII so the look matches the rest of
  // the TUI theme. Close gets a danger-tinted hover for the destructive
  // signal.

  import { onMount } from 'svelte';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  // Windows 11 Snap Layouts: the plugin drops a transparent native child HWND
  // over the maximize button (matched by id "snap-max-btn") so hovering it
  // shows the OS snap-zone flyout. attach/detach are safe no-ops off Win11.
  import { attach, detach } from 'tauri-plugin-snap-layout';

  let { title = 'cImp' }: { title?: string } = $props();

  let isMaximized = $state(false);

  onMount(() => {
    const win = getCurrentWindow();
    void win.isMaximized().then((v) => (isMaximized = v));
    const unlisten = win.onResized(() => {
      void win.isMaximized().then((v) => (isMaximized = v));
    });
    // Bind the snap overlay to this window's maximize button. This bar only
    // mounts for borderless (decorations:false) themes, so tying attach/detach
    // to the component lifetime keeps the overlay off native-chrome themes
    // (whose real OS maximize button already drives snap) and re-binds it when
    // switching back — the overlay HWND must be recreated after a decorations
    // toggle. Per-webview, so the main and settings windows stay isolated.
    attach('snap-max-btn');
    return () => {
      void unlisten.then((u) => u());
      void detach();
    };
  });

  function onMinimize(): void {
    void getCurrentWindow().minimize();
  }
  function onToggleMaximize(): void {
    void getCurrentWindow().toggleMaximize();
  }
  function onClose(): void {
    void getCurrentWindow().close();
  }
</script>

<div class="tui-titlebar" data-tauri-drag-region>
  <span class="tui-title" data-tauri-drag-region>{title}</span>
  <div class="tui-controls">
    <button type="button" onclick={onMinimize} title="Minimize" aria-label="Minimize">
      _
    </button>
    <button
      id="snap-max-btn"
      type="button"
      onclick={onToggleMaximize}
      title={isMaximized ? 'Restore' : 'Maximize'}
      aria-label={isMaximized ? 'Restore' : 'Maximize'}
    >
      {isMaximized ? '▢' : '□'}
    </button>
    <button
      type="button"
      class="close"
      onclick={onClose}
      title="Close"
      aria-label="Close"
    >
      ×
    </button>
  </div>
</div>

<style>
  .tui-titlebar {
    display: flex;
    align-items: center;
    height: 22px;
    flex: 0 0 22px;
    padding: 0 1ch;
    border-bottom: 1px solid var(--border-default);
    background: var(--surface-0);
    color: var(--text-quiet);
    font-family:
      'Cascadia Code', 'JetBrains Mono', 'Fira Code', 'Source Code Pro',
      Consolas, 'Courier New', monospace;
    font-size: var(--font-size-md);
    line-height: 20px;
    user-select: none;
    cursor: default;
  }
  .tui-title {
    flex: 1 1 auto;
    text-align: center;
    color: var(--accent);
    font-weight: 400;
  }
  .tui-controls {
    display: flex;
    flex: 0 0 auto;
    gap: 0;
  }
  .tui-controls button {
    appearance: none;
    background: transparent;
    border: none;
    color: var(--text-primary);
    font-family: inherit;
    font-size: inherit;
    line-height: inherit;
    cursor: pointer;
    padding: 0 1ch;
    height: 22px;
    border-radius: 0;
  }
  .tui-controls button:hover,
  /* The snap overlay swallows pointer events, so `:hover` never fires on the
     maximize button; the plugin toggles `.is-hovered` on cursor enter/leave.
     `:global(...)` keeps Svelte from pruning this runtime-added class as an
     unused selector (it can't see the plugin applying it). */
  #snap-max-btn:global(.is-hovered) {
    background: var(--surface-3);
    color: var(--text-bright);
  }
  .tui-controls button.close:hover {
    background: var(--danger);
    color: var(--surface-0);
  }
  .tui-controls button:focus-visible {
    outline: 1px solid var(--accent);
    outline-offset: -1px;
  }
</style>

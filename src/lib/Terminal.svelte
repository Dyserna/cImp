<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { Terminal } from '@xterm/xterm';
  import { FitAddon } from '@xterm/addon-fit';
  import '@xterm/xterm/css/xterm.css';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';

  import {
    createBytesChannel,
    ptyStart,
    ptyRestart,
    ptyWrite,
    ptyResize,
    onPtyExit,
    decodeBase64,
    type BytesChannel,
  } from './ipc';
  import { display as displaySettings } from './settings/store';
  import { setTerminalFocuser } from './terminalFocus';
  import { activeTab } from './tabs/state';
  import type { TabId } from './tabs/types';

  let { tabId }: { tabId: TabId } = $props();

  let containerEl: HTMLDivElement;
  let statusEl: HTMLDivElement;
  let term: Terminal | undefined;
  let fitAddon: FitAddon | undefined;
  let resizeObserver: ResizeObserver | undefined;
  let unlistenExit: (() => void) | undefined;
  let unlistenRestart: UnlistenFn | undefined;
  let unsubActive: (() => void) | undefined;
  let resizeTimer: ReturnType<typeof setTimeout> | undefined;
  let status = $state('booting…');

  let initialFontFamily = 'Consolas, Menlo, "DejaVu Sans Mono", monospace';
  let initialFontSize = 14;
  const unsubInit = displaySettings.subscribe((d) => {
    initialFontFamily = d.terminal_font_family;
    initialFontSize = d.terminal_font_size;
  });
  unsubInit();

  let unsubFont: (() => void) | undefined;

  function rebindBytesChannel(): BytesChannel {
    const channel = createBytesChannel();
    channel.onmessage = (encoded) => {
      if (term) term.write(decodeBase64(encoded));
    };
    return channel;
  }

  /// True when the container has real layout (not in a `display: none`
  /// subtree). offsetWidth/Height are 0 for hidden elements regardless of
  /// declared CSS width — exactly the signal we need.
  function isVisible(): boolean {
    return containerEl.offsetWidth > 0 && containerEl.offsetHeight > 0;
  }

  /// Fit and propagate the size to the PTY. Caller must ensure the container
  /// is visible — fitting a hidden container makes FitAddon compute tiny
  /// dimensions that re-wrap xterm's grid (mangling scrollback) and resize
  /// the child, which we never want.
  function fitAndResize(): void {
    if (!term || !fitAddon) return;
    fitAddon.fit();
    const { rows, cols } = term;
    ptyResize(tabId, rows, cols).catch((e) =>
      console.error('pty_resize failed:', e),
    );
  }

  onMount(async () => {
    try {
      status = 'creating xterm';
      term = new Terminal({
        fontFamily: initialFontFamily,
        fontSize: initialFontSize,
        cursorBlink: true,
        allowProposedApi: true,
        theme: {
          background: '#000000',
          foreground: '#e0e0e0',
        },
      });
      fitAddon = new FitAddon();
      term.loadAddon(fitAddon);
      term.open(containerEl);
      // Only fit if the container is visible. For a tab that mounts hidden
      // (the non-default tab), xterm stays at its default 80x24 — a sensible
      // TTY size. The first activation re-fits to actual dimensions.
      if (isVisible()) {
        fitAddon.fit();
      }

      unsubFont = displaySettings.subscribe((d) => {
        if (!term || !fitAddon) return;
        if (
          term.options.fontFamily !== d.terminal_font_family ||
          term.options.fontSize !== d.terminal_font_size
        ) {
          term.options.fontFamily = d.terminal_font_family;
          term.options.fontSize = d.terminal_font_size;
          if (isVisible()) {
            fitAndResize();
          }
        }
      });

      status = 'wiring channel';
      let bytesChannel = rebindBytesChannel();

      term.onData((data) => {
        ptyWrite(tabId, data).catch((e) => console.error('pty_write failed:', e));
      });

      unlistenExit = await onPtyExit((payload) => {
        if (payload.tab !== tabId) return;
        if (term) term.write(`\r\n[${tabId} exited: ${payload.exit}]\r\n`);
      });

      // Restart event is Claude-only in v2 — the existing settings UI flow
      // for Claude flags / instruction overrides triggers it. Aider tab
      // ignores the event.
      if (tabId === 'claude') {
        unlistenRestart = await listen('claude-code-restart', async () => {
          if (!term || !fitAddon) return;
          try {
            term.write('\r\n[restarting claude…]\r\n');
            bytesChannel = rebindBytesChannel();
            const { rows, cols } = term;
            await ptyRestart(tabId, bytesChannel, rows, cols);
            term.focus();
          } catch (e) {
            console.error('pty_restart failed:', e);
            term.write(`\r\n[restart failed: ${(e as Error).message ?? e}]\r\n`);
          }
        });
      }

      resizeObserver = new ResizeObserver(() => {
        if (resizeTimer) clearTimeout(resizeTimer);
        resizeTimer = setTimeout(() => {
          // Hidden tabs report a near-zero box; fitting on that produces a
          // tiny grid that mangles existing scrollback and triggers a SIGWINCH
          // to the child telling it the terminal is e.g. 11 columns wide. We
          // catch this up on activation instead (see the activeTab subscription
          // below).
          if (!isVisible()) return;
          fitAndResize();
        }, 250);
      });
      resizeObserver.observe(containerEl);

      status = 'starting pty';
      const { rows, cols } = term;
      try {
        await ptyStart(tabId, bytesChannel, rows, cols);
        status = 'running';
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        // Don't crash the rest of the app — surface the error in the
        // tab's terminal so the user sees what went wrong (per V2-01
        // acceptance #10: aider missing must not break Claude).
        term.write(`\r\n[${tabId} failed to start: ${msg}]\r\n`);
        status = `start failed: ${msg}`;
        console.error(`pty_start failed for ${tabId}:`, e);
      }

      // Register this tab's focus function. Re-focus when the active tab
      // becomes this one — display:none → display:block doesn't
      // automatically restore focus, and xterm.js needs an explicit
      // focus() call to receive keystrokes.
      setTerminalFocuser(tabId, () => term?.focus());
      unsubActive = activeTab.subscribe((t) => {
        if (t !== tabId) return;
        // Defer past Svelte's flush so the visibility class is in the DOM
        // before we measure. rAF is more reliable than setTimeout(0) for
        // "after the next paint" — it ensures offsetWidth reflects layout.
        requestAnimationFrame(() => {
          if (!term) return;
          term.focus();
          // Catch up on any size changes that happened while we were
          // hidden (the ResizeObserver skips hidden containers). Without
          // this, a tab that mounted hidden stays at its default 80x24
          // until something else resizes the window.
          if (isVisible()) {
            fitAndResize();
          }
        });
      });
    } catch (e) {
      const msg = e instanceof Error ? `${e.message}\n${e.stack ?? ''}` : String(e);
      status = `ERROR: ${msg}`;
      console.error('Terminal onMount failed:', e);
      if (statusEl) statusEl.style.color = '#ff6666';
    }
  });

  onDestroy(() => {
    if (resizeTimer) clearTimeout(resizeTimer);
    resizeObserver?.disconnect();
    unlistenExit?.();
    unlistenRestart?.();
    unsubFont?.();
    unsubActive?.();
    setTerminalFocuser(tabId, null);
    term?.dispose();
  });
</script>

<div class="wrap">
  <div bind:this={containerEl} class="terminal-container"></div>
  <div bind:this={statusEl} class="status">{status}</div>
</div>

<style>
  .wrap {
    position: relative;
    width: 100%;
    height: 100%;
  }
  .terminal-container {
    width: 100%;
    height: 100%;
    box-sizing: border-box;
    padding: 4px;
    background: #000;
  }
  .terminal-container :global(.xterm) {
    height: 100%;
  }
  .terminal-container :global(.xterm-viewport) {
    background: #000 !important;
  }
  .status {
    position: absolute;
    bottom: 4px;
    right: 8px;
    font-family: monospace;
    font-size: 11px;
    color: #888;
    background: rgba(0, 0, 0, 0.6);
    padding: 2px 6px;
    border-radius: 3px;
    pointer-events: none;
    white-space: pre-wrap;
    max-width: 60%;
    text-align: right;
  }
</style>

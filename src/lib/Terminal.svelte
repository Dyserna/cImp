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
    ttsTest,
    onPtyExit,
    decodeBase64,
    type BytesChannel,
  } from './ipc';
  import { display as displaySettings } from './settings/store';

  // Expose a global helper so we can test TTS directly from the WebView
  // DevTools console: `window.ttsTest("hello world")`.
  // @ts-expect-error - dev-only debug surface
  window.ttsTest = (text: string) => ttsTest(text).catch(console.error);

  let containerEl: HTMLDivElement;
  let statusEl: HTMLDivElement;
  let term: Terminal | undefined;
  let fitAddon: FitAddon | undefined;
  let resizeObserver: ResizeObserver | undefined;
  let unlistenExit: (() => void) | undefined;
  let unlistenRestart: UnlistenFn | undefined;
  let resizeTimer: ReturnType<typeof setTimeout> | undefined;
  let status = $state('booting…');

  let initialFontFamily = 'Consolas, Menlo, "DejaVu Sans Mono", monospace';
  let initialFontSize = 14;
  // Capture initial values synchronously before xterm boot so the first
  // paint already uses the persisted settings rather than the placeholder.
  const unsubInit = displaySettings.subscribe((d) => {
    initialFontFamily = d.terminal_font_family;
    initialFontSize = d.terminal_font_size;
  });
  unsubInit();

  // Subscribe live so font changes apply on the fly. We keep the
  // unsubscribe handle and tear it down in onDestroy.
  let unsubFont: (() => void) | undefined;

  function rebindBytesChannel(): BytesChannel {
    const channel = createBytesChannel();
    channel.onmessage = (encoded) => {
      if (term) term.write(decodeBase64(encoded));
    };
    return channel;
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
      fitAddon.fit();

      // Apply font changes whenever display settings update. xterm exposes
      // `options` for live changes; after a font swap we re-fit so the
      // grid re-measures to the new metrics.
      unsubFont = displaySettings.subscribe((d) => {
        if (!term || !fitAddon) return;
        if (
          term.options.fontFamily !== d.terminal_font_family ||
          term.options.fontSize !== d.terminal_font_size
        ) {
          term.options.fontFamily = d.terminal_font_family;
          term.options.fontSize = d.terminal_font_size;
          fitAddon.fit();
          const { rows, cols } = term;
          ptyResize(rows, cols).catch((e) => console.error('pty_resize failed:', e));
        }
      });

      status = 'wiring channel';
      let bytesChannel = rebindBytesChannel();

      term.onData((data) => {
        ptyWrite(data).catch((e) => console.error('pty_write failed:', e));
      });

      unlistenExit = await onPtyExit((payload) => {
        if (term) term.write(`\r\n[claude exited: ${payload}]\r\n`);
      });

      // Settings asks for a Claude Code restart by emitting this event;
      // we own the channel/rows/cols so the actual reconnect happens here.
      unlistenRestart = await listen('claude-code-restart', async () => {
        if (!term || !fitAddon) return;
        try {
          term.write('\r\n[restarting claude…]\r\n');
          // Drop the old channel handler — once `pty_restart` returns,
          // bytes will flow through the new one.
          bytesChannel = rebindBytesChannel();
          const { rows, cols } = term;
          await ptyRestart(bytesChannel, rows, cols);
          term.focus();
        } catch (e) {
          console.error('pty_restart failed:', e);
          term.write(`\r\n[restart failed: ${(e as Error).message ?? e}]\r\n`);
        }
      });

      resizeObserver = new ResizeObserver(() => {
        if (resizeTimer) clearTimeout(resizeTimer);
        resizeTimer = setTimeout(() => {
          if (!term || !fitAddon) return;
          fitAddon.fit();
          const { rows, cols } = term;
          ptyResize(rows, cols).catch((e) => console.error('pty_resize failed:', e));
        }, 250);
      });
      resizeObserver.observe(containerEl);

      status = 'starting pty';
      const { rows, cols } = term;
      await ptyStart(bytesChannel, rows, cols);
      status = 'running';

      term.focus();
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

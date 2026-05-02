<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { Terminal } from '@xterm/xterm';
  import { FitAddon } from '@xterm/addon-fit';
  import '@xterm/xterm/css/xterm.css';

  import {
    createBytesChannel,
    ptyStart,
    ptyWrite,
    ptyResize,
    ttsTest,
    onPtyExit,
    decodeBase64,
  } from './ipc';

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
  let resizeTimer: ReturnType<typeof setTimeout> | undefined;
  let status = $state('booting…');

  onMount(async () => {
    try {
      status = 'creating xterm';
      term = new Terminal({
        fontFamily: 'Consolas, Menlo, "DejaVu Sans Mono", monospace',
        fontSize: 14,
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

      status = 'wiring channel';
      const bytesChannel = createBytesChannel();
      bytesChannel.onmessage = (encoded) => {
        if (term) term.write(decodeBase64(encoded));
      };

      term.onData((data) => {
        ptyWrite(data).catch((e) => console.error('pty_write failed:', e));
      });

      unlistenExit = await onPtyExit((payload) => {
        if (term) term.write(`\r\n[claude exited: ${payload}]\r\n`);
      });

      resizeObserver = new ResizeObserver(() => {
        if (resizeTimer) clearTimeout(resizeTimer);
        // 250ms debounce: long enough that a window drag across DPI
        // boundaries (which can fire many micro-resizes) settles into a
        // single ptyResize, short enough that intentional resizes still
        // feel responsive. A shorter window let mid-drag jitter chain
        // PTY redraws into a sustained byte burst, which the avatar
        // state detector misread as claude output.
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

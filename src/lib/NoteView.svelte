<script lang="ts">
  // The Note scratchpad tab's content — a rudimentary plain-text editor
  // (old-Notepad style) backed by `<launch_cwd>/.cimp/cimp.note.txt`. Rendered
  // by Pane.svelte for the reserved `note` tab id (no xterm / no PTY).
  //
  // Autosave, per the feature spec:
  //   - debounced ~800ms after the last keystroke (the common case),
  //   - a 5s safety-net timer,
  //   - on tab close (onDestroy) and on window blur / app close (beforeunload).
  // Every save is a no-op when the text is unchanged since the last write, so
  // the timers are cheap. Writes go through the backend's atomic writer, so a
  // crash mid-save can't truncate the note.
  import { onMount, onDestroy } from 'svelte';
  import { readNote, writeNote } from './ipc';

  type SaveStatus = 'loading' | 'saved' | 'saving' | 'dirty' | 'error';

  let text = $state('');
  let status = $state<SaveStatus>('loading');
  let loaded = $state(false);
  // Last value confirmed written to disk — the dirty check compares against it.
  let lastSaved = '';

  let editorEl: HTMLTextAreaElement | undefined = $state();
  let debounceTimer: ReturnType<typeof setTimeout> | undefined;
  let intervalTimer: ReturnType<typeof setInterval> | undefined;

  const STATUS_LABEL: Record<SaveStatus, string> = {
    loading: 'Loading…',
    saved: 'Saved',
    saving: 'Saving…',
    dirty: 'Unsaved changes',
    error: 'Save failed',
  };

  async function flush(): Promise<void> {
    if (!loaded) return;
    if (text === lastSaved) {
      if (status !== 'error') status = 'saved';
      return;
    }
    const snapshot = text;
    status = 'saving';
    try {
      await writeNote(snapshot);
      lastSaved = snapshot;
      // The user may have typed more while the write was in flight.
      status = text === lastSaved ? 'saved' : 'dirty';
    } catch (e) {
      console.error('writeNote failed:', e);
      status = 'error';
    }
  }

  // Fire-and-forget flush for event listeners (blur/beforeunload) that can't
  // await. The debounced saves mean the note is almost always already current
  // by the time the app closes; this is the best-effort final catch.
  function flushNow(): void {
    void flush();
  }

  function onInput(): void {
    status = 'dirty';
    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = setTimeout(flushNow, 800);
  }

  // Notepad-style: Tab inserts a tab character at the caret instead of moving
  // focus out of the editor.
  function onKeydown(e: KeyboardEvent): void {
    if (e.key === 'Tab' && !e.ctrlKey && !e.altKey && !e.metaKey) {
      e.preventDefault();
      const el = editorEl;
      if (!el) return;
      const start = el.selectionStart;
      const end = el.selectionEnd;
      text = text.slice(0, start) + '\t' + text.slice(end);
      // Restore the caret after the inserted tab on the next tick (after the
      // bound value re-renders).
      requestAnimationFrame(() => {
        el.selectionStart = el.selectionEnd = start + 1;
      });
      onInput();
    }
  }

  onMount(() => {
    (async () => {
      try {
        text = await readNote();
        lastSaved = text;
        status = 'saved';
      } catch (e) {
        console.error('readNote failed:', e);
        status = 'error';
      } finally {
        loaded = true;
      }
    })();

    intervalTimer = setInterval(flushNow, 5000);
    window.addEventListener('blur', flushNow);
    window.addEventListener('beforeunload', flushNow);
  });

  onDestroy(() => {
    if (debounceTimer) clearTimeout(debounceTimer);
    if (intervalTimer) clearInterval(intervalTimer);
    window.removeEventListener('blur', flushNow);
    window.removeEventListener('beforeunload', flushNow);
    // Final flush on tab close.
    flushNow();
  });
</script>

<div class="note">
  <header class="note-head">
    <span class="path" title="Stored at .cimp/cimp.note.txt in this project">.cimp/cimp.note.txt</span>
    <span class="status" class:error={status === 'error'} aria-live="polite">
      {STATUS_LABEL[status]}
    </span>
  </header>
  <textarea
    bind:this={editorEl}
    class="editor"
    bind:value={text}
    oninput={onInput}
    onkeydown={onKeydown}
    onblur={flushNow}
    disabled={!loaded}
    spellcheck="false"
    autocomplete="off"
    autocapitalize="off"
    placeholder="Scratchpad — commands, ideas, notes. Autosaves to .cimp/cimp.note.txt."
    aria-label="Note scratchpad editor"
  ></textarea>
</div>

<style>
  .note {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    background: var(--surface-0, #0d1117);
  }
  .note-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    flex: 0 0 auto;
    padding: 0.3rem 0.7rem;
    font-family: var(--font-sans, system-ui, sans-serif);
    font-size: 0.75rem;
    color: var(--text-secondary, #8b949e);
    border-bottom: 1px solid var(--border-subtle, #21262d);
    background: var(--surface-sunken, #010409);
  }
  .path {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    opacity: 0.85;
  }
  .status {
    flex: 0 0 auto;
    font-variant-numeric: tabular-nums;
  }
  .status.error {
    color: var(--danger, #d08770);
  }
  .editor {
    flex: 1 1 auto;
    width: 100%;
    min-height: 0;
    box-sizing: border-box;
    resize: none;
    border: 0;
    outline: none;
    padding: 0.6rem 0.8rem;
    background: transparent;
    color: var(--text-primary, #c9d1d9);
    font-family: var(--font-mono, ui-monospace, 'Cascadia Code', Consolas, monospace);
    font-size: var(--font-size-md, 13px);
    line-height: 1.5;
    white-space: pre-wrap;
    overflow-wrap: break-word;
    tab-size: 4;
  }
  .editor::placeholder {
    color: var(--text-secondary, #8b949e);
    opacity: 0.6;
  }
</style>

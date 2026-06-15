<script lang="ts">
  import { tick } from 'svelte';
  import { composeOpen, composeContent, composeFocused, submitCompose } from './composeState';
  import { compose as composeSettings } from './settings/store';

  let textareaEl: HTMLTextAreaElement | undefined = $state();

  // Settings-driven heights. Reactive so live changes from the settings
  // window update the bounds without remounting the sheet.
  const minHeight = $derived($composeSettings.min_height_px);
  const maxHeight = $derived($composeSettings.max_height_px);

  // Auto-grow: re-measure scrollHeight after each input, clamping between
  // min and max. Beyond max the textarea scrolls internally (overflow-y).
  function adjustHeight(): void {
    if (!textareaEl) return;
    textareaEl.style.height = 'auto';
    const desired = Math.min(
      Math.max(textareaEl.scrollHeight, minHeight),
      maxHeight,
    );
    textareaEl.style.height = `${desired}px`;
  }

  // When the sheet opens, focus the textarea on the next tick (after the
  // {#if} mount). On close the element is gone — nothing to focus.
  $effect(() => {
    if ($composeOpen) {
      void tick().then(() => {
        // `preventScroll`: the sheet is mid slide-up (translated partly below
        // the viewport) when we focus, and a plain focus() makes the browser
        // scroll the page to reveal the textarea — which visibly shoves the
        // whole terminal area down for a frame. Focusing without scrolling
        // keeps everything still.
        textareaEl?.focus({ preventScroll: true });
        adjustHeight();
      });
    }
  });

  // Re-clamp when min/max settings change while the sheet is open.
  $effect(() => {
    void minHeight;
    void maxHeight;
    if ($composeOpen) adjustHeight();
  });

  function handleInput(): void {
    adjustHeight();
  }

  // Insert text at the caret and mirror it into the bound store so
  // submitCompose sees it. Shared by the Tab and Alt+Enter handlers.
  function insertAtCaret(text: string): void {
    const ta = textareaEl;
    if (!ta) return;
    const start = ta.selectionStart;
    const end = ta.selectionEnd;
    ta.value = ta.value.slice(0, start) + text + ta.value.slice(end);
    ta.selectionStart = ta.selectionEnd = start + text.length;
    composeContent.set(ta.value);
    adjustHeight();
  }

  // Compose key handling, tuned for one-handed dictation/typing:
  //   Enter            → submit (send to the active tab)
  //   Alt+Enter        → newline (the universal "soft return")
  //   Shift+Enter      → newline (textarea default; left untouched)
  //   Ctrl/Cmd+Enter   → left for the configurable `submit_compose` shortcut
  //   Tab              → literal tab at the caret (not focus-cycle)
  // Plain Enter is also bound as the default `submit_compose` shortcut, which
  // the dispatcher handles first; this is the fallback for users whose submit
  // key isn't Enter.
  function handleKeydown(e: KeyboardEvent): void {
    if (e.key === 'Enter') {
      if (e.ctrlKey || e.metaKey) return;
      if (e.altKey) {
        e.preventDefault();
        insertAtCaret('\n');
        return;
      }
      if (e.shiftKey) return; // textarea inserts the newline itself
      e.preventDefault();
      void submitCompose();
      return;
    }
    if (e.key === 'Tab' && !e.shiftKey && !e.ctrlKey && !e.altKey && !e.metaKey) {
      e.preventDefault();
      insertAtCaret('\t');
    }
  }

  function handleFocus(): void {
    composeFocused.set(true);
  }

  function handleBlur(): void {
    composeFocused.set(false);
  }

  // Slide-up on enter, slide-down on leave. Custom transition is just a
  // translateY interpolation so we don't fade opacity (the milestone calls
  // out a brief slide animation, not a fade).
  function slideY(_node: HTMLElement, { duration = 200 } = {}) {
    return {
      duration,
      css: (t: number) => `transform: translateY(${(1 - t) * 100}%);`,
    };
  }
</script>

{#if $composeOpen}
  <div class="compose-sheet" transition:slideY={{ duration: 200 }}>
    <textarea
      bind:this={textareaEl}
      bind:value={$composeContent}
      oninput={handleInput}
      onkeydown={handleKeydown}
      onfocus={handleFocus}
      onblur={handleBlur}
      spellcheck="true"
      placeholder="Compose message..."
      style="min-height: {minHeight}px; max-height: {maxHeight}px;"
    ></textarea>
  </div>
{/if}

<style>
  .compose-sheet {
    position: absolute;
    bottom: 0;
    left: 0;
    right: 0;
    background: var(--surface-2);
    border-top: 1px solid var(--border-subtle);
    border-radius: var(--radius-lg) var(--radius-lg) 0 0;
    box-shadow: var(--shadow-lg);
    padding: var(--space-3);
    /* Mask any subpixel artifacts at the rounded top corners. */
    overflow: hidden;
    /* Above the terminal but below the avatar (z-index: 10) and below the
       settings window. The avatar sits in a corner so the overlap is
       tolerable; if it ever conflicts visually, raise this. */
    z-index: 50;
    box-sizing: border-box;
  }

  textarea {
    width: 100%;
    box-sizing: border-box;
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    font-size: 14px;
    line-height: 1.4;
    color: var(--text-primary);
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    border-radius: var(--radius-md);
    padding: 10px;
    resize: none;
    outline: none;
    overflow-y: auto;
    transition: border-color var(--motion-fast) var(--easing-standard);
  }

  textarea:focus {
    border-color: var(--accent);
  }
</style>

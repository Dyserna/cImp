<script lang="ts">
  import { tick } from 'svelte';
  import { composeOpen, composeContent, composeFocused } from './composeState';
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
        textareaEl?.focus();
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

  // Tab key inserts a literal tab character at the caret instead of
  // shifting focus out of the sheet. Multi-line code paste workflows expect
  // it. Shift+Tab is left alone (default focus-cycle behavior preserved).
  function handleKeydown(e: KeyboardEvent): void {
    if (e.key !== 'Tab' || e.shiftKey || e.ctrlKey || e.altKey || e.metaKey) {
      return;
    }
    e.preventDefault();
    const ta = textareaEl;
    if (!ta) return;
    const start = ta.selectionStart;
    const end = ta.selectionEnd;
    const value = ta.value;
    ta.value = value.slice(0, start) + '\t' + value.slice(end);
    ta.selectionStart = ta.selectionEnd = start + 1;
    // Mirror the change into the bound store so submitCompose sees it.
    composeContent.set(ta.value);
    adjustHeight();
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

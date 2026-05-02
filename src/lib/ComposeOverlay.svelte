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
    background: #1e1e1e;
    border-top: 1px solid #444;
    box-shadow: 0 -4px 12px rgba(0, 0, 0, 0.3);
    padding: 12px;
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
    color: #e0e0e0;
    background: #2a2a2a;
    border: 1px solid #555;
    border-radius: 4px;
    padding: 10px;
    resize: none;
    outline: none;
    overflow-y: auto;
  }

  textarea:focus {
    border-color: #6699cc;
  }
</style>

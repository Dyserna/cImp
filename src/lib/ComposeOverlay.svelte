<script lang="ts">
  import { tick } from 'svelte';
  import { get } from 'svelte/store';
  import { getCurrentWebview } from '@tauri-apps/api/webview';
  import {
    composeOpen,
    composeContent,
    composeFocused,
    composeOpenPickerSignal,
    composeAttachments,
    submitCompose,
  } from './composeState';
  import { compose as composeSettings } from './settings/store';
  import { listenManaged } from './listenManaged';
  import TemplatePicker from './TemplatePicker.svelte';
  import {
    composeTemplates,
    substituteTemplate,
    filterTemplates,
    nextPlaceholderRange,
    hasPlaceholder,
    type ResolvedTemplate,
  } from './compose/templates';
  import {
    clipboardHasImage,
    filterImagePaths,
    readClipboardImagePng,
    composeAttachImage,
  } from './compose/attachments';

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
    } else {
      // Sheet closed (submit/cancel) — the picker must not still be "open"
      // the next time the sheet opens fresh.
      showPicker = false;
    }
  });

  // Re-measure when the draft changes programmatically (V13 Phase B's
  // `openComposeWith` appends a sent hunk to an already-open sheet) — a
  // direct store write doesn't fire the textarea's own `oninput`, so without
  // this the box wouldn't grow to fit newly-appended text until the user
  // typed something themselves.
  $effect(() => {
    void $composeContent;
    if ($composeOpen) adjustHeight();
  });

  // Re-clamp when min/max settings change while the sheet is open.
  $effect(() => {
    void minHeight;
    void maxHeight;
    if ($composeOpen) adjustHeight();
  });

  // ── V14 Phase A: prompt-template picker ─────────────────────────────
  // Trigger: `/` typed when the textarea is EMPTY opens a popover listing
  // the by-name-resolved templates (project shadows global), filtered by
  // continued typing (subsequence-fuzzy against the name). The keystrokes
  // themselves are NEVER intercepted — the '/' and every following
  // character land in the textarea exactly like normal typing, and the
  // query is just that content read back out (minus the leading '/'). This
  // is what makes "Esc, or any non-matching input flow, dismisses into the
  // literal text" free: there is no separate buffer to reconcile: the
  // literal text is already sitting in the textarea the whole time.
  let showPicker = $state(false);
  let pickerTemplates = $state<ResolvedTemplate[]>([]);
  let pickerQuery = $state('');
  let pickerIndex = $state(0);
  const pickerFiltered = $derived(filterTemplates(pickerTemplates, pickerQuery));

  async function openPicker(): Promise<void> {
    showPicker = true;
    pickerQuery = '';
    pickerIndex = 0;
    try {
      pickerTemplates = await composeTemplates();
    } catch (e) {
      console.warn('compose_templates fetch failed:', e);
      pickerTemplates = [];
    }
  }

  function closePicker(): void {
    showPicker = false;
  }

  // Replace the textarea's current content — mid-picker this is just the
  // "/query" the user typed, which the picker only ever exists ahead of
  // inserting — with the chosen template's substituted body, then select
  // the first remaining `{placeholder}` (if any) so the user can overtype
  // it immediately. Not reachable once the picker is closed, so there is
  // nothing to "undo" on Esc: dismissing just leaves the literal text.
  async function insertTemplate(index: number): Promise<void> {
    const chosen = pickerFiltered[index];
    closePicker();
    if (!chosen) return;
    const substituted = await substituteTemplate(chosen.body);
    composeContent.set(substituted);
    const ta = textareaEl;
    if (!ta) return;
    ta.value = substituted;
    adjustHeight();
    ta.focus();
    const ph = nextPlaceholderRange(substituted, 0);
    if (ph) {
      ta.setSelectionRange(ph.start, ph.end);
    } else {
      ta.setSelectionRange(substituted.length, substituted.length);
    }
  }

  // The `open_compose_picker` shortcut (App.svelte) bumps this counter
  // store; react by opening the picker once the sheet itself is open. A
  // counter (not a boolean) so a second press while the picker is already
  // showing still re-focuses/refreshes it. Seeded from the store's current
  // value (not 0) so mounting this component doesn't spuriously fire.
  let lastPickerSignal = get(composeOpenPickerSignal);
  $effect(() => {
    const sig = $composeOpenPickerSignal;
    if (sig !== lastPickerSignal) {
      lastPickerSignal = sig;
      if ($composeOpen) void openPicker();
    }
  });

  function handleInput(): void {
    adjustHeight();
    const val = textareaEl?.value ?? '';
    if (!showPicker) {
      // The whole content is now exactly '/' — i.e. it was empty and the
      // user just typed '/'. Anything else (a '/' appended to existing
      // text) is a literal slash, not the picker trigger.
      if (val === '/') void openPicker();
      return;
    }
    if (!val.startsWith('/')) {
      // The picker's premise (a leading '/') is gone — deleted, selected
      // over, pasted over. Dismiss into whatever literal text remains.
      closePicker();
      return;
    }
    pickerQuery = val.slice(1);
    pickerIndex = 0;
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
  //   (picker open)    → ↑↓ move selection, Enter inserts, Esc dismisses;
  //                       every other key (including further filter
  //                       characters) falls through to normal typing.
  //   (placeholders)   → Tab jumps to the next `{placeholder}` tab-stop
  //                       instead of inserting a literal tab.
  //   Enter            → submit (send to the active tab)
  //   Alt+Enter        → newline (the universal "soft return")
  //   Shift+Enter      → newline (textarea default; left untouched)
  //   Ctrl/Cmd+Enter   → left for the configurable `submit_compose` shortcut
  //   Tab              → literal tab at the caret (not focus-cycle)
  // Plain Enter is also bound as the default `submit_compose` shortcut, which
  // the dispatcher handles first; this is the fallback for users whose submit
  // key isn't Enter.
  function handleKeydown(e: KeyboardEvent): void {
    if (showPicker) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        pickerIndex = Math.min(pickerIndex + 1, Math.max(pickerFiltered.length - 1, 0));
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        pickerIndex = Math.max(pickerIndex - 1, 0);
        return;
      }
      if (e.key === 'Enter') {
        e.preventDefault();
        void insertTemplate(pickerIndex);
        return;
      }
      if (e.key === 'Escape') {
        e.preventDefault();
        closePicker();
        return;
      }
      // Anything else (more filter characters, Backspace, Tab, …) falls
      // through to the textarea, whose own `oninput` re-syncs the query.
    }

    // V14 Phase A: while the draft still has unresolved `{placeholder}`
    // tab-stops (from a just-inserted template), Tab cycles to the next
    // one instead of inserting a literal tab. `hasPlaceholder` re-scans the
    // live text, so this scope shrinks to nothing — and stops fighting the
    // textarea's normal Tab behavior below — the moment every placeholder
    // has been overtyped.
    if (
      !showPicker &&
      hasPlaceholder($composeContent) &&
      e.key === 'Tab' &&
      !e.shiftKey && !e.ctrlKey && !e.altKey && !e.metaKey
    ) {
      e.preventDefault();
      const ta = textareaEl;
      if (ta) {
        const ph = nextPlaceholderRange(ta.value, ta.selectionEnd ?? 0);
        if (ph) ta.setSelectionRange(ph.start, ph.end);
      }
      return;
    }

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

  // ── V14 Phase B: image paste/drop ───────────────────────────────────
  // Paste: `event.clipboardData.items` gives the MIME types synchronously
  // (a plain DOM capability, unlike the denied `navigator.clipboard.read()`)
  // — used ONLY to decide whether this is an image paste. A text-only paste
  // returns from `clipboardHasImage` false and the event is left completely
  // alone, so normal text paste behavior is untouched. An image paste is
  // `preventDefault`ed (there's no textarea-native way to paste an image
  // anyway) and the actual pixels are fetched via the Tauri clipboard
  // plugin (`readClipboardImagePng`), which WebView2 does allow.
  function handlePaste(e: ClipboardEvent): void {
    const types = Array.from(e.clipboardData?.items ?? []).map((item) => item.type);
    if (!clipboardHasImage(types)) return;
    e.preventDefault();
    void attachClipboardImage();
  }

  async function attachClipboardImage(): Promise<void> {
    const bytes = await readClipboardImagePng();
    if (!bytes) return; // no image, or re-encoding failed — nothing to attach
    try {
      const path = await composeAttachImage(bytes);
      composeAttachments.update((a) => [...a, path]);
    } catch (e) {
      console.warn('compose_attach_image failed:', e);
    }
  }

  function removeAttachment(path: string): void {
    composeAttachments.update((a) => a.filter((p) => p !== path));
  }

  /// Chip label — the file name, not the full absolute path (the full path
  /// is still available as the chip's `title` tooltip).
  function attachmentName(path: string): string {
    return path.split(/[\\/]/).pop() ?? path;
  }

  // Drop: the Tauri NATIVE drag-drop event (not HTML5 DOM drag events —
  // `dragDropEnabled` defaults on, which is what makes this the right event
  // to listen to). Registered once for the component's lifetime;
  // `$composeOpen` is checked inside the handler so drops are only acted on
  // while the sheet is actually showing — the terminal beneath keeps
  // whatever drop behavior it already has the rest of the time. Files are
  // referenced IN PLACE (no copy — `filterImagePaths` just filters the
  // native absolute paths the OS already gave us down to image extensions).
  listenManaged(() =>
    getCurrentWebview().onDragDropEvent((event) => {
      if (!get(composeOpen)) return;
      if (event.payload.type !== 'drop') return;
      const images = filterImagePaths(event.payload.paths);
      if (images.length === 0) return;
      composeAttachments.update((a) => [...a, ...images]);
    }),
  );

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
    {#if showPicker}
      <TemplatePicker
        templates={pickerFiltered}
        activeIndex={pickerIndex}
        onPick={(i) => void insertTemplate(i)}
      />
    {/if}
    {#if $composeAttachments.length > 0}
      <div class="attachments-row">
        {#each $composeAttachments as path (path)}
          <span class="attachment-chip" title={path}>
            <span class="attachment-name">{attachmentName(path)}</span>
            <button
              type="button"
              class="attachment-remove"
              onclick={() => removeAttachment(path)}
              aria-label="Remove attachment {attachmentName(path)}"
            >×</button>
          </span>
        {/each}
      </div>
    {/if}
    <div class="compose-row">
      <button
        type="button"
        class="template-btn"
        onclick={() => void openPicker()}
        title="Insert prompt template"
        aria-label="Insert prompt template"
      >📋</button>
      <textarea
        bind:this={textareaEl}
        bind:value={$composeContent}
        oninput={handleInput}
        onkeydown={handleKeydown}
        onpaste={handlePaste}
        onfocus={handleFocus}
        onblur={handleBlur}
        spellcheck="true"
        placeholder="Compose message... (/ for templates, paste/drop an image to attach)"
        style="min-height: {minHeight}px; max-height: {maxHeight}px;"
      ></textarea>
    </div>
  </div>
{/if}

<style>
  .compose-sheet {
    position: absolute;
    bottom: 0;
    left: 0;
    right: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2, 6px);
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

  .attachments-row {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  .attachment-chip {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    max-width: 220px;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    border-radius: var(--radius-md);
    padding: 3px 4px 3px 8px;
    font-size: 12px;
    color: var(--text-primary);
  }

  .attachment-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .attachment-remove {
    flex: 0 0 auto;
    display: flex;
    align-items: center;
    justify-content: center;
    width: 16px;
    height: 16px;
    background: none;
    border: none;
    border-radius: var(--radius-sm);
    cursor: pointer;
    color: var(--text-secondary);
    font-size: 13px;
    line-height: 1;
  }

  .attachment-remove:hover {
    background: var(--surface-3);
    color: var(--text-primary);
  }

  .compose-row {
    display: flex;
    align-items: flex-end;
    gap: 8px;
  }

  .template-btn {
    flex: 0 0 auto;
    height: 32px;
    width: 32px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    border-radius: var(--radius-md);
    cursor: pointer;
    font-size: 15px;
    line-height: 1;
    color: var(--text-primary);
  }

  .template-btn:hover {
    border-color: var(--accent);
  }

  textarea {
    flex: 1 1 auto;
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
